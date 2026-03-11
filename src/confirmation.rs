use std::time::Duration;

use gpui::{
  Animation, AnimationExt, AnyElement, App, Context, ElementId, Entity, FocusHandle, Focusable,
  IntoElement, Render, SharedString, Styled, Task, Window, actions, div, prelude::*, px, rgba,
};

use crate::util::{ResultExt, h_flex, v_flex};

const CONTEXT: &str = "confirmation_prompt";
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(100);

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
  Closing,
  Dismiss,
  Confirm,
}

pub struct ConfirmationPrompt {
  focus_handle: FocusHandle,
  selected: SelectedButton,
  message: SharedString,
  closing: bool,
  confirmed: bool,
  dismiss_task: Option<Task<()>>,
}

impl ConfirmationPrompt {
  pub fn new(
    message: impl Into<SharedString>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
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
      message: message.into(),
      closing: false,
      confirmed: false,
      dismiss_task: None,
    }
  }

  fn start_exit(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }

    self.closing = true;
    cx.emit(ConfirmationEvent::Closing);
    cx.notify();

    self.dismiss_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;

      this
        .update(cx, |this, cx| {
          if this.confirmed {
            cx.emit(ConfirmationEvent::Confirm);
          } else {
            cx.emit(ConfirmationEvent::Dismiss);
          }
        })
        .log_err();
    }));
  }

  fn dismiss_action(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
    self.start_exit(cx);
  }

  fn confirm_action(&mut self, _: &Confirm, _window: &mut Window, cx: &mut Context<Self>) {
    match self.selected {
      SelectedButton::Cancel => self.start_exit(cx),
      SelectedButton::Yes => {
        self.confirmed = true;
        self.start_exit(cx);
      }
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
    let closing = self.closing;
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);

    div()
      .when(!closing, |this| {
        this
          .track_focus(&self.focus_handle)
          .key_context(CONTEXT)
          .on_action(cx.listener(Self::dismiss_action))
          .on_action(cx.listener(Self::confirm_action))
          .on_action(cx.listener(Self::select_next))
          .on_action(cx.listener(Self::select_previous))
      })
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
          .when(!closing, |this| this.occlude())
          .absolute()
          .top_0()
          .left_0()
          .size_full()
          .rounded_xl()
          .bg(rgba(0x00000088))
          .when(!closing, |this| {
            this.on_mouse_down(
              gpui::MouseButton::Left,
              cx.listener(|this, _, _, cx| {
                this.start_exit(cx);
              }),
            )
          })
          .with_animation(
            ElementId::NamedInteger("confirmation-backdrop-fade".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let opacity = if closing { 1.0 - delta } else { delta };
              this.opacity(opacity)
            },
          ),
      )
      .child(
        div()
          .id("confirmation-content")
          .when(!closing, |this| this.occlude())
          .w(px(280.))
          .p_4()
          .bg(rgba(0x1D1D1DF0))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .rounded_lg()
          .shadow_lg()
          .child(
            v_flex()
              .gap_4()
              .child(
                div()
                  .text_sm()
                  .text_color(rgba(0xFFFFFFCC))
                  .child(self.message.clone()),
              )
              .child(
                h_flex()
                  .justify_between()
                  .child(render_button(
                    "Cancel",
                    cancel_selected,
                    false,
                    cx.listener(|this, _, _, cx| {
                      this.start_exit(cx);
                    }),
                  ))
                  .child(render_button(
                    "Yes",
                    yes_selected,
                    true,
                    cx.listener(|this, _, _, cx| {
                      this.confirmed = true;
                      this.start_exit(cx);
                    }),
                  )),
              ),
          )
          .with_animation(
            ElementId::NamedInteger("confirmation-slide".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let progress = if closing { delta } else { 1.0 - delta };
              let opacity = if closing { 1.0 - delta } else { delta };
              let offset = 8.0 * progress;
              this.mt(px(offset)).opacity(opacity)
            },
          ),
      )
  }
}

fn render_button(
  label: &'static str,
  is_selected: bool,
  has_background: bool,
  on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
  div()
    .id(label)
    .cursor_pointer()
    .px_3()
    .py_1()
    .rounded_md()
    .text_sm()
    .when(is_selected && has_background, |this| {
      this.bg(rgba(0xFFFFFF22)).text_color(rgba(0xFFFFFFEE))
    })
    .when(is_selected && !has_background, |this| {
      this.text_color(rgba(0xFFFFFFEE))
    })
    .when(!is_selected && has_background, |this| {
      this.bg(rgba(0xFFFFFF0A)).text_color(rgba(0xFFFFFF88))
    })
    .when(!is_selected && !has_background, |this| {
      this.text_color(rgba(0xFFFFFF88))
    })
    .when(has_background, |this| {
      this.hover(|this| this.bg(rgba(0xFFFFFF22)))
    })
    .when(!has_background, |this| {
      this.hover(|this| this.text_color(rgba(0xFFFFFFCC)))
    })
    .on_click(on_click)
    .child(label)
    .into_any_element()
}

pub fn render_confirmation_overlay(prompt: &Entity<ConfirmationPrompt>) -> AnyElement {
  div().child(prompt.clone()).into_any_element()
}
