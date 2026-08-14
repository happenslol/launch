use std::{
  ops::Range,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use gpui::{
  Action, AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
  IntoElement, KeyBinding, ListAlignment, ListOffset, ListState, Pixels, ScrollStrategy,
  SharedString, StyleRefinement, Subscription, Task, UniformListScrollHandle, Window, actions, div,
  list, prelude::*, px, rems, rgb, rgba, uniform_list,
};

use crate::icon::{Icon, IconName, Spinner};
use crate::input::{
  input,
  state::{InputEvent, InputState},
};
use crate::launcher::GoBack;

actions!(picker, [SelectNext, SelectPrev]);

pub struct Category<T> {
  pub name: SharedString,
  pub filter: Box<dyn Fn(&T) -> bool>,
}

impl<T> Category<T> {
  pub fn new(name: impl Into<SharedString>, filter: impl Fn(&T) -> bool + 'static) -> Self {
    Self {
      name: name.into(),
      filter: Box::new(filter),
    }
  }
}

#[derive(PartialEq)]
enum VisualEntry {
  Header(SharedString),
  Item(usize), // index into matches
}

struct SelectableItem {
  match_index: usize,
  list_index: usize, // index in the flat visual_entries list (for scroll_to_reveal_item)
}

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

  fn sort_items(&self, _cx: &App, _items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    // Default implementation: sort by score descending
    matches.sort_by_key(|(_, score)| std::cmp::Reverse(*score));
  }

  fn categories(&self) -> Option<Vec<Category<Self::ListItem>>> {
    None
  }
}

pub struct Picker<D: PickerDelegate> {
  pub delegate: D,
  items: Arc<Vec<D::ListItem>>,
  matches: Option<Vec<(usize, u32)>>,

  pub search_input: Entity<InputState>,

  focus_handle: FocusHandle,
  selected_index: Option<usize>,
  current_query: String,
  list_scroll_handle: UniformListScrollHandle,
  subscriptions: Vec<Subscription>,

  search_id: Option<usize>,
  next_search_id: usize,
  search_task: Option<Task<()>>,
  cancel_flag: Arc<AtomicBool>,

  visual_entries: Option<Vec<VisualEntry>>,
  selectable_items: Option<Vec<SelectableItem>>,
  category_list_state: ListState,
}

pub enum PickerEvent<D: PickerDelegate> {
  Picked(D::ListItem),
  SecondaryPicked(D::ListItem),
  QueryChanged(String),
}

impl<D: PickerDelegate> EventEmitter<PickerEvent<D>> for Picker<D> {}

