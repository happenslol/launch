//! Screen locker.
//!
//! Locks the session through the `ext-session-lock-v1` Wayland protocol and puts
//! a lock surface on every output. The compositor hands input to those surfaces
//! only, and keeps the session locked even if this process dies - which is what
//! makes the protocol safe, and why [`is_locked`] holds the daemon back from
//! being replaced by a newer build while the screen is locked (see
//! [`crate::instance`]).
//!
//! Two ways of unlocking run side by side: a password verified through PAM, and,
//! when the machine has a reader with enrolled prints, a fingerprint verified
//! through fprintd. Either one unlocks, and neither blocks the other.
//!
//! # Recovering from a crash
//!
//! If the daemon dies while locked, the compositor keeps the session locked and
//! paints the outputs a flat colour with nothing to type into. The protocol lets
//! a new client take the lock over, though, so from a VT:
//!
//! ```sh
//! export XDG_RUNTIME_DIR=/run/user/$(id -u)
//! export WAYLAND_DISPLAY=$(systemctl --user show-environment | sed -n 's/^WAYLAND_DISPLAY=//p')
//! pkill -x launch  # a daemon that is hung rather than dead still holds the lock
//! launch lock
//! ```
//!
//! Switching back to the session then shows a fresh lock screen. Whether the
//! takeover is granted is up to the compositor: niri and Hyprland replace a lock
//! whose client has died, while a lock held by a live client is always refused.

mod pam;

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Local, Timelike as _};
use futures::future::Shared;
use futures::{FutureExt as _, StreamExt as _, select_biased, stream};
use gpui::{
  AnyElement, App, AsyncApp, Context, Entity, EntityId, EventEmitter, Focusable, FontWeight,
  Global, ImageSource, IntoElement, MouseButton, ObjectFit, Pixels, Render, Resource, SharedString,
  Styled, Subscription, Task, WeakEntity, Window, div, img, prelude::*, px, rems, rgb, rgba,
};
use tracing::{debug, error, info, warn};
use uzers::os::unix::UserExt as _;

use crate::config::{ConfigState, LockConfig};
use crate::dbus::GlobalDbusConnection;
use crate::dbus::fprintd::{FingerprintReader, VerifyStatus};
use crate::dbus::logind::{Session, SessionRequest};
use crate::dbus::upower::Battery;
use crate::icon::{Icon, IconName, Spinner};
use crate::input::input;
use crate::input::state::{InputEvent, InputState};
use crate::launcher::Launcher;
use crate::lock::pam::AuthEvent;
use crate::util::{ResultExt, h_flex, v_flex};

/// The clock only shows hours and minutes, but is checked every second so it
/// flips over roughly when it should.
const CLOCK_TICK: Duration = Duration::from_secs(1);

/// Consecutive unrecognized fingers after which verification stops and the user
/// is pointed at their password, mirroring what `pam_fprintd` does.
const MAX_FINGERPRINT_FAILURES: u32 = 5;

/// How long fprintd gets to answer while the reader is being set up. It talks to
/// the hardware over USB during those calls, and a reader in a bad state - one
/// that is enumerated but won't open, say - can leave them outstanding
/// indefinitely. The lock screen falls back to password-only rather than
/// promising a finger prompt that will never arrive.
const FINGERPRINT_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The clock is a copy of the desktop one from the `status` overlay, which the
/// lock surfaces cover up: same corner, same formats, same weights, same wash.
const TIME_FORMAT: &str = "%H:%M";
const DATE_FORMAT: &str = "%a, %e %b";
const CLOCK_OPACITY: f32 = 0.35;
const CLOCK_MARGIN_RIGHT: Pixels = px(10.);
const CLOCK_MARGIN_BOTTOM: Pixels = px(5.);

/// Mirrors the lock state for readers outside the app's foreground thread.
static LOCKED: AtomicBool = AtomicBool::new(false);

/// Whether the session is currently locked.
///
/// Read by the instance listener thread, which has no access to the app, hence
/// the atomic rather than a global entity.
pub fn is_locked() -> bool {
  LOCKED.load(Ordering::Acquire)
}

struct GlobalLock(Option<Entity<Lock>>);

impl Global for GlobalLock {}

pub fn init(cx: &mut App) {
  cx.set_global(GlobalLock(None));
  watch_session_requests(cx);
}

