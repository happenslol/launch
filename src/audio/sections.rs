use std::ops::Range;

use anyhow::Result;
use gpui::{
  AnyElement, AnyView, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement,
  Render, Styled, Subscription, Window, div, prelude::*, uniform_list, white,
};

use crate::{
  audio::{
    AudioState,
    types::{SinkId, SinkInfo},
  },
  launcher::{Item, ItemAction},
  text_input::{TextInput, TextInputEvent},
  util::{h_flex, v_flex},
};

pub fn get_items() -> Result<Vec<Item>> {
  Ok(vec![Item {
    name: "sinks".into(),
    action: ItemAction::Section(Box::new(AudioSection::view)),
  }])
}

pub struct AudioSection {
  search_input: Entity<TextInput>,
  focus_handle: FocusHandle,
  subscriptions: Vec<Subscription>,
  audio_state: Entity<AudioState>,
}

impl AudioSection {
  pub fn view(window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| AudioSection::new(window, cx)).into()
  }

  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle();
    let search_input = cx.new(|cx| TextInput::new(window, cx));

    cx.focus_view(&search_input, window);

    let mut this = Self {
      focus_handle,
      search_input: search_input.clone(),
      subscriptions: Vec::new(),
      audio_state: AudioState::global(cx),
    };

    this
      .subscriptions
      .extend([cx.subscribe_in(&search_input, window, {
        let _search_input = search_input.clone();
        move |_this, _, _ev: &TextInputEvent, _, _cx| {}
      })]);

    this
  }
}

impl Focusable for AudioSection {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for AudioSection {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let count = self.audio_state.read(cx).sinks.len();

    v_flex()
      .track_focus(&self.focus_handle)
      .size_full()
      .child(self.search_input.clone())
      .child(
        uniform_list(
          "sinks",
          count,
          cx.processor(move |this, range, _, cx| this.render_sink_list(range, cx)),
        )
        .h_full(),
      )
  }
}

impl AudioSection {
  fn render_sink_list(&self, range: Range<usize>, cx: &mut Context<Self>) -> Vec<AnyElement> {
    let sink_ids = self
      .audio_state
      .read(cx)
      .sinks
      .keys()
      .skip(range.start.saturating_sub(1))
      .take(range.len())
      .copied()
      .enumerate()
      .collect::<Vec<_>>();

    sink_ids
      .into_iter()
      .map(|(ix, id)| self.render_sink_list_item(ix, id, cx))
      .collect::<Vec<_>>()
  }

  fn render_sink_list_item(&self, ix: usize, id: SinkId, cx: &mut Context<Self>) -> AnyElement {
    let sink = self
      .audio_state
      .read(cx)
      .sinks
      .get(&id)
      .expect("invalid sink id");

    h_flex()
      .id(("sink", ix))
      .on_click(cx.listener(move |this, _, _, cx| {
        this
          .audio_state
          .update(cx, |state, cx| state.set_default_sink(id, cx))
          .detach_and_log_err(cx);
      }))
      .bg(white())
      .w_full()
      .gap_2()
      .child(
        div()
          .text_ellipsis()
          .overflow_x_hidden()
          .flex_1()
          .child(sink.name.clone().unwrap_or_default()),
      )
      .child(div().child(format!("{}%", sink.volume.0[0])))
      .into_any()
  }
}
