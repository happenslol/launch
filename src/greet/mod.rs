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

mod client;

use std::time::Duration;

use gpui::{
  App, Bounds, Context, DisplayId, Entity, EventEmitter, Focusable, Global, IntoElement,
  MouseButton, Render, SharedString, Size, Styled, Subscription, Task, Window,
  WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px,
};
use greet_ipc::{AuthFailure, AuthSource, Event, PROTOCOL_VERSION, Request, Secret};
use tracing::{debug, error, info, warn};

use crate::auth_screen::{
  AuthPrompt, AuthUser, FingerprintState, PasswordMirror, REJECTION_HIGHLIGHT,
  apply_password_mirror, render_auth_screen,
};
use crate::config::ConfigState;
use crate::greet::client::{ClientEvent, GreetClient};
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

/// What a [`Event::Failed`] does to the password field.
///
/// Split out because getting it wrong is invisible: the difference between
/// waiting for a prompt and asking for a new attempt is a login screen that
/// looks completely normal and silently swallows everything typed into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
  /// Nothing at all.
  Ignore,
  /// Say so, but leave the password field exactly as it is.
  ShowOnly,
  /// This path is dead; ask the daemon to start over.
  NewAttempt,
}

fn rejection(phase: &Phase, source: AuthSource) -> Rejection {
  match (phase, source) {
    // The race is over and the session is on its way. A losing worker's
    // complaint would only replace "Starting session" with an error for whatever
    // is left of the login screen's life.
    (Phase::Starting, _) => Rejection::Ignore,
    // A fingerprint giving up leaves the password alone. In particular it must
    // not clear `prompt_ready`: the password worker is still parked on its
    // prompt, and pretending otherwise sends the next thing typed to
    // `pending_password` to wait for a prompt that has already been and gone.
    (_, AuthSource::Fingerprint) => Rejection::ShowOnly,
    // The daemon never re-arms a slot, so there is no prompt coming for the
    // worker that just died - the whole attempt has to be asked for again.
    (_, AuthSource::Password) => Rejection::NewAttempt,
  }
}

/// The line shown under the field while the reader is armed.
///
/// Written here rather than passed through from `pam_fprintd`, whose own wording
/// ("Swipe your finger across the fingerprint reader") reads as the next thing to
/// do when it is really an alternative to the field above it. Phrased as an aside
/// for that reason.
fn fingerprint_hint(state: FingerprintState) -> Option<SharedString> {
  match state {
    FingerprintState::Off => None,
    FingerprintState::Starting => None,
    FingerprintState::Waiting => Some("or use your fingerprint".into()),
    FingerprintState::Reading => Some("Reading your fingerprint".into()),
  }
}

/// Shared state of the login screen, rendered by a [`GreetScreen`] per output.
pub struct Greeter {
  client: Option<GreetClient>,
  users: Vec<AuthUser>,
  selected: usize,
  phase: Phase,
  /// Whether a worker is actually blocked waiting for the password. Typing can
  /// beat the PAM stack to it, and answering a prompt that has not arrived is an
  /// error, so the value waits here instead.
  prompt_ready: bool,
  /// A password typed before the prompt arrived.
  pending_password: Option<Secret>,
  /// Output the daemon says should carry the prompt, which the greeter has no
  /// config of its own to learn from.
  primary_output: Option<SharedString>,
  /// Why the last attempt failed, if it did.
  error: Option<SharedString>,
  /// Whatever the password stack had to say during an attempt. The fingerprint
  /// line is not here - it comes from [`fingerprint_hint`], so it cannot outlive
  /// the reader that prompted it.
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
  /// How long to wait before the next attempt to reach the daemon.
  retry_delay: Duration,
  _events: Option<Task<()>>,
  _rejection: Option<Task<()>>,
  _retry: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

/// First delay before reconnecting, doubled on each failure up to
/// [`RETRY_MAX`]. A login screen never stops trying: one that gives up is a
/// machine nobody can log into.
const RETRY_INITIAL: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

impl EventEmitter<PasswordMirror> for Greeter {}

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

    let mut this = Self {
      client: None,
      users: Vec::new(),
      selected: 0,
      phase: Phase::Connecting,
      prompt_ready: false,
      pending_password: None,
      primary_output: None,
      error: None,
      hint: None,
      rejected: false,
      fingerprint: FingerprintState::Off,
      screens: Vec::new(),
      displays: Vec::new(),
      retry_delay: RETRY_INITIAL,
      _events: None,
      _rejection: None,
      _retry: None,
      _subscriptions: subscriptions,
    };

