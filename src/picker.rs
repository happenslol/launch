#![allow(dead_code, unused_variables)]

use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, Subscription, UniformListScrollHandle,
  Window, div, prelude::*,
};

use crate::text_input::{TextInput, TextInputEvent};

pub trait PickerDelegate: Sized + 'static {
  type ListItem: IntoElement;

  fn match_count(&self) -> usize;
  fn selected_index(&self) -> Option<usize>;
  fn set_selected_index(&mut self, index: usize);
  fn render_list_item(&self, index: usize) -> Option<Self::ListItem>;
}

pub struct Picker<D: PickerDelegate> {
  delegate: D,
  search_input: Entity<TextInput>,
  selected_index: Option<usize>,
  current_query: String,
  list_scroll_handle: UniformListScrollHandle,
  subscriptions: Vec<Subscription>,
}

impl<D: PickerDelegate> Picker<D> {
  fn new(delegate: D, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let search_input = cx.new(|cx| TextInput::new(window, cx));

    let mut this = Self {
      delegate,
      search_input: search_input.clone(),
      selected_index: None,
      current_query: String::new(),
      list_scroll_handle: UniformListScrollHandle::new(),
      subscriptions: Vec::new(),
    };

    this.subscriptions.push(cx.subscribe_in(
      &search_input,
      window,
      move |this, search_input, ev: &TextInputEvent, window, cx| match *ev {
        TextInputEvent::Submit => this.launch_selected(window, cx),
        TextInputEvent::Change => {
          let new_value = &search_input.read(cx).content.trim();
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

  fn launch_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {}
  fn update_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {}
}

impl<D: PickerDelegate> Focusable for Picker<D> {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.search_input.read(cx).focus_handle.clone()
  }
}

impl<D: PickerDelegate> Render for Picker<D> {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    div()
  }
}