impl<D: PickerDelegate> Picker<D> {
  pub fn new(
    delegate: D,
    items: Arc<Vec<D::ListItem>>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-j", SelectNext, None),
      KeyBinding::new("down", SelectNext, None),
      KeyBinding::new("ctrl-k", SelectPrev, None),
      KeyBinding::new("up", SelectPrev, None),
    ]);

    let has_items = !items.is_empty();
    let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search..."));

    let mut this = Self {
      delegate,
      items,
      focus_handle: cx.focus_handle(),
      search_input: search_input.clone(),
      selected_index: if has_items { Some(0) } else { None },
      matches: None,
      current_query: String::new(),
      list_scroll_handle: UniformListScrollHandle::new(),
      subscriptions: Vec::new(),

      search_id: None,
      next_search_id: 0,
      search_task: None,
      cancel_flag: Arc::new(AtomicBool::new(false)),

      visual_entries: None,
      selectable_items: None,
      category_list_state: ListState::new(0, ListAlignment::Top, px(1000.)),
    };

    this.subscriptions.push(cx.subscribe_in(
      &search_input,
      window,
      move |this, search_input, ev, window, cx| match *ev {
        InputEvent::PressEnter { secondary } => this.launch_selected(secondary, cx),
        InputEvent::Change => {
          let new_value = &search_input.read(cx).value();
          if &this.current_query == new_value {
            // Query hasn't changed
            return;
          }

          this.current_query = new_value.to_string();
          cx.emit(PickerEvent::QueryChanged(this.current_query.clone()));
          this.update_matches(window, cx);
        }
        _ => {}
      },
    ));

    // Perform initial search/sort with empty query
    this.update_matches(window, cx);

    this
  }

  pub fn set_items(
    &mut self,
    items: Vec<D::ListItem>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.items = Arc::new(items);
    if self.selected_index.is_none() && !self.items.is_empty() {
      self.selected_index = Some(0);
    }

    cx.notify();

    self.update_matches(window, cx);
  }

  pub fn placeholder(&mut self, placeholder: impl Into<SharedString>, cx: &mut Context<Self>) {
    self.search_input.update(cx, |state, _cx| {
      state.placeholder = placeholder.into();
    });
  }

  fn resolve_item_index(&self, selected_index: usize) -> Option<usize> {
    if let Some(selectable_items) = &self.selectable_items {
      let match_index = selectable_items.get(selected_index)?.match_index;
      self
        .matches
        .as_ref()
        .and_then(|matches| matches.get(match_index).map(|(ix, _)| *ix))
    } else {
      self
        .matches
        .as_ref()
        .map_or(Some(selected_index), |matches| {
          matches.get(selected_index).map(|(ix, _)| *ix)
        })
    }
  }

  fn visible_item_count(&self) -> usize {
    if let Some(selectable_items) = &self.selectable_items {
      selectable_items.len()
    } else {
      self
        .matches
        .as_ref()
        .map_or(self.items.len(), |matches| matches.len())
    }
  }

  pub fn get_selected_item(&self) -> Option<&D::ListItem> {
    let selected_index = self.selected_index?;
    let resolved_ix = self.resolve_item_index(selected_index)?;
    self.items.get(resolved_ix)
  }

  /// Moves the selection to a position in the visible list.
  ///
  /// Lists that are rebuilt on a timer use this to keep the selection on the
  /// same thing across a reorder, by looking up where that thing ended up.
  pub fn set_selected_index(&mut self, index: usize, cx: &mut Context<Self>) {
    let item_count = if self.selectable_items.is_some() {
      self.visible_item_count()
    } else {
      // `set_items` swaps the items in right away while the search that rebuilds
      // `matches` finishes later, so the match count can still be the previous
      // list's. The item count is the bound that will hold once it lands, and
      // `complete_search` clamps against the real count then anyway.
      self.items.len()
    };

    if item_count == 0 {
      self.selected_index = None;
      return;
    }

    let index = index.min(item_count - 1);
    self.selected_index = Some(index);

    if let Some(selectable_items) = &self.selectable_items {
      if let Some(item) = selectable_items.get(index) {
        self
          .category_list_state
          .scroll_to_reveal_item(item.list_index);
      }
    } else {
      // Non-strict, so this only moves the list when the row has actually gone
      // out of view rather than fighting the scroll position every tick.
      self
        .list_scroll_handle
        .scroll_to_item(index, ScrollStrategy::Top);
    }

    cx.notify();
  }

  pub fn remove_selected_item(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Option<D::ListItem> {
    let selected_index = self.selected_index?;
    let resolved_ix = self.resolve_item_index(selected_index)?;

    let items = Arc::make_mut(&mut self.items);
    if resolved_ix >= items.len() {
      return None;
    }
    let removed = items.remove(resolved_ix);

    // Adjust selected index
    let item_count = self.visible_item_count();
    if item_count == 0 {
      self.selected_index = None;
    } else if selected_index >= item_count {
      self.selected_index = Some(item_count - 1);
    }

    self.update_matches(window, cx);
    cx.notify();
    Some(removed)
  }

  fn launch_selected(&mut self, secondary: bool, cx: &mut Context<Self>) {
    let Some(ix) = self.selected_index else {
      return;
    };

    let Some(resolved_ix) = self.resolve_item_index(ix) else {
      return;
    };
    let Some(item) = self.items.get(resolved_ix) else {
      return;
    };

    if secondary {
      cx.emit(PickerEvent::SecondaryPicked(item.clone()));
    } else {
      cx.emit(PickerEvent::Picked(item.clone()));
    }
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
    matches: Option<Vec<(usize, u32)>>,
  ) {
    if self.search_id.is_some_and(|id| id > search_id) {
      return;
    }

    let mut matches = matches.unwrap_or_else(|| {
      // No matches provided (empty query), create indices for all items with score 0
      (0..self.items.len()).map(|i| (i, 0)).collect()
    });

    // Sort items using the delegate's sorting logic
    self.delegate.sort_items(cx, &self.items, &mut matches);

    self.search_id = Some(search_id);
    self.matches = Some(matches);

    // Build visual entries if categories are defined
    if let Some(categories) = self.delegate.categories() {
      let matches = self.matches.as_ref().expect("matches just set above");
      let mut visual_entries = Vec::new();
      let mut selectable_items = Vec::new();

      for category in &categories {
        let mut category_items = Vec::new();

        for (match_index, &(item_index, _score)) in matches.iter().enumerate() {
          if let Some(item) = self.items.get(item_index)
            && (category.filter)(item)
          {
            category_items.push(match_index);
          }
        }

        if !category_items.is_empty() {
          visual_entries.push(VisualEntry::Header(category.name.clone()));

          for match_index in category_items {
            let list_index = visual_entries.len();
            visual_entries.push(VisualEntry::Item(match_index));
            selectable_items.push(SelectableItem {
              match_index,
              list_index,
            });
          }
        }
      }

      let selectable_count = selectable_items.len();

      if let Some(old_entries) = &self.visual_entries {
        splice_diff(&self.category_list_state, old_entries, &visual_entries);
      } else {
        self.category_list_state.reset(visual_entries.len());
      }

      self.visual_entries = Some(visual_entries);
      self.selectable_items = Some(selectable_items);

      self.selected_index = if selectable_count > 0 {
        Some(
          self
            .selected_index
            .map(|ix| ix.min(selectable_count - 1))
            .unwrap_or(0),
        )
      } else {
        None
      };

      if self.selected_index == Some(0) {
        self.scroll_to_top();
      }
    } else {
      self.visual_entries = None;
      self.selectable_items = None;

      let match_count = self.matches.as_ref().map_or(self.items.len(), |m| m.len());
      self.selected_index = if match_count > 0 {
        Some(
          self
            .selected_index
            .map(|ix| ix.min(match_count - 1))
            .unwrap_or(0),
        )
      } else {
        None
      };
    }

    cx.notify();
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self.visible_item_count();
    if item_count == 0 {
      return;
    }

    let wrapped = matches!(self.selected_index, Some(i) if i + 1 >= item_count);

    match self.selected_index {
      Some(i) => self.selected_index = Some((i + 1) % item_count),
      None => self.selected_index = Some(0),
    }

    let ix = self.selected_index.expect("just set above");
    if wrapped {
      self.scroll_to_top();
    } else if let Some(selectable_items) = &self.selectable_items {
      if let Some(item) = selectable_items.get(ix) {
        self
          .category_list_state
          .scroll_to_reveal_item(item.list_index);
      }
    } else {
      self
        .list_scroll_handle
        .scroll_to_item(ix, ScrollStrategy::Bottom);
    }

    cx.notify();
  }

  fn select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self.visible_item_count();
    if item_count == 0 {
      return;
    }

    let wrapped = matches!(self.selected_index, Some(0) | None);

    match self.selected_index {
      Some(0) | None => self.selected_index = Some(item_count - 1),
      Some(i) => self.selected_index = Some(i - 1),
    }

    let ix = self.selected_index.expect("just set above");
    if wrapped {
      self.scroll_to_bottom();
    } else if let Some(selectable_items) = &self.selectable_items {
      if let Some(item) = selectable_items.get(ix) {
        self
          .category_list_state
          .scroll_to_reveal_item(item.list_index);
      }
    } else {
      self
        .list_scroll_handle
        .scroll_to_item(ix, ScrollStrategy::Top);
    }

    cx.notify();
  }

  fn scroll_to_top(&self) {
    if self.selectable_items.is_some() {
      self.category_list_state.scroll_to(ListOffset {
        item_ix: 0,
        offset_in_item: px(0.),
      });
    } else {
      self
        .list_scroll_handle
        .scroll_to_item(0, ScrollStrategy::Top);
    }
  }

  fn scroll_to_bottom(&self) {
    if let Some(visual_entries) = &self.visual_entries {
      let last_ix = visual_entries.len().saturating_sub(1);
      self.category_list_state.scroll_to_reveal_item(last_ix);
    } else {
      let item_count = self.visible_item_count();
      if item_count > 0 {
        self
          .list_scroll_handle
          .scroll_to_item(item_count - 1, ScrollStrategy::Bottom);
      }
    }
  }

  /// Renders one row, given an index into `items`.
  ///
  /// The index can be stale: `set_items` swaps the item list in immediately
  /// while the matches that index into it are only replaced once the search
  /// task completes. A list that is rebuilt on a timer will render at least one
  /// frame in between, so a missing item is left blank for that frame rather
  /// than panicking.
  fn render_list_item(
    &self,
    window: &mut Window,
    cx: &mut Context<Self>,
    ix: usize,
    is_selected: bool,
  ) -> AnyElement {
    let Some(item) = self.items.get(ix) else {
      return div().into_any_element();
    };

    div()
      .id(("item", ix))
      .cursor_pointer()
      .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
        if let Some(item) = this.items.get(ix) {
          cx.emit(PickerEvent::Picked(item.clone()));
        }

        // Maintain focus on the search input
        cx.stop_propagation();
      }))
      .child(
        self
          .delegate
          .render_list_item(window, cx, item, is_selected),
      )
      .into_any_element()
  }
}

