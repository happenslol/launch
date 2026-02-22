use gpui::{
  AnyElement, App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled, Window,
  actions, div, prelude::*, rgba,
};

use crate::util::{h_flex, v_flex};

const CONTEXT: &str = "confirmation_prompt";

actions!(
  confirmation_prompt,
  [Dismiss, Confirm, SelectNext, SelectPrevious]
);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectedButton {
  Cancel,
  Yes,
}

pub enum ConfirmationEvent {
  Dismiss,
  Confirm,
}

pub struct ConfirmationPrompt {
  focus_handle: FocusHandle,
  selected: SelectedButton,
}

impl ConfirmationPrompt {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      gpui::KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
      gpui::KeyBinding::new("enter", Confirm, Some(CONTEXT)),
      gpui::KeyBinding::new("tab", SelectNext, Some(CONTEXT)),
      gpui::KeyBinding::new("l", SelectNext, Some(CONTEXT)),
      gpui::KeyBinding::new("right", SelectNext, Some(CONTEXT)),
      gpui::KeyBinding::new("h", SelectPrevious, Some(CONTEXT)),
      gpui::KeyBinding::new("left", SelectPrevious, Some(CONTEXT)),
    ]);

    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);

    Self {
      focus_handle,
      selected: SelectedButton::Yes,
    }
  }

  fn dismiss(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
    cx.emit(ConfirmationEvent::Dismiss);
  }

  fn confirm(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
    match self.selected {
      SelectedButton::Cancel => cx.emit(ConfirmationEvent::Dismiss),
      SelectedButton::Yes => cx.emit(ConfirmationEvent::Confirm),
    }
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    self.selected = match self.selected {
      SelectedButton::Cancel => SelectedButton::Yes,
      SelectedButton::Yes => SelectedButton::Cancel,
    };
    cx.notify();
  }

  fn select_previous(&mut self, _: &SelectPrevious, _window: &mut Window, cx: &mut Context<Self>) {
    self.selected = match self.selected {
      SelectedButton::Cancel => SelectedButton::Yes,
      SelectedButton::Yes => SelectedButton::Cancel,
    };
    cx.notify();
  }
}

impl gpui::EventEmitter<ConfirmationEvent> for ConfirmationPrompt {}

impl Focusable for ConfirmationPrompt {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for ConfirmationPrompt {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let cancel_selected = self.selected == SelectedButton::Cancel;
    let yes_selected = self.selected == SelectedButton::Yes;

    div()
      .track_focus(&self.focus_handle)
      .key_context(CONTEXT)
      .on_action(cx.listener(Self::dismiss))
      .on_action(cx.listener(Self::confirm))
      .on_action(cx.listener(Self::select_next))
      .on_action(cx.listener(Self::select_previous))
      .on_mouse_down_out(cx.listener(|_, _, _, cx| {
        cx.emit(ConfirmationEvent::Dismiss);
      }))
      .w(gpui::px(220.))
      .p_4()
      .bg(rgba(0x171717F0))
      .border_1()
      .border_color(rgba(0xFFFFFF15))
      .rounded_md()
      .shadow_lg()
      .child(
        v_flex()
          .gap_3()
          .items_center()
          .child(
            div()
              .text_sm()
              .text_color(rgba(0xFFFFFFCC))
              .child("Delete entry?"),
          )
          .child(
            h_flex()
              .gap_2()
              .child(render_button("Cancel", cancel_selected, cx.listener(|_, _, _, cx| {
                cx.emit(ConfirmationEvent::Dismiss);
              })))
              .child(render_button("Yes", yes_selected, cx.listener(|_, _, _, cx| {
                cx.emit(ConfirmationEvent::Confirm);
              }))),
          ),
      )
  }
}

fn render_button(
  label: &'static str,
  is_selected: bool,
  on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
  div()
    .id(label)
    .cursor_pointer()
    .px_3()
    .py_1()
    .rounded_md()
    .text_sm()
    .when(is_selected, |this| {
      this.bg(rgba(0xFFFFFF22)).text_color(rgba(0xFFFFFFEE))
    })
    .when(!is_selected, |this| {
      this.bg(rgba(0xFFFFFF0A)).text_color(rgba(0xFFFFFF88))
    })
    .hover(|this| this.bg(rgba(0xFFFFFF22)))
    .on_click(on_click)
    .child(label)
    .into_any_element()
}

pub fn render_confirmation_overlay(prompt: &Entity<ConfirmationPrompt>) -> AnyElement {
  let prompt = prompt.clone();
  div()
    .absolute()
    .top_0()
    .left_0()
    .size_full()
    .flex()
    .items_center()
    .justify_center()
    .child(
      div()
        .id("confirmation-backdrop")
        .occlude()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .rounded_xl()
        .bg(rgba(0x00000088)),
    )
    .child(div().occlude().child(prompt))
    .into_any_element()
}
