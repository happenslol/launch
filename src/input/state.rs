//! Adapted from gpui-component/input
use anyhow::Result;
use gpui::{
  Action, App, AppContext, Bounds, ClipboardItem, Context, Div, Entity, EntityInputHandler,
  EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
  KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
  Pixels, Point, Render, ScrollHandle, ScrollWheelEvent, SharedString, Stateful, Styled as _,
  Subscription, Task, UTF16Selection, Window, actions, div, point, prelude::FluentBuilder as _, px,
};
use ropey::{Rope, RopeSlice};
use std::ops::Range;
use std::rc::Rc;
use unicode_segmentation::*;

pub const MASK_CHAR: &str = "•";
pub const MASK_CHAR_LEN: usize = MASK_CHAR.len();

use crate::input::{
  blink_cursor::BlinkCursor,
  change::Change,
  history::History,
  mode::InputMode,
  rope_ext::{Bias, Position, RopeExt as _},
  selection::Selection,
  text_element::{RIGHT_MARGIN, text_element},
  text_wrapper::{LineItem, LineLayout, TextWrapper},
};

#[derive(Action, Clone, PartialEq, Eq)]
#[action(namespace = input, no_json)]
pub struct Enter {
  /// Is confirm with secondary.
  pub secondary: bool,
}

actions!(
  input,
  [
    Backspace,
    Delete,
    DeleteToBeginningOfLine,
    DeleteToEndOfLine,
    DeleteToPreviousWordStart,
    DeleteToNextWordEnd,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    MovePageUp,
    MovePageDown,
    Cancel,
    SelectUp,
    SelectDown,
    SelectLeft,
    SelectRight,
    SelectAll,
    SelectToStartOfLine,
    SelectToEndOfLine,
    SelectToStart,
    SelectToEnd,
    SelectToPreviousWordStart,
    SelectToNextWordEnd,
    Copy,
    Cut,
    Paste,
    Undo,
    Redo,
    MoveToStartOfLine,
    MoveToEndOfLine,
    MoveToStart,
    MoveToEnd,
    MoveToPreviousWord,
    MoveToNextWord,
    Escape,
  ]
);

#[derive(Clone)]
pub enum InputEvent {
  Change,
  PressEnter { secondary: bool },
  Focus,
  Blur,
}

pub const CONTEXT: &str = "Input";

#[derive(Clone)]
pub struct LastLayout {
  /// The visible range (no wrap) of lines in the viewport, the value is row (0-based) index.
  pub visible_range: Range<usize>,
  /// The first visible line top position in scroll viewport.
  pub visible_top: Pixels,
  /// The range of byte offset of the visible lines.
  pub visible_range_offset: Range<usize>,
  /// The last layout lines (Only have visible lines).
  pub lines: Rc<Vec<LineLayout>>,
  /// The line_height of text layout, this will change will InputElement painted.
  pub line_height: Pixels,
  /// The wrap width of text layout, this will change will InputElement painted.
  pub wrap_width: Option<Pixels>,
  /// The cursor position (top, left) in pixels.
  pub cursor_bounds: Option<Bounds<Pixels>>,
}

impl LastLayout {
  /// Get the line layout for the given row (0-based).
  ///
  /// 0 is the viewport first visible line.
  ///
  /// Returns None if the row is out of range.
  pub fn line(&self, row: usize) -> Option<&LineLayout> {
    if row < self.visible_range.start || row >= self.visible_range.end {
      return None;
    }

    self.lines.get(row.saturating_sub(self.visible_range.start))
  }
}

/// InputState to keep editing state of the [`super::Input`].
pub struct InputState {
  pub focus_handle: FocusHandle,
  pub mode: InputMode,
  pub text: Rope,
  pub text_wrapper: TextWrapper,
  pub history: History<Change>,
  pub blink_cursor: Entity<BlinkCursor>,
  pub loading: bool,
  pub selected_range: Selection,
  /// Range for save the selected word, use to keep word range when drag move.
  pub selected_word_range: Option<Selection>,
  pub selection_reversed: bool,
  /// The marked range is the temporary insert text on IME typing.
  pub ime_marked_range: Option<Selection>,
  pub last_layout: Option<LastLayout>,
  pub last_cursor: Option<usize>,
  /// The input container bounds
  pub input_bounds: Bounds<Pixels>,
  /// The text bounds
  pub last_bounds: Option<Bounds<Pixels>>,
  pub last_selected_range: Option<Selection>,
  pub selecting: bool,
  pub disabled: bool,
  pub masked: bool,
  pub clean_on_escape: bool,
  pub soft_wrap: bool,
  #[allow(clippy::type_complexity)]
  pub validate: Option<Box<dyn Fn(&str, &mut Context<Self>) -> bool + 'static>>,
  pub scroll_handle: ScrollHandle,
  /// The deferred scroll offset to apply on next layout.
  pub deferred_scroll_offset: Option<Point<Pixels>>,
  /// The size of the scrollable content.
  pub scroll_size: gpui::Size<Pixels>,
  pub placeholder: SharedString,

  /// A flag to indicate if we have a pending update to the text.
  ///
  /// If true, will call some update (for example LSP, Syntax Highlight) before render.
  _pending_update: bool,
  /// A flag to indicate if we should ignore the next completion event.
  pub silent_replace_text: bool,

  /// To remember the horizontal column (x-coordinate) of the cursor position for keep column for move up/down.
  ///
  /// The first element is the x-coordinate (Pixels), preferred to use this.
  /// The second element is the column (usize), fallback to use this.
  pub preferred_column: Option<(Pixels, usize)>,
  _subscriptions: Vec<Subscription>,
}

impl EventEmitter<InputEvent> for InputState {}

