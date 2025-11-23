use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  AnyView, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled, Subscription, Task, Window, div, prelude::*, rgb
};

use crate::{
  audio::{AudioEvent, AudioState, types::SinkInfo},
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{h_flex, v_flex},
};

pub struct AudioSinksSection {
  picker: Entity<Picker<SinksDelegate>>,
  audio_state: Entity<AudioState>,
  _subscriptions: Vec<Subscription>,
}

impl AudioSinksSection {
  pub fn view(window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| AudioSinksSection::new(window, cx)).into()
  }

  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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

    Self {
      picker,
      audio_state,
      _subscriptions: subscriptions,
    }
  }
}

impl Focusable for AudioSinksSection {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for AudioSinksSection {
  fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
    v_flex().size_full().child(self.picker.clone())
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
          .when(is_default, |div| div.child("DEF: "))
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
