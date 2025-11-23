#![allow(unused)]
mod blink_cursor;
mod change;
mod history;
mod mode;
mod movement;
mod rope_ext;
mod selection;
pub mod state;
mod text_element;
mod text_wrapper;

use gpui::{App, Entity, Focusable, Rems, StyleRefinement, Styled, Window, div, prelude::*};

use crate::input::state::{CONTEXT, InputState, input_state_listeners};

pub fn input(state: &Entity<InputState>) -> Input {
  Input {
    state: state.clone(),
    style: StyleRefinement::default(),
    disabled: false,
  }
}

#[derive(IntoElement)]
pub struct Input {
  state: Entity<InputState>,
  style: StyleRefinement,
  disabled: bool,
}

impl Styled for Input {
  fn style(&mut self) -> &mut StyleRefinement {
    &mut self.style
  }
}

impl RenderOnce for Input {
  fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
    const LINE_HEIGHT: Rems = Rems(1.25);
    let font = window.text_style().font();
    let font_size = window.text_style().font_size.to_pixels(window.rem_size());

    self.state.update(cx, |state, cx| {
      state.text_wrapper.set_font(font, font_size, cx);
      state.text_wrapper.prepare_if_need(&state.text, cx);
      state.disabled = self.disabled;
    });

    let state = self.state.read(cx);
    let focused = state.focus_handle.is_focused(window);

    div()
      .id(("input", self.state.entity_id()))
      .key_context(CONTEXT)
      .track_focus(&state.focus_handle)
      .flex()
      .map(|div| {
        input_state_listeners(
          &self.state,
          div,
          window,
          state.disabled,
          state.mode.is_multi_line(),
        )
      })
      .on_action(window.listener_for(&self.state, InputState::backspace))
      .size_full()
      .line_height(LINE_HEIGHT)
      .cursor_text()
      .text_size(font_size)
      .items_center()
      .when(state.mode.is_multi_line(), |this| this.h_auto())
      .map(|mut div| {
        div.style().refine(&self.style);
        div
      })
      .child(self.state.clone())
  }
}