impl InputState {
  pub fn init(cx: &mut App) {
    cx.bind_keys([
      KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
      KeyBinding::new("delete", Delete, Some(CONTEXT)),
      KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
      KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(CONTEXT)),
      KeyBinding::new("enter", Enter { secondary: false }, Some(CONTEXT)),
      KeyBinding::new("secondary-enter", Enter { secondary: true }, Some(CONTEXT)),
      KeyBinding::new("escape", Escape, Some(CONTEXT)),
      KeyBinding::new("up", MoveUp, Some(CONTEXT)),
      KeyBinding::new("down", MoveDown, Some(CONTEXT)),
      KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
      KeyBinding::new("right", MoveRight, Some(CONTEXT)),
      KeyBinding::new("pageup", MovePageUp, Some(CONTEXT)),
      KeyBinding::new("pagedown", MovePageDown, Some(CONTEXT)),
      KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
      KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
      KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
      KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
      KeyBinding::new("home", MoveHome, Some(CONTEXT)),
      KeyBinding::new("end", MoveEnd, Some(CONTEXT)),
      KeyBinding::new("shift-home", SelectToStartOfLine, Some(CONTEXT)),
      KeyBinding::new("shift-end", SelectToEndOfLine, Some(CONTEXT)),
      KeyBinding::new("ctrl-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
      KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
      KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
      KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
      KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
      KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
      KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(CONTEXT)),
      KeyBinding::new("ctrl-right", MoveToNextWord, Some(CONTEXT)),
      KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
      KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
    ]);
  }

  /// Create a Input state with default [`InputMode::SingleLine`] mode.
  ///
  /// See also: [`Self::multi_line`], [`Self::auto_grow`] to set other mode.
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle().tab_stop(true);
    let blink_cursor = cx.new(|_| BlinkCursor::new());
    let history = History::new().group_interval(std::time::Duration::from_secs(1));

    let _subscriptions = vec![
      // Observe the blink cursor to repaint the view when it changes.
      cx.observe(&blink_cursor, |_, _, cx| cx.notify()),
      // Blink the cursor when the window is active, pause when it's not.
      cx.observe_window_activation(window, |input, window, cx| {
        if window.is_window_active() {
          let focus_handle = input.focus_handle.clone();
          if focus_handle.is_focused(window) {
            input.blink_cursor.update(cx, |blink_cursor, cx| {
              blink_cursor.start(cx);
            });
          }
        }
      }),
      cx.on_focus(&focus_handle, window, Self::on_focus),
      cx.on_blur(&focus_handle, window, Self::on_blur),
    ];

    let text_style = window.text_style();

    Self {
      focus_handle,
      text: "".into(),
      text_wrapper: TextWrapper::new(
        text_style.font(),
        text_style.font_size.to_pixels(window.rem_size()),
        None,
      ),
      blink_cursor,
      history,
      selected_range: Selection::default(),
      selected_word_range: None,
      selection_reversed: false,
      ime_marked_range: None,
      input_bounds: Bounds::default(),
      selecting: false,
      disabled: false,
      masked: false,
      clean_on_escape: false,
      soft_wrap: true,
      loading: false,
      validate: None,
      mode: InputMode::SingleLine,
      last_layout: None,
      last_bounds: None,
      last_selected_range: None,
      last_cursor: None,
      scroll_handle: ScrollHandle::new(),
      scroll_size: gpui::size(px(0.), px(0.)),
      deferred_scroll_offset: None,
      preferred_column: None,
      placeholder: SharedString::default(),
      silent_replace_text: false,
      _subscriptions,
      _pending_update: false,
    }
  }

  /// Set Input to use [`InputMode::MultiLine`] mode.
  ///
  /// Default rows is 2.
  pub fn multi_line(mut self) -> Self {
    self.mode = InputMode::MultiLine { rows: 2 };
    self
  }

  /// Set Input to use [`InputMode::AutoGrow`] mode with min, max rows limit.
  pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self {
    self.mode = InputMode::AutoGrow {
      rows: min_rows,
      min_rows,
      max_rows,
    };
    self
  }

  /// Set placeholder
  pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
    self.placeholder = placeholder.into();
    self
  }

  /// Set the number of rows for the multi-line Textarea.
  ///
  /// This is only used when `multi_line` is set to true.
  ///
  /// default: 2
  pub fn rows(mut self, rows: usize) -> Self {
    match &mut self.mode {
      InputMode::MultiLine { rows: r, .. } => *r = rows,
      InputMode::AutoGrow {
        max_rows: max_r,
        rows: r,
        ..
      } => {
        *r = rows;
        *max_r = rows;
      }
      _ => {}
    }
    self
  }

  /// Set placeholder
  pub fn set_placeholder(
    &mut self,
    placeholder: impl Into<SharedString>,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.placeholder = placeholder.into();
    cx.notify();
  }

  /// Find which line and sub-line the given offset belongs to, along with the position within that sub-line.
  ///
  /// Returns:
  ///
  /// - The index of the line (zero-based) containing the offset.
  /// - The index of the sub-line (zero-based) within the line containing the offset.
  /// - The position of the offset.
  pub fn line_and_position_for_offset(
    &self,
    offset: usize,
  ) -> (usize, usize, Option<Point<Pixels>>) {
    let Some(last_layout) = &self.last_layout else {
      return (0, 0, None);
    };
    let line_height = last_layout.line_height;

    let mut prev_lines_offset = last_layout.visible_range_offset.start;
    let mut y_offset = last_layout.visible_top;
    for (line_index, line) in last_layout.lines.iter().enumerate() {
      let local_offset = offset.saturating_sub(prev_lines_offset);
      if let Some(pos) = line.position_for_index(local_offset, line_height) {
        let sub_line_index = (pos.y / line_height) as usize;
        let adjusted_pos = point(pos.x, pos.y + y_offset);
        return (line_index, sub_line_index, Some(adjusted_pos));
      }

      y_offset += line.size(line_height).height;
      prev_lines_offset += line.len() + 1;
    }
    (0, 0, None)
  }

  /// Set the text of the input field.
  ///
  /// And the selection_range will be reset to 0..0.
  pub fn set_value(
    &mut self,
    value: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.history.ignore = true;
    let was_disabled = self.disabled;
    self.disabled = false;
    self.replace_text(value, window, cx);
    self.disabled = was_disabled;
    self.history.ignore = false;
    // Ensure cursor to start when set text
    if self.mode.is_single_line() {
      self.selected_range = (self.text.len()..self.text.len()).into();
    } else {
      self.selected_range.clear();

      self._pending_update = true;
    }
    // Move scroll to top
    self.scroll_handle.set_offset(point(px(0.), px(0.)));

    cx.notify();
  }

  /// Insert text at the current cursor position.
  ///
  /// And the cursor will be moved to the end of inserted text.
  pub fn insert(
    &mut self,
    text: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let text: SharedString = text.into();
    let range_utf16 = self.range_to_utf16(&(self.cursor()..self.cursor()));
    self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
    self.selected_range = (self.selected_range.end..self.selected_range.end).into();
  }

  /// Replace text at the current cursor position.
  ///
  /// And the cursor will be moved to the end of replaced text.
  pub fn replace(
    &mut self,
    text: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let text: SharedString = text.into();
    self.replace_text_in_range_silent(None, &text, window, cx);
    self.selected_range = (self.selected_range.end..self.selected_range.end).into();
  }

  fn replace_text(
    &mut self,
    text: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let text: SharedString = text.into();
    let range = 0..self.text.chars().map(|c| c.len_utf16()).sum();
    self.replace_text_in_range_silent(Some(range), &text, window, cx);
  }

  /// Set with disabled mode.
  ///
  /// See also: [`Self::set_disabled`], [`Self::is_disabled`].
  pub fn disabled(mut self, disabled: bool) -> Self {
    self.disabled = disabled;
    self
  }

  /// Set with password masked state.
  ///
  /// Only for [`InputMode::SingleLine`] mode.
  pub fn masked(mut self, masked: bool) -> Self {
    debug_assert!(self.mode.is_single_line());
    self.masked = masked;
    self
  }

  /// Set the password masked state of the input field.
  ///
  /// Only for [`InputMode::SingleLine`] mode.
  pub fn set_masked(&mut self, masked: bool, _: &mut Window, cx: &mut Context<Self>) {
    debug_assert!(self.mode.is_single_line());
    self.masked = masked;
    cx.notify();
  }

  /// Set true to clear the input by pressing Escape key.
  pub fn clean_on_escape(mut self) -> Self {
    self.clean_on_escape = true;
    self
  }

  /// Set the soft wrap mode for multi-line input, default is true.
  pub fn soft_wrap(mut self, wrap: bool) -> Self {
    debug_assert!(self.mode.is_multi_line());
    self.soft_wrap = wrap;
    self
  }

  /// Update the soft wrap mode for multi-line input, default is true.
  pub fn set_soft_wrap(&mut self, wrap: bool, _: &mut Window, cx: &mut Context<Self>) {
    debug_assert!(self.mode.is_multi_line());
    self.soft_wrap = wrap;
    if wrap {
      let wrap_width = self
        .last_layout
        .as_ref()
        .and_then(|b| b.wrap_width)
        .unwrap_or(self.input_bounds.size.width);

      self.text_wrapper.set_wrap_width(Some(wrap_width), cx);

      // Reset scroll to left 0
      let mut offset = self.scroll_handle.offset();
      offset.x = px(0.);
      self.scroll_handle.set_offset(offset);
    } else {
      self.text_wrapper.set_wrap_width(None, cx);
    }
    cx.notify();
  }

  /// Set the validation function of the input field.
  ///
  /// Only for [`InputMode::SingleLine`] mode.
  pub fn validate(mut self, f: impl Fn(&str, &mut Context<Self>) -> bool + 'static) -> Self {
    debug_assert!(self.mode.is_single_line());
    self.validate = Some(Box::new(f));
    self
  }

  /// Set true to show indicator at the input right.
  ///
  /// Only for [`InputMode::SingleLine`] mode.
  pub fn set_loading(&mut self, loading: bool, _: &mut Window, cx: &mut Context<Self>) {
    debug_assert!(self.mode.is_single_line());
    self.loading = loading;
    cx.notify();
  }

  /// Set the default value of the input field.
  pub fn default_value(mut self, value: impl Into<SharedString>) -> Self {
    let text: SharedString = value.into();
    self.text = Rope::from(text.as_str());
    self.text_wrapper.set_default_text(&self.text);
    self._pending_update = true;
    self
  }

  /// Return the value of the input field.
  pub fn value(&self) -> SharedString {
    SharedString::new(self.text.to_string())
  }

  /// Return the text [`Rope`] of the input field.
  pub fn text(&self) -> &Rope {
    &self.text
  }

  /// Return the (0-based) [`Position`] of the cursor.
  pub fn cursor_position(&self) -> Position {
    let offset = self.cursor();
    self.text.offset_to_position(offset)
  }

  /// Set (0-based) [`Position`] of the cursor.
  ///
  /// This will move the cursor to the specified line and column, and update the selection range.
  pub fn set_cursor_position(
    &mut self,
    position: impl Into<Position>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let position: Position = position.into();
    let offset = self.text.position_to_offset(&position);

    self.move_to(offset, cx);
    self.update_preferred_column();
    self.focus(window, cx);
  }

  /// Focus the input field.
  pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
    self.focus_handle.focus(window);
    self.blink_cursor.update(cx, |cursor, cx| {
      cursor.start(cx);
    });
  }

  pub fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
    self.select_to(self.previous_boundary(self.cursor()), cx);
  }

  pub fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
    self.select_to(self.next_boundary(self.cursor()), cx);
  }

  pub fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
    if self.mode.is_single_line() {
      return;
    }
    let offset = self.start_of_line().saturating_sub(1);
    self.select_to(self.previous_boundary(offset), cx);
  }

  pub fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
    if self.mode.is_single_line() {
      return;
    }
    let offset = (self.end_of_line() + 1).min(self.text.len());
    self.select_to(self.next_boundary(offset), cx);
  }

  pub fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
    self.selected_range = (0..self.text.len()).into();
    self.selection_reversed = false;
    cx.notify();
  }

  pub fn select_to_start(&mut self, _: &SelectToStart, _: &mut Window, cx: &mut Context<Self>) {
    self.select_to(0, cx);
  }

  pub fn select_to_end(&mut self, _: &SelectToEnd, _: &mut Window, cx: &mut Context<Self>) {
    let end = self.text.len();
    self.select_to(end, cx);
  }

  pub fn select_to_start_of_line(
    &mut self,
    _: &SelectToStartOfLine,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let offset = self.start_of_line();
    self.select_to(offset, cx);
  }

  pub fn select_to_end_of_line(
    &mut self,
    _: &SelectToEndOfLine,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let offset = self.end_of_line();
    self.select_to(offset, cx);
  }

  pub fn select_to_previous_word(
    &mut self,
    _: &SelectToPreviousWordStart,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let offset = self.previous_start_of_word();
    self.select_to(offset, cx);
  }

  pub fn select_to_next_word(
    &mut self,
    _: &SelectToNextWordEnd,
    _: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let offset = self.next_end_of_word();
    self.select_to(offset, cx);
  }

  /// Return the start offset of the previous word.
  pub fn previous_start_of_word(&mut self) -> usize {
    let offset = self.selected_range.start;
    let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
    let left_part = self.text.slice(0..offset).to_string();

    UnicodeSegmentation::split_word_bound_indices(left_part.as_str())
      .rfind(|(_, s)| !s.trim_start().is_empty())
      .map(|(i, _)| i)
      .unwrap_or(0)
  }

  /// Return the next end offset of the next word.
  pub fn next_end_of_word(&mut self) -> usize {
    let offset = self.cursor();
    let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
    let right_part = self.text.slice(offset..self.text.len()).to_string();

    UnicodeSegmentation::split_word_bound_indices(right_part.as_str())
      .find(|(_, s)| !s.trim_start().is_empty())
      .map(|(i, s)| offset + i + s.len())
      .unwrap_or(self.text.len())
  }

  /// Get start of line byte offset of cursor
  pub fn start_of_line(&self) -> usize {
    if self.mode.is_single_line() {
      return 0;
    }

    let row = self.text.offset_to_point(self.cursor()).row;
    self.text.line_start_offset(row)
  }

  /// Get end of line byte offset of cursor
  pub fn end_of_line(&self) -> usize {
    if self.mode.is_single_line() {
      return self.text.len();
    }

    let row = self.text.offset_to_point(self.cursor()).row;
    self.text.line_end_offset(row)
  }

  /// Get start line of selection start or end (The min value).
  ///
  /// This is means is always get the first line of selection.
  pub fn start_of_line_of_selection(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> usize {
    if self.mode.is_single_line() {
      return 0;
    }

    let mut offset = self.previous_boundary(self.selected_range.start.min(self.selected_range.end));
    if self.text.char_at(offset) == Some('\r') {
      offset += 1;
    }

    self
      .text_for_range(self.range_to_utf16(&(0..offset + 1)), &mut None, window, cx)
      .unwrap_or_default()
      .rfind('\n')
      .map(|i| i + 1)
      .unwrap_or(0)
  }

  /// Get indent string of next line.
  ///
  /// To get current and next line indent, to return more depth one.
  pub fn indent_of_next_line(&mut self) -> String {
    if self.mode.is_single_line() {
      return "".into();
    }

    let mut current_indent = String::new();
    let mut next_indent = String::new();
    let current_line_start_pos = self.start_of_line();
    let next_line_start_pos = self.end_of_line();
    for c in self.text.slice(current_line_start_pos..).chars() {
      if !c.is_whitespace() {
        break;
      }
      if c == '\n' || c == '\r' {
        break;
      }
      current_indent.push(c);
    }

    for c in self.text.slice(next_line_start_pos..).chars() {
      if !c.is_whitespace() {
        break;
      }
      if c == '\n' || c == '\r' {
        break;
      }
      next_indent.push(c);
    }

    if next_indent.len() > current_indent.len() {
      next_indent
    } else {
      current_indent
    }
  }

  pub fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
    if self.selected_range.is_empty() {
      self.select_to(self.previous_boundary(self.cursor()), cx)
    }
    self.replace_text_in_range(None, "", window, cx);
    self.pause_blink_cursor(cx);
  }

  pub fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
    if self.selected_range.is_empty() {
      self.select_to(self.next_boundary(self.cursor()), cx)
    }
    self.replace_text_in_range(None, "", window, cx);
    self.pause_blink_cursor(cx);
  }

  pub fn delete_to_beginning_of_line(
    &mut self,
    _: &DeleteToBeginningOfLine,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if !self.selected_range.is_empty() {
      self.replace_text_in_range(None, "", window, cx);
      self.pause_blink_cursor(cx);
      return;
    }

    let mut offset = self.start_of_line();
    if offset == self.cursor() {
      offset = offset.saturating_sub(1);
    }
    self.replace_text_in_range_silent(
      Some(self.range_to_utf16(&(offset..self.cursor()))),
      "",
      window,
      cx,
    );
    self.pause_blink_cursor(cx);
  }

  pub fn delete_to_end_of_line(
    &mut self,
    _: &DeleteToEndOfLine,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if !self.selected_range.is_empty() {
      self.replace_text_in_range(None, "", window, cx);
      self.pause_blink_cursor(cx);
      return;
    }

    let mut offset = self.end_of_line();
    if offset == self.cursor() {
      offset = (offset + 1).clamp(0, self.text.len());
    }
    self.replace_text_in_range_silent(
      Some(self.range_to_utf16(&(self.cursor()..offset))),
      "",
      window,
      cx,
    );
    self.pause_blink_cursor(cx);
  }

  pub fn delete_previous_word(
    &mut self,
    _: &DeleteToPreviousWordStart,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if !self.selected_range.is_empty() {
      self.replace_text_in_range(None, "", window, cx);
      self.pause_blink_cursor(cx);
      return;
    }

    let offset = self.previous_start_of_word();
    self.replace_text_in_range_silent(
      Some(self.range_to_utf16(&(offset..self.cursor()))),
      "",
      window,
      cx,
    );
    self.pause_blink_cursor(cx);
  }

  pub fn delete_next_word(
    &mut self,
    _: &DeleteToNextWordEnd,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if !self.selected_range.is_empty() {
      self.replace_text_in_range(None, "", window, cx);
      self.pause_blink_cursor(cx);
      return;
    }

    let offset = self.next_end_of_word();
    self.replace_text_in_range_silent(
      Some(self.range_to_utf16(&(self.cursor()..offset))),
      "",
      window,
      cx,
    );
    self.pause_blink_cursor(cx);
  }

  pub fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
    if self.mode.is_multi_line() {
      self.replace_text_in_range_silent(None, "\n", window, cx);
      self.pause_blink_cursor(cx);
    } else {
      // Single line input, just emit the event (e.g.: In a modal dialog to confirm).
      cx.propagate();
    }

    cx.emit(InputEvent::PressEnter {
      secondary: action.secondary,
    });
  }

  pub fn clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.replace_text("", window, cx);
    self.selected_range = (0..0).into();
    self.scroll_to(0, cx);
  }

  pub fn escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
    if self.ime_marked_range.is_some() {
      self.unmark_text(window, cx);
    }

    if self.clean_on_escape {
      return self.clean(window, cx);
    }

    cx.propagate();
  }

  pub fn on_mouse_down(
    &mut self,
    event: &MouseDownEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    // If there have IME marked range and is empty (Means pressed Esc to abort IME typing)
    // Clear the marked range.
    if let Some(ime_marked_range) = &self.ime_marked_range
      && ime_marked_range.is_empty()
    {
      self.ime_marked_range = None;
    }

    self.selecting = true;
    let offset = self.index_for_mouse_position(event.position);

    // Double click to select word
    if event.button == MouseButton::Left && event.click_count == 2 {
      self.select_word(offset, window, cx);
      return;
    }

    if event.modifiers.shift {
      self.select_to(offset, cx);
    } else {
      self.move_to(offset, cx)
    }
  }

  pub fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
    self.selecting = false;
    self.selected_word_range = None;
  }

  pub fn on_scroll_wheel(
    &mut self,
    event: &ScrollWheelEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    cx.stop_propagation();

    let line_height = self
      .last_layout
      .as_ref()
      .map(|layout| layout.line_height)
      .unwrap_or(window.line_height());
    let delta = event.delta.pixel_delta(line_height);
    self.update_scroll_offset(Some(self.scroll_handle.offset() + delta), cx);
  }

  pub fn update_scroll_offset(&mut self, offset: Option<Point<Pixels>>, cx: &mut Context<Self>) {
    let mut offset = offset.unwrap_or(self.scroll_handle.offset());

    let safe_y_range =
      (-self.scroll_size.height + self.input_bounds.size.height).min(px(0.0))..px(0.);
    let safe_x_range =
      (-self.scroll_size.width + self.input_bounds.size.width).min(px(0.0))..px(0.);

    offset.y = if self.mode.is_single_line() {
      px(0.)
    } else {
      offset.y.clamp(safe_y_range.start, safe_y_range.end)
    };
    offset.x = offset.x.clamp(safe_x_range.start, safe_x_range.end);
    self.scroll_handle.set_offset(offset);
    cx.notify();
  }

  pub fn scroll_to(&mut self, offset: usize, cx: &mut Context<Self>) {
    let Some(last_layout) = self.last_layout.as_ref() else {
      return;
    };
    let Some(bounds) = self.last_bounds.as_ref() else {
      return;
    };

    let mut scroll_offset = self.scroll_handle.offset();
    let line_height = last_layout.line_height;

    let point = self.text.offset_to_point(offset);
    let row = point.row;

    let mut row_offset_y = px(0.);
    for (ix, wrap_line) in self.text_wrapper.lines.iter().enumerate() {
      if ix == row {
        break;
      }

      row_offset_y += wrap_line.height(line_height);
    }

    if let Some(line) = last_layout
      .lines
      .get(row.saturating_sub(last_layout.visible_range.start))
    {
      // Check to scroll horizontally
      if let Some(pos) = line.position_for_index(point.column, line_height) {
        let bounds_width = bounds.size.width;
        let col_offset_x = pos.x;
        if col_offset_x - RIGHT_MARGIN < -scroll_offset.x {
          // If the position is out of the visible area, scroll to make it visible
          scroll_offset.x = -col_offset_x + RIGHT_MARGIN;
        } else if col_offset_x + RIGHT_MARGIN > -scroll_offset.x + bounds_width {
          scroll_offset.x = -(col_offset_x - bounds_width + RIGHT_MARGIN);
        }
      }
    }

    // Check if row_offset_y is out of the viewport
    // If row offset is not in the viewport, scroll to make it visible
    let edge_height = line_height;
    if row_offset_y - edge_height < -scroll_offset.y {
      // Scroll up
      scroll_offset.y = -row_offset_y + edge_height;
    } else if row_offset_y + edge_height > -scroll_offset.y + bounds.size.height {
      // Scroll down
      scroll_offset.y = -(row_offset_y - bounds.size.height + edge_height);
    }

    scroll_offset.x = scroll_offset.x.min(px(0.));
    scroll_offset.y = scroll_offset.y.min(px(0.));
    self.deferred_scroll_offset = Some(scroll_offset);
    cx.notify();
  }

  pub fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
    if self.selected_range.is_empty() {
      return;
    }

    let selected_text = self.text.slice(self.selected_range).to_string();
    cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
  }

  pub fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
    if self.selected_range.is_empty() {
      return;
    }

    let selected_text = self.text.slice(self.selected_range).to_string();
    cx.write_to_clipboard(ClipboardItem::new_string(selected_text));

    self.replace_text_in_range_silent(None, "", window, cx);
  }

  pub fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(clipboard) = cx.read_from_clipboard() {
      let mut new_text = clipboard.text().unwrap_or_default();
      if !self.mode.is_multi_line() {
        new_text = new_text.replace('\n', "");
      }

      self.replace_text_in_range_silent(None, &new_text, window, cx);
      self.scroll_to(self.cursor(), cx);
    }
  }

  fn push_history(&mut self, text: &Rope, range: &Range<usize>, new_text: &str) {
    if self.history.ignore {
      return;
    }

    let old_text = text.slice(range.clone()).to_string();
    let new_range = range.start..range.start + new_text.len();

    self
      .history
      .push(Change::new(range.clone(), &old_text, new_range, new_text));
  }

  pub fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
    self.history.ignore = true;
    if let Some(changes) = self.history.undo() {
      for change in changes {
        let range_utf16 = self.range_to_utf16(&change.new_range.into());
        self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
      }
    }
    self.history.ignore = false;
  }

  pub fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
    self.history.ignore = true;
    if let Some(changes) = self.history.redo() {
      for change in changes {
        let range_utf16 = self.range_to_utf16(&change.old_range.into());
        self.replace_text_in_range_silent(Some(range_utf16), &change.new_text, window, cx);
      }
    }
    self.history.ignore = false;
  }

  /// Get byte offset of the cursor.
  ///
  /// The offset is the UTF-8 offset.
  pub fn cursor(&self) -> usize {
    if let Some(ime_marked_range) = &self.ime_marked_range {
      return ime_marked_range.end;
    }

    if self.selection_reversed {
      self.selected_range.start
    } else {
      self.selected_range.end
    }
  }

  pub fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
    // If the text is empty, always return 0
    if self.text.len() == 0 {
      return 0;
    }

    let (Some(bounds), Some(last_layout)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
    else {
      return 0;
    };

    let line_height = last_layout.line_height;

    // TIP: About the IBeam cursor
    //
    // If cursor style is IBeam, the mouse mouse position is in the middle of the cursor (This is special in OS)

    // The position is relative to the bounds of the text input
    //
    // bounds.origin:
    //
    // - included the input padding.
    // - included the scroll offset.
    let inner_position = position - bounds.origin;

    let mut index = last_layout.visible_range_offset.start;
    let mut y_offset = last_layout.visible_top;
    for (ix, line) in self
      .text_wrapper
      .lines
      .iter()
      .skip(last_layout.visible_range.start)
      .enumerate()
    {
      let line_origin = self.line_origin_with_y_offset(&mut y_offset, line, line_height);
      let pos = inner_position - line_origin;

      let Some(line_layout) = last_layout.lines.get(ix) else {
        if pos.y < line_origin.y + line_height {
          break;
        }

        continue;
      };

      // Return offset by use closest_index_for_x if is single line mode.
      if self.mode.is_single_line() {
        index = line_layout.closest_index_for_x(pos.x);
        break;
      }

      if let Some(v) = line_layout.closest_index_for_position(pos, line_height) {
        index += v;
        break;
      } else if pos.y < px(0.) {
        break;
      }

      // +1 for `\n`
      index += line_layout.len() + 1;
    }

    if self.masked {
      // index is a byte offset into the masked display text (MASK_CHAR per char).
      // Convert to a char index, then to a byte offset in the original text.
      let char_count = self.text.offset_to_char_index(self.text.len());
      let char_index = (index / MASK_CHAR_LEN).min(char_count);
      self.text.char_index_to_offset(char_index)
    } else if index > self.text.len() {
      self.text.len()
    } else {
      index
    }
  }

  /// Returns a y offsetted point for the line origin.
  fn line_origin_with_y_offset(
    &self,
    y_offset: &mut Pixels,
    line: &LineItem,
    line_height: Pixels,
  ) -> Point<Pixels> {
    // NOTE: About line.wrap_boundaries.len()
    //
    // If only 1 line, the value is 0
    // If have 2 line, the value is 1
    if self.mode.is_multi_line() {
      let p = point(px(0.), *y_offset);
      *y_offset += line.height(line_height);
      p
    } else {
      point(px(0.), px(0.))
    }
  }

  /// Select the text from the current cursor position to the given offset.
  ///
  /// The offset is the UTF-8 offset.
  ///
  /// Ensure the offset use self.next_boundary or self.previous_boundary to get the correct offset.
  pub fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
    let offset = offset.clamp(0, self.text.len());
    if self.selection_reversed {
      self.selected_range.start = offset
    } else {
      self.selected_range.end = offset
    };

    if self.selected_range.end < self.selected_range.start {
      self.selection_reversed = !self.selection_reversed;
      self.selected_range = (self.selected_range.end..self.selected_range.start).into();
    }

    // Ensure keep word selected range
    if let Some(word_range) = self.selected_word_range.as_ref() {
      if self.selected_range.start > word_range.start {
        self.selected_range.start = word_range.start;
      }
      if self.selected_range.end < word_range.end {
        self.selected_range.end = word_range.end;
      }
    }
    if self.selected_range.is_empty() {
      self.update_preferred_column();
    }
    cx.notify()
  }

  /// Select the word at the given offset.
  ///
  /// The offset is the UTF-8 offset.
  ///
  /// FIXME: When click on a non-word character, the word is not selected.
  fn select_word(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
    #[inline(always)]
    fn is_word(c: char) -> bool {
      c.is_alphanumeric() || matches!(c, '_')
    }

    let mut start = offset;
    let mut end = start;
    let prev_text = self
      .text_for_range(self.range_to_utf16(&(0..start)), &mut None, window, cx)
      .unwrap_or_default();
    let next_text = self
      .text_for_range(
        self.range_to_utf16(&(end..self.text.len())),
        &mut None,
        window,
        cx,
      )
      .unwrap_or_default();

    let prev_chars = prev_text.chars().rev();
    let next_chars = next_text.chars();

    let pre_chars_count = prev_chars.clone().count();
    for (ix, c) in prev_chars.enumerate() {
      if !is_word(c) {
        break;
      }

      if ix < pre_chars_count {
        start = start.saturating_sub(c.len_utf8());
      }
    }

    for c in next_chars {
      if !is_word(c) {
        break;
      }

      end += c.len_utf8();
    }

    if start == end {
      return;
    }

    self.selected_range = (start..end).into();
    self.selected_word_range = Some(self.selected_range);
    cx.notify()
  }

  /// Unselects the currently selected text.
  pub fn unselect(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    let offset = self.cursor();
    self.selected_range = (offset..offset).into();
    cx.notify()
  }

  #[inline]
  pub fn offset_from_utf16(&self, offset: usize) -> usize {
    self.text.offset_utf16_to_offset(offset)
  }

  #[inline]
  pub fn offset_to_utf16(&self, offset: usize) -> usize {
    self.text.offset_to_offset_utf16(offset)
  }

  #[inline]
  pub fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
    self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
  }

  #[inline]
  pub fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
    self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
  }

  pub fn previous_boundary(&self, offset: usize) -> usize {
    let mut offset = self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
    if let Some(ch) = self.text.char_at(offset)
      && ch == '\r'
    {
      offset -= 1;
    }

    offset
  }

  pub fn next_boundary(&self, offset: usize) -> usize {
    let mut offset = self.text.clip_offset(offset + 1, Bias::Right);
    if let Some(ch) = self.text.char_at(offset)
      && ch == '\r'
    {
      offset += 1;
    }

    offset
  }

  /// Returns the true to let InputElement to render cursor, when Input is focused and current BlinkCursor is visible.
  pub fn show_cursor(&self, window: &Window, cx: &App) -> bool {
    self.focus_handle.is_focused(window)
      && self.blink_cursor.read(cx).visible()
      && window.is_window_active()
  }

  fn on_focus(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    self.blink_cursor.update(cx, |cursor, cx| {
      cursor.start(cx);
    });
    cx.emit(InputEvent::Focus);
  }

  fn on_blur(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    self.blink_cursor.update(cx, |cursor, cx| {
      cursor.stop(cx);
    });
    cx.emit(InputEvent::Blur);
    cx.notify();
  }

  pub fn pause_blink_cursor(&mut self, cx: &mut Context<Self>) {
    self.blink_cursor.update(cx, |cursor, cx| {
      cursor.pause(cx);
    });
  }

  pub fn on_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
    self.pause_blink_cursor(cx);
  }

  pub fn on_drag_move(
    &mut self,
    event: &MouseMoveEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.text.len() == 0 {
      return;
    }

    if self.last_layout.is_none() {
      return;
    }

    if !self.focus_handle.is_focused(window) {
      return;
    }

    if !self.selecting {
      return;
    }

    let offset = self.index_for_mouse_position(event.position);
    self.select_to(offset, cx);
  }

  fn is_valid_input(&self, new_text: &str, cx: &mut Context<Self>) -> bool {
    if new_text.is_empty() {
      return true;
    }

    if let Some(validate) = &self.validate
      && !validate(new_text, cx)
    {
      return false;
    }

    true
  }

  pub fn set_input_bounds(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
    let wrap_width_changed = self.input_bounds.size.width != new_bounds.size.width;
    self.input_bounds = new_bounds;

    // Update text_wrapper wrap_width if changed.
    if let Some(last_layout) = self.last_layout.as_ref()
      && wrap_width_changed
    {
      let wrap_width = if !self.soft_wrap {
        // None to disable wrapping (will use Pixels::MAX)
        None
      } else {
        last_layout.wrap_width
      };

      self.text_wrapper.set_wrap_width(wrap_width, cx);
      self.mode.update_auto_grow(&self.text_wrapper);
      cx.notify();
    }
  }

  pub fn selected_text(&self) -> RopeSlice<'_> {
    let range_utf16 = self.range_to_utf16(&self.selected_range.into());
    let range = self.range_from_utf16(&range_utf16);
    self.text.slice(range)
  }

  pub fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
    let last_layout = self.last_layout.as_ref()?;
    let last_bounds = self.last_bounds?;

    let (_, _, start_pos) = self.line_and_position_for_offset(range.start);
    let (_, _, end_pos) = self.line_and_position_for_offset(range.end);

    let start_pos = start_pos?;
    let end_pos = end_pos?;

    Some(Bounds::from_corners(
      last_bounds.origin + start_pos,
      last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
    ))
  }

  /// Replace text in range in silent.
  ///
  /// This will not trigger any UI interaction, such as auto-completion.
  pub fn replace_text_in_range_silent(
    &mut self,
    range_utf16: Option<Range<usize>>,
    new_text: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.silent_replace_text = true;
    self.replace_text_in_range(range_utf16, new_text, window, cx);
    self.silent_replace_text = false;
  }
}

