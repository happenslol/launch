use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, ImageSource, IntoElement, KeyBinding,
  Render, Resource, SharedString, Styled, Subscription, Task, Window, actions, div, img,
  prelude::*, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  audio::{
    AudioState,
    pulse::{SetMute, SetVolume},
    types::{
      SinkId, SinkInfo, SinkInputEvent, SinkInputId, SinkInputInfo, SinkInputListEvent,
      SinkListEvent,
    },
  },
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  submenu::{SubMenu, SubMenuEvent},
  util::{ResultExt, h_flex, v_flex},
  xdg::XdgIconCache,
};

use super::VolumeBar;

#[derive(Clone)]
pub struct StreamEntry {
  id: SinkInputId,
  search_string: String,
  entry: Entity<SinkInputEntryInner>,
}

impl StreamEntry {
  pub fn new(
    sink_input: SinkInputInfo,
    audio_state: &Entity<AudioState>,
    sinks: &[SinkInfo],
    window: &mut Window,
    cx: &mut App,
  ) -> Self {
    let id = sink_input.id;
    let mut search_parts = Vec::new();
    if let Some(ref name) = sink_input.name {
      search_parts.push(name.to_string());
    }
    if let Some(ref app_name) = sink_input.application_name {
      search_parts.push(app_name.to_string());
    }
    let search_string = search_parts.join(" ");
    let entry = cx.new(|cx| SinkInputEntryInner::new(sink_input, audio_state, sinks, window, cx));

    Self {
      id,
      search_string,
      entry,
    }
  }
}

pub struct SinkInputEntryInner {
  sink_input: SinkInputInfo,
  sink_description: Option<SharedString>,
  _event_listener: Task<()>,
}

impl SinkInputEntryInner {
  pub fn new(
    sink_input: SinkInputInfo,
    audio_state: &Entity<AudioState>,
    sinks: &[SinkInfo],
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let sink_input_id = sink_input.id;
    let event_rx = audio_state.read(cx).subscribe_sink_input(sink_input_id);

    let sink_description = sinks
      .iter()
      .find(|s| s.id == sink_input.sink_id)
      .and_then(|s| s.description.clone().or_else(|| s.name.clone()));

    let event_listener = cx.spawn_in(window, async move |this, cx| {
      while let Ok(event) = event_rx.recv_async().await {
        let should_break = matches!(event, SinkInputEvent::Removed);

        let _ = this.update(cx, |this, cx| match event {
          SinkInputEvent::VolumeChanged(volume) => {
            this.sink_input.volume = volume;
            cx.notify();
          }
          SinkInputEvent::MuteChanged(mute) => {
            this.sink_input.mute = mute;
            cx.notify();
          }
          SinkInputEvent::SinkChanged(sink_id) => {
            this.sink_input.sink_id = sink_id;
            cx.notify();
          }
          SinkInputEvent::InfoChanged(info) => {
            this.sink_input = info;
            cx.notify();
          }
          SinkInputEvent::Removed => {}
        });

        if should_break {
          break;
        }
      }
    });

    Self {
      sink_input,
      sink_description,
      _event_listener: event_listener,
    }
  }

  pub fn update_sink_description(&mut self, sinks: &[SinkInfo]) {
    self.sink_description = sinks
      .iter()
      .find(|s| s.id == self.sink_input.sink_id)
      .and_then(|s| s.description.clone().or_else(|| s.name.clone()));
  }
}

pub struct AudioStreamsPanel {
  picker: Entity<Picker<StreamsDelegate>>,
  sink_submenu: Option<(SinkInputId, Entity<SubMenu<SinkPickerDelegate>>)>,
  audio_state: Entity<AudioState>,
  sink_inputs: Vec<StreamEntry>,
  sinks: Vec<SinkInfo>,
  _subscriptions: Vec<Subscription>,
  _list_subscription_task: Task<()>,
  _sink_list_subscription_task: Task<()>,
  _initial_load_task: Task<()>,
}

const CONTEXT: &str = "streams";

actions!(streams, [VolumeUp, VolumeDown, Mute]);

