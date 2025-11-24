use std::{
  ops::Range,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use gpui::{
  App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, KeyBinding,
  ScrollStrategy, Subscription, Task, UniformListScrollHandle, Window, actions, div, prelude::*,
  uniform_list,
};

use crate::{
  input::{
    input,
    state::{InputEvent, InputState},
  },
  util::v_flex,
};

actions!(picker, [SelectNext, SelectPrev]);

pub trait PickerDelegate: Sized + 'static {
  type ListItem: Clone;

  fn render_list_item(
    &self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement;

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    query: String,
    cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()>;
}

pub struct Picker<D: PickerDelegate> {
  delegate: D,
  items: Arc<Vec<D::ListItem>>,
  matches: Option<Vec<(usize, u32)>>,

  search_input: Entity<InputState>,
  selected_index: Option<usize>,
  current_query: String,
  list_scroll_handle: UniformListScrollHandle,
  subscriptions: Vec<Subscription>,

  search_id: Option<usize>,
  next_search_id: usize,
  search_task: Option<Task<()>>,
  cancel_flag: Arc<AtomicBool>,
}

pub enum PickerEvent<D: PickerDelegate> {
  Picked(D::ListItem),
}

impl<D: PickerDelegate> EventEmitter<PickerEvent<D>> for Picker<D> {}

impl<D: PickerDelegate> Picker<D> {
  pub fn new(
    delegate: D,
    items: Vec<D::ListItem>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-j", SelectNext, None),
      KeyBinding::new("down", SelectNext, None),
      KeyBinding::new("ctrl-k", SelectPrev, None),
      KeyBinding::new("up", SelectPrev, None),
    ]);

    let search_input = cx.new(|cx| InputState::new(window, cx));

    let mut this = Self {
      delegate,
      items: Arc::new(items),
      search_input: search_input.clone(),
      selected_index: None,
      matches: None,
      current_query: String::new(),
      list_scroll_handle: UniformListScrollHandle::new(),
      subscriptions: Vec::new(),

      search_id: None,
      next_search_id: 0,
      search_task: None,
      cancel_flag: Arc::new(AtomicBool::new(false)),
    };

    this.subscriptions.push(cx.subscribe_in(
      &search_input,
      window,
      move |this, search_input, ev, window, cx| match *ev {
        InputEvent::PressEnter { .. } => this.launch_selected(cx),
        InputEvent::Change => {
          let new_value = &search_input.read(cx).value();
          if &this.current_query == new_value {
            // Query hasn't changed
            return;
          }

          this.current_query = new_value.to_string();
          this.update_matches(window, cx);
        }
        _ => {}
      },
    ));

    this
  }

  pub fn set_items(
    &mut self,
    items: Vec<D::ListItem>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.items = Arc::new(items);
    cx.notify();

    self.update_matches(window, cx);
  }

  pub fn get_selected_item(&self) -> Option<&D::ListItem> {
    let selected_index = self.selected_index?;

    let resolved_ix = self
      .matches
      .as_ref()
      .map_or(selected_index, |matches| matches[selected_index].0);

    self.items.get(resolved_ix)
  }

  fn launch_selected(&mut self, cx: &mut Context<Self>) {
    let Some(ix) = self.selected_index else {
      return;
    };

    let resolved_ix = self.matches.as_ref().map_or(ix, |matches| matches[ix].0);
    let Some(item) = self.items.get(resolved_ix) else {
      return;
    };

    cx.emit(PickerEvent::Picked(item.clone()));
  }

  fn update_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let query = self.current_query.clone();

    // Cancel ongoing search
    self.cancel_flag.store(true, Ordering::Release);
    self.cancel_flag = Arc::new(AtomicBool::new(false));
    self.search_task = None;

    // Get the next search id
    let current_search_id = self.next_search_id;
    self.next_search_id += 1;

    self.search_task = Some(self.delegate.update_matches(
      window,
      cx,
      query,
      self.cancel_flag.clone(),
      current_search_id,
      self.items.clone(),
    ));
  }

  pub fn complete_search(
    &mut self,
    cx: &mut Context<Self>,
    search_id: usize,
    matches: Vec<(usize, u32)>,
  ) {
    if self.search_id.is_some_and(|id| id > search_id) {
      return;
    }

    self.selected_index = if !matches.is_empty() {
      Some(
        self
          .selected_index
          .map(|ix| ix.min(matches.len() - 1))
          .unwrap_or(0),
      )
    } else {
      None
    };

    self.search_id = Some(search_id);
    self.matches = Some(matches);
    cx.notify();
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self
      .matches
      .as_ref()
      .map_or(self.items.len(), |matches| matches.len());
    if item_count == 0 {
      return;
    }

    // Wrap around to first item
    match self.selected_index {
      Some(i) => self.selected_index = Some((i + 1) % item_count),
      None => self.selected_index = Some(0),
    }

    self
      .list_scroll_handle
      .scroll_to_item(self.selected_index.unwrap(), ScrollStrategy::Bottom);

    cx.notify();
  }

  fn select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self
      .matches
      .as_ref()
      .map_or(self.items.len(), |matches| matches.len());
    if item_count == 0 {
      return;
    }

    match self.selected_index {
      Some(0) | None => self.selected_index = Some(item_count - 1),
      Some(i) => self.selected_index = Some(i - 1),
    }

    self
      .list_scroll_handle
      .scroll_to_item(self.selected_index.unwrap(), ScrollStrategy::Top);

    cx.notify();
  }

  fn render_list_item(
    &self,
    window: &mut Window,
    cx: &mut Context<Self>,
    ix: usize,
    is_selected: bool,
  ) -> impl IntoElement + use<D> {
    div()
      .id(("item", ix))
      .cursor_pointer()
      .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
        let resolved_ix = this.matches.as_ref().map_or(ix, |matches| matches[ix].0);
        cx.emit(PickerEvent::Picked(this.items[resolved_ix].clone()));

        // Maintain focus on the search input
        cx.focus_self(window);
      }))
      .child(
        self
          .delegate
          .render_list_item(window, cx, &self.items[ix], is_selected),
      )
  }
}

impl<D: PickerDelegate> Focusable for Picker<D> {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.search_input.read(cx).focus_handle.clone()
  }
}

impl<D: PickerDelegate> Render for Picker<D> {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let count = self
      .matches
      .as_ref()
      .map_or_else(|| self.items.len(), |matches| matches.len());

    v_flex()
      .on_action(cx.listener(Self::select_next))
      .on_action(cx.listener(Self::select_prev))
      .size_full()
      .child(input(&self.search_input))
      .child(
        uniform_list(
          "matches",
          count,
          cx.processor(|this, range: Range<usize>, window, cx| {
            range
              .map(|ix| {
                let is_selected = this.selected_index.is_some_and(|selected| selected == ix);
                let resolved_ix = this.matches.as_ref().map_or(ix, |matches| matches[ix].0);
                this.render_list_item(window, cx, resolved_ix, is_selected)
              })
              .collect()
          }),
        )
        .track_scroll(self.list_scroll_handle.clone())
        .h_full(),
      )
  }
}
