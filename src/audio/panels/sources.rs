use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, Render,
  Styled, Subscription, Task, Window, actions, div, prelude::*,
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
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, v_flex},
};

use super::VolumeBar;

#[derive(Clone)]
pub struct SourceEntry {
  id: SourceId,
  search_string: String,
  entry: Entity<SourceEntryInner>,
}

impl SourceEntry {
  pub fn new(
    source: SourceInfo,
    audio_state: &Entity<AudioState>,
    window: &mut Window,
    cx: &mut App,
  ) -> Self {
    let id = source.id;
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
      search_string,
      entry,
    }
  }
}

pub struct SourceEntryInner {
  source: SourceInfo,
  _event_listener: Task<()>,
}

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
  _subscriptions: Vec<Subscription>,
  _list_subscription_task: Task<()>,
  _initial_load_task: Task<()>,
}

const CONTEXT: &str = "sources";

actions!(sources, [VolumeUp, VolumeDown, Mute]);

impl AudioSourcesPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
    ]);

    let audio_state = AudioState::global(cx);
    let delegate = SourcesDelegate {
      audio_state: audio_state.clone(),
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
            .map(|source| SourceEntry::new(source, &audio_state, window, cx))
            .collect();

          picker.update(cx, |picker, cx| {
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
                let new_entry = SourceEntry::new(source_info, &audio_state, window, cx);
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
      _subscriptions: subscriptions,
      _list_subscription_task: list_subscription_task,
      _initial_load_task: initial_load_task,
    }
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
      .child(picker_input(&self.picker))
      .child(picker_results(&self.picker))
  }
}

struct SourcesDelegate {
  audio_state: Entity<AudioState>,
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
    use crate::util::h_flex;

    let source = &item.entry.read(cx).source;
    let is_default = self.audio_state.read(cx).default_source == Some(source.id);
    let volume_percent = source.volume.as_percent(source.base_volume);

    v_flex()
      .w_full()
      .relative()
      .px_2()
      .py_3()
      .rounded_md()
      .child(VolumeBar::new(volume_percent, source.mute, is_selected).default(is_default))
      .child(
        h_flex()
          .w_full()
          .gap_2()
          .child(
            h_flex()
              .flex_1()
              .text_ellipsis()
              .overflow_x_hidden()
              .when(source.mute, |div| div.child("MUTE "))
              .child(
                source
                  .description
                  .clone()
                  .unwrap_or_else(|| source.name.clone().unwrap_or_default()),
              ),
          )
          .child(div().child(format!("{}%", volume_percent))),
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