impl AudioStreamsPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
    ]);

    let audio_state = AudioState::global(cx);
    let delegate = StreamsDelegate;

    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search playback streams...", cx);
      picker
    });

    // Load initial data async
    let initial_load_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        // Load sink inputs
        let sink_inputs = cx
          .update(|_, cx| audio_state.read(cx).list_sink_inputs(cx))
          .ok();
        let Some(sink_inputs_task) = sink_inputs else {
          return;
        };
        let sink_inputs = sink_inputs_task.await;

        // Load sinks for the picker
        let sinks = cx.update(|_, cx| audio_state.read(cx).list_sinks(cx)).ok();
        let Some(sinks_task) = sinks else { return };
        let sinks = sinks_task.await;

        let _ = this.update_in(cx, |this, window, cx| {
          this.sinks = sinks.clone();

          this.sink_inputs = sink_inputs
            .into_iter()
            .map(|input| StreamEntry::new(input, &audio_state, &sinks, window, cx))
            .collect();

          picker.update(cx, |picker, cx| {
            picker.set_items(this.sink_inputs.clone(), window, cx);
          });
        });
      }
    });

    // Subscribe to sink input list changes
    let list_rx = audio_state.read(cx).subscribe_sink_input_list();
    let list_subscription_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      async move |this, cx| {
        while let Ok(event) = list_rx.recv_async().await {
          match event {
            SinkInputListEvent::Added(input_info) => {
              let _ = this.update_in(cx, |this, window, cx| {
                let new_entry = StreamEntry::new(input_info, &audio_state, &this.sinks, window, cx);
                this.sink_inputs.push(new_entry);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sink_inputs.clone(), window, cx);
                });
              });
            }
            SinkInputListEvent::Removed(input_id) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this.sink_inputs.retain(|entry| entry.id != input_id);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sink_inputs.clone(), window, cx);
                });
              });
            }
          }
        }
      }
    });

    // Subscribe to sink list changes to keep sinks cache updated
    let sink_list_rx = audio_state.read(cx).subscribe_sink_list();
    let sink_list_subscription_task = cx.spawn_in(window, {
      let picker = picker.clone();
      async move |this, cx| {
        while let Ok(event) = sink_list_rx.recv_async().await {
          match event {
            SinkListEvent::Added(sink) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this.sinks.push(*sink.clone());
                // Update sink descriptions in entries
                for stream_entry in &this.sink_inputs {
                  stream_entry.entry.update(cx, |inner, cx| {
                    inner.update_sink_description(&this.sinks);
                    cx.notify();
                  });
                }
                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sink_inputs.clone(), window, cx);
                });
              });
            }
            SinkListEvent::Removed(sink_id) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this.sinks.retain(|s| s.id != sink_id);
                // Update sink descriptions in entries
                for stream_entry in &this.sink_inputs {
                  stream_entry.entry.update(cx, |inner, cx| {
                    inner.update_sink_description(&this.sinks);
                    cx.notify();
                  });
                }
                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sink_inputs.clone(), window, cx);
                });
              });
            }
            SinkListEvent::DefaultChanged(_) => {}
          }
        }
      }
    });

    // Handle picker selection - open sink picker
    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(stream_entry) = ev {
          let current_sink_id = stream_entry.entry.read(cx).sink_input.sink_id;
          this.open_sink_picker(stream_entry.id, current_sink_id, window, cx);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    Self {
      picker,
      sink_submenu: None,
      audio_state,
      sink_inputs: Vec::new(),
      sinks: Vec::new(),
      _subscriptions: subscriptions,
      _list_subscription_task: list_subscription_task,
      _sink_list_subscription_task: sink_list_subscription_task,
      _initial_load_task: initial_load_task,
    }
  }

  fn open_sink_picker(
    &mut self,
    input_id: SinkInputId,
    current_sink_id: SinkId,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let delegate = SinkPickerDelegate { current_sink_id };

    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(self.sinks.clone()), window, cx);
      picker.placeholder("Search audio outputs...", cx);
      picker
    });

    // Build header from stream info
    let stream_entry = self.sink_inputs.iter().find(|e| e.id == input_id);
    let (display_name, icon): (SharedString, Option<Resource>) =
      stream_entry.map_or(("Unknown Stream".into(), None), |entry| {
        let inner = entry.entry.read(cx);
        let sink_input = &inner.sink_input;
        let app_name = sink_input.application_name.clone();
        let media_name = sink_input.name.clone();
        let display_name = match (app_name.as_ref(), media_name) {
          (Some(app), Some(media)) => format!("{} \u{2022} {}", app, media).into(),
          (Some(app), None) => app.clone(),
          (None, Some(media)) => media,
          (None, None) => "Unknown Stream".into(),
        };
        let icon_cache = XdgIconCache::global(cx);
        let icon = app_name
          .as_ref()
          .and_then(|name| icon_cache.read(cx).get(&name.to_lowercase()).cloned());
        (display_name, icon)
      });

    let submenu = cx.new(|cx| {
      SubMenu::new(picker, window, cx).header(move |_window, _cx| {
        h_flex()
          .px_3()
          .py_2()
          .gap_3()
          .items_center()
          .overflow_x_hidden()
          .bg(rgb(0x1D1D1D))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .rounded_lg()
          .when_some(icon.clone(), |this, icon| {
            this.child(img(ImageSource::Resource(icon)).size_5())
          })
          .child(
            div()
              .flex_1()
              .text_ellipsis()
              .overflow_x_hidden()
              .child(display_name.clone()),
          )
          .into_any_element()
      })
    });

    self._subscriptions.push(cx.subscribe_in(
      &submenu,
      window,
      move |this, _submenu, ev: &PickerEvent<SinkPickerDelegate>, _window, cx| {
        if let PickerEvent::Picked(sink) = ev {
          let sink_id = sink.id;
          this.audio_state.read(cx).move_sink_input(input_id, sink_id);

          if let Some(stream_entry) = this.sink_inputs.iter().find(|e| e.id == input_id) {
            stream_entry.entry.update(cx, |inner, cx| {
              inner.sink_input.sink_id = sink_id;
              inner.update_sink_description(&this.sinks);
              cx.notify();
            });
          }
        }
      },
    ));

    self._subscriptions.push(cx.subscribe_in(
      &submenu,
      window,
      |this, _submenu, _ev: &SubMenuEvent, window, cx| {
        this.sink_submenu = None;
        cx.focus_view(&this.picker.read(cx).search_input.clone(), window);
        cx.notify();
      },
    ));

    self.sink_submenu = Some((input_id, submenu));
  }

  fn get_selected_id(&self, cx: &mut Context<Self>) -> Option<SinkInputId> {
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
      .set_sink_input_volume(selected_id, SetVolume::RelativePercent(1));
  }

  fn volume_down(&mut self, _: &VolumeDown, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_sink_input_volume(selected_id, SetVolume::RelativePercent(-1));
  }

  fn mute(&mut self, _: &Mute, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected_id) = self.get_selected_id(cx) else {
      return;
    };

    self
      .audio_state
      .read(cx)
      .set_sink_input_mute(selected_id, SetMute::Toggle)
  }

}