impl<D: PickerDelegate> Focusable for Picker<D> {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl<D: PickerDelegate> Picker<D> {
  fn render_flat(&mut self, cx: &mut Context<Self>) -> AnyElement {
    // Capped at the item count so that matches left over from a previous, longer
    // item list do not ask for rows there is nothing to put in.
    let count = self.matches.as_ref().map_or_else(
      || self.items.len(),
      |matches| matches.len().min(self.items.len()),
    );

    uniform_list(
      "matches",
      count,
      cx.processor(|this, range: Range<usize>, window, cx| {
        range
          .map(|ix| {
            let is_selected = this.selected_index.is_some_and(|selected| selected == ix);
            let resolved_ix = this
              .matches
              .as_ref()
              .map_or(Some(ix), |matches| matches.get(ix).map(|(ix, _)| *ix));

            match resolved_ix {
              Some(resolved_ix) => this.render_list_item(window, cx, resolved_ix, is_selected),
              None => div().into_any_element(),
            }
          })
          .collect()
      }),
    )
    .track_scroll(&self.list_scroll_handle)
    .flex_grow()
    .p_2()
    .into_any_element()
  }

  fn render_categorized(&mut self, cx: &mut Context<Self>) -> AnyElement {
    list(
      self.category_list_state.clone(),
      cx.processor(|this, ix, window, cx| {
        let visual_entries = this.visual_entries.as_ref().expect("categories are active");
        match &visual_entries[ix] {
          VisualEntry::Header(name) => div()
            .px_4()
            .when(ix == 0, |this| this.pt_2())
            .when(ix > 0, |this| this.pt_4())
            .pb_1()
            .child(
              div()
                .text_xs()
                .text_color(rgb(0x888888))
                .child(name.clone()),
            )
            .into_any_element(),
          VisualEntry::Item(match_index) => {
            let item_index = this.matches.as_ref().map_or(Some(*match_index), |m| {
              m.get(*match_index).map(|(ix, _)| *ix)
            });
            let Some(item_index) = item_index else {
              return div().into_any_element();
            };
            let selected_list_ix = this
              .selected_index
              .and_then(|si| this.selectable_items.as_ref()?.get(si))
              .map(|item| item.list_index);
            let is_selected = selected_list_ix == Some(ix);
            div()
              .px_2()
              .child(this.render_list_item(window, cx, item_index, is_selected))
              .into_any_element()
          }
        }
      }),
    )
    .flex_grow()
    .pb_2()
    .into_any_element()
  }
}

impl<D: PickerDelegate> Render for Picker<D> {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    if self.visual_entries.is_some() {
      self.render_categorized(cx)
    } else {
      self.render_flat(cx)
    }
  }
}

