//! polkit authentication dialog.
//!
//! Owns the layer-shell window that prompts for a password (or shows fingerprint
//! instructions) when the [`crate::dbus::polkit`] agent drives an
//! authentication. The agent runs on the D-Bus executor and speaks to this
//! module over an [`AgentEvent`] channel; a single foreground task consumes
//! those events and opens, updates and dismisses the dialog.

use std::time::Duration;

use flume::{Receiver, Sender};
use gpui::{
  Animation, AnimationExt, App, AppContext as _, AsyncApp, Bounds, Context, ElementId, Entity,
  FocusHandle, Focusable, IntoElement, KeyBinding, Render, SharedString, Size, Styled, Subscription,
  Task, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
  actions, div, point, prelude::*, px, rems, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use tracing::error;

use crate::dbus::polkit::{self, AgentEvent};
use crate::icon::{Icon, IconName, Spinner};
use crate::input::input;
use crate::input::state::{Escape as InputEscape, InputEvent, InputState};
use crate::util::{ResultExt, h_flex, v_flex};

const CONTEXT: &str = "polkit_dialog";
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(120);

actions!(polkit_dialog, [Cancel]);

pub fn init(cx: &mut App) {
  let (events, receiver) = flume::unbounded::<AgentEvent>();
  polkit::init(cx, events);

  cx.spawn(async move |cx| run(receiver, cx).await).detach();
}

/// Consumes agent events on the foreground thread, driving a single dialog
/// window. The window is reused across a rapid close/reopen (e.g. polkit
/// reissuing after a wrong password) to avoid a visible flicker.
async fn run(receiver: Receiver<AgentEvent>, cx: &mut AsyncApp) {
  let mut window: Option<WindowHandle<PolkitDialog>> = None;

  while let Ok(event) = receiver.recv_async().await {
    match event {
      AgentEvent::Begin { message, cancel } => {
        let reused = window.is_some_and(|handle| {
          handle
            .update(cx, |dialog, window, cx| {
              dialog.begin(message.clone(), cancel.clone(), window, cx);
            })
            .is_ok()
        });

        if !reused {
          match cx.update(|cx| open_window(message, cancel, cx)) {
            Ok(handle) => window = Some(handle),
            Err(error) => {
              error!(?error, "failed to open polkit dialog");
              window = None;
            }
          }
        }
      }
      AgentEvent::Prompt {
        prompt,
        echo,
        reply,
      } => {
        if let Some(handle) = window {
          handle
            .update(cx, |dialog, window, cx| {
              dialog.set_prompt(prompt, echo, reply, window, cx);
            })
            .log_err();
        }
      }
      AgentEvent::Error { message } => {
        if let Some(handle) = window {
          handle
            .update(cx, |dialog, _window, cx| dialog.set_error(message, cx))
            .log_err();
        }
      }
      AgentEvent::Info { message } => {
        if let Some(handle) = window {
          handle
            .update(cx, |dialog, _window, cx| dialog.set_info(message, cx))
            .log_err();
        }
      }
      AgentEvent::Close => {
        if let Some(handle) = window.take() {
          handle
            .update(cx, |dialog, _window, cx| dialog.start_exit(cx))
            .log_err();
        }
      }
    }
  }
}

fn open_window(
  message: SharedString,
  cancel: Sender<()>,
  cx: &mut App,
) -> anyhow::Result<WindowHandle<PolkitDialog>> {
  let handle = cx.open_window(window_options(), move |window, cx| {
    cx.new(|cx| PolkitDialog::new(message, cancel, window, cx))
  })?;
  Ok(handle)
}

fn window_options() -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-polkit".to_string()),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(px(800.), px(600.)),
    })),
    window_background: WindowBackgroundAppearance::Transparent,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-polkit".to_string(),
      layer: Layer::Overlay,
      // Anchoring opposite edges makes the compositor stretch the surface to
      // fill the output, giving us a full-screen dimming backdrop.
      anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: None,
      keyboard_interactivity: KeyboardInteractivity::Exclusive,
    }),
    ..Default::default()
  }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
  /// Waiting for the first prompt from the helper.
  Waiting,
  /// A secret is being requested. `echo` mirrors the PAM prompt visibility.
  Prompt { echo: bool },
}