impl Focusable for AudioStreamsPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    if let Some((_, submenu)) = &self.sink_submenu {
      submenu.read(cx).focus_handle(cx)
    } else {
      self.picker.read(cx).focus_handle(cx)
    }
  }
}

impl Render for AudioStreamsPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let sink_submenu = self.sink_submenu.as_ref().map(|(_, s)| s.clone());

    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .on_action(cx.listener(Self::volume_up))
      .on_action(cx.listener(Self::volume_down))
      .on_action(cx.listener(Self::mute))
      .child(picker_input(&self.picker))
      .child(picker_results(&self.picker))
      .when_some(sink_submenu, |this, submenu| this.child(submenu))
  }
}

struct StreamsDelegate;

impl PickerDelegate for StreamsDelegate {
  type ListItem = StreamEntry;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let icon_cache = XdgIconCache::global(cx);
    let icon_cache = icon_cache.read(cx);
    let inner = item.entry.read(cx);
    let sink_input = &inner.sink_input;

    let icon = sink_input
      .application_name
      .as_ref()
      .and_then(|name| icon_cache.get(&name.to_lowercase()));

    // Use NORMAL volume as base since sink inputs don't have base_volume
    let base_volume = crate::audio::types::Volume(pulse::volume::Volume::NORMAL.0);
    let volume_percent = sink_input.volume.as_percent(base_volume);

    let app_name = sink_input.application_name.clone();
    let media_name = sink_input.name.clone();

    let display_name = match (app_name, media_name) {
      (Some(app), Some(media)) => format!("{} • {}", app, media),
      (Some(app), None) => app.to_string(),
      (None, Some(media)) => media.to_string(),
      (None, None) => "Unknown Stream".into(),
    };

    let display_name = SharedString::from(display_name);

    // Get cached sink description
    let sink_description = inner.sink_description.clone();

    v_flex()
      .w_full()
      .relative()
      .px_2()
      .py_3()
      .rounded_md()
      .child(VolumeBar::new(volume_percent, sink_input.mute, is_selected))
      .child(
        h_flex()
          .w_full()
          .gap_2()
          .child(
            h_flex()
              .flex_1()
              .gap_2()
              .items_center()
              .when_some(icon, |this, icon| {
                this.child(img(ImageSource::Resource(icon.clone())).size_5())
              })
              .when_none(&icon, |this| this.child(div().size_5()))
              .child(
                h_flex()
                  .flex_1()
                  .text_ellipsis()
                  .overflow_x_hidden()
                  .when(sink_input.mute, |div| div.child("MUTE "))
                  .child(display_name),
              ),
          )
          .child(div().child(format!("{}%", volume_percent))),
      )
      .when_some(sink_description, |this, desc| {
        this.child(div().text_sm().text_color(rgb(0x888888)).child(desc))
      })
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

// Delegate for the sink picker used when reassigning a stream
struct SinkPickerDelegate {
  current_sink_id: SinkId,
}

impl PickerDelegate for SinkPickerDelegate {
  type ListItem = SinkInfo;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let is_current = item.id == self.current_sink_id;

    h_flex()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .when(is_current, |this| this.opacity(0.5))
      .gap_2()
      .child(
        h_flex()
          .flex_1()
          .text_ellipsis()
          .overflow_x_hidden()
          .child(
            item
              .description
              .clone()
              .unwrap_or_else(|| item.name.clone().unwrap_or_default()),
          ),
      )
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

      for (index, sink) in items.iter().enumerate() {
        let mut max_score: Option<u32> = None;

        if let Some(score) = sink
          .name
          .as_ref()
          .and_then(|name| needle.score(Utf32Str::new(name, &mut buf), &mut matcher))
        {
          max_score = Some(max_score.map_or(score, |m| m.max(score)));
        }

        if let Some(score) = sink
          .description
          .as_ref()
          .and_then(|desc| needle.score(Utf32Str::new(desc, &mut buf), &mut matcher))
        {
          max_score = Some(max_score.map_or(score, |m| m.max(score)));
        }

        if let Some(score) = max_score {
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
