use std::{
  collections::HashSet,
  sync::{Arc, atomic::AtomicBool},
};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
  Render, SharedString, Styled, Subscription, Task, Transformation, Window, actions, div, point,
  prelude::*, px, rems, rgb,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  audio::{
    AudioState,
    pulse::{SetMute, SetVolume},
    types::{SourceEvent, SourceId, SourceInfo, SourceListEvent},
  },
  db::DB,
  icon::{Icon, IconName},
  matcher::MatcherPool,
  picker::{Category, Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, h_flex, v_flex},
};

use super::{VolumeBar, sinks::sink_icon};

struct SourceFavoritesDb;

impl SourceFavoritesDb {
  fn ensure_table() {
    let conn = DB.lock();
    conn
      .execute_batch(
        "CREATE TABLE IF NOT EXISTS source_favorites (source_name TEXT PRIMARY KEY) STRICT;",
      )
      .log_err();
  }

  fn toggle_favorite(source_name: &str) {
    let conn = DB.lock();
    let exists = conn
      .prepare_cached("SELECT 1 FROM source_favorites WHERE source_name = ?1")
      .and_then(|mut stmt| stmt.exists([source_name]))
      .unwrap_or(false);

    if exists {
      conn
        .prepare_cached("DELETE FROM source_favorites WHERE source_name = ?1")
        .and_then(|mut stmt| stmt.execute([source_name]))
        .log_err();
    } else {
      conn
        .prepare_cached("INSERT INTO source_favorites (source_name) VALUES (?1)")
        .and_then(|mut stmt| stmt.execute([source_name]))
        .log_err();
    }
  }

  fn get_favorites() -> HashSet<String> {
    let conn = DB.lock();
    let Ok(mut stmt) = conn.prepare_cached("SELECT source_name FROM source_favorites") else {
      return HashSet::new();
    };

    let Ok(rows) = stmt.query_map([], |row| row.get(0)) else {
      return HashSet::new();
    };

    rows.filter_map(|row| row.ok()).collect()
  }
}

