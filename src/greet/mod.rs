//! The login screen.
//!
//! Runs as an unprivileged user inside a compositor that `launch-greetd`
//! started, and talks to that daemon over a unix socket. It has no privileges
//! of its own: it cannot read other users' home directories, reach the
//! fingerprint reader, or start a session. Everything it knows arrives over the
//! socket, and everything it wants done is asked for there.
//!
//! Unlike the lock screen, the prompt appears on **one** output. Other outputs
//! get a surface too - otherwise the compositor's bare background would show
//! through on them - but those draw only the backdrop and never ask for the
//! keyboard. With exactly one input field in the process there is nothing to
//! keep in step across surfaces, and no question of which output has focus.

use gpui::{
  App, Bounds, Context, DisplayId, Entity, Focusable, Global, IntoElement, MouseButton, Render,
  SharedString, Size, Styled, Subscription, Task, Window, WindowBackgroundAppearance, WindowBounds,
  WindowHandle, WindowKind, WindowOptions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px,
};
use tracing::{debug, error, info, warn};

use crate::auth_screen::{
  AuthPrompt, AuthUser, FingerprintState, REJECTION_HIGHLIGHT, current_user, render_auth_screen,
};
use crate::config::ConfigState;
use crate::input::state::{InputEvent, InputState};
use crate::status::{self, Clock};
use crate::util::ResultExt;

struct GlobalGreeter(#[allow(dead_code)] Entity<Greeter>);

impl Global for GlobalGreeter {}

pub fn start(cx: &mut App) {
  let greeter = cx.new(Greeter::new);

  // Deliberately after `cx.new` has returned rather than inside it. Opening a
  // window draws it there and then, and the first draw reads the greeter - so
  // doing this while the entity is still being constructed, or from inside any
  // later update of it, panics on the double borrow.
  sync_screens(&greeter, cx);

  cx.set_global(GlobalGreeter(greeter));
}

/// What the screen is waiting for.
#[derive(Clone, PartialEq)]
pub enum Phase {
  /// Opening the socket and waiting to be told who can log in.
  Connecting,
  /// A user is selected and the daemon is listening. Typing is allowed.
  Ready,
  /// A password went out and no worker has answered.
  Verifying,
  /// PAM said yes and the session is starting. There is no way back.
  Starting,
  /// The daemon cannot be reached. Nothing to type into.
  Unavailable(SharedString),
}

/// Shared state of the login screen, rendered by a [`GreetScreen`] per output.
pub struct Greeter {
  users: Vec<AuthUser>,
  selected: usize,
  phase: Phase,
  /// Why the last attempt failed, if it did.
  error: Option<SharedString>,
  /// Whatever PAM had to say during an attempt.
  hint: Option<SharedString>,
  /// Whether an attempt was turned down just now, which the field is outlined
  /// for.
  rejected: bool,
  fingerprint: FingerprintState,
  /// Surfaces, in the order the displays were enumerated.
  screens: Vec<WindowHandle<GreetScreen>>,
  /// What `screens` was built for, so a display change that changes nothing is
  /// not acted on.
  displays: Vec<DisplayId>,
  _rejection: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl Greeter {
  fn new(cx: &mut Context<Self>) -> Self {
    let subscriptions = vec![cx.on_display_changed({
      let this = cx.weak_entity();
      move |cx| {
        let Some(this) = this.upgrade() else {
          return;
        };

        // Deferred, not immediate: this callback runs while the Wayland
        // client's state is still borrowed for the output event, and both
        // `cx.displays()` and opening a window borrow it again.
        cx.defer(move |cx| sync_screens(&this, cx));
      }
    })];

    // Until the daemon is wired up there is one user to show: whoever this
    // process runs as.
    let users = current_user().into_iter().collect();

    Self {
      users,
      selected: 0,
      phase: Phase::Ready,
      error: None,
      hint: None,
      rejected: false,
      fingerprint: FingerprintState::Off,
      screens: Vec::new(),
      displays: Vec::new(),
      _rejection: None,
      _subscriptions: subscriptions,
    }
  }

  pub fn user(&self) -> Option<&AuthUser> {
    self.users.get(self.selected)
  }

  pub fn phase(&self) -> &Phase {
    &self.phase
  }

  pub fn error(&self) -> Option<SharedString> {
    self.error.clone()
  }

  pub fn hint(&self) -> Option<SharedString> {
    self.hint.clone()
  }

  pub fn rejected(&self) -> bool {
    self.rejected
  }

  pub fn fingerprint(&self) -> FingerprintState {
    self.fingerprint
  }

  /// Whether typing should be refused, which is any state where the field's
  /// contents are about to be taken away or ignored.
  pub fn busy(&self) -> bool {
    !matches!(self.phase, Phase::Ready)
  }

  /// A line under the field for whatever is being waited on that the field's
  /// own spinner doesn't already explain.
  pub fn status(&self) -> Option<SharedString> {
    match &self.phase {
      Phase::Connecting => Some("Connecting to the login service…".into()),
      Phase::Starting => Some("Starting session…".into()),
      _ => None,
    }
  }

  fn submit(&mut self, password: SharedString, cx: &mut Context<Self>) {
    if self.busy() || password.is_empty() {
      return;
    }

    // Not wired to the daemon yet; the state machine that consumes this lands
    // with the IPC client.
    debug!(length = password.len(), "Password submitted");
    self.phase = Phase::Verifying;
    self.error = None;
    cx.notify();
  }

  /// Outlines the field for [`REJECTION_HIGHLIGHT`]. That is the whole of what
  /// a rejection says: the field it was typed into is the message.
  #[allow(dead_code)]
  fn reject(&mut self, cx: &mut Context<Self>) {
    self.rejected = true;
    cx.notify();

    self._rejection = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(REJECTION_HIGHLIGHT).await;

      this
        .update(cx, |this, cx| {
          this.rejected = false;
          cx.notify();
        })
        .log_err();
    }));
  }

  /// Which output carries the prompt: the configured one, else the first
  /// attached. Falls back rather than leaving the machine with a login screen
  /// nobody can type into.
  fn primary_index(&self, cx: &App) -> usize {
    let config = ConfigState::get(cx);
    let Some(configured) = config.primary_display else {
      return 0;
    };

    let found = cx
      .displays()
      .iter()
      .position(|display| display.name() == Some(configured.as_str()));

    if found.is_none() {
      warn!(display = %configured, "Configured display not attached, prompting on the first one");
    }

    found.unwrap_or(0)
  }

  fn all_open(&self, cx: &App) -> bool {
    !self.screens.is_empty()
      && self
        .screens
        .iter()
        .all(|screen| cx.windows().contains(&(*screen).into()))
  }
}

