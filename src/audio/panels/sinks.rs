use std::{
  collections::HashSet,
  sync::{Arc, atomic::AtomicBool},
};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
  Render, RenderOnce, SharedString, Styled, Subscription, Task, Transformation, Window, actions,
  div, point, prelude::*, px, relative, rems, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  audio::{
    AudioState,
    pulse::{SetMute, SetVolume},
    types::{SinkEvent, SinkId, SinkInfo, SinkListEvent},
  },
  db::DB,
  icon::{Icon, IconName},
  matcher::MatcherPool,
  picker::{Category, Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, h_flex, v_flex},
};

pub(crate) struct SinkFavoritesDb;

impl SinkFavoritesDb {
  fn ensure_table() {
    let conn = DB.lock();
    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS sink_favorites (sink_name TEXT PRIMARY KEY) STRICT;",
      )
      .log_err();
  }

  fn toggle_favorite(sink_name: &str) {
    let conn = DB.lock();
    let exists = conn
      .prepare_cached("SELECT 1 FROM sink_favorites WHERE sink_name = ?1")
      .and_then(|mut stmt| stmt.exists([sink_name]))
      .unwrap_or(false);

    if exists {
      conn
        .prepare_cached("DELETE FROM sink_favorites WHERE sink_name = ?1")
        .and_then(|mut stmt| stmt.execute([sink_name]))
        .log_err();
    } else {
      conn
        .prepare_cached("INSERT INTO sink_favorites (sink_name) VALUES (?1)")
        .and_then(|mut stmt| stmt.execute([sink_name]))
        .log_err();
    }
  }

  pub(crate) fn get_favorites() -> HashSet<String> {
    let conn = DB.lock();
    let Ok(mut stmt) = conn.prepare_cached("SELECT sink_name FROM sink_favorites") else {
      return HashSet::new();
    };

    let Ok(rows) = stmt.query_map([], |row| row.get(0)) else {
      return HashSet::new();
    };

    rows.filter_map(|row| row.ok()).collect()
  }
}

#[derive(Clone)]
pub struct SinkEntry {
  id: SinkId,
  name: Option<SharedString>,
  description: Option<SharedString>,
  search_string: String,
  port_available: Option<bool>,
  is_favorite: bool,
  entry: Entity<SinkEntryInner>,
}

impl SinkEntry {
  pub fn new(
    sink: SinkInfo,
    favorites: &HashSet<String>,
    audio_state: &Entity<AudioState>,
    window: &mut Window,
    cx: &mut App,
  ) -> Self {
    let id = sink.id;
    let name = sink.name.clone();
    let description = sink.description.clone();
    let port_available = sink.port_available;
    let is_favorite = name
      .as_ref()
      .map(|n| favorites.contains(n.as_ref()))
      .unwrap_or(false);
    let mut search_parts = Vec::new();
    if let Some(ref name) = sink.name {
      search_parts.push(name.to_string());
    }
    if let Some(ref desc) = sink.description {
      search_parts.push(desc.to_string());
    }
    let search_string = search_parts.join(" ");
    let entry = cx.new(|cx| SinkEntryInner::new(sink, audio_state, window, cx));

    Self {
      id,
      name,
      description,
      search_string,
      port_available,
      is_favorite,
      entry,
    }
  }
}

pub struct SinkEntryInner {
  sink: SinkInfo,
  _event_listener: Task<()>,
}

pub struct SinkStateChanged;

impl gpui::EventEmitter<SinkStateChanged> for SinkEntryInner {}

impl SinkEntryInner {
  pub fn new(
    sink: SinkInfo,
    audio_state: &Entity<AudioState>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let sink_id = sink.id;
    let event_rx = audio_state.read(cx).subscribe_sink(sink_id);

    let event_listener = cx.spawn_in(window, async move |this, cx| {
      while let Ok(event) = event_rx.recv_async().await {
        let should_break = matches!(event, SinkEvent::Removed);

        let _ = this.update(cx, |this, cx| {
          match event {
            SinkEvent::VolumeChanged(volume) => {
              this.sink.volume = volume;
              cx.notify();
            }
            SinkEvent::MuteChanged(mute) => {
              this.sink.mute = mute;
              cx.notify();
            }
            SinkEvent::InfoChanged(info) => {
              this.sink = info;
              cx.emit(SinkStateChanged);
              cx.notify();
            }
            SinkEvent::BecameDefault | SinkEvent::NoLongerDefault => {
              cx.notify();
            }
            SinkEvent::Removed => {
              // Sink was removed, listener will stop
            }
          }
        });

        if should_break {
          break;
        }
      }
    });

    Self {
      sink,
      _event_listener: event_listener,
    }
  }
}

