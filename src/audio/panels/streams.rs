use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, ImageSource, IntoElement, KeyBinding,
  Render, Resource, SharedString, Styled, Subscription, Task, Window, actions, div, img,
  prelude::*, rgb,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
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
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{h_flex, v_flex},
  xdg,
};

use super::VolumeBar;

pub struct SinkInputEntry {
  sink_input: SinkInputInfo,
  sink_description: Option<SharedString>,
  icon: Option<Resource>,
  _event_listener: Task<()>,
}

impl SinkInputEntry {
  pub fn new(
    sink_input: SinkInputInfo,
    audio_state: &Entity<AudioState>,
    sinks: &[SinkInfo],
    locales: &[String],
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let sink_input_id = sink_input.id;
    let event_rx = audio_state.read(cx).subscribe_sink_input(sink_input_id);

    let sink_description = sinks
      .iter()
      .find(|s| s.id == sink_input.sink_id)
      .and_then(|s| s.description.clone().or_else(|| s.name.clone()));

    let icon = sink_input
      .application_name
      .as_ref()
      .and_then(|app_name| xdg::get_icon_for_app(app_name, locales))
      .map(|path| Resource::Path(path.into()));

    let locales = locales.to_vec();

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
            this.update_from_info(&locales);
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
      icon,
      _event_listener: event_listener,
    }
  }

  pub fn sink_input(&self) -> &SinkInputInfo {
    &self.sink_input
  }

  pub fn sink_description(&self) -> Option<&SharedString> {
    self.sink_description.as_ref()
  }

  pub fn icon(&self) -> Option<&Resource> {
    self.icon.as_ref()
  }

  pub fn update_sink_description(&mut self, sinks: &[SinkInfo]) {
    self.sink_description = sinks
      .iter()
      .find(|s| s.id == self.sink_input.sink_id)
      .and_then(|s| s.description.clone().or_else(|| s.name.clone()));
  }

  pub fn update_from_info(&mut self, locales: &[String]) {
    self.icon = self
      .sink_input
      .application_name
      .as_ref()
      .and_then(|app_name| xdg::get_icon_for_app(app_name, locales))
      .map(|path| Resource::Path(path.into()));
  }
}

pub struct AudioStreamsPanel {
  picker: Entity<Picker<StreamsDelegate>>,
  sink_picker: Option<(SinkInputId, Entity<Picker<SinkPickerDelegate>>)>,
  audio_state: Entity<AudioState>,
  sink_inputs: Vec<Entity<SinkInputEntry>>,
  sinks: Vec<SinkInfo>,
  locales: Vec<String>,
  _subscriptions: Vec<Subscription>,
  _list_subscription_task: Task<()>,
  _sink_list_subscription_task: Task<()>,
  _initial_load_task: Task<()>,
}

const CONTEXT: &str = "streams";
const SINK_PICKER_CONTEXT: &str = "streams_sink_picker";

actions!(streams, [VolumeUp, VolumeDown, Mute, CloseSinkPicker]);

