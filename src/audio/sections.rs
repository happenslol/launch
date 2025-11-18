use std::ops::Range;

use anyhow::Result;
use gpui::{
  AnyView, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled,
  Subscription, Window, div, prelude::*, uniform_list, white,
};

use crate::{
  audio::AudioStateAppExt,
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
    };

    this
      .subscriptions
      .extend([cx.subscribe_in(&search_input, window, {
        let search_input = search_input.clone();
        move |this, _, ev: &TextInputEvent, window, cx| {}
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
    let sinks = &cx.audio().sinks;

    v_flex()
      .track_focus(&self.focus_handle)
      .size_full()
      .child(self.search_input.clone())
      .child(
        uniform_list(
          "sinks",
          sinks.len(),
          cx.processor(move |_this, range: Range<usize>, _window, cx| {
            range
              .map(move |i| {
                let sinks = &cx.audio().sinks;
                let keys = sinks.keys().collect::<Vec<_>>();
                let sink = sinks[keys[i]]
                  .name
                  .as_ref()
                  .map(|name| name.to_string())
                  .unwrap_or_default();

                h_flex()
                  .id(i)
                  .bg(white())
                  .w_full()
                  .gap_2()
                  .child(
                    div()
                      .text_ellipsis()
                      .overflow_x_hidden()
                      .flex_1()
                      .child(sink),
                  )
                  .child(div().child(format!("{}%", sinks[keys[i]].volume.clone().0[0])))
              })
              .collect()
          }),
        )
        .h_full(),
      )
  }
}