/// Locks the session, unless it already is.
pub fn lock(cx: &mut App) {
  // The surfaces are the source of truth, not our own bookkeeping: a compositor
  // that takes the lock away closes them without going through `unlock`.
  if lock_screens_exist(cx) {
    debug!("Session is already locked");
    return;
  }

  if cx.global::<GlobalLock>().0.is_some() {
    warn!("Previous lock ended without unlocking, cleaning up after it");
    clear_lock_state(cx);
  }

  let Some(user) = current_user() else {
    error!("Could not determine the current user, refusing to lock");
    return;
  };

  let config = ConfigState::get(cx).lock;
  let lock = cx.new(|cx| Lock::new(user, config, cx));

  close_launcher_windows(cx);

  // `lock_session` creates one surface per display, walking the same list
  // `clock_display` just read, in the same order. Nothing can reorder or resize
  // it in between - both run on this thread without yielding - so counting the
  // callbacks is what tells a surface which display it belongs to. The window
  // itself can't say: it learns its output from a Wayland event that hasn't
  // arrived yet while it is being built.
  let clock_display = clock_display(cx);
  let screen = Cell::new(0usize);

  let result = cx.lock_session({
    let lock = lock.clone();
    move |window, cx| {
      let index = screen.replace(screen.get() + 1);
      let clock = clock_display.is_none_or(|display| display == index);
      let lock = lock.clone();
      cx.new(move |cx| LockScreen::new(lock, clock, window, cx))
    }
  });

  if let Err(error) = result {
    error!(?error, "Failed to lock the session");

    // A half-locked session is worse than none at all: the compositor hides
    // everything while there may be no surface left to type into.
    if let Err(error) = cx.unlock_session() {
      debug!(?error, "Nothing to unlock after the failed lock");
    }

    return;
  }

  LOCKED.store(true, Ordering::Release);
  cx.global_mut::<GlobalLock>().0 = Some(lock.clone());
  lock.update(cx, |lock, cx| lock.start(cx));
  set_locked_hint(true, cx);

  info!("Session locked");
}

/// Unlocks the session without asking for anything, for use by logind's unlock
/// request only: logind authorizes the caller of `loginctl unlock-session`
/// before forwarding it to us.
fn unlock(cx: &mut App) {
  let Some(lock) = cx.global::<GlobalLock>().0.clone() else {
    debug!("Session is not locked");
    return;
  };

  lock.update(cx, |lock, cx| lock.unlock(cx));
}

/// Drops the lock surfaces and lets the compositor show the session again.
fn unlock_session(cx: &mut App) {
  if let Err(error) = cx.unlock_session() {
    error!(?error, "Failed to unlock the session");
  }

  clear_lock_state(cx);
  info!("Session unlocked");
}

/// Forgets the current lock without touching the compositor, for when the lock
/// is already gone.
fn clear_lock_state(cx: &mut App) {
  cx.global_mut::<GlobalLock>().0.take();
  LOCKED.store(false, Ordering::Release);
  set_locked_hint(false, cx);
}

fn lock_screens_exist(cx: &App) -> bool {
  cx.windows()
    .iter()
    .any(|window| window.downcast::<LockScreen>().is_some())
}

/// Follows lock and unlock requests from logind. This is how `loginctl
/// lock-session`, a lid switch or an idle daemon reaches a locker.
fn watch_session_requests(cx: &mut App) {
  let connection = GlobalDbusConnection::system(cx);

  cx.spawn(async move |cx| {
    let Some(connection) = connection.await else {
      warn!("System bus unavailable, session lock requests will be ignored");
      return;
    };

    let session = match Session::current(&connection).await {
      Ok(session) => session,
      Err(error) => {
        error!(?error, "Failed to reach the login session");
        return;
      }
    };

    let mut requests = match session.listen_requests().await {
      Ok(requests) => requests,
      Err(error) => {
        error!(?error, "Failed to subscribe to session lock requests");
        return;
      }
    };

    while let Some(request) = requests.next().await {
      debug!(?request, "Received session request from logind");

      cx.update(|cx| match request {
        SessionRequest::Lock => lock(cx),
        SessionRequest::Unlock => unlock(cx),
      });
    }
  })
  .detach();
}

/// Publishes the lock state to logind so `loginctl` and anything watching
/// `LockedHint` agrees with what is on screen.
fn set_locked_hint(locked: bool, cx: &mut App) {
  let connection = GlobalDbusConnection::system(cx);

  cx.spawn(async move |_cx| {
    let Some(connection) = connection.await else {
      return;
    };

    match Session::current(&connection).await {
      Ok(session) => {
        session.set_locked_hint(locked).await.log_err();
      }
      Err(error) => warn!(?error, "Failed to reach the login session"),
    }
  })
  .detach();
}