    this.connect(cx);
    this
  }

  /// Opens the socket and starts folding what comes back into this state.
  fn connect(&mut self, cx: &mut Context<Self>) {
    let (sender, receiver) = flume::unbounded();

    self.phase = Phase::Connecting;
    self.prompt_ready = false;
    self.client = Some(GreetClient::connect(sender, cx));

    self._events = Some(cx.spawn(async move |this, cx| {
      while let Ok(event) = receiver.recv_async().await {
        let resync = match this.update(cx, |this, cx| this.handle(event, cx)) {
          Ok(resync) => resync,
          // The login screen is gone; nothing left to deliver to.
          Err(_) => break,
        };

        // Outside the update above, deliberately: opening a window draws it, and
        // that draw reads this entity.
        if resync && let Some(entity) = this.upgrade() {
          cx.update(|cx| sync_screens(&entity, cx));
        }
      }
    }));

    self.request(Request::Hello {
      version: PROTOCOL_VERSION,
    });
  }

  fn request(&self, request: Request) {
    let Some(client) = &self.client else {
      debug!("No connection to send a request on");
      return;
    };

    client.send(request);
  }

  /// Folds one frame from the daemon into this state, returning whether the
  /// surfaces need rebuilding.
  fn handle(&mut self, event: ClientEvent, cx: &mut Context<Self>) -> bool {
    let mut resync = false;

    match event {
      ClientEvent::Message(message) => resync = self.apply(message, cx),
      ClientEvent::Disconnected(reason) => self.disconnected(reason, cx),
    }

    cx.notify();
    resync
  }

  fn apply(&mut self, message: Event, cx: &mut Context<Self>) -> bool {
    match message {
      Event::Welcome {
        version,
        users,
        default_user,
        fingerprint,
        primary_output,
      } => {
        if version != PROTOCOL_VERSION {
          warn!(
            daemon = version,
            greeter = PROTOCOL_VERSION,
            "Protocol version mismatch"
          );
        }

        info!(count = users.len(), default_user, "Accounts offered");

        self.users = users.into_iter().map(AuthUser::from_ipc).collect();
        self.selected = self
          .users
          .iter()
          .position(|user| user.name == default_user)
          .unwrap_or(0);

        self.primary_output = primary_output.map(SharedString::from);
        self.retry_delay = RETRY_INITIAL;
        self.fingerprint = match fingerprint {
          true => FingerprintState::Starting,
          false => FingerprintState::Off,
        };

        self.begin(cx);

        // The daemon may have named a different output than the one guessed
        // before it answered.
        true
      }

      // Only the password worker's prompts are answered from the field; the
      // fingerprint worker never asks for anything typed.
      Event::Prompt {
        source: AuthSource::Password,
        ..
      } => {
        self.prompt_ready = true;

        match self.pending_password.take() {
          Some(secret) => {
            self.phase = Phase::Verifying;
            self.request(Request::Password { value: secret });
          }
          None => self.phase = Phase::Ready,
        }

        false
      }

      Event::Prompt { .. } => false,

      Event::Info { message, .. } => {
        self.hint = Some(message.into());
        false
      }

      Event::Error { message, .. } => {
        self.error = Some(message.into());
        false
      }

      Event::Fingerprint { state } => {
        self.fingerprint = state.into();
        false
      }

      Event::Failed { source, failure } => {
        match rejection(&self.phase, source) {
          Rejection::Ignore => {}
          Rejection::ShowOnly => self.show_failure(failure, cx),
          Rejection::NewAttempt => {
            self.show_failure(failure, cx);
            self.disarm_password(cx);
            self.begin(cx);
          }
        }

        false
      }

      Event::Authenticated { via } => {
        info!(?via, "Authenticated");
        self.phase = Phase::Starting;
        self.fingerprint = FingerprintState::Off;
        self.hint = None;
        self.error = None;
        self.request(Request::StartSession);
        false
      }

      Event::SessionStarted => {
        info!("Session starting, closing the login screen");
        cx.quit();
        false
      }

      Event::SessionFailed { message } => {
        // The screen stays up: a login screen that has gone dark is worse than
        // one saying the session would not start.
        error!(message, "The session could not be started");
        self.error = Some(message.into());
        self.clear_password(cx);
        self.begin(cx);
        false
      }

      Event::RequestFailed { message } => {
        warn!(message, "The login service refused a request");
        self.error = Some(message.into());
        self.phase = Phase::Ready;
        false
      }
    }
  }

  fn show_failure(&mut self, failure: AuthFailure, cx: &mut Context<Self>) {
    match failure {
      // The outlined field is the whole message.
      AuthFailure::Rejected => self.reject(cx),
      AuthFailure::Error { message } => self.error = Some(message.into()),
    }
  }

  /// Forgets that a worker was waiting on the field, and empties it.
  fn disarm_password(&mut self, cx: &mut Context<Self>) {
    self.prompt_ready = false;
    self.pending_password = None;
    self.clear_password(cx);
  }

  /// Asks the daemon to start authenticating the selected user.
  fn begin(&mut self, cx: &mut Context<Self>) {
    let Some(user) = self.user() else {
      warn!("No accounts to authenticate");
      self.phase = Phase::Unavailable("No accounts are available".into());
      return;
    };

    let username = user.name.to_string();
    self.prompt_ready = false;
    self.pending_password = None;
    self.phase = Phase::Ready;
    self.hint = None;
    cx.notify();

    self.request(Request::Authenticate { username });
  }

  fn disconnected(&mut self, reason: SharedString, cx: &mut Context<Self>) {
    warn!(%reason, "Disconnected from the login service");

    self.client = None;
    self.prompt_ready = false;
    self.pending_password = None;
    self.phase = Phase::Unavailable(reason.clone());

    // A missing socket means we were not started by the daemon, and no amount of
    // retrying will conjure one.
    if std::env::var_os(greet_ipc::SOCKET_ENV_VAR).is_none() {
      error!("Not started by launch-greetd; giving up");
      return;
    }

    let delay = self.retry_delay;
    self.retry_delay = (delay * 2).min(RETRY_MAX);

    self._retry = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(delay).await;

      this
        .update(cx, |this, cx| {
          debug!(?delay, "Reconnecting to the login service");
          this.connect(cx);
          cx.notify();
        })
        .log_err();
    }));
  }

  /// Empties the field on the surface that has one.
  fn clear_password(&mut self, cx: &mut Context<Self>) {
    cx.emit(PasswordMirror {
      value: SharedString::default(),
      source: None,
    });
  }

  pub fn user(&self) -> Option<&AuthUser> {
    self.users.get(self.selected)
  }

  pub fn error(&self) -> Option<SharedString> {
    self.error.clone()
  }

  /// A word from the password stack wins the single hint row over the standing
  /// fingerprint line: it is something the user has to read, where the reader
  /// being armed is also visible from the indicator in the field.
  pub fn hint(&self) -> Option<SharedString> {
    self
      .hint
      .clone()
      .or_else(|| fingerprint_hint(self.fingerprint))
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

    let secret = match Secret::new(password.to_string()) {
      Ok(secret) => secret,
      Err(_) => {
        self.error = Some("That password is too long".into());
        cx.notify();
        return;
      }
    };

    self.phase = Phase::Verifying;
    self.error = None;
    cx.notify();

    // Typing can outrun the PAM stack. Answering a prompt that has not been
    // asked is an error, so an early password waits for it instead.
    match self.prompt_ready {
      true => self.request(Request::Password { value: secret }),
      false => self.pending_password = Some(secret),
    }
  }

  /// Outlines the field for [`REJECTION_HIGHLIGHT`]. That is the whole of what
  /// a rejection says: the field it was typed into is the message.
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
    // What the daemon says wins: it has the system configuration, whereas the
    // greeter user's own config file usually does not exist.
    let configured = self
      .primary_output
      .clone()
      .or_else(|| ConfigState::get(cx).primary_display.map(SharedString::from));

    let Some(configured) = configured else {
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

      // The greeter empties the field after a rejection, which it cannot do
      // itself: the field lives here.
      subscriptions.push(cx.subscribe_in(
        &greeter,
        window,
        |this, _greeter, event: &PasswordMirror, window, cx| {
          if let Some(password) = this.password.clone() {
            apply_password_mirror(event, &password, cx.entity_id(), window, cx);
          }
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

#[cfg(test)]
mod tests {
  use super::*;

  /// The invariant worth pinning: a fingerprint failure never disturbs the
  /// password field. Everything else on this screen is visible when it breaks;
  /// this is the one that looks fine and silently swallows what is typed.
  #[test]
  fn a_fingerprint_failure_leaves_the_password_field_alone() {
    assert_eq!(
      rejection(&Phase::Ready, AuthSource::Fingerprint),
      Rejection::ShowOnly
    );
  }

  #[test]
  fn a_failed_password_asks_for_a_new_attempt() {
    assert_eq!(
      rejection(&Phase::Ready, AuthSource::Password),
      Rejection::NewAttempt
    );
  }

  /// The reader going away has to take its line with it, or the screen offers a
  /// fingerprint nothing is listening for.
  #[test]
  fn the_fingerprint_line_only_exists_while_the_reader_does() {
    assert_eq!(fingerprint_hint(FingerprintState::Off), None);
    assert!(fingerprint_hint(FingerprintState::Waiting).is_some());
  }

  /// The losing worker's failure can arrive after the winner authenticated.
  #[test]
  fn nothing_is_applied_once_the_session_is_starting() {
    for source in [AuthSource::Password, AuthSource::Fingerprint] {
      assert_eq!(
        rejection(&Phase::Starting, source),
        Rejection::Ignore,
        "for {source:?}"
      );
    }
  }
}