pub fn picker_input<D: PickerDelegate>(picker: &Entity<Picker<D>>) -> PickerInput<D> {
  PickerInput {
    picker: picker.clone(),
    style: StyleRefinement::default(),
    show_back_button: false,
    is_loading: false,
    text_size: None,
    suffix: None,
  }
}

#[derive(IntoElement)]
pub struct PickerInput<D: PickerDelegate> {
  picker: Entity<Picker<D>>,
  style: StyleRefinement,
  show_back_button: bool,
  is_loading: bool,
  text_size: Option<Pixels>,
  suffix: Option<AnyElement>,
}

impl<D: PickerDelegate> PickerInput<D> {
  pub fn show_back_button(mut self, show: bool) -> Self {
    self.show_back_button = show;
    self
  }

  pub fn loading(mut self, loading: bool) -> Self {
    self.is_loading = loading;
    self
  }

  pub fn text_size(mut self, size: Pixels) -> Self {
    self.text_size = Some(size);
    self
  }

  pub fn suffix(mut self, element: impl IntoElement) -> Self {
    self.suffix = Some(element.into_any_element());
    self
  }
}

impl<D: PickerDelegate> Styled for PickerInput<D> {
  fn style(&mut self) -> &mut StyleRefinement {
    &mut self.style
  }
}