/// Closes any launcher window. The compositor hides it while locked, but it
/// would still be sitting there, with a stale query, after unlocking.
fn close_launcher_windows(cx: &mut App) {
  for handle in cx.windows() {
    let Some(launcher) = handle.downcast::<Launcher>() else {
      continue;
    };

    if let Err(error) = launcher.update(cx, |_launcher, window, _cx| window.remove_window()) {
      debug!(?error, "Launcher window was already gone");
    }
  }
}

/// The user the lock screen authenticates, shown on it and handed to PAM.
struct LockUser {
  /// Login name; this is what PAM verifies the password for.
  name: String,
  display_name: SharedString,
  /// First letter of the display name, for when there is no avatar to show.
  initial: SharedString,
  avatar: Option<PathBuf>,
}

fn current_user() -> Option<LockUser> {
  let user = uzers::get_user_by_uid(uzers::get_current_uid())?;
  let name = user.name().to_str()?.to_owned();

  // The GECOS field holds the full name in its first comma-separated part.
  let display_name = user
    .gecos()
    .to_str()
    .and_then(|gecos| gecos.split(',').next())
    .map(str::trim)
    .filter(|full_name| !full_name.is_empty())
    .unwrap_or(&name)
    .to_owned();

  let initial = display_name
    .chars()
    .next()
    .map(|first| first.to_uppercase().to_string())
    .unwrap_or_default();

  Some(LockUser {
    avatar: find_avatar(&name),
    display_name: display_name.into(),
    initial: initial.into(),
    name,
  })
}

/// Looks for a user picture in the places desktops agree on.
fn find_avatar(username: &str) -> Option<PathBuf> {
  let mut candidates = Vec::new();

  if let Some(home) = dirs::home_dir() {
    candidates.push(home.join(".face"));
    candidates.push(home.join(".face.icon"));
  }

  candidates.push(PathBuf::from("/var/lib/AccountsService/icons").join(username));

  candidates.into_iter().find(|path| path.is_file())
}

/// What the fingerprint reader is up to, so the password field can show it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FingerprintState {
  /// There is no reader to use, or it gave up.
  Off,
  /// Looking the reader up and claiming it.
  Starting,
  /// Armed, with nothing on the sensor.
  Waiting,
  /// A finger is on the sensor and being read.
  Reading,
}

/// Shared state of one lock session, rendered by a [`LockScreen`] per output.
pub struct Lock {
  user: LockUser,
  pam_service: SharedString,
  fingerprint_enabled: bool,
  /// Set while a password attempt is in flight. Further submissions are ignored
  /// until it resolves.
  authenticating: bool,
  /// Why the last attempt failed, if it did.
  error: Option<SharedString>,
  /// Whatever PAM had to say during an attempt.
  hint: Option<SharedString>,
  fingerprint_state: FingerprintState,
  /// The claimed reader, while fingerprint verification is running.
  fingerprint: Option<FingerprintReader>,
  now: DateTime<Local>,
  /// Charge left in percent, on machines that have a battery.
  battery: Option<f64>,
  _clock: Task<()>,
  _battery: Task<()>,
  _auth: Option<Task<()>>,
  /// Never cleared from within the task itself: dropping a task from its own
  /// body would cancel the future that is running.
  _fingerprint: Option<Task<()>>,
  _finger_present: Option<Task<()>>,
}

/// Keeps the screens in step with each other.
pub enum LockEvent {
  /// The password was typed on `source`'s screen, or cleared when there is no
  /// source, and the other screens should follow.
  Password {
    value: SharedString,
    source: Option<EntityId>,
  },
}

impl EventEmitter<LockEvent> for Lock {}

impl Lock {
  fn new(user: LockUser, config: LockConfig, cx: &mut Context<Self>) -> Self {
    let pam_service = pam::resolve_service(&config.pam_service);
    debug!(service = %pam_service, "Authenticating against PAM service");

    let connection = GlobalDbusConnection::system(cx);

    Self {
      user,
      pam_service,
      fingerprint_enabled: config.fingerprint,
      authenticating: false,
      error: None,
      hint: None,
      fingerprint_state: FingerprintState::Off,
      fingerprint: None,
      now: Local::now(),
      battery: None,
      _clock: cx.spawn(async move |this, cx| tick_clock(this, cx).await),
      _battery: cx.spawn(async move |this, cx| watch_battery(connection, this, cx).await),
      _auth: None,
      _fingerprint: None,
      _finger_present: None,
    }
  }

  /// Starts what only makes sense once the lock surfaces are actually up.
  fn start(&mut self, cx: &mut Context<Self>) {
    if !self.fingerprint_enabled {
      debug!("Fingerprint unlock is disabled");
      return;
    }

    self.start_fingerprint(cx);
  }