pub struct AudioSinksPanel {
  picker: Entity<Picker<SinksDelegate>>,
  audio_state: Entity<AudioState>,
  sinks: Vec<SinkEntry>,
  favorites: HashSet<String>,
  _subscriptions: Vec<Subscription>,
  _list_subscription_task: Task<()>,
  _initial_load_task: Task<()>,
}

const CONTEXT: &str = "sinks";

actions!(sinks, [VolumeUp, VolumeDown, Mute, ToggleFavorite]);

impl AudioSinksPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
      KeyBinding::new("ctrl-f", ToggleFavorite, Some(CONTEXT)),
    ]);

    SinkFavoritesDb::ensure_table();
    let favorites = SinkFavoritesDb::get_favorites();

    let audio_state = AudioState::global(cx);
    let delegate = SinksDelegate {
      audio_state: audio_state.clone(),
      favorites: Arc::new(favorites.clone()),
    };

    // Start with empty picker
    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search audio outputs...", cx);
      picker
    });

    // Load initial data async
    let initial_load_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        let sinks = cx.update(|_, cx| audio_state.read(cx).list_sinks(cx)).ok();
        let Some(sinks_task) = sinks else { return };
        let sinks = sinks_task.await;

        let _ = this.update_in(cx, |this, window, cx| {
          this.sinks = sinks
            .into_iter()
            .map(|sink| SinkEntry::new(sink, &this.favorites, &audio_state, window, cx))
            .collect();

          let entries: Vec<_> = this.sinks.clone();
          for entry in &entries {
            this.subscribe_to_sink_entry(entry, window, cx);
          }

          let favorites = Arc::new(this.favorites.clone());
          picker.update(cx, |picker, cx| {
            picker.delegate.favorites = favorites;
            picker.set_items(this.sinks.clone(), window, cx);
          });
        });
      }
    });

    // Subscribe to list changes
    let list_rx = audio_state.read(cx).subscribe_sink_list();
    let list_subscription_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        while let Ok(event) = list_rx.recv_async().await {
          match event {
            SinkListEvent::Added(sink_info) => {
              let _ = this.update_in(cx, |this, window, cx| {
                let new_entry =
                  SinkEntry::new(*sink_info, &this.favorites, &audio_state, window, cx);
                this.subscribe_to_sink_entry(&new_entry, window, cx);
                this.sinks.push(new_entry);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sinks.clone(), window, cx);
                });
              });
            }
            SinkListEvent::Removed(sink_id) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this.sinks.retain(|entry| entry.id != sink_id);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sinks.clone(), window, cx);
                });
              });
            }
            SinkListEvent::DefaultChanged(_) => {
              // Just notify to re-render (default_sink updated via AudioState's internal sub)
              let _ = picker.update_in(cx, |_, _, cx| cx.notify());
            }
          }
        }
      }
    });

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, _window, cx| {
        if let PickerEvent::Picked(entry) = ev {
          if entry.port_available == Some(false) {
            return;
          }
          this
            .audio_state
            .update(cx, |state, cx| state.set_default_sink(entry.id, cx))
            .detach_and_log_err(cx);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    Self {
      picker,
      audio_state,
      sinks: Vec::new(),
      favorites,
      _subscriptions: subscriptions,
      _list_subscription_task: list_subscription_task,
      _initial_load_task: initial_load_task,
    }
  }

  fn subscribe_to_sink_entry(
    &mut self,
    entry: &SinkEntry,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let picker = self.picker.clone();
    self._subscriptions.push(cx.subscribe_in(
      &entry.entry,
      window,
      move |this, entry, _event: &SinkStateChanged, window, cx| {
        let sink = &entry.read(cx).sink;
        if let Some(sink_entry) = this.sinks.iter_mut().find(|e| e.entry == *entry) {
          sink_entry.name = sink.name.clone();
          sink_entry.description = sink.description.clone();
          sink_entry.port_available = sink.port_available;
          sink_entry.is_favorite = sink_entry
            .name
            .as_ref()
            .map(|n| this.favorites.contains(n.as_ref()))
            .unwrap_or(false);
        }
        picker.update(cx, |picker, cx| {
          picker.set_items(this.sinks.clone(), window, cx);
        });
      },
    ));
  }

  fn get_selected_id(&self, cx: &mut Context<Self>) -> Option<SinkId> {
    self
      .picker
      .read(cx)
      .get_selected_item()
      .map(|entry| entry.id)
  }

  fn volume_up(&mut self, _: &VolumeUp, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_sink_volume(selected_id, SetVolume::RelativePercent(1));
  }

  fn volume_down(&mut self, _: &VolumeDown, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_sink_volume(selected_id, SetVolume::RelativePercent(-1));
  }

  fn mute(&mut self, _: &Mute, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_sink_mute(selected_id, SetMute::Toggle)
  }

  fn toggle_favorite(&mut self, _: &ToggleFavorite, window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected) = self.picker.read(cx).get_selected_item().cloned() else {
      return;
    };
    let Some(sink_name) = selected.name.as_ref() else {
      return;
    };

    SinkFavoritesDb::toggle_favorite(sink_name);

    if self.favorites.contains(sink_name.as_ref()) {
      self.favorites.remove(sink_name.as_ref());
    } else {
      self.favorites.insert(sink_name.to_string());
    }

    self.sync_favorites(window, cx);
  }

  fn sync_favorites(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    for entry in &mut self.sinks {
      entry.is_favorite = entry
        .name
        .as_ref()
        .map(|n| self.favorites.contains(n.as_ref()))
        .unwrap_or(false);
    }

    let favorites = Arc::new(self.favorites.clone());
    let sinks = self.sinks.clone();
    self.picker.update(cx, |picker, cx| {
      picker.delegate.favorites = favorites;
      picker.set_items(sinks, window, cx);
    });
  }
}