impl EntityInputHandler for InputState {
  fn text_for_range(
    &mut self,
    range_utf16: Range<usize>,
    adjusted_range: &mut Option<Range<usize>>,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<String> {
    let range = self.range_from_utf16(&range_utf16);
    adjusted_range.replace(self.range_to_utf16(&range));
    Some(self.text.slice(range).to_string())
  }

  fn selected_text_range(
    &mut self,
    _ignore_disabled_input: bool,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<UTF16Selection> {
    Some(UTF16Selection {
      range: self.range_to_utf16(&self.selected_range.into()),
      reversed: false,
    })
  }

  fn marked_text_range(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<Range<usize>> {
    self
      .ime_marked_range
      .map(|range| self.range_to_utf16(&range.into()))
  }

  fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
    self.ime_marked_range = None;
  }

  /// Replace text in range.
  ///
  /// - If the new text is invalid, it will not be replaced.
  /// - If `range_utf16` is not provided, the current selected range will be used.
  fn replace_text_in_range(
    &mut self,
    range_utf16: Option<Range<usize>>,
    new_text: &str,
    _window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.disabled {
      return;
    }

    self.pause_blink_cursor(cx);

    let range = range_utf16
      .as_ref()
      .map(|range_utf16| self.range_from_utf16(range_utf16))
      .or(self.ime_marked_range.map(|range| {
        let range = self.range_to_utf16(&(range.start..range.end));
        self.range_from_utf16(&range)
      }))
      .unwrap_or(self.selected_range.into());

    let old_text = self.text.clone();
    self.text.replace(range.clone(), new_text);

    let new_offset = (range.start + new_text.len()).min(self.text.len());

    if self.mode.is_single_line() {
      let pending_text = self.text.to_string();
      // Check if the new text is valid
      if !self.is_valid_input(&pending_text, cx) {
        self.text = old_text;
        return;
      }
    }

    self.push_history(&old_text, &range, new_text);
    self.history.end_grouping();
    self
      .text_wrapper
      .update(&self.text, &range, &Rope::from(new_text), cx);
    self.selected_range = (new_offset..new_offset).into();
    self.ime_marked_range.take();
    self.update_preferred_column();
    self.mode.update_auto_grow(&self.text_wrapper);
    cx.emit(InputEvent::Change);
    cx.notify();
  }

  /// Mark text is the IME temporary insert on typing.
  fn replace_and_mark_text_in_range(
    &mut self,
    range_utf16: Option<Range<usize>>,
    new_text: &str,
    new_selected_range_utf16: Option<Range<usize>>,
    _window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.disabled {
      return;
    }

    let range = range_utf16
      .as_ref()
      .map(|range_utf16| self.range_from_utf16(range_utf16))
      .or(self.ime_marked_range.map(|range| {
        let range = self.range_to_utf16(&(range.start..range.end));
        self.range_from_utf16(&range)
      }))
      .unwrap_or(self.selected_range.into());

    let old_text = self.text.clone();
    self.text.replace(range.clone(), new_text);

    if self.mode.is_single_line() {
      let pending_text = self.text.to_string();
      if !self.is_valid_input(&pending_text, cx) {
        self.text = old_text;
        return;
      }
    }

    self
      .text_wrapper
      .update(&self.text, &range, &Rope::from(new_text), cx);
    if new_text.is_empty() {
      // Cancel selection, when cancel IME input.
      self.selected_range = (range.start..range.start).into();
      self.ime_marked_range = None;
    } else {
      self.ime_marked_range = Some((range.start..range.start + new_text.len()).into());
      self.selected_range = new_selected_range_utf16
        .as_ref()
        .map(|range_utf16| self.range_from_utf16(range_utf16))
        .map(|new_range| new_range.start + range.start..new_range.end + range.end)
        .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len())
        .into();
    }
    self.mode.update_auto_grow(&self.text_wrapper);
    self.history.start_grouping();
    self.push_history(&old_text, &range, new_text);
    cx.notify();
  }

  /// Used to position IME candidates.
  fn bounds_for_range(
    &mut self,
    range_utf16: Range<usize>,
    bounds: Bounds<Pixels>,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<Bounds<Pixels>> {
    let last_layout = self.last_layout.as_ref()?;
    let line_height = last_layout.line_height;
    let range = self.range_from_utf16(&range_utf16);

    let mut start_origin = None;
    let mut end_origin = None;
    let mut y_offset = last_layout.visible_top;
    let mut index_offset = last_layout.visible_range_offset.start;

    for line in last_layout.lines.iter() {
      if start_origin.is_some() && end_origin.is_some() {
        break;
      }

      if start_origin.is_none()
        && let Some(p) =
          line.position_for_index(range.start.saturating_sub(index_offset), line_height)
      {
        start_origin = Some(p + point(px(0.), y_offset));
      }

      if end_origin.is_none()
        && let Some(p) =
          line.position_for_index(range.end.saturating_sub(index_offset), line_height)
      {
        end_origin = Some(p + point(px(0.), y_offset));
      }

      index_offset += line.len() + 1;
      y_offset += line.size(line_height).height;
    }

    let start_origin = start_origin.unwrap_or_default();
    let mut end_origin = end_origin.unwrap_or_default();
    // Ensure at same line.
    end_origin.y = start_origin.y;

    Some(Bounds::from_corners(
      bounds.origin + start_origin,
      // + line_height for show IME panel under the cursor line.
      bounds.origin + point(end_origin.x, end_origin.y + line_height),
    ))
  }

  fn character_index_for_point(
    &mut self,
    point: gpui::Point<Pixels>,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<usize> {
    let last_layout = self.last_layout.as_ref()?;
    let line_height = last_layout.line_height;
    let line_point = self.last_bounds?.localize(&point)?;
    let offset = last_layout.visible_range_offset.start;

    for line in last_layout.lines.iter() {
      if let Some(utf8_index) = line.index_for_position(line_point, line_height) {
        return Some(self.offset_to_utf16(offset + utf8_index));
      }
    }

    None
  }
}

impl Focusable for InputState {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for InputState {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    if self._pending_update {
      self._pending_update = false;
    }

    div()
      .id("input-state")
      .flex_1()
      .when(self.mode.is_multi_line(), |this| this.h_full())
      .flex_grow()
      .overflow_x_hidden()
      .child(text_element(cx.entity().clone()).placeholder(self.placeholder.clone()))
  }
}

pub fn input_state_listeners(
  this: &Entity<InputState>,
  div: Stateful<Div>,
  window: &mut Window,
  disabled: bool,
  is_multi_line: bool,
) -> Stateful<Div> {
  div
    .when(!disabled, |div| {
      div
        .on_action(window.listener_for(this, InputState::backspace))
        .on_action(window.listener_for(this, InputState::delete))
        .on_action(window.listener_for(this, InputState::delete_to_beginning_of_line))
        .on_action(window.listener_for(this, InputState::delete_to_end_of_line))
        .on_action(window.listener_for(this, InputState::delete_previous_word))
        .on_action(window.listener_for(this, InputState::delete_next_word))
        .on_action(window.listener_for(this, InputState::enter))
        .on_action(window.listener_for(this, InputState::escape))
        .on_action(window.listener_for(this, InputState::paste))
        .on_action(window.listener_for(this, InputState::cut))
        .on_action(window.listener_for(this, InputState::undo))
        .on_action(window.listener_for(this, InputState::redo))
    })
    .on_action(window.listener_for(this, InputState::left))
    .on_action(window.listener_for(this, InputState::right))
    .on_action(window.listener_for(this, InputState::select_left))
    .on_action(window.listener_for(this, InputState::select_right))
    .when(is_multi_line, |div| {
      div
        .on_action(window.listener_for(this, InputState::up))
        .on_action(window.listener_for(this, InputState::down))
        .on_action(window.listener_for(this, InputState::select_up))
        .on_action(window.listener_for(this, InputState::select_down))
        .on_action(window.listener_for(this, InputState::page_up))
        .on_action(window.listener_for(this, InputState::page_down))
    })
    .on_action(window.listener_for(this, InputState::select_all))
    .on_action(window.listener_for(this, InputState::select_to_start_of_line))
    .on_action(window.listener_for(this, InputState::select_to_end_of_line))
    .on_action(window.listener_for(this, InputState::select_to_previous_word))
    .on_action(window.listener_for(this, InputState::select_to_next_word))
    .on_action(window.listener_for(this, InputState::home))
    .on_action(window.listener_for(this, InputState::end))
    .on_action(window.listener_for(this, InputState::move_to_start))
    .on_action(window.listener_for(this, InputState::move_to_end))
    .on_action(window.listener_for(this, InputState::move_to_previous_word))
    .on_action(window.listener_for(this, InputState::move_to_next_word))
    .on_action(window.listener_for(this, InputState::select_to_start))
    .on_action(window.listener_for(this, InputState::select_to_end))
    .on_action(window.listener_for(this, InputState::copy))
    .on_key_down(window.listener_for(this, InputState::on_key_down))
    .on_mouse_down(
      MouseButton::Left,
      window.listener_for(this, InputState::on_mouse_down),
    )
    .on_mouse_down(
      MouseButton::Right,
      window.listener_for(this, InputState::on_mouse_down),
    )
    .on_mouse_up(
      MouseButton::Left,
      window.listener_for(this, InputState::on_mouse_up),
    )
    .on_mouse_up(
      MouseButton::Right,
      window.listener_for(this, InputState::on_mouse_up),
    )
    .on_scroll_wheel(window.listener_for(this, InputState::on_scroll_wheel))
}