  /// Verifies the password, unlocking the session if PAM accepts it.
  fn submit(&mut self, password: String, cx: &mut Context<Self>) {
    if self.authenticating || password.is_empty() {
      return;
    }

    self.authenticating = true;
    self.error = None;
    cx.notify();

    let events = pam::authenticate(&self.pam_service, &self.user.name, password);

    self._auth = Some(cx.spawn(async move |this, cx| {
      while let Ok(event) = events.recv_async().await {
        match this.update(cx, |this, cx| this.apply_auth_event(event, cx)) {
          Ok(false) => {}
          // Either the attempt ended or the lock screen is gone.
          Ok(true) | Err(_) => break,
        }
      }
    }));
  }

  /// Applies one message from the PAM stack, returning whether the attempt is
  /// over.
  fn apply_auth_event(&mut self, event: AuthEvent, cx: &mut Context<Self>) -> bool {
    let finished = match event {
      AuthEvent::Info(message) => {
        self.hint = Some(message);
        false
      }
      AuthEvent::Error(message) => {
        self.error = Some(message);
        false
      }
      AuthEvent::Finished(Ok(())) => {
        info!("Password accepted");
        self.unlock(cx);
        true
      }
      AuthEvent::Finished(Err(message)) => {
        self.authenticating = false;
        self.error = Some(message);
        self.clear_password(cx);
        true
      }
    };

    cx.notify();
    finished
  }

  /// Mirrors the typed password onto the other screens and drops the last
  /// error, so typing again starts from a clean slate.
  fn password_changed(&mut self, value: SharedString, source: EntityId, cx: &mut Context<Self>) {
    if self.error.take().is_some() {
      cx.notify();
    }

    cx.emit(LockEvent::Password {
      value,
      source: Some(source),
    });
  }

  /// Empties the field on every screen. Sourceless, so none of them skips it.
  fn clear_password(&mut self, cx: &mut Context<Self>) {
    cx.emit(LockEvent::Password {
      value: SharedString::default(),
      source: None,
    });
  }

  fn start_fingerprint(&mut self, cx: &mut Context<Self>) {
    let connection = GlobalDbusConnection::system(cx);

    // Claiming a reader takes a moment - fprintd is activated on demand and the
    // device has to be opened - and it may not pan out at all, so nothing is
    // shown until it does.
    self.set_fingerprint(FingerprintState::Starting, cx);

    self._fingerprint = Some(cx.spawn(async move |this, cx| {
      let setup = match connection.await {
        Some(connection) => claim_reader(&connection, cx).await,
        None => {
          warn!("System bus unavailable, fingerprint unlock is off");
          ReaderSetup::Unavailable
        }
      };

      let reader = match setup {
        ReaderSetup::Ready(reader) => reader,
        ReaderSetup::Unavailable => {
          this
            .update(cx, |this, cx| {
              this.set_fingerprint(FingerprintState::Off, cx);
            })
            .log_err();
          return;
        }
        ReaderSetup::Unresponsive => {
          this
            .update(cx, |this, cx| {
              this.fingerprint_failed("The fingerprint reader isn't responding".into(), cx);
            })
            .log_err();
          return;
        }
      };

      let claimed = this.update(cx, |this, cx| {
        this.fingerprint = Some(reader.clone());
        this.watch_finger_present(reader.clone(), cx);
        cx.notify();
      });

      if claimed.is_err() {
        // The lock screen went away between claiming and storing the reader, so
        // nothing else will hand it back.
        reader.release().await.log_err();
        return;
      }

      verify_fingerprints(reader, this, cx).await;
    }));
  }

  fn set_fingerprint(&mut self, state: FingerprintState, cx: &mut Context<Self>) {
    self.fingerprint_state = state;
    cx.notify();
  }

  /// Takes the reader off the screen and says why, in the same place a rejected
  /// password lands.
  fn fingerprint_failed(&mut self, message: SharedString, cx: &mut Context<Self>) {
    self.error = Some(message);
    self.set_fingerprint(FingerprintState::Off, cx);
  }

