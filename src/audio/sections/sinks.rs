use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  AnyView, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding,
  Render, Styled, Subscription, Task, Window, actions, div, prelude::*, rgb,
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

pub struct AudioSinksSection {
  picker: Entity<Picker<SinksDelegate>>,
  audio_state: Entity<AudioState>,
  _subscriptions: Vec<Subscription>,
}

const CONTEXT: &str = "sinks";

actions!(sinks, [VolumeUp, VolumeDown, Mute]);

impl AudioSinksSection {
  pub fn view(window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| AudioSinksSection::new(window, cx)).into()
  }

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
      cx.subscribe_in(
        &audio_state,
        window,
        |this, audio_state, ev, window, cx| match ev {
          AudioEvent::SinksChanged => {
            let sinks = audio_state.read(cx).sinks.values().cloned().collect();
            this.picker.update(cx, |picker, cx| {
              picker.set_items(sinks, window, cx);
            });
          }
        },
      ),
      cx.subscribe_in(&picker, window, |this, _picker, ev, _window, cx| match ev {
        PickerEvent::Picked(item) => {
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

impl Focusable for AudioSinksSection {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for AudioSinksSection {
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
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    _query: String,
    _cancel_flag: Arc<AtomicBool>,
    _search_id: usize,
    _items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    Task::ready(())
  }
}