/// Opens a surface on every output, reopening when the set of outputs changes.
///
/// A free function rather than a method because opening a window draws it
/// immediately, and that first draw reads the greeter: doing this from inside
/// `Greeter::update` would be a double borrow. The state is read and written
/// around the windowing rather than across it.
///
/// A display going away takes its window with it, and the primary output can
/// move, so this rebuilds rather than patching. Reopening only when the id list
/// actually differs keeps an unrelated display event from flashing the screen.
fn sync_screens(greeter: &Entity<Greeter>, cx: &mut App) {
  let displays: Vec<DisplayId> = cx.displays().iter().map(|display| display.id()).collect();

  let (known, all_open, primary, clock_enabled) = {
    let state = greeter.read(cx);
    (
      state.displays.clone(),
      state.all_open(cx),
      state.primary_index(cx),
      ConfigState::get(cx).status.enabled,
    )
  };

  if displays == known && all_open {
    return;
  }

  for screen in greeter.update(cx, |state, _cx| std::mem::take(&mut state.screens)) {
    if let Err(error) = screen.update(cx, |_view, window, _cx| window.remove_window()) {
      debug!(?error, "Login surface was already gone");
    }
  }

  if displays.is_empty() {
    // Not an error: a laptop with the lid shut has no outputs, and one will
    // appear when it opens.
    warn!("No outputs attached, nothing to draw the login screen on");
    greeter.update(cx, |state, _cx| state.displays = displays);
    return;
  }

  let mut screens = Vec::new();

  for (index, display) in displays.iter().enumerate() {
    let prompt = index == primary;
    let clock = clock_enabled && prompt;
    let greeter = greeter.clone();

    let window = cx.open_window(window_options(*display, prompt), move |window, cx| {
      cx.new(move |cx| GreetScreen::new(greeter, prompt, clock, window, cx))
    });

    match window {
      Ok(handle) => screens.push(handle),
      // One bad output must not leave the machine with no way to log in.
      Err(error) => error!(?error, index, "Failed to open a login surface"),
    }
  }

  info!(surfaces = screens.len(), primary, "Login screen opened");

  greeter.update(cx, |state, _cx| {
    state.screens = screens;
    state.displays = displays;
  });
}

