#![allow(unused)]
use gpui::{Div, Styled, div};

pub trait StyledExt: Styled + Sized {
  fn h_flex(self) -> Self {
    self.flex().flex_row().items_center()
  }

  fn v_flex(self) -> Self {
    self.flex().flex_col()
  }
}

impl StyledExt for Div {}

#[track_caller]
pub fn h_flex() -> Div {
  div().h_flex()
}

#[track_caller]
pub fn v_flex() -> Div {
  div().v_flex()
}