  /// Follows the reader's own view of whether a finger is on the sensor, so the
  /// indicator spins while one is being read instead of for the whole time the
  /// lock screen waits.
  fn watch_finger_present(&mut self, reader: FingerprintReader, cx: &mut Context<Self>) {
    self._finger_present = Some(cx.spawn(async move |this, cx| {
      let changes = match reader.listen_finger_present().await {
        Ok(changes) => changes,
        Err(error) => {
          warn!(
            ?error,
            "Failed to follow the state of the fingerprint sensor"
          );
          return;
        }
      };

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

  /// Moves only between waiting and reading. A reader that has stopped, or that
  /// hasn't finished starting, shouldn't be brought back to life by a stray
  /// property change.
  fn set_finger_present(&mut self, present: bool, cx: &mut Context<Self>) {
    let state = match (self.fingerprint_state, present) {
      (FingerprintState::Waiting, true) => FingerprintState::Reading,
      (FingerprintState::Reading, false) => FingerprintState::Waiting,
      _ => return,
    };

    self.fingerprint_state = state;
    cx.notify();
  }

  /// Hands the reader back to fprintd. The claim is held for the whole process,
  /// so keeping it would lock out `pam_fprintd` and every other user of the
  /// reader.
  fn release_fingerprint(&mut self, cx: &mut Context<Self>) {
    // Nothing is listening for a finger from here on, so stop asking for one.
    self.set_fingerprint(FingerprintState::Off, cx);

    let Some(reader) = self.fingerprint.take() else {
      return;
    };

    cx.background_spawn(async move {
      // A verification may still be running, and fprintd wants it stopped
      // before the release. It ending on its own first is just as fine.
      if let Err(error) = reader.stop_verification().await {
        debug!(?error, "No fingerprint verification to stop");
      }

      reader.release().await.log_err();
    })
    .detach();
  }

  fn unlock(&mut self, cx: &mut Context<Self>) {
    self.release_fingerprint(cx);
    unlock_session(cx);
  }
}

/// Keeps [`Lock::now`] current, repainting only when the displayed minute
/// actually changes.
async fn tick_clock(lock: WeakEntity<Lock>, cx: &mut AsyncApp) {
  loop {
    cx.background_executor().timer(CLOCK_TICK).await;

    let updated = lock.update(cx, |lock, cx| {
      let now = Local::now();
      if now.minute() == lock.now.minute() && now.hour() == lock.now.hour() {
        return;
      }

      lock.now = now;
      cx.notify();
    });

    if updated.is_err() {
      break;
    }
  }
}

/// Keeps [`Lock::battery`] current. Machines without a battery never report one,
/// and the clock then shows the time alone.
async fn watch_battery(
  connection: Shared<Task<Option<zbus::Connection>>>,
  lock: WeakEntity<Lock>,
  cx: &mut AsyncApp,
) {
  let Some(connection) = connection.await else {
    warn!("System bus unavailable, the lock screen won't show the battery");
    return;
  };

  let battery = match Battery::find(&connection).await {
    Ok(Some(battery)) => battery,
    Ok(None) => return,
    Err(error) => {
      warn!(?error, "Failed to look up the battery");
      return;
    }
  };

  let changes = match battery.listen().await {
    Ok(changes) => changes,
    Err(error) => {
      warn!(?error, "Failed to follow the battery charge");
      return;
    }
  };

  // Subscribed before the first read, so a change that lands in between is not
  // lost.
  let initial = stream::iter(battery.percentage().await.log_err());
  let percentages = initial.chain(changes);
  futures::pin_mut!(percentages);

  while let Some(percentage) = percentages.next().await {
    let updated = lock.update(cx, |lock, cx| {
      lock.battery = Some(percentage);
      cx.notify();
    });

    if updated.is_err() {
      break;
    }
  }
}

/// How setting up the fingerprint reader turned out.
enum ReaderSetup {
  Ready(FingerprintReader),
  /// No fprintd, no reader or no enrolled prints. Expected on plenty of
  /// machines, and nothing the lock screen should mention.
  Unavailable,
  /// fprintd stopped answering. Worth saying out loud, because the alternative
  /// is the user pressing a finger against a reader that will never reply.
  Unresponsive,
}

/// Finds the default reader and claims it.
async fn claim_reader(connection: &zbus::Connection, cx: &mut AsyncApp) -> ReaderSetup {
  let reader = match with_timeout(cx, FingerprintReader::find(connection)).await {
    Some(Ok(Some(reader))) => reader,
    Some(Ok(None)) => return ReaderSetup::Unavailable,
    Some(Err(error)) => {
      warn!(?error, "Failed to look up the fingerprint reader");
      return ReaderSetup::Unavailable;
    }
    None => {
      warn!("Timed out looking up the fingerprint reader");
      return ReaderSetup::Unresponsive;
    }
  };

  match with_timeout(cx, reader.claim()).await {
    Some(Ok(())) => {}
    Some(Err(error)) => {
      warn!(?error, reader = %reader.name, "Failed to claim the fingerprint reader");
      return ReaderSetup::Unavailable;
    }
    None => {
      // The call is still outstanding. Should it land after we gave up, the
      // reader would stay claimed for the rest of this process's life, so undo
      // it in the background.
      warn!(reader = %reader.name, "Timed out claiming the fingerprint reader");
      cx.background_spawn(async move {
        if let Err(error) = reader.release().await {
          debug!(?error, "Nothing to release after the claim timed out");
        }
      })
      .detach();
      return ReaderSetup::Unresponsive;
    }
  }

  debug!(reader = %reader.name, "Claimed fingerprint reader");
  ReaderSetup::Ready(reader)
}

/// Runs `future` with a [`FINGERPRINT_SETUP_TIMEOUT`] deadline. `None` means it
/// didn't finish in time; the future is dropped, but whatever call it had in
/// flight carries on at the other end.
async fn with_timeout<T>(cx: &AsyncApp, future: impl Future<Output = T>) -> Option<T> {
  let timer = cx.background_executor().timer(FINGERPRINT_SETUP_TIMEOUT);

  select_biased! {
    output = future.fuse() => Some(output),
    _ = timer.fuse() => None,
  }
}

/// How one verification attempt ended.
enum Attempt {
  /// A finger matched an enrolled print.
  Matched,
  /// The attempt ended without a match; another one can be started.
  Rejected,
  /// The reader can't be used any more.
  Failed,
  /// The lock screen went away mid-attempt.
  Gone,
}

/// Verifies fingers until one matches, the reader gives up, or too many attempts
/// fail. Password entry stays usable the whole time.
async fn verify_fingerprints(reader: FingerprintReader, lock: WeakEntity<Lock>, cx: &mut AsyncApp) {
  let mut failures = 0;

  loop {
    let attempt = verify_once(&reader, &lock, cx).await;
    reader.stop_verification().await.log_err();

    match attempt {
      Attempt::Matched => {
        info!("Fingerprint accepted");
        lock.update(cx, |lock, cx| lock.unlock(cx)).log_err();
        return;
      }
      Attempt::Gone => {
        // Nothing left to hand the reader back for us.
        reader.release().await.log_err();
        return;
      }
      Attempt::Failed => break,
      Attempt::Rejected => {
        failures += 1;
        if failures >= MAX_FINGERPRINT_FAILURES {
          debug!(failures, "Giving up on fingerprint verification");
          lock
            .update(cx, |lock, cx| {
              lock.fingerprint_failed("Too many attempts, use your password".into(), cx);
            })
            .log_err();
          break;
        }
      }
    }
  }

  lock
    .update(cx, |lock, cx| lock.release_fingerprint(cx))
    .log_err();
}

/// Runs a single verification attempt, showing the reader's hints as they
/// arrive. The reader still needs to be stopped afterwards, whatever the
/// outcome.
async fn verify_once(
  reader: &FingerprintReader,
  lock: &WeakEntity<Lock>,
  cx: &mut AsyncApp,
) -> Attempt {
  let updates = match with_timeout(cx, reader.start_verification()).await {
    Some(Ok(updates)) => updates,
    Some(Err(error)) => {
      warn!(?error, "Failed to start fingerprint verification");
      return Attempt::Failed;
    }
    None => {
      warn!("Timed out starting fingerprint verification");
      lock
        .update(cx, |lock, cx| {
          lock.fingerprint_failed("The fingerprint reader isn't responding".into(), cx);
        })
        .log_err();
      return Attempt::Failed;
    }
  };

  // Only now is the reader actually listening, so this is where asking for a
  // finger becomes honest - for the first attempt as well as every retry.
  let armed = lock.update(cx, |lock, cx| {
    lock.set_fingerprint(FingerprintState::Waiting, cx);
  });

  if armed.is_err() {
    return Attempt::Gone;
  }

  futures::pin_mut!(updates);

  while let Some(update) = updates.next().await {
    let mut outcome = None;

    let applied = lock.update(cx, |lock, cx| {
      match update.status {
        VerifyStatus::Match => outcome = Some(Attempt::Matched),
        VerifyStatus::NoMatch => {
          lock.error = Some("Fingerprint not recognized".into());
          outcome = Some(Attempt::Rejected);
        }
        VerifyStatus::Retry(hint) => lock.error = Some(hint),
        VerifyStatus::Failed(message) => {
          lock.fingerprint_failed(message, cx);
          outcome = Some(Attempt::Failed);
        }
      }

      cx.notify();
    });

    if applied.is_err() {
      return Attempt::Gone;
    }

    if let Some(outcome) = outcome {
      return outcome;
    }

    // A retry hint that ends the attempt anyway leaves the reader idle, so it
    // has to be restarted like any other rejection.
    if update.done {
      return Attempt::Rejected;
    }
  }

  Attempt::Failed
}

/// The lock surface of a single output.
pub struct LockScreen {
  lock: Entity<Lock>,
  password: Entity<InputState>,
  /// Whether this is the screen the clock goes on. Decided when the surface is
  /// created: the lock surfaces are made in one go, and outputs attached later
  /// get none.
  clock: bool,
  _subscriptions: Vec<Subscription>,
}

impl LockScreen {
  fn new(lock: Entity<Lock>, clock: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
    let password = cx.new(|cx| {
      InputState::new(window, cx)
        .masked(true)
        .placeholder("Password")
        .clean_on_escape()
    });

    window.focus(&password.focus_handle(cx), cx);

    let subscriptions = vec![
      cx.subscribe_in(
        &password,
        window,
        |this, _password, event: &InputEvent, _window, cx| match event {
          InputEvent::PressEnter { .. } => this.submit(cx),
          InputEvent::Change => this.mirror_password(cx),
          InputEvent::Focus | InputEvent::Blur => {}
        },
      ),
      cx.subscribe_in(
        &lock,
        window,
        |this, _lock, event: &LockEvent, window, cx| {
          this.apply_lock_event(event, window, cx);
        },
      ),
      cx.observe(&lock, |_this, _lock, cx| cx.notify()),
    ];

    Self {
      lock,
      password,
      clock,
      _subscriptions: subscriptions,
    }
  }

  fn submit(&mut self, cx: &mut Context<Self>) {
    let password = self.password.read(cx).value().to_string();
    self.lock.update(cx, |lock, cx| lock.submit(password, cx));
  }

  fn mirror_password(&mut self, cx: &mut Context<Self>) {
    let value = self.password.read(cx).value();
    let source = cx.entity_id();
    self
      .lock
      .update(cx, |lock, cx| lock.password_changed(value, source, cx));
  }

  fn apply_lock_event(&mut self, event: &LockEvent, window: &mut Window, cx: &mut Context<Self>) {
    let LockEvent::Password { value, source } = event;

    if *source == Some(cx.entity_id()) {
      return;
    }

    // Comparing first also stops the mirroring from bouncing back and forth:
    // setting the value emits another change on this screen.
    if self.password.read(cx).value() == *value {
      return;
    }

    self.password.update(cx, |password, cx| {
      password.set_value(value.clone(), window, cx);
    });
  }
}

impl Render for LockScreen {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let lock = self.lock.read(cx);
    let now = lock.now;
    let display_name = lock.user.display_name.clone();
    let username: SharedString = lock.user.name.clone().into();
    let initial = lock.user.initial.clone();
    let avatar = lock.user.avatar.clone();
    let authenticating = lock.authenticating;
    let fingerprint = lock.fingerprint_state;
    let battery = lock.battery;
    let hint = lock.hint.clone();
    let error = lock.error.clone();

    div()
      .size_full()
      .relative()
      .flex()
      .flex_col()
      .items_center()
      .justify_center()
      .bg(rgb(0x0D0D0D))
      .text_color(rgb(0xFFFFFF))
      .on_mouse_down(
        MouseButton::Left,
        cx.listener(|this, _event, window, cx| {
          // Clicking anywhere puts the keyboard back on the password field.
          window.focus(&this.password.focus_handle(cx), cx);
        }),
      )
      .child(
        v_flex()
          .items_center()
          .gap_4()
          .w(px(360.))
          .child(render_avatar(avatar, initial))
          .child(
            v_flex()
              .items_center()
              .gap_1()
              .child(
                div()
                  .text_size(px(17.))
                  .font_weight(FontWeight::MEDIUM)
                  .child(display_name.clone()),
              )
              .when(display_name != username, |this| {
                this.child(div().text_sm().text_color(rgba(0xFFFFFFAA)).child(username))
              }),
          )
          .child(self.render_password(authenticating, fingerprint))
          .when_some(hint, |this, hint| {
            this.child(hint_row(
              Icon::new(IconName::Asterisk)
                .size(rems(0.95))
                .text_color(rgba(0xFFFFFF88))
                .into_any_element(),
              hint,
            ))
          })
          .when_some(error, |this, error| {
            this.child(div().text_sm().text_color(rgb(0xE07070)).child(error))
          }),
      )
      .when(self.clock, |this| this.child(render_clock(now, battery)))
  }
}

/// Which display the clock goes on, as an index into [`App::displays`]. It
/// belongs on the primary display, the same one the desktop clock uses; `None`
/// puts it on every screen, which is what happens when no primary display is
/// configured or the configured one isn't attached - a clock nobody asked to
/// hide beats no clock at all.
fn clock_display(cx: &App) -> Option<usize> {
  let primary = ConfigState::get(cx).primary_display?;

  let index = cx
    .displays()
    .iter()
    .position(|display| display.name() == Some(primary.as_str()));

  if index.is_none() {
    warn!(
      display = %primary,
      "Configured primary display not found, showing the clock on every screen"
    );
  }

  index
}

impl LockScreen {
  /// The password field stays mounted while authenticating, so it keeps keyboard
  /// focus and holds on to what was typed; only the leading icon turns into a
  /// spinner.
  ///
  /// Typing is off while either check is mid-flight, since a rejected attempt
  /// empties the field and would take anything typed since with it.
  fn render_password(
    &self,
    authenticating: bool,
    fingerprint: FingerprintState,
  ) -> impl IntoElement {
    let leading = if authenticating {
      Spinner::new()
        .color(rgb(0x888888).into())
        .into_any_element()
    } else {
      Icon::new(IconName::Lock)
        .size(rems(0.95))
        .text_color(rgba(0xFFFFFF66))
        .into_any_element()
    };

    let reading = fingerprint == FingerprintState::Reading;

    h_flex()
      .w_full()
      .gap_2()
      .px_3()
      .py_2()
      .rounded_lg()
      .bg(rgba(0xFFFFFF0F))
      .border_1()
      .border_color(rgba(0xFFFFFF1F))
      .child(leading)
      .child(
        input(&self.password)
          .flex_grow()
          .disabled(authenticating || reading),
      )
      .when_some(render_fingerprint(fingerprint), |this, indicator| {
        this.child(indicator)
      })
  }
}

/// The desktop clock, as it looks when the `status` overlay draws it - the lock
/// surfaces cover that up, so this stands in for it.
fn render_clock(now: DateTime<Local>, battery: Option<f64>) -> impl IntoElement {
  let time = now.format(TIME_FORMAT).to_string();
  let date = now.format(DATE_FORMAT).to_string().to_uppercase();

  v_flex()
    .absolute()
    .bottom(CLOCK_MARGIN_BOTTOM)
    .right(CLOCK_MARGIN_RIGHT)
    .items_end()
    .font_family("Noto Sans")
    .opacity(CLOCK_OPACITY)
    .when_some(battery, |this, percentage| {
      this.child(
        div()
          .text_size(rems(2.))
          .line_height(rems(1.6))
          .font_weight(FontWeight::SEMIBOLD)
          .child(format!("{percentage:.0}")),
      )
    })
    .child(
      h_flex()
        .items_end()
        .gap_2()
        .child(
          div()
            .text_size(rems(1.4))
            .line_height(rems(1.95))
            .font_weight(FontWeight::SEMIBOLD)
            .child(date),
        )
        .child(
          div()
            .text_size(rems(3.5))
            .line_height(rems(3.5))
            .font_weight(FontWeight::BOLD)
            .child(time),
        ),
    )
}

fn render_avatar(avatar: Option<PathBuf>, initial: SharedString) -> AnyElement {
  let frame = div()
    .size(px(72.))
    .flex_none()
    .rounded_full()
    .overflow_hidden()
    .bg(rgba(0xFFFFFF14))
    .flex()
    .items_center()
    .justify_center();

  match avatar {
    Some(path) => frame
      .child(
        img(ImageSource::Resource(Resource::Path(path.into())))
          .size_full()
          .object_fit(ObjectFit::Cover),
      )
      .into_any_element(),
    None => frame
      .child(
        div()
          .text_size(px(28.))
          .text_color(rgba(0xFFFFFFCC))
          .child(initial),
      )
      .into_any_element(),
  }
}

/// Shows a reader that is armed, spinning while a finger is actually on the
/// sensor so an idle lock screen doesn't animate for hours. A reader that is
/// still starting up, or that isn't there at all, shows nothing rather than
/// offering a way in that may never work.
fn render_fingerprint(state: FingerprintState) -> Option<AnyElement> {
  match state {
    FingerprintState::Off | FingerprintState::Starting => None,
    FingerprintState::Waiting => Some(
      Icon::new(IconName::Fingerprint)
        .size(rems(0.95))
        .text_color(rgba(0xFFFFFF66))
        .into_any_element(),
    ),
    FingerprintState::Reading => Some(
      Spinner::new()
        .color(rgba(0xFFFFFFAA).into())
        .into_any_element(),
    ),
  }
}

fn hint_row(leading: AnyElement, hint: SharedString) -> impl IntoElement {
  h_flex()
    .w_full()
    .gap_2()
    .items_center()
    .text_sm()
    .text_color(rgba(0xFFFFFFAA))
    .child(leading)
    .child(div().flex_1().child(hint))
}