fn source_icon(icon_name: Option<&str>, device_class: Option<&str>, muted: bool) -> IconName {
  if device_class == Some("monitor") {
    return sink_icon(icon_name, muted);
  }

  let Some(icon_name) = icon_name else {
    return if muted {
      IconName::MicrophoneOff
    } else {
      IconName::Microphone
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
  } else if icon_name.contains("card") {
    if muted {
      IconName::VolumeOff
    } else {
      IconName::Volume
    }
  } else {
    if muted {
      IconName::MicrophoneOff
    } else {
      IconName::Microphone
    }
  }
}

#[derive(Clone)]
pub struct SourceEntry {
  id: SourceId,
  name: Option<SharedString>,
  description: Option<SharedString>,
  device_class: Option<SharedString>,
  port_available: Option<bool>,
  is_favorite: bool,
  search_string: String,
  entry: Entity<SourceEntryInner>,
}

impl SourceEntry {
  pub fn new(
    source: SourceInfo,
    favorites: &HashSet<String>,
    audio_state: &Entity<AudioState>,
    window: &mut Window,
    cx: &mut App,
  ) -> Self {
    let id = source.id;
    let name = source.name.clone();
    let description = source.description.clone();
    let device_class = source.device_class.clone();
    let port_available = source.port_available;
    let is_favorite = name
      .as_ref()
      .map(|n| favorites.contains(n.as_ref()))
      .unwrap_or(false);
    let mut search_parts = Vec::new();
    if let Some(ref name) = source.name {
      search_parts.push(name.to_string());
    }
    if let Some(ref desc) = source.description {
      search_parts.push(desc.to_string());
    }
    let search_string = search_parts.join(" ");
    let entry = cx.new(|cx| SourceEntryInner::new(source, audio_state, window, cx));

    Self {
      id,
      name,
      description,
      device_class,
      port_available,
      is_favorite,
      search_string,
      entry,
    }
  }

  fn is_monitor(&self) -> bool {
    self.device_class.as_ref().map(|s| s.as_ref()) == Some("monitor")
  }
}

pub struct SourceEntryInner {
  source: SourceInfo,
  _event_listener: Task<()>,
}

pub struct SourceStateChanged;

impl gpui::EventEmitter<SourceStateChanged> for SourceEntryInner {}

impl SourceEntryInner {
  pub fn new(
    source: SourceInfo,
    audio_state: &Entity<AudioState>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let source_id = source.id;
    let event_rx = audio_state.read(cx).subscribe_source(source_id);

    let event_listener = cx.spawn_in(window, async move |this, cx| {
      while let Ok(event) = event_rx.recv_async().await {
        let should_break = matches!(event, SourceEvent::Removed);

        let _ = this.update(cx, |this, cx| {
          match event {
            SourceEvent::VolumeChanged(volume) => {
              this.source.volume = volume;
              cx.notify();
            }
            SourceEvent::MuteChanged(mute) => {
              this.source.mute = mute;
              cx.notify();
            }
            SourceEvent::InfoChanged(info) => {
              this.source = info;
              cx.emit(SourceStateChanged);
              cx.notify();
            }
            SourceEvent::BecameDefault | SourceEvent::NoLongerDefault => {
              cx.notify();
            }
            SourceEvent::Removed => {
              // Source was removed, listener will stop
            }
          }
        });

        if should_break {
          break;
        }
      }
    });

    Self {
      source,
      _event_listener: event_listener,
    }
  }
}

pub struct AudioSourcesPanel {
  picker: Entity<Picker<SourcesDelegate>>,
  audio_state: Entity<AudioState>,
  sources: Vec<SourceEntry>,
  favorites: HashSet<String>,
  _subscriptions: Vec<Subscription>,
  _list_subscription_task: Task<()>,
  _initial_load_task: Task<()>,
}

const CONTEXT: &str = "sources";

actions!(sources, [VolumeUp, VolumeDown, Mute, ToggleFavorite]);

impl AudioSourcesPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
      KeyBinding::new("ctrl-f", ToggleFavorite, Some(CONTEXT)),
    ]);

    SourceFavoritesDb::ensure_table();
    let favorites = SourceFavoritesDb::get_favorites();

    let audio_state = AudioState::global(cx);
    let delegate = SourcesDelegate {
      audio_state: audio_state.clone(),
      favorites: Arc::new(favorites.clone()),
    };

    // Start with empty picker
    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search audio inputs...", cx);
      picker
    });

    // Load initial data async
    let initial_load_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        let sources = cx
          .update(|_, cx| audio_state.read(cx).list_sources(cx))
          .ok();
        let Some(sources_task) = sources else { return };
        let sources = sources_task.await;

        let _ = this.update_in(cx, |this, window, cx| {
          this.sources = sources
            .into_iter()
            .map(|source| SourceEntry::new(source, &this.favorites, &audio_state, window, cx))
            .collect();

          let entries: Vec<_> = this.sources.clone();
          for entry in &entries {
            this.subscribe_to_source_entry(entry, window, cx);
          }

          let favorites = Arc::new(this.favorites.clone());
          picker.update(cx, |picker, cx| {
            picker.delegate.favorites = favorites;
            picker.set_items(this.sources.clone(), window, cx);
          });
        });
      }
    });

    // Subscribe to list changes
    let list_rx = audio_state.read(cx).subscribe_source_list();
    let list_subscription_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        while let Ok(event) = list_rx.recv_async().await {
          match event {
            SourceListEvent::Added(source_info) => {
              let _ = this.update_in(cx, |this, window, cx| {
                let new_entry =
                  SourceEntry::new(source_info, &this.favorites, &audio_state, window, cx);
                this.subscribe_to_source_entry(&new_entry, window, cx);
                this.sources.push(new_entry);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sources.clone(), window, cx);
                });
              });
            }
            SourceListEvent::Removed(source_id) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this.sources.retain(|entry| entry.id != source_id);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sources.clone(), window, cx);
                });
              });
            }
            SourceListEvent::DefaultChanged(_) => {
              // Just notify to re-render (default_source updated via AudioState's internal sub)
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
            .update(cx, |state, cx| state.set_default_source(entry.id, cx))
            .detach_and_log_err(cx);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    Self {
      picker,
      audio_state,
      sources: Vec::new(),
      favorites,
      _subscriptions: subscriptions,
      _list_subscription_task: list_subscription_task,
      _initial_load_task: initial_load_task,
    }
  }

  fn subscribe_to_source_entry(
    &mut self,
    entry: &SourceEntry,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let picker = self.picker.clone();
    self._subscriptions.push(cx.subscribe_in(
      &entry.entry,
      window,
      move |this, entry, _event: &SourceStateChanged, window, cx| {
        let source = &entry.read(cx).source;
        if let Some(source_entry) = this.sources.iter_mut().find(|e| e.entry == *entry) {
          source_entry.name = source.name.clone();
          source_entry.description = source.description.clone();
          source_entry.device_class = source.device_class.clone();
          source_entry.port_available = source.port_available;
          source_entry.is_favorite = source_entry
            .name
            .as_ref()
            .map(|n| this.favorites.contains(n.as_ref()))
            .unwrap_or(false);
        }
        picker.update(cx, |picker, cx| {
          picker.set_items(this.sources.clone(), window, cx);
        });
      },
    ));
  }

  fn toggle_favorite(&mut self, _: &ToggleFavorite, window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected) = self.picker.read(cx).get_selected_item().cloned() else {
      return;
    };
    let Some(source_name) = selected.name.as_ref() else {
      return;
    };

    SourceFavoritesDb::toggle_favorite(source_name);

    if self.favorites.contains(source_name.as_ref()) {
      self.favorites.remove(source_name.as_ref());
    } else {
      self.favorites.insert(source_name.to_string());
    }

    self.sync_favorites(window, cx);
  }

  fn sync_favorites(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    for entry in &mut self.sources {
      entry.is_favorite = entry
        .name
        .as_ref()
        .map(|n| self.favorites.contains(n.as_ref()))
        .unwrap_or(false);
    }

    let favorites = Arc::new(self.favorites.clone());
    let sources = self.sources.clone();
    self.picker.update(cx, |picker, cx| {
      picker.delegate.favorites = favorites;
      picker.set_items(sources, window, cx);
    });
  }

  fn get_selected_id(&self, cx: &mut Context<Self>) -> Option<SourceId> {
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
      .set_source_volume(selected_id, SetVolume::RelativePercent(1));
  }

  fn volume_down(&mut self, _: &VolumeDown, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_source_volume(selected_id, SetVolume::RelativePercent(-1));
  }

  fn mute(&mut self, _: &Mute, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_source_mute(selected_id, SetMute::Toggle)
  }
}