impl AudioStreamsPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
      KeyBinding::new("escape", CloseSinkPicker, Some(SINK_PICKER_CONTEXT)),
    ]);

    let audio_state = AudioState::global(cx);
    let delegate = StreamsDelegate;

    let picker = cx.new(|cx| Picker::new(delegate, Arc::new(vec![]), window, cx));

    let locales = freedesktop_desktop_entry::get_languages_from_env();

    // Load initial data async
    let initial_load_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      let locales = locales.clone();
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

          let entries: Vec<Entity<SinkInputEntry>> = sink_inputs
            .into_iter()
            .map(|input| {
              cx.new(|cx| SinkInputEntry::new(input, &audio_state, &sinks, &locales, window, cx))
            })
            .collect();

          this.sink_inputs = entries.clone();
          this.locales = locales.clone();
          picker.update(cx, |picker, cx| {
            picker.set_items(entries, window, cx);
          });
        });
      }
    });

    // Subscribe to sink input list changes
    let list_rx = audio_state.read(cx).subscribe_sink_input_list();
    let list_subscription_task = cx.spawn_in(window, {
      let picker = picker.clone();
      let audio_state = audio_state.clone();
      let locales = locales.clone();
      async move |this, cx| {
        while let Ok(event) = list_rx.recv_async().await {
          match event {
            SinkInputListEvent::Added(input_info) => {
              let _ = this.update_in(cx, |this, window, cx| {
                let new_entry = cx.new(|cx| {
                  SinkInputEntry::new(input_info, &audio_state, &this.sinks, &locales, window, cx)
                });
                this.sink_inputs.push(new_entry);

                picker.update(cx, |picker, cx| {
                  picker.set_items(this.sink_inputs.clone(), window, cx);
                });
              });
            }
            SinkInputListEvent::Removed(input_id) => {
              let _ = this.update_in(cx, |this, window, cx| {
                this
                  .sink_inputs
                  .retain(|entry| entry.read(cx).sink_input().id != input_id);

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
                this.sinks.push(sink.clone());
                // Update sink descriptions in entries
                for entry in &this.sink_inputs {
                  entry.update(cx, |entry, cx| {
                    entry.update_sink_description(&this.sinks);
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
                for entry in &this.sink_inputs {
                  entry.update(cx, |entry, cx| {
                    entry.update_sink_description(&this.sinks);
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
        if let PickerEvent::Picked(entry) = ev {
          let input_id = entry.read(cx).sink_input().id;
          let current_sink_id = entry.read(cx).sink_input().sink_id;
          this.open_sink_picker(input_id, current_sink_id, window, cx);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    Self {
      picker,
      sink_picker: None,
      audio_state,
      sink_inputs: Vec::new(),
      sinks: Vec::new(),
      locales,
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

    let sink_picker = cx.new(|cx| Picker::new(delegate, Arc::new(self.sinks.clone()), window, cx));

    // Subscribe to sink picker selection
    let subscription = cx.subscribe_in(&sink_picker, window, {
      let input_id = input_id;
      move |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(sink) = ev {
          let sink_id = sink.id;
          this.audio_state.read(cx).move_sink_input(input_id, sink_id);

          // Update the entry's sink description
          if let Some(entry) = this
            .sink_inputs
            .iter()
            .find(|e| e.read(cx).sink_input().id == input_id)
          {
            entry.update(cx, |entry, cx| {
              entry.sink_input.sink_id = sink_id;
              entry.update_sink_description(&this.sinks);
              cx.notify();
            });
          }

          this.close_sink_picker(window, cx);
        }
      }
    });

    cx.focus_view(&sink_picker.read(cx).search_input.clone(), window);
    self._subscriptions.push(subscription);
    self.sink_picker = Some((input_id, sink_picker));
  }

  fn close_sink_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.sink_picker = None;
    cx.focus_view(&self.picker.read(cx).search_input.clone(), window);
    cx.notify();
  }

  fn get_selected_id(&self, cx: &mut Context<Self>) -> Option<SinkInputId> {
    self
      .picker
      .read(cx)
      .get_selected_item()
      .map(|entry| entry.read(cx).sink_input().id)
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

  fn close_sink_picker_action(
    &mut self,
    _: &CloseSinkPicker,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.close_sink_picker(window, cx);
  }
}

impl Focusable for AudioStreamsPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    if let Some((_, sink_picker)) = &self.sink_picker {
      sink_picker.read(cx).focus_handle(cx)
    } else {
      self.picker.read(cx).focus_handle(cx)
    }
  }
}

impl Render for AudioStreamsPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .on_action(cx.listener(Self::volume_up))
      .on_action(cx.listener(Self::volume_down))
      .on_action(cx.listener(Self::mute))
      .on_action(cx.listener(Self::close_sink_picker_action))
      .child(if let Some((_, sink_picker)) = &self.sink_picker {
        div()
          .key_context(SINK_PICKER_CONTEXT)
          .size_full()
          .child(sink_picker.clone())
          .into_any_element()
      } else {
        self.picker.clone().into_any_element()
      })
  }
}

struct StreamsDelegate;

impl PickerDelegate for StreamsDelegate {
  type ListItem = Entity<SinkInputEntry>;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let entry = item.read(cx);
    let sink_input = entry.sink_input();

    let icon = entry.icon();

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
    let sink_description = entry.sink_description().cloned();

    v_flex()
      .w_full()
      .when(is_selected, |this| this.bg(rgb(0x444444)))
      .gap_1()
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
      .child(VolumeBar::new(volume_percent, sink_input.mute))
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

    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = Vec::new();
    let mut buf = Vec::new();

    for (index, entry) in items.iter().enumerate() {
      let sink_input = entry.read(cx).sink_input();
      let mut max_score: Option<u32> = None;

      // Match against name
      if let Some(score) = sink_input
        .name
        .as_ref()
        .and_then(|name| needle.score(Utf32Str::new(name, &mut buf), &mut matcher))
      {
        max_score = Some(max_score.map_or(score, |m| m.max(score)));
      }

      // Match against application_name
      if let Some(score) = sink_input
        .application_name
        .as_ref()
        .and_then(|name| needle.score(Utf32Str::new(name, &mut buf), &mut matcher))
      {
        max_score = Some(max_score.map_or(score, |m| m.max(score)));
      }

      if let Some(score) = max_score {
        matches.push((index, score));
      }
    }

    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, Some(matches));
    });

    Task::ready(())
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
      .when(is_selected, |this| this.bg(rgb(0x444444)))
      .gap_2()
      .child(
        h_flex()
          .flex_1()
          .text_ellipsis()
          .overflow_x_hidden()
          .when(is_current, |div| div.child("* "))
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

    let mut matcher = Matcher::new(Config::DEFAULT);
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

    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, Some(matches));
    });

    Task::ready(())
  }
}