pub struct PolkitDialog {
  message: SharedString,
  prompt: SharedString,
  cancel: Sender<()>,
  password: Entity<InputState>,
  reply: Option<Sender<String>>,
  phase: Phase,
  error: Option<SharedString>,
  info: Option<SharedString>,
  submitting: bool,
  closing: bool,
  focus_handle: FocusHandle,
  _subscriptions: Vec<Subscription>,
  _exit_task: Option<Task<()>>,
}

impl PolkitDialog {
  fn new(
    message: SharedString,
    cancel: Sender<()>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    cx.bind_keys([KeyBinding::new("escape", Cancel, Some(CONTEXT))]);

    let password = cx.new(|cx| InputState::new(window, cx).masked(true));
    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);

    let subscription = cx.subscribe_in(
      &password,
      window,
      |this, _input, event: &InputEvent, window, cx| {
        if let InputEvent::PressEnter { .. } = event {
          this.submit(window, cx);
        }
      },
    );

    Self {
      message,
      prompt: SharedString::default(),
      cancel,
      password,
      reply: None,
      phase: Phase::Waiting,
      error: None,
      info: None,
      submitting: false,
      closing: false,
      focus_handle,
      _subscriptions: vec![subscription],
      _exit_task: None,
    }
  }

  /// Resets the dialog for a fresh authentication session, reviving it if it was
  /// mid-exit (a reused window).
  fn begin(
    &mut self,
    message: SharedString,
    cancel: Sender<()>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.message = message;
    self.cancel = cancel;
    self.prompt = SharedString::default();
    self.reply = None;
    self.phase = Phase::Waiting;
    self.error = None;
    self.info = None;
    self.submitting = false;
    self.closing = false;
    self._exit_task = None;
    self.password.update(cx, |input, cx| {
      input.set_value("", window, cx);
    });
    window.focus(&self.focus_handle, cx);
    cx.notify();
  }

  fn set_prompt(
    &mut self,
    prompt: SharedString,
    echo: bool,
    reply: Sender<String>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.phase = Phase::Prompt { echo };
    self.prompt = prompt.clone();
    self.reply = Some(reply);
    self.submitting = false;

    let placeholder = match prompt.trim().trim_end_matches(':').trim() {
      "" => "Password".to_owned(),
      text => text.to_owned(),
    };
    self.password.update(cx, |input, cx| {
      input.set_masked(!echo, window, cx);
      input.set_value("", window, cx);
      input.set_placeholder(placeholder, window, cx);
    });

    let handle = self.password.focus_handle(cx);
    window.focus(&handle, cx);
    cx.notify();
  }

  fn set_error(&mut self, message: SharedString, cx: &mut Context<Self>) {
    self.error = Some(message);
    cx.notify();
  }

  fn set_info(&mut self, message: SharedString, cx: &mut Context<Self>) {
    self.info = Some(message);
    cx.notify();
  }

  fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if self.submitting {
      return;
    }
    let Some(reply) = self.reply.take() else {
      return;
    };

    let secret = self.password.read(cx).value().to_string();
    reply.send(secret).log_err();

    self.submitting = true;
    self.error = None;
    self.password.update(cx, |input, cx| {
      input.set_value("", window, cx);
    });
    // The input is unmounted while authenticating; keep focus on the dialog so
    // escape still cancels.
    window.focus(&self.focus_handle, cx);
    cx.notify();
  }

  fn cancel(&mut self, cx: &mut Context<Self>) {
    self.cancel.send(()).log_err();
    self.start_exit(cx);
  }

  fn start_exit(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }
    self.closing = true;
    self.reply = None;
    cx.notify();

    self._exit_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;
      this
        .update_in(cx, |_this, window, _cx| window.remove_window())
        .log_err();
    }));
  }

  fn on_cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
    self.cancel(cx);
  }

  fn on_input_escape(&mut self, _: &InputEscape, _window: &mut Window, cx: &mut Context<Self>) {
    self.cancel(cx);
  }
}