impl Focusable for AudioSourcesPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for AudioSourcesPanel {
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

struct SourcesDelegate {
  audio_state: Entity<AudioState>,
  favorites: Arc<HashSet<String>>,
}

impl PickerDelegate for SourcesDelegate {
  type ListItem = SourceEntry;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let source = &item.entry.read(cx).source;
    let is_default = self.audio_state.read(cx).default_source == Some(source.id);
    let is_unavailable = source.port_available == Some(false);
    let is_dimmed = source.mute || is_unavailable;
    let volume_percent = source.volume.as_percent(source.base_volume);
    let icon = source_icon(
      source.icon_name.as_ref().map(|s| s.as_ref()),
      source.device_class.as_ref().map(|s| s.as_ref()),
      source.mute,
    );

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
                source
                  .description
                  .clone()
                  .unwrap_or_else(|| source.name.clone().unwrap_or_default()),
              ),
          )
          .child(
            div()
              .text_xs()
              .text_color(rgb(0x888888))
              .when(source.mute || is_unavailable, |div| {
                div.text_color(rgb(0x666666)).font_weight(FontWeight::BOLD)
              })
              .child(if is_unavailable {
                "UNAVAILABLE".to_string()
              } else if source.mute {
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
      Category::new("Favorites", |entry: &SourceEntry| {
        entry.port_available != Some(false) && entry.is_favorite
      }),
      Category::new("Microphones", |entry: &SourceEntry| {
        entry.port_available != Some(false) && !entry.is_monitor() && !entry.is_favorite
      }),
      Category::new("Monitors", |entry: &SourceEntry| {
        entry.port_available != Some(false) && entry.is_monitor() && !entry.is_favorite
      }),
      Category::new("Unavailable", |entry: &SourceEntry| {
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