fn window_options(display: DisplayId, prompt: bool) -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-greeter".to_string()),
    display_id: Some(display),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(px(800.), px(600.)),
    })),
    // Opaque, unlike the polkit backdrop: this covers the screen rather than
    // dimming what is behind it.
    window_background: WindowBackgroundAppearance::Opaque,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-greeter".to_string(),
      layer: Layer::Overlay,
      // Opposite edges make the compositor stretch the surface to fill the
      // output.
      anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
      // Cover other layers' exclusive zones too. Nothing in the greeter session
      // claims one, but a login screen a bar could carve into would be a hole.
      exclusive_zone: Some(px(-1.)),
      exclusive_edge: None,
      margin: None,
      // Only the prompt takes the keyboard. Asking for it on every output would
      // leave the compositor to pick, and there is only one field to type into.
      keyboard_interactivity: match prompt {
        true => KeyboardInteractivity::Exclusive,
        false => KeyboardInteractivity::None,
      },
    }),
    ..Default::default()
  }
}

/// One output's surface. Only the one on the primary output has a field.
pub struct GreetScreen {
  greeter: Entity<Greeter>,
  /// The field, on the surface that has one.
  password: Option<Entity<InputState>>,
  clock: bool,
  _subscriptions: Vec<Subscription>,
}

impl GreetScreen {
  fn new(
    greeter: Entity<Greeter>,
    prompt: bool,
    clock: bool,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let mut subscriptions = vec![cx.observe(&greeter, |_this, _greeter, cx| cx.notify())];

    let password = prompt.then(|| {
      let password = cx.new(|cx| {
        InputState::new(window, cx)
          .masked(true)
          .placeholder("Password")
          .clean_on_escape()
      });

      window.focus(&password.focus_handle(cx), cx);

      subscriptions.push(cx.subscribe_in(
        &password,
        window,
        |this, _password, event: &InputEvent, _window, cx| match event {
          InputEvent::PressEnter { .. } => this.submit(cx),
          InputEvent::Change | InputEvent::Focus | InputEvent::Blur => {}
        },
      ));

      password
    });

    if clock {
      let clock_state = Clock::global(cx);
      subscriptions.push(cx.observe(&clock_state, |_this, _clock, cx| cx.notify()));
    }

    Self {
      greeter,
      password,
      clock,
      _subscriptions: subscriptions,
    }
  }

  fn submit(&mut self, cx: &mut Context<Self>) {
    let Some(password) = &self.password else {
      return;
    };

    let value = password.read(cx).value();
    self
      .greeter
      .update(cx, |greeter, cx| greeter.submit(value, cx));
  }

  /// The clock, drawn in the same corner the desktop overlay would use.
  fn render_clock(&self, cx: &App) -> gpui::Div {
    let clock = Clock::global(cx);
    let clock = clock.read(cx);

    status::render_clock_in_corner(
      clock.now,
      clock.battery,
      ConfigState::get(cx).status.opacity,
    )
  }
}

impl Render for GreetScreen {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let greeter = self.greeter.read(cx);

    // Outputs without a field draw the backdrop alone, so the compositor's
    // background never shows through next to the login screen.
    let Some(password) = self.password.clone() else {
      return div()
        .size_full()
        .bg(gpui::rgb(0x0D0D0D))
        .when(self.clock, |this| this.child(self.render_clock(cx)));
    };

    let Some(user) = greeter.user() else {
      return div()
        .size_full()
        .bg(gpui::rgb(0x0D0D0D))
        .when(self.clock, |this| this.child(self.render_clock(cx)));
    };

    render_auth_screen(AuthPrompt {
      user,
      password: &password,
      busy: greeter.busy(),
      fingerprint: greeter.fingerprint(),
      rejected: greeter.rejected(),
      hint: greeter.hint(),
      error: greeter.error(),
      status: greeter.status(),
      // The user switcher lands with the IPC client, which is what supplies a
      // list longer than one.
      below: None,
    })
    .on_mouse_down(
      MouseButton::Left,
      cx.listener(|this, _event, window, cx| {
        if let Some(password) = &this.password {
          window.focus(&password.focus_handle(cx), cx);
        }
      }),
    )
    .when(self.clock, |this| this.child(self.render_clock(cx)))
  }
}
