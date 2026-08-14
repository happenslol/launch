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

pub enum ConfirmationEvent {
  Closing,
  Dismiss,
  /// Carries which of the choices was picked, indexed as they were passed to
  /// [`ConfirmationPrompt::with_choices`]. Prompts built with
  /// [`ConfirmationPrompt::new`] only ever report `1`, their sole choice.
  Confirm(usize),
}

pub struct ConfirmationPrompt {
  focus_handle: FocusHandle,
  /// Index into `choices`, where `0` is always the one that cancels.
  selected: usize,
  choices: Vec<SharedString>,
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
    Self::with_choices(message, ["Cancel", "Yes"], window, cx)
  }

  /// Builds a prompt offering more than one way to say yes, such as a signal to
  /// send. The first choice is the one that cancels; the rest are laid out from
  /// left to right and the last is selected to start with.
  pub fn with_choices(
    message: impl Into<SharedString>,
    choices: impl IntoIterator<Item = impl Into<SharedString>>,
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

    let choices = choices
      .into_iter()
      .map(Into::into)
      .collect::<Vec<SharedString>>();

    // The first choice that isn't the cancel one. With two choices that is the
    // familiar "Yes"; with more it is the mildest of them, which is the right
    // thing to have under the return key when the rest escalate.
    let selected = if choices.len() > 1 { 1 } else { 0 };

    Self {
      focus_handle,
      selected,
      choices,
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

    let choice = self.selected;
    self.dismiss_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;

      this
        .update(cx, |this, cx| {
          if this.confirmed {
            cx.emit(ConfirmationEvent::Confirm(choice));
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
    self.confirmed = self.selected > 0;
    self.start_exit(cx);
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    if self.choices.is_empty() {
      return;
    }

    self.selected = (self.selected + 1) % self.choices.len();
    cx.notify();
  }

  fn select_previous(&mut self, _: &SelectPrevious, _window: &mut Window, cx: &mut Context<Self>) {
    if self.choices.is_empty() {
      return;
    }

    self.selected = (self.selected + self.choices.len() - 1) % self.choices.len();
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
    let selected = self.selected;
    let many_choices = self.choices.len() > 2;
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
          .w(if many_choices { px(340.) } else { px(280.) })
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
                  .gap_2()
                  // Two choices keep the familiar cancel-left, confirm-right
                  // split. More than that will not fit on one line, so they wrap
                  // and gather to the right instead.
                  .when(many_choices, |this| this.flex_wrap().justify_end())
                  .when(!many_choices, |this| this.justify_between())
                  .children(self.choices.iter().enumerate().map(|(index, label)| {
                    render_button(
                      index,
                      label.clone(),
                      index == selected,
                      index > 0,
                      cx.listener(move |this, _, _, cx| {
                        this.selected = index;
                        this.confirmed = index > 0;
                        this.start_exit(cx);
                      }),
                    )
                  })),
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
  index: usize,
  label: SharedString,
  is_selected: bool,
  has_background: bool,
  on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
  div()
    .id(("confirmation-choice", index))
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

/// Renders the prompt as the last child of whatever it is covering.
///
/// The prompt positions itself absolutely and fills its parent, and taffy - the
/// layout engine under gpui - resolves that against the *immediate* parent
/// rather than the nearest `relative` ancestor the way CSS would. So the prompt
/// has to be added directly: wrapping it in a plain `div` would make that
/// wrapper the box being filled, and since an absolute child contributes no
/// size, the wrapper is a zero-height row that leaves the prompt centred on the
/// bottom edge of the panel and half cut off.
pub fn render_confirmation_overlay(prompt: &Entity<ConfirmationPrompt>) -> AnyElement {
  prompt.clone().into_any_element()
}
