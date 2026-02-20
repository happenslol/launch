use gpui::{Div, Refineable, StyleRefinement, Styled, div};

pub trait StyledExt: Styled + Sized {
  fn h_flex(self) -> Self {
    self.flex().flex_row().items_center()
  }

  fn v_flex(self) -> Self {
    self.flex().flex_col()
  }

  fn refine_style(mut self, style: &StyleRefinement) -> Self {
    self.style().refine(style);
    self
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

pub trait ResultExt<E> {
  type Ok;

  fn log_err(self) -> Option<Self::Ok>;

  #[allow(unused)]
  fn warn_on_err(self) -> Option<Self::Ok>;
}

impl<T, E> ResultExt<E> for Result<T, E>
where
  E: std::fmt::Debug,
{
  type Ok = T;

  #[track_caller]
  fn log_err(self) -> Option<T> {
    match self {
      Ok(value) => Some(value),
      Err(error) => {
        tracing::error!("{:?}", error);
        None
      }
    }
  }

  #[track_caller]
  fn warn_on_err(self) -> Option<T> {
    match self {
      Ok(value) => Some(value),
      Err(error) => {
        tracing::error!("{:?}", error);
        None
      }
    }
  }
}
