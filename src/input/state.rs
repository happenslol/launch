use std::{
  ops::{Range, RangeBounds},
  time::{Duration, Instant},
};

use gpui::{
  Bounds, Context, EntityInputHandler, EventEmitter, FocusHandle, Pixels, Point, Subscription,
  UTF16Selection, Window,
};
use ropey::Rope;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Bias {
  Left,
  Right,
}

// Custom selection type instead of range since we want Copy
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Selection {
  start: usize,
  end: usize,
}

impl From<Range<usize>> for Selection {
  fn from(value: Range<usize>) -> Self {
    Self {
      start: value.start,
      end: value.end,
    }
  }
}
impl From<Selection> for Range<usize> {
  fn from(value: Selection) -> Self {
    value.start..value.end
  }
}
impl RangeBounds<usize> for Selection {
  fn start_bound(&self) -> std::ops::Bound<&usize> {
    std::ops::Bound::Included(&self.start)
  }

  fn end_bound(&self) -> std::ops::Bound<&usize> {
    std::ops::Bound::Excluded(&self.end)
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Change {
  before: (Selection, String),
  after: (Selection, String),
}

struct History {
  undos: Vec<(usize, Change)>,
  redos: Vec<(usize, Change)>,
  version: usize,
  max: usize,
  grouping_threshold: Duration,
  last_change_at: Instant,
}

impl History {
  fn new() -> Self {
    Self {
      undos: Vec::new(),
      redos: Vec::new(),
      version: 0,
      max: 100,
      grouping_threshold: Duration::from_millis(500),
      last_change_at: Instant::now(),
    }
  }

  fn push(&mut self, item: Change) {
    // Do we need to increment the version?
    if self.last_change_at.elapsed() > self.grouping_threshold {
      self.version += 1;
    }

    self.last_change_at = Instant::now();
    self.undos.push((self.version, item));
  }

  fn undo(&mut self) -> Option<Vec<Change>> {
    let (version, item) = self.undos.pop()?;

    let mut changes = vec![item];
    while let Some((_, i)) = self.redos.pop_if(|(v, _)| *v < version) {
      changes.push(i);
    }

    self
      .redos
      .extend(changes.iter().map(|i| (version, i.clone())));

    Some(changes)
  }
}

pub struct InputState {
  focus_handle: FocusHandle,
  value: Rope,
  history: History,
  selection: Option<Selection>,
  selection_reversed: bool,
  ime_marked_range: Option<Selection>,
  disabled: bool,
  _subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
pub enum InputEvent {
  Change,
  Submit,
  Focus,
  Blur,
}

impl EventEmitter<InputEvent> for InputState {}

impl InputState {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle().tab_stop(true);
    let history = History::new();

    let subscriptions = vec![
      cx.on_focus(&focus_handle, window, Self::on_focus),
      cx.on_blur(&focus_handle, window, Self::on_blur),
    ];

    Self {
      focus_handle,
      value: Rope::new(),
      history,
      selection: None,
      selection_reversed: false,
      ime_marked_range: None,
      disabled: false,
      _subscriptions: subscriptions,
    }
  }

  fn on_focus(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    cx.emit(InputEvent::Focus);
  }

  fn on_blur(&mut self, _: &mut Window, cx: &mut Context<Self>) {
    cx.emit(InputEvent::Blur);
  }

  #[inline]
  fn offset_from_utf16(&self, offset_utf16: usize) -> usize {
    self
      .value
      .try_utf16_cu_to_char(offset_utf16)
      .unwrap_or_else(|_| self.value.len_bytes())
  }

  #[inline]
  fn offset_to_utf16(&self, offset: usize) -> usize {
    self
      .value
      .try_char_to_utf16_cu(offset)
      .unwrap_or_else(|_| self.value.len_utf16_cu())
  }

  #[inline]
  fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
    self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
  }

  #[inline]
  fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
    self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
  }

  fn clamp_offset(&self, offset: usize, bias: Bias) -> usize {
    if offset > self.value.len_bytes() {
      return self.value.len_bytes();
    }

    0
    // TODO: wip
  }
}

impl EntityInputHandler for InputState {
  fn text_for_range(
    &mut self,
    range: Range<usize>,
    adjusted_range: &mut Option<Range<usize>>,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<String> {
    let range = self.range_from_utf16(&range);
    adjusted_range.replace(self.range_to_utf16(&range));
    Some(self.value.slice(range).to_string())
  }

  fn selected_text_range(
    &mut self,
    _ignore_disabled_input: bool,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<UTF16Selection> {
    Some(UTF16Selection {
      range: self.range_to_utf16(&self.selection?.into()),
      reversed: false,
    })
  }

  fn marked_text_range(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Self>,
  ) -> Option<Range<usize>> {
    Some(self.range_to_utf16(&self.ime_marked_range?.into()))
  }

  fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
    self.ime_marked_range = None;
  }

  fn replace_text_in_range(
    &mut self,
    range_utf16: Option<Range<usize>>,
    text: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.disabled {
      return;
    }

    // self.pause_blink_cursor(cx);

    let range = range_utf16
      .as_ref()
      .map(|range_utf16| self.range_from_utf16(range_utf16))
      .or_else(|| {
        self.ime_marked_range.map(|range| {
          let range = self.range_to_utf16(&(range.start..range.end));
          self.range_from_utf16(&range)
        })
      })
      .or_else(|| self.selection.map(|selection| selection.into()))
      .unwrap_or(0..0);

    let prev = self.value.clone();

    self.value.replace(range.clone(), new_text);

    let mut new_offset = (range.start + new_text.len()).min(self.text.len());

    if self.mode.is_single_line() {
      let pending_text = self.text.to_string();
      // Check if the new text is valid
      if !self.is_valid_input(&pending_text, cx) {
        self.text = old_text;
        return;
      }

      if !self.mask_pattern.is_none() {
        let mask_text = self.mask_pattern.mask(&pending_text);
        self.text = Rope::from(mask_text.as_str());
        let new_text_len = (new_text.len() + mask_text.len()).saturating_sub(pending_text.len());
        new_offset = (range.start + new_text_len).min(mask_text.len());
      }
    }

    self.push_history(&old_text, &range, &new_text);
    self.history.end_grouping();
    if let Some(diagnostics) = self.mode.diagnostics_mut() {
      diagnostics.reset(&self.text)
    }
    self
      .text_wrapper
      .update(&self.text, &range, &Rope::from(new_text), cx);
    self
      .mode
      .update_highlighter(&range, &self.text, &new_text, true, cx);
    self.lsp.update(&self.text, window, cx);
    self.selected_range = (new_offset..new_offset).into();
    self.ime_marked_range.take();
    self.update_preferred_column();
    self.update_search(cx);
    self.mode.update_auto_grow(&self.text_wrapper);
    if !self.silent_replace_text {
      self.handle_completion_trigger(&range, &new_text, window, cx);
    }
    cx.emit(InputEvent::Change);
    cx.notify();
  }

  fn replace_and_mark_text_in_range(
    &mut self,
    range: Option<Range<usize>>,
    new_text: &str,
    new_selected_range: Option<Range<usize>>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    todo!()
  }

  fn bounds_for_range(
    &mut self,
    range_utf16: Range<usize>,
    element_bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Option<Bounds<Pixels>> {
    todo!()
  }

  fn character_index_for_point(
    &mut self,
    point: Point<Pixels>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Option<usize> {
    todo!()
  }
}