impl Focusable for AudioSinksPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for AudioSinksPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .on_action(cx.listener(Self::volume_up))
      .on_action(cx.listener(Self::volume_down))
      .on_action(cx.listener(Self::mute))
      .on_action(cx.listener(Self::toggle_favorite))
      .child(picker_input(&self.picker).show_back_button(true))
      .child(picker_results(&self.picker))
  }
}

#[derive(IntoElement)]
pub struct VolumeBar {
  volume_percent: u32,
  is_muted: bool,
  is_default: bool,
  is_selected: bool,
}

// TODO: This can just be a function?
impl VolumeBar {
  pub fn new(volume_percent: u32, is_muted: bool, is_selected: bool) -> Self {
    Self {
      volume_percent,
      is_muted,
      is_selected,
      is_default: false,
    }
  }

  pub fn default(mut self, is_default: bool) -> Self {
    self.is_default = is_default;
    self
  }
}

impl RenderOnce for VolumeBar {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    let fill_percentage = (self.volume_percent as f32 / 100.0).min(1.0);

    let (track_color, fill_color) = match (self.is_muted, self.is_default, self.is_selected) {
      (true, true, true) => (rgba(0x3B82F60C), rgba(0x3B82F618)),
      (true, true, false) => (rgba(0x3B82F608), rgba(0x3B82F610)),
      (true, false, true) => (rgba(0xFFFFFF08), rgba(0xFFFFFF0C)),
      (true, false, false) => (rgba(0xFFFFFF03), rgba(0xFFFFFF05)),
      (_, true, true) => (rgba(0x3B82F618), rgba(0x3B82F635)),
      (_, true, false) => (rgba(0x3B82F610), rgba(0x3B82F625)),
      (_, false, true) => (rgba(0xFFFFFF0C), rgba(0xFFFFFF22)),
      (_, false, false) => (rgba(0xFFFFFF06), rgba(0xFFFFFF15)),
    };

    let inset_y = px(4.0);
    let inset_x = px(0.0);
    let rounding = px(8.0);

    div()
      .absolute()
      .top(inset_y)
      .bottom(inset_y)
      .left(inset_x)
      .right(inset_x)
      .rounded(rounding)
      .bg(track_color)
      .child(
        div()
          .h_full()
          .rounded(rounding)
          .bg(fill_color)
          .w(relative(fill_percentage)),
      )
  }
}

