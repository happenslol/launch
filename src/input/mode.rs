use super::text_wrapper::TextWrapper;

#[derive(Default, Clone)]
pub enum InputMode {
  #[default]
  SingleLine,
  MultiLine {
    rows: usize,
  },
  AutoGrow {
    rows: usize,
    min_rows: usize,
    max_rows: usize,
  },
}

impl InputMode {
  #[inline]
  pub(super) fn is_single_line(&self) -> bool {
    matches!(self, InputMode::SingleLine)
  }

  #[inline]
  pub(super) fn is_auto_grow(&self) -> bool {
    matches!(self, InputMode::AutoGrow { .. })
  }

  #[inline]
  pub(super) fn is_multi_line(&self) -> bool {
    matches!(
      self,
      InputMode::MultiLine { .. } | InputMode::AutoGrow { .. }
    )
  }

  pub(super) fn set_rows(&mut self, new_rows: usize) {
    match self {
      InputMode::MultiLine { rows, .. } => {
        *rows = new_rows;
      }
      InputMode::AutoGrow {
        rows,
        min_rows,
        max_rows,
      } => {
        *rows = new_rows.clamp(*min_rows, *max_rows);
      }
      _ => {}
    }
  }

  pub(super) fn update_auto_grow(&mut self, text_wrapper: &TextWrapper) {
    if self.is_single_line() {
      return;
    }

    let wrapped_lines = text_wrapper.len();
    self.set_rows(wrapped_lines);
  }

  /// At least 1 row be return.
  pub(super) fn rows(&self) -> usize {
    match self {
      InputMode::MultiLine { rows, .. } => *rows,
      InputMode::AutoGrow { rows, .. } => *rows,
      _ => 1,
    }
    .max(1)
  }

  /// At least 1 row be return.
  #[allow(unused)]
  pub(super) fn min_rows(&self) -> usize {
    match self {
      InputMode::MultiLine { .. } => 1,
      InputMode::AutoGrow { min_rows, .. } => *min_rows,
      _ => 1,
    }
    .max(1)
  }

  #[allow(unused)]
  pub(super) fn max_rows(&self) -> usize {
    match self {
      InputMode::MultiLine { .. } => usize::MAX,
      InputMode::AutoGrow { max_rows, .. } => *max_rows,
      _ => 1,
    }
  }
}
