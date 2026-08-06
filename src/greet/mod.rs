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

use futures::StreamExt as _;
use gpui::{
  App, Bounds, Context, DisplayId, Entity, EventEmitter, FocusHandle, Focusable, Global,
  IntoElement, KeyBinding, MouseButton, Render, SharedString, Size, Styled, Subscription, Task,
  Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions,
  actions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rems, rgb, rgba,
};
use greet_ipc::{AuthFailure, AuthSource, Event, PROTOCOL_VERSION, Request, Secret};
use tracing::{debug, error, info, warn};

use crate::auth_screen::{
  AuthPrompt, AuthUser, FingerprintState, PasswordMirror, REJECTION_HIGHLIGHT,
  apply_password_mirror, render_auth_screen, render_avatar,
};
use crate::config::ConfigState;
use crate::dbus::GlobalDbusConnection;
use crate::dbus::fprintd::FingerprintReader;
use crate::greet::client::{ClientEvent, GreetClient};
use crate::icon::{Icon, IconName};
use crate::input::state::{InputEvent, InputState};
use crate::status::{self, Clock};
use crate::util::{ResultExt, h_flex, v_flex};

struct GlobalGreeter(#[allow(dead_code)] Entity<Greeter>);

impl Global for GlobalGreeter {}

actions!(
  greeter,
  [
    /// Replace the prompt with the list of accounts.
    SwitchUser,
    /// Return to the prompt, changing nothing.
    DismissSwitcher,
    SelectNextUser,
    SelectPrevUser,
    /// Authenticate as the highlighted account.
    ConfirmUser,
  ]
);

/// Active whenever the prompt has the keyboard, so `alt-u` reaches us from
/// inside the password field. No `InputState` binding uses `alt`, so nothing is
/// shadowed.
const CONTEXT: &str = "greeter";

/// Active only while the list is up, which is what lets it claim the bare arrows
/// and `enter` that belong to the field the rest of the time.
const SWITCHER_CONTEXT: &str = "greeter_switcher";

/// Avatars in the list are chips beside a name rather than the portrait the
/// prompt leads with, so they get their own size through the same renderer.
const SWITCHER_AVATAR_SIZE: gpui::Pixels = px(40.);

pub fn start(cx: &mut App) {
  // Bound once here rather than per surface: `bind_keys` is global, and a
  // greeter with three outputs would otherwise register everything three times.
  cx.bind_keys([
    KeyBinding::new("alt-u", SwitchUser, Some(CONTEXT)),
    KeyBinding::new("escape", DismissSwitcher, Some(SWITCHER_CONTEXT)),
    KeyBinding::new("down", SelectNextUser, Some(SWITCHER_CONTEXT)),
    KeyBinding::new("up", SelectPrevUser, Some(SWITCHER_CONTEXT)),
    KeyBinding::new("enter", ConfirmUser, Some(SWITCHER_CONTEXT)),
  ]);

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
  /// Whatever the password stack had to say during an attempt, which is the only
  /// thing this row ever carries - the same as the lock screen. The reader is
  /// offered and its progress shown by the indicator in the field alone.
  hint: Option<SharedString>,
  /// Whether an attempt was turned down just now, which the field is outlined
  /// for.
  rejected: bool,
  /// Which account the attempt in flight is for, which is not always the
  /// highlighted one: the list moves its highlight freely and only commits on
  /// enter.
  authenticating: Option<SharedString>,
  /// Whether the account list has replaced the prompt.
  ///
  /// Never the opening screen, even with several accounts on offer: the machine
  /// almost always wants the same one, and a list would make every login two
  /// steps.
  switching: bool,
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
  /// Follows the sensor so the field can spin while a finger is actually on it.
  _finger_present: Option<Task<()>>,
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
      authenticating: None,
      switching: false,
      fingerprint: FingerprintState::Off,
      screens: Vec::new(),
      displays: Vec::new(),
      retry_delay: RETRY_INITIAL,
      _events: None,
      _finger_present: None,
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

        // Armed for the first time: start following the sensor. `pam_fprintd`
        // reports nothing between arming and success - verified in its source, it
        // only speaks on retries - so a finger landing is invisible through the
        // daemon. fprintd itself does say, and `finger-present` needs no claim
        // and no polkit, which is the one thing an unprivileged login screen can
        // still ask about.
        if self.fingerprint == FingerprintState::Waiting && self._finger_present.is_none() {
          self.watch_finger_present(cx);
        }

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
        // A finger can land on the reader while the list is open, and there is no
        // account left to choose once one has been let in.
        self.switching = false;
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

  /// Mirrors the sensor into the field's indicator, exactly as the lock screen
  /// does - it is the same fprintd property behind the same two states.
  fn watch_finger_present(&mut self, cx: &mut Context<Self>) {
    let connection = GlobalDbusConnection::system(cx);

    self._finger_present = Some(cx.spawn(async move |this, cx| {
      // The global connection is lazy; nothing has forced it before now.
      let Some(connection) = connection.await else {
        warn!("System bus unavailable, the sensor cannot be followed");
        return;
      };

      let reader = match FingerprintReader::observe(&connection).await {
        Ok(Some(reader)) => reader,
        Ok(None) => {
          debug!("No fingerprint reader to follow");
          return;
        }
        Err(error) => {
          warn!(?error, "Could not reach the fingerprint reader");
          return;
        }
      };

      let changes = match reader.listen_finger_present().await {
        Ok(changes) => changes,
        Err(error) => {
          warn!(?error, "Could not follow the state of the sensor");
          return;
        }
      };

      debug!(reader = %reader.name, "Following the fingerprint sensor");

      futures::pin_mut!(changes);

      while let Some(present) = changes.next().await {
        if this
          .update(cx, |this, cx| this.set_finger_present(present, cx))
          .is_err()
        {
          break;
        }
      }
    }));
  }

  /// Moves only between waiting and reading. A reader the daemon has turned off,
  /// or an attempt already won, must not be brought back to life by a stray
  /// property change.
  fn set_finger_present(&mut self, present: bool, cx: &mut Context<Self>) {
    let state = match (self.fingerprint, present) {
      (FingerprintState::Waiting, true) => FingerprintState::Reading,
      (FingerprintState::Reading, false) => FingerprintState::Waiting,
      _ => return,
    };

    self.fingerprint = state;
    cx.notify();
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
    self.authenticating = Some(user.name.clone());
    self.prompt_ready = false;
    self.pending_password = None;
    self.phase = Phase::Ready;
    self.hint = None;
    cx.notify();

    self.request(Request::Authenticate { username });
  }

  /// Shows or hides the account list. Only ever reachable when there is more
  /// than one account, so it cannot strand a single-user machine in a list of
  /// one.
  fn set_switching(&mut self, switching: bool, cx: &mut Context<Self>) {
    if self.users.len() < 2 {
      return;
    }

    self.switching = switching;
    cx.notify();
  }

  /// Points the highlight at one account, for a pointer that skipped the arrows.
  fn highlight(&mut self, index: usize, cx: &mut Context<Self>) {
    if index >= self.users.len() {
      return;
    }

    self.selected = index;
    cx.notify();
  }

  /// Moves the highlight without committing to it, so arrowing past an account
  /// does not tear down a perfectly good attempt on the way.
  fn move_highlight(&mut self, delta: isize, cx: &mut Context<Self>) {
    if self.users.is_empty() {
      return;
    }

    let count = self.users.len() as isize;
    let next = (self.selected as isize + delta).rem_euclid(count);
    self.selected = next as usize;
    cx.notify();
  }

  /// Commits the highlighted account and returns to the prompt.
  fn confirm_highlight(&mut self, cx: &mut Context<Self>) {
    self.switching = false;

    // Landed back on the account already being authenticated - a plain escape by
    // another route. Restarting would throw away a live attempt, and with it a
    // reader that is already armed, for nothing.
    if self.authenticating.as_ref() == self.user().map(|user| &user.name) {
      cx.notify();
      return;
    }

    self.error = None;
    self.rejected = false;
    self.fingerprint = FingerprintState::Off;
    self.clear_password(cx);

    // No `Cancel` first: the daemon abandons the attempt in flight when it is
    // asked to authenticate somebody else, so sending one would only be a second
    // way of saying the same thing.
    self.begin(cx);
  }

  fn disconnected(&mut self, reason: SharedString, cx: &mut Context<Self>) {
    warn!(%reason, "Disconnected from the login service");

    self.client = None;
    self.prompt_ready = false;
    self.pending_password = None;
    // A list of accounts none of which can be authenticated is worse than none.
    self.switching = false;
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

  pub fn users(&self) -> &[AuthUser] {
    &self.users
  }

  pub fn selected(&self) -> usize {
    self.selected
  }

  pub fn switching(&self) -> bool {
    self.switching
  }

  /// Whether there is anywhere to switch to. With one account the button is not
  /// drawn at all, so the common machine never sees it.
  pub fn can_switch(&self) -> bool {
    self.users.len() > 1
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
  /// Held by the account list while it is up, because it needs the arrows and
  /// `enter` that the field binds the rest of the time.
  focus_handle: FocusHandle,
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
      focus_handle: cx.focus_handle(),
      _subscriptions: subscriptions,
    }
  }

  /// Puts the account list up and gives it the keyboard.
  ///
  /// Focus has to move off the field: the list wants the bare arrows and `enter`
  /// that `InputState` binds, and it only gets them once the field is no longer
  /// the focused element.
  fn on_switch_user(&mut self, _: &SwitchUser, window: &mut Window, cx: &mut Context<Self>) {
    if !self.greeter.read(cx).can_switch() {
      return;
    }

    window.focus(&self.focus_handle, cx);
    self
      .greeter
      .update(cx, |greeter, cx| greeter.set_switching(true, cx));
  }

  fn on_dismiss_switcher(
    &mut self,
    _: &DismissSwitcher,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.close_switcher(window, cx);
    self
      .greeter
      .update(cx, |greeter, cx| greeter.set_switching(false, cx));
  }

  fn on_select_next_user(&mut self, _: &SelectNextUser, _: &mut Window, cx: &mut Context<Self>) {
    self
      .greeter
      .update(cx, |greeter, cx| greeter.move_highlight(1, cx));
  }

  fn on_select_prev_user(&mut self, _: &SelectPrevUser, _: &mut Window, cx: &mut Context<Self>) {
    self
      .greeter
      .update(cx, |greeter, cx| greeter.move_highlight(-1, cx));
  }

  fn on_confirm_user(&mut self, _: &ConfirmUser, window: &mut Window, cx: &mut Context<Self>) {
    self.confirm(window, cx);
  }

  fn confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    self.close_switcher(window, cx);
    self
      .greeter
      .update(cx, |greeter, cx| greeter.confirm_highlight(cx));
  }

  /// Hands the keyboard back to the field, which is where every route out of the
  /// list ends up.
  fn close_switcher(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    if let Some(password) = &self.password {
      window.focus(&password.focus_handle(cx), cx);
    }
  }

  /// One row of the account list.
  fn render_user_row(
    &self,
    index: usize,
    user: &AuthUser,
    highlighted: bool,
    cx: &mut Context<Self>,
  ) -> gpui::Stateful<gpui::Div> {
    let display_name = user.display_name.clone();
    let username = user.name.clone();

    h_flex()
      .id(("greeter-user", index))
      .w_full()
      .items_center()
      .gap_3()
      .p_2()
      .rounded(px(8.))
      .when(highlighted, |this| this.bg(rgba(0xFFFFFF14)))
      .hover(|this| this.bg(rgba(0xFFFFFF1F)))
      .child(render_avatar(
        user.avatar.as_ref(),
        user.initial.clone(),
        SWITCHER_AVATAR_SIZE,
      ))
      .child(
        v_flex()
          .gap_0p5()
          .child(div().text_sm().child(display_name.clone()))
          .when(display_name != username, |this| {
            this.child(div().text_xs().text_color(rgba(0xFFFFFFAA)).child(username))
          }),
      )
      .on_click(cx.listener(move |this, _event, window, cx| {
        this
          .greeter
          .update(cx, |greeter, cx| greeter.highlight(index, cx));
        this.confirm(window, cx);
      }))
  }

  /// The list itself, which replaces the prompt rather than covering it: a login
  /// screen with two things asking for input at once is a login screen where it
  /// is unclear what typing does.
  fn render_switcher(&mut self, cx: &mut Context<Self>) -> gpui::Div {
    let greeter = self.greeter.read(cx);
    let selected = greeter.selected();

    let rows: Vec<_> = greeter
      .users()
      .iter()
      .enumerate()
      .map(|(index, user)| (index, user.clone()))
      .collect();

    v_flex()
      .track_focus(&self.focus_handle)
      .key_context(SWITCHER_CONTEXT)
      .on_action(cx.listener(Self::on_dismiss_switcher))
      .on_action(cx.listener(Self::on_select_next_user))
      .on_action(cx.listener(Self::on_select_prev_user))
      .on_action(cx.listener(Self::on_confirm_user))
      .w(px(360.))
      .gap_1()
      .child(
        div()
          .text_sm()
          .text_color(rgba(0xFFFFFFAA))
          .pb_2()
          .child("Log in as"),
      )
      .children(
        rows
          .into_iter()
          .map(|(index, user)| self.render_user_row(index, &user, index == selected, cx)),
      )
  }

  /// The quiet way back into the list, for a pointer or for somebody who does not
  /// know about `alt-u`.
  fn render_switch_button(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
    h_flex()
      .id("greeter-switch-user")
      .items_center()
      .gap_2()
      .px_2()
      .py_1()
      .rounded(px(6.))
      .text_sm()
      .text_color(rgba(0xFFFFFFAA))
      .hover(|this| this.bg(rgba(0xFFFFFF14)).text_color(rgb(0xFFFFFF)))
      .child(Icon::new(IconName::Users).size(rems(0.95)))
      .child("Switch user")
      .on_click(
        cx.listener(|this, _event, window, cx| this.on_switch_user(&SwitchUser, window, cx)),
      )
      .into_any_element()
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

    if greeter.switching() {
      return backdrop()
        .child(self.render_switcher(cx))
        .when(self.clock, |this| this.child(self.render_clock(cx)));
    }

    let Some(user) = greeter.user() else {
      return div()
        .size_full()
        .bg(gpui::rgb(0x0D0D0D))
        .when(self.clock, |this| this.child(self.render_clock(cx)));
    };

    let can_switch = greeter.can_switch();
    let busy = greeter.busy();
    let fingerprint = greeter.fingerprint();
    let rejected = greeter.rejected();
    let hint = greeter.hint();
    let error = greeter.error();
    let status = greeter.status();
    let user = user.clone();

    let below = match can_switch {
      true => Some(self.render_switch_button(cx)),
      false => None,
    };

    render_auth_screen(AuthPrompt {
      user: &user,
      password: &password,
      busy,
      fingerprint,
      rejected,
      hint,
      error,
      status,
      below,
    })
    // On the prompt rather than only on the field, so `alt-u` works wherever the
    // keyboard happens to be within the login screen.
    .key_context(CONTEXT)
    .on_action(cx.listener(Self::on_switch_user))
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

/// The frame both states share, so switching does not move the background.
fn backdrop() -> gpui::Div {
  div()
    .size_full()
    .relative()
    .flex()
    .flex_col()
    .items_center()
    .justify_center()
    .bg(rgb(0x0D0D0D))
    .text_color(rgb(0xFFFFFF))
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