pub fn sink_icon(icon_name: Option<&str>, muted: bool) -> IconName {
  let Some(icon_name) = icon_name else {
    return if muted {
      IconName::DeviceSpeakerOff
    } else {
      IconName::DeviceSpeaker
    };
  };

  if icon_name.contains("headphone") {
    if muted {
      IconName::HeadphonesOff
    } else {
      IconName::Headphones
    }
  } else if icon_name.contains("headset") {
    if muted {
      IconName::HeadsetOff
    } else {
      IconName::Headset
    }
  } else if icon_name.contains("speaker") {
    if muted {
      IconName::DeviceSpeakerOff
    } else {
      IconName::DeviceSpeaker
    }
  } else if icon_name.contains("card") {
    if muted {
      IconName::VolumeOff
    } else {
      IconName::Volume
    }
  } else {
    if muted {
      IconName::DeviceSpeakerOff
    } else {
      IconName::DeviceSpeaker
    }
  }
}

struct SinksDelegate {
  audio_state: Entity<AudioState>,
  favorites: Arc<HashSet<String>>,
}

impl PickerDelegate for SinksDelegate {
  type ListItem = SinkEntry;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let sink = &item.entry.read(cx).sink;
    let is_default = self.audio_state.read(cx).default_sink == Some(sink.id);
    let is_unavailable = sink.port_available == Some(false);
    let is_dimmed = sink.mute || is_unavailable;
    let volume_percent = sink.volume.as_percent(sink.base_volume);
    let icon = sink_icon(sink.form_factor.as_ref().map(|s| s.as_ref()), sink.mute);

    v_flex()
      .w_full()
      .relative()
      .px_2()
      .py_3()
      .rounded_md()
      .child(VolumeBar::new(volume_percent, is_dimmed, is_selected).default(is_default))
      .child(
        h_flex()
          .w_full()
          .gap_2()
          .child({
            let icon_color = match (is_dimmed, is_default, is_selected) {
              (true, _, _) => rgb(0x555555),
              (_, true, true) => rgb(0x6EA8F0),
              (_, true, false) => rgb(0x5B93D5),
              (_, false, true) => rgb(0xBBBBBB),
              (_, false, false) => rgb(0x888888),
            };
            Icon::new(icon).size(rems(1.1)).text_color(icon_color)
          })
          .when(item.is_favorite, |this| {
            this.child(
              Icon::new(IconName::StarFilled)
                .size(rems(0.85))
                .transform(Transformation::translate(point(px(0.), px(-0.75))))
                .text_color(rgb(0xD4A017)),
            )
          })
          .child(
            h_flex()
              .flex_1()
              .text_ellipsis()
              .overflow_x_hidden()
              .when(is_dimmed, |div| div.opacity(0.5))
              .child(
                sink
                  .description
                  .clone()
                  .unwrap_or_else(|| sink.name.clone().unwrap_or_default()),
              ),
          )
          .child(
            div()
              .text_xs()
              .text_color(rgb(0x888888))
              .when(sink.mute || is_unavailable, |div| {
                div.text_color(rgb(0x666666)).font_weight(FontWeight::BOLD)
              })
              .child(if is_unavailable {
                "UNAVAILABLE".to_string()
              } else if sink.mute {
                "MUTE".to_string()
              } else {
                format!("{}%", volume_percent)
              }),
          ),
      )
  }

  fn sort_items(&self, _cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by_key(|(index, score)| {
      let description = items[*index].description.clone().unwrap_or_default();
      (std::cmp::Reverse(*score), description)
    });
  }

  fn categories(&self) -> Option<Vec<Category<Self::ListItem>>> {
    Some(vec![
      Category::new("Favorites", |entry: &SinkEntry| {
        entry.port_available != Some(false) && entry.is_favorite
      }),
      Category::new("Available", |entry: &SinkEntry| {
        entry.port_available != Some(false) && !entry.is_favorite
      }),
      Category::new("Unavailable", |entry: &SinkEntry| {
        entry.port_available == Some(false)
      }),
    ])
  }

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    query: String,
    _cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    if query.is_empty() {
      cx.defer_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, None);
      });

      return Task::ready(());
    }

    let matchers = MatcherPool::global(cx);
    cx.spawn_in(window, async move |cx, window| {
      let mut matcher = matchers.get().await.unwrap();
      let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
      let mut matches = Vec::new();
      let mut buf = Vec::new();

      for (index, item) in items.iter().enumerate() {
        if let Some(score) =
          needle.score(Utf32Str::new(&item.search_string, &mut buf), &mut matcher)
        {
          matches.push((index, score));
        }
      }

      cx.update_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, Some(matches));
      })
      .log_err();
    })
  }
}