impl<D: PickerDelegate> RenderOnce for PickerInput<D> {
  fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
    let search_input = self.picker.read(cx).search_input.clone();
    let focus_handle = self.picker.read(cx).focus_handle.clone();

    let mut element = div()
      .track_focus(&focus_handle)
      .on_action(window.listener_for(&self.picker, Picker::select_next))
      .on_action(window.listener_for(&self.picker, Picker::select_prev))
      .p_3()
      .border_b_1()
      .border_color(rgba(0xFFFFFF12))
      .when_some(self.text_size, |this, size: Pixels| this.text_size(size));

    element.style().refine(&self.style);

    element.child(
      div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .h(rems(1.5))
        .when(self.show_back_button, |this| {
          this.child(
            div()
              .id("back-button")
              .flex()
              .items_center()
              .justify_center()
              .rounded_md()
              .bg(rgba(0xFFFFFF0F))
              .p_1()
              .cursor_pointer()
              .on_click(|_event, window, cx| {
                window.dispatch_action(GoBack.boxed_clone(), cx);
              })
              .child(Icon::new(IconName::ArrowLeft).text_color(rgb(0xCCCCCC))),
          )
        })
        .child(input(&search_input).flex_grow())
        .when(self.is_loading, |this| {
          this.child(Spinner::new().color(rgb(0x888888).into()))
        })
        .when_some(self.suffix, |this, suffix| this.child(suffix)),
    )
  }
}

pub fn picker_results<D: PickerDelegate>(picker: &Entity<Picker<D>>) -> PickerResults<D> {
  PickerResults {
    picker: picker.clone(),
    style: StyleRefinement::default(),
  }
}

#[derive(IntoElement)]
pub struct PickerResults<D: PickerDelegate> {
  picker: Entity<Picker<D>>,
  style: StyleRefinement,
}

impl<D: PickerDelegate> Styled for PickerResults<D> {
  fn style(&mut self) -> &mut StyleRefinement {
    &mut self.style
  }
}

impl<D: PickerDelegate> RenderOnce for PickerResults<D> {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    self.picker
  }
}

/// Update `list_state` by splicing only the ranges that differ between old and new entries.
/// This preserves the list's scroll position for unchanged regions.
fn splice_diff(list_state: &ListState, old: &[VisualEntry], new: &[VisualEntry]) {
  let common_len = old.len().min(new.len());

  // Find the length of the matching prefix
  let prefix_len = old[..common_len]
    .iter()
    .zip(&new[..common_len])
    .take_while(|(a, b)| a == b)
    .count();

  // Find the length of the matching suffix (after the prefix)
  let suffix_len = old[prefix_len..common_len]
    .iter()
    .rev()
    .zip(new[prefix_len..common_len].iter().rev())
    .take_while(|(a, b)| a == b)
    .count();

  let old_end = old.len() - suffix_len;
  let new_end = new.len() - suffix_len;

  if prefix_len < old_end || prefix_len < new_end {
    list_state.splice(prefix_len..old_end, new_end - prefix_len);
  }
}
