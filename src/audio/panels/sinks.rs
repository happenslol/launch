use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, Render,
  Styled, Subscription, Task, Window, actions, div, prelude::*, rgb,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  audio::{
    AudioEvent, AudioState,
    pulse::{SetMute, SetVolume},
    types::{SinkId, SinkInfo},
  },
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{h_flex, v_flex},
};

pub struct AudioSinksPanel {
  picker: Entity<Picker<SinksDelegate>>,
  audio_state: Entity<AudioState>,
  _subscriptions: Vec<Subscription>,
}

const CONTEXT: &str = "sinks";

actions!(sinks, [VolumeUp, VolumeDown, Mute]);

impl AudioSinksPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-l", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-h", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-up", VolumeUp, Some(CONTEXT)),
      KeyBinding::new("ctrl-down", VolumeDown, Some(CONTEXT)),
      KeyBinding::new("ctrl-m", Mute, Some(CONTEXT)),
    ]);

    let audio_state = AudioState::global(cx);
    let delegate = SinksDelegate {
      audio_state: audio_state.clone(),
    };

    let sinks = audio_state
      .read(cx)
      .sinks
      .values()
      .cloned()
      .collect::<Vec<_>>();

    let picker = cx.new(|cx| Picker::new(delegate, sinks, window, cx));

    let subscriptions = vec![
      cx.subscribe_in(&audio_state, window, |this, audio_state, ev, window, cx| {
        // TODO: Probably better to not replace the entire list and do a new search every time the
        // volume changes, maybe we can differentiate between changes that affect the search and
        // others?
        // The sinks can pull their volume directly from the audio global.
        if let AudioEvent::SinksChanged = ev {
          let sinks = audio_state.read(cx).sinks.values().cloned().collect();
          this.picker.update(cx, |picker, cx| {
            picker.set_items(sinks, window, cx);
          });
        }
      }),
      cx.subscribe_in(&picker, window, |this, _picker, ev, _window, cx| {
        if let PickerEvent::Picked(item) = ev {
          this
            .audio_state
            .update(cx, |state, cx| state.set_default_sink(item.id, cx))
            .detach_and_log_err(cx);
        }
      }),
    ];

    cx.focus_self(window);

    Self {
      picker,
      audio_state,
      _subscriptions: subscriptions,
    }
  }

  fn get_selected_id(&self, cx: &mut Context<Self>) -> Option<SinkId> {
    self.picker.read(cx).get_selected_item().map(|item| item.id)
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
      .child(self.picker.clone())
  }
}

struct SinksDelegate {
  audio_state: Entity<AudioState>,
}

impl PickerDelegate for SinksDelegate {
  type ListItem = SinkInfo;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let is_default = self.audio_state.read(cx).default_sink == Some(item.id);

    h_flex()
      .w_full()
      .when_else(
        is_selected,
        |div| div.bg(rgb(0xDDDDDD)),
        |div| div.bg(rgb(0xFFFFFF)),
      )
      .w_full()
      .gap_2()
      .child(
        h_flex()
          .text_ellipsis()
          .overflow_x_hidden()
          .flex_1()
          .when(is_default, |div| div.child("---> "))
          .when(item.mute, |div| div.child("MUTE "))
          .child(
            item
              .description
              .clone()
              .unwrap_or_else(|| item.name.clone().unwrap_or_default()),
          ),
      )
      .child(div().child(format!("{}%", item.volume.as_percent(item.base_volume))))
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
    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = Vec::new();
    let mut buf = Vec::new();

    for (index, item) in items.iter().enumerate() {
      let mut max_score: Option<u32> = None;

      if let Some(score) = item
        .name
        .as_ref()
        .and_then(|name| needle.score(Utf32Str::new(name, &mut buf), &mut matcher))
      {
        max_score = if let Some(max_score) = max_score {
          Some(max_score.max(score))
        } else {
          Some(score)
        }
      }

      if let Some(score) = item
        .description
        .as_ref()
        .and_then(|name| needle.score(Utf32Str::new(name, &mut buf), &mut matcher))
      {
        max_score = if let Some(max_score) = max_score {
          Some(max_score.max(score))
        } else {
          Some(score)
        }
      }

      if let Some(score) = max_score {
        matches.push((index, score));
      }
    }

    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, matches);
    });

    Task::ready(())
  }
}