impl Focusable for PolkitDialog {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for PolkitDialog {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let closing = self.closing;
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);
    let can_submit = matches!(self.phase, Phase::Prompt { .. }) && !self.submitting;

    div()
      .when(!closing, |this| {
        this
          .track_focus(&self.focus_handle)
          .key_context(CONTEXT)
          .on_action(cx.listener(Self::on_cancel))
          .on_action(cx.listener(Self::on_input_escape))
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
          .id("polkit-backdrop")
          .when(!closing, |this| this.occlude())
          .absolute()
          .top_0()
          .left_0()
          .size_full()
          .bg(rgba(0x000000AA))
          .when(!closing, |this| {
            this.on_mouse_down(
              gpui::MouseButton::Left,
              cx.listener(|this, _, _, cx| this.cancel(cx)),
            )
          })
          .with_animation(
            ElementId::NamedInteger("polkit-backdrop-fade".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| this.opacity(if closing { 1.0 - delta } else { delta }),
          ),
      )
      .child(
        div()
          .id("polkit-card")
          .when(!closing, |this| this.occlude())
          .w(px(400.))
          .p_5()
          .bg(rgba(0x1D1D1DF5))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .rounded_xl()
          .shadow_lg()
          .child(
            v_flex()
              .gap_4()
              .child(self.render_header())
              .child(self.render_body(can_submit))
              .when_some(self.info.clone(), Self::render_info)
              .when_some(self.error.clone(), Self::render_error)
              .child(self.render_buttons(can_submit, cx)),
          )
          .with_animation(
            ElementId::NamedInteger("polkit-card-slide".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let progress = if closing { delta } else { 1.0 - delta };
              let opacity = if closing { 1.0 - delta } else { delta };
              this.mt(px(8.0 * progress)).opacity(opacity)
            },
          ),
      )
  }
}

impl PolkitDialog {
  fn render_header(&self) -> impl IntoElement {
    h_flex()
      .gap_3()
      .items_start()
      .child(
        div()
          .flex_none()
          .flex()
          .items_center()
          .justify_center()
          .size(px(40.))
          .rounded_lg()
          .bg(rgba(0xFFFFFF0F))
          .child(Icon::new(IconName::ShieldLock).size(rems(1.4)).text_color(rgb(0xE0E0E0))),
      )
      .child(
        v_flex()
          .gap_1()
          .flex_1()
          .child(
            div()
              .text_color(rgba(0xFFFFFFEE))
              .font_weight(gpui::FontWeight::MEDIUM)
              .child("Authentication Required"),
          )
          .when(!self.message.is_empty(), |this| {
            this.child(self.render_message())
          }),
      )
  }

  /// Renders the polkit message. When it embeds a command (polkit quotes it as
  /// `` `cmd' ``) the command is pulled out and shown as a middle-ellipsized
  /// monospace code block, since it is typically a long store path whose
  /// meaningful tail (the binary and its arguments) must stay visible.
  fn render_message(&self) -> gpui::AnyElement {
    let muted = |text: String| {
      div()
        .text_sm()
        .text_color(rgba(0xFFFFFFAA))
        .child(text)
    };

    match split_command(&self.message) {
      Some((prefix, command, suffix)) => {
        let prefix = prefix.trim_end();
        let suffix = suffix.trim_start();
        v_flex()
          .gap_1()
          .when(!prefix.is_empty(), |this| this.child(muted(prefix.to_owned())))
          .child(command_chip(command.trim()))
          .when(!suffix.is_empty(), |this| this.child(muted(suffix.to_owned())))
          .into_any_element()
      }
      None => muted(self.message.to_string()).into_any_element(),
    }
  }

  fn render_body(&self, can_submit: bool) -> gpui::AnyElement {
    if self.submitting {
      return status_row(
        Spinner::new().color(rgb(0x888888).into()).into_any_element(),
        "Authenticating…",
      )
      .into_any_element();
    }

    if !can_submit {
      // Waiting for a prompt, or in a promptless flow (e.g. fingerprint) where
      // the info line carries the instructions.
      return status_row(
        Spinner::new().color(rgb(0x888888).into()).into_any_element(),
        "Waiting for authentication…",
      )
      .into_any_element();
    }

    h_flex()
      .gap_2()
      .px_3()
      .py_2()
      .rounded_lg()
      .bg(rgba(0x00000055))
      .border_1()
      .border_color(rgba(0xFFFFFF1F))
      .child(Icon::new(IconName::Lock).size(rems(0.95)).text_color(rgba(0xFFFFFF66)))
      .child(input(&self.password).flex_grow())
      .into_any_element()
  }

  fn render_info(this: gpui::Div, info: SharedString) -> gpui::Div {
    this.child(
      h_flex()
        .gap_2()
        .items_center()
        .text_sm()
        .text_color(rgba(0xFFFFFFAA))
        .child(Icon::new(IconName::Fingerprint).size(rems(0.95)).text_color(rgba(0xFFFFFF88)))
        .child(div().flex_1().child(info)),
    )
  }

  fn render_error(this: gpui::Div, error: SharedString) -> gpui::Div {
    this.child(
      div()
        .text_sm()
        .text_color(rgb(0xE07070))
        .child(error),
    )
  }

  fn render_buttons(&self, can_submit: bool, cx: &mut Context<Self>) -> impl IntoElement {
    h_flex()
      .justify_end()
      .gap_2()
      .child(
        div()
          .id("polkit-cancel")
          .cursor_pointer()
          .px_3()
          .py_1()
          .rounded_md()
          .text_sm()
          .text_color(rgba(0xFFFFFFAA))
          .hover(|this| this.bg(rgba(0xFFFFFF0F)).text_color(rgba(0xFFFFFFEE)))
          .on_click(cx.listener(|this, _, _window, cx| this.cancel(cx)))
          .child("Cancel"),
      )
      .when(can_submit, |this| {
        this.child(
          div()
            .id("polkit-authenticate")
            .cursor_pointer()
            .px_3()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .bg(rgba(0xFFFFFF1A))
            .text_color(rgba(0xFFFFFFEE))
            .hover(|this| this.bg(rgba(0xFFFFFF2A)))
            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
            .child("Authenticate"),
        )
      })
  }
}

/// Splits a polkit message around a command quoted GNU-style as `` `cmd' ``,
/// returning `(prefix, command, suffix)`. Returns `None` if no such command is
/// present, so the caller can render the message verbatim.
fn split_command(message: &str) -> Option<(&str, &str, &str)> {
  let open = message.find('`')?;
  let after = &message[open + 1..];
  let close = after.find('\'')?;
  Some((&message[..open], &after[..close], &after[close + 1..]))
}

/// A monospace, code-styled chip for the command being authorized. The command
/// is middle-ellipsized so both the store path prefix and the trailing binary +
/// arguments remain readable; `truncate` is a pixel-level safety net for very
/// narrow widths.
fn command_chip(command: &str) -> impl IntoElement {
  div()
    .w_full()
    .truncate()
    .px_2()
    .py_1()
    .rounded_md()
    .bg(rgba(0xFFFFFF14))
    .font_family("Iosevka")
    .text_xs()
    .text_color(rgba(0xFFFFFFDD))
    .child(ellipsize_middle(command, 40))
}

/// Shortens `text` to at most `max_chars` by dropping the middle and inserting
/// an ellipsis, keeping more of the tail than the head.
fn ellipsize_middle(text: &str, max_chars: usize) -> String {
  let count = text.chars().count();
  if count <= max_chars {
    return text.to_owned();
  }

  let budget = max_chars.saturating_sub(1);
  let tail = (budget * 2) / 3;
  let head = budget - tail;

  let head_str: String = text.chars().take(head).collect();
  let tail_str: String = text.chars().skip(count - tail).collect();
  format!("{head_str}…{tail_str}")
}

fn status_row(leading: gpui::AnyElement, label: &'static str) -> gpui::Div {
  h_flex()
    .gap_2()
    .items_center()
    .text_sm()
    .text_color(rgba(0xFFFFFFAA))
    .child(leading)
    .child(label)
}
