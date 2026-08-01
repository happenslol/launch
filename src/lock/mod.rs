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
//! # A daemon that is hung rather than dead still holds the lock. Matched on the
//! # path because an installed build runs as `.launch-wrapped`, not `launch`.
//! pkill -f 'bin/[.]?launch'
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

use futures::{FutureExt as _, StreamExt as _, select_biased};
use gpui::{
  AnyElement, App, AsyncApp, Context, Div, Entity, EntityId, EventEmitter, Focusable, FontWeight,
  Global, ImageSource, IntoElement, MouseButton, ObjectFit, Pixels, Rems, Render, Resource,
  SharedString, Styled, Subscription, Task, WeakEntity, Window, div, img, prelude::*, px, rems,
  rgb, rgba,
};
use tracing::{debug, error, info, warn};
use uzers::os::unix::UserExt as _;

use crate::config::{ConfigState, LockConfig, config_dir};
use crate::dbus::GlobalDbusConnection;
use crate::dbus::fprintd::{FingerprintReader, VerifyStatus};
use crate::dbus::logind::{Logind, Session, SessionRequest};
use crate::icon::{Icon, IconName, Spinner};
use crate::input::input;
use crate::input::state::{InputEvent, InputState};
use crate::launcher::Launcher;
use crate::lock::pam::{AuthEvent, AuthFailure};
use crate::status::{self, Clock};
use crate::util::{ResultExt, h_flex, v_flex};

/// Consecutive unrecognized fingers after which verification stops and the user
/// is pointed at their password, mirroring what `pam_fprintd` does.
const MAX_FINGERPRINT_FAILURES: u32 = 5;

/// How long fprintd gets to answer while the reader is being set up. It talks to
/// the hardware over USB during those calls, and a reader in a bad state - one
/// that is enumerated but won't open, say - can leave them outstanding
/// indefinitely. The lock screen falls back to password-only rather than
/// promising a finger prompt that will never arrive.
const FINGERPRINT_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

const AVATAR_SIZE: Pixels = px(104.);

/// The password field and the icons that flank it. The icons are sized in rems
/// so they track the text.
const FIELD_TEXT_SIZE: Pixels = px(20.);
const FIELD_ICON_SIZE: Rems = rems(1.4);

/// How far the password field fades while it takes no input.
const FIELD_DISABLED_OPACITY: f32 = 0.6;

/// The colour a turned-down attempt is reported in, as the message and as the
/// outline of the field it came from.
const ERROR_COLOR: u32 = 0xE07070;

/// How long the field stays outlined after an attempt is turned down. Long
/// enough to be noticed, short enough to be gone by the time the next one is
/// typed.
const REJECTION_HIGHLIGHT: Duration = Duration::from_secs(3);

/// Why the screen is holding sleep up, as `systemd-inhibit --list` shows it.
const INHIBIT_REASON: &str = "Lock the screen before sleeping";

/// How long the suspend is held after locking, for the request to get out to the
/// compositor. logind allows `InhibitDelayMaxSec` for this, 5 seconds by default.
const SLEEP_LOCK_GRACE: Duration = Duration::from_millis(250);

/// What a profile picture in the config directory can be called. The first one
/// that exists wins, so a user with several only sees one of them.
const AVATAR_NAMES: &[&str] = &["profile.png", "profile.jpg", "profile.webp"];

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
  watch_sleep(cx);
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
  let lock = cx.new(|_cx| Lock::new(user, config));

  close_launcher_windows(cx);

  // `lock_session` creates one surface per display, walking the same list
  // `clock_display` just read, in the same order. Nothing can reorder or resize
  // it in between - both run on this thread without yielding - so counting the
  // callbacks is what tells a surface which display it belongs to. The window
  // itself can't say: it learns its output from a Wayland event that hasn't
  // arrived yet while it is being built.
  let clock_display = clock_display(cx);
  let clock_enabled = ConfigState::get(cx).status.enabled;
  let screen = Cell::new(0usize);

  let result = cx.lock_session({
    let lock = lock.clone();
    move |window, cx| {
      let index = screen.replace(screen.get() + 1);
      let clock = clock_enabled && clock_display.is_none_or(|display| display == index);
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

/// Locks the screen before the system suspends, and puts the fingerprint reader
/// back together after it wakes.
///
/// A sleep inhibitor is what buys the time to lock: logind holds the suspend
/// until every delay lock is released, so the lock surfaces are up before the
/// screen comes back on. This is the same thing `swayidle before-sleep` does,
/// and it replaces the systemd unit ordered before `sleep.target` - which has no
/// way of knowing whether the locker got anywhere before the machine went down.
fn watch_sleep(cx: &mut App) {
  let connection = GlobalDbusConnection::system(cx);

  cx.spawn(async move |cx| {
    let Some(connection) = connection.await else {
      warn!("System bus unavailable, the screen won't lock before sleeping");
      return;
    };

    let transitions = match Logind::listen_sleep(&connection).await {
      Ok(transitions) => transitions,
      Err(error) => {
        error!(?error, "Failed to subscribe to sleep transitions");
        return;
      }
    };

    futures::pin_mut!(transitions);

    let mut inhibitor = Logind::inhibit_sleep(&connection, INHIBIT_REASON)
      .await
      .log_err();

    while let Some(sleeping) = transitions.next().await {
      if sleeping {
        debug!("System is going to sleep, locking");
        cx.update(lock);

        // Locking only queues the request; letting go of the inhibitor right
        // here would race the suspend against it reaching the compositor. This
        // is a fraction of what logind waits for.
        cx.background_executor().timer(SLEEP_LOCK_GRACE).await;
        drop(inhibitor.take());
        continue;
      }

      debug!("System woke up");
      inhibitor = Logind::inhibit_sleep(&connection, INHIBIT_REASON)
        .await
        .log_err();

      cx.update(restart_fingerprint);
    }
  })
  .detach();
}

/// Gives the fingerprint reader another go after a suspend. fprintd puts its
/// readers to sleep along with the machine, which ends whatever verification was
/// running and leaves the claim we hold pointing at a device that may not even
/// come back under the same name.
fn restart_fingerprint(cx: &mut App) {
  let Some(lock) = cx.global::<GlobalLock>().0.clone() else {
    return;
  };

  lock.update(cx, |lock, cx| lock.restart_fingerprint(cx));
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

/// Looks for a user picture: one dropped into the config directory first, then
/// the places desktops agree on.
fn find_avatar(username: &str) -> Option<PathBuf> {
  let mut candidates = Vec::new();

  if let Some(directory) = config_dir() {
    candidates.extend(AVATAR_NAMES.iter().map(|name| directory.join(name)));
  }

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
  /// Whether an attempt was turned down just now, which the field is outlined
  /// for. Outlives neither [`REJECTION_HIGHLIGHT`] nor the next attempt.
  rejected: bool,
  /// Whatever PAM had to say during an attempt.
  hint: Option<SharedString>,
  fingerprint_state: FingerprintState,
  /// The claimed reader, while fingerprint verification is running.
  fingerprint: Option<FingerprintReader>,
  _auth: Option<Task<()>>,
  _rejection: Option<Task<()>>,
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
  fn new(user: LockUser, config: LockConfig) -> Self {
    let pam_service = pam::resolve_service(&config.pam_service);
    debug!(service = %pam_service, "Authenticating against PAM service");

    Self {
      user,
      pam_service,
      fingerprint_enabled: config.fingerprint,
      authenticating: false,
      error: None,
      rejected: false,
      hint: None,
      fingerprint_state: FingerprintState::Off,
      fingerprint: None,
      _auth: None,
      _rejection: None,
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
      AuthEvent::Finished(Err(failure)) => {
        self.authenticating = false;

        // A wrong password is answered by the outline alone; anything else is
        // rare enough, and odd enough, to be worth spelling out.
        if let AuthFailure::Error(message) = failure {
          self.error = Some(message);
        }

        self.reject(cx);
        self.clear_password(cx);
        true
      }
    };

    cx.notify();
    finished
  }

  /// Turns an attempt down by outlining the field for as long as
  /// [`REJECTION_HIGHLIGHT`]. That is the whole of what a rejection says: the
  /// user knows what they just tried. A second one restarts the outline, so it
  /// always tracks the most recent attempt.
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

  /// Mirrors the typed password onto the other screens and drops the last
  /// error, so typing again starts from a clean slate.
  fn password_changed(&mut self, value: SharedString, source: EntityId, cx: &mut Context<Self>) {
    // Emptying the field is what a rejection does to it, and the message saying
    // why has to outlive that. Only something actually typed clears it.
    if !value.is_empty() && self.error.take().is_some() {
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

  /// Starts over with the fingerprint reader, for when the one we were using is
  /// no longer good - after a suspend, say. Whatever is held is handed back
  /// first, in the same task, so the release can't land on top of the new claim.
  fn restart_fingerprint(&mut self, cx: &mut Context<Self>) {
    if !self.fingerprint_enabled {
      return;
    }

    // Nothing that was said about the old reader still applies.
    self.error = None;

    let previous = self.fingerprint.take();
    self.start_fingerprint_after(previous, cx);
  }

  fn start_fingerprint(&mut self, cx: &mut Context<Self>) {
    self.start_fingerprint_after(None, cx);
  }

  fn start_fingerprint_after(
    &mut self,
    previous: Option<FingerprintReader>,
    cx: &mut Context<Self>,
  ) {
    let connection = GlobalDbusConnection::system(cx);

    // Claiming a reader takes a moment - fprintd is activated on demand and the
    // device has to be opened - and it may not pan out at all, so nothing is
    // shown until it does.
    self.set_fingerprint(FingerprintState::Starting, cx);

    self._fingerprint = Some(cx.spawn(async move |this, cx| {
      if let Some(previous) = previous {
        // Hand the old one back before asking for a device again: fprintd
        // refuses a claim while one is outstanding, even one that no longer
        // works.
        if let Err(error) = previous.stop_verification().await {
          debug!(?error, "No fingerprint verification left to stop");
        }

        if let Err(error) = previous.release().await {
          debug!(?error, "Nothing left to release of the old reader");
        }
      }

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
          lock.reject(cx);
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

    let mut subscriptions = vec![
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

    // Only the screen that draws the clock repaints when it ticks.
    if clock {
      let clock_state = Clock::global(cx);
      subscriptions.push(cx.observe(&clock_state, |_this, _clock, cx| cx.notify()));
    }

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
    let display_name = lock.user.display_name.clone();
    let username: SharedString = lock.user.name.clone().into();
    let initial = lock.user.initial.clone();
    let avatar = lock.user.avatar.clone();
    let authenticating = lock.authenticating;
    let fingerprint = lock.fingerprint_state;
    let hint = lock.hint.clone();
    let error = lock.error.clone();
    let rejected = lock.rejected;

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
                  .text_size(px(22.))
                  .font_weight(FontWeight::BOLD)
                  .child(display_name.clone()),
              )
              .when(display_name != username, |this| {
                this.child(div().text_sm().text_color(rgba(0xFFFFFFAA)).child(username))
              }),
          )
          // The field sits a little further from the name than the rest of the
          // column is spaced, so the two don't read as one block.
          .child(
            self
              .render_password(authenticating, fingerprint, rejected)
              .mt_2(),
          )
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
            this.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error))
          }),
      )
      .when(self.clock, |this| this.child(self.render_clock(cx)))
  }
}

/// Which display the clock goes on, as an index into [`App::displays`]. It
/// belongs on the screen the overlay it stands in for uses. `None` puts it on
/// every screen, which is what happens when no display is configured or the
/// configured one isn't attached - a clock nobody asked to hide beats no clock
/// at all.
fn clock_display(cx: &App) -> Option<usize> {
  let config = ConfigState::get(cx);
  let configured = config.status.display.or(config.primary_display)?;

  let index = cx
    .displays()
    .iter()
    .position(|display| display.name() == Some(configured.as_str()));

  if index.is_none() {
    warn!(
      display = %configured,
      "Configured clock display not found, showing the clock on every screen"
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
    rejected: bool,
  ) -> Div {
    let leading = if authenticating {
      Spinner::new()
        .size(FIELD_ICON_SIZE)
        .color(rgb(0x888888).into())
        .into_any_element()
    } else {
      Icon::new(IconName::Lock)
        .size(FIELD_ICON_SIZE)
        .text_color(rgba(0xFFFFFF66))
        .into_any_element()
    };

    let disabled = authenticating || fingerprint == FingerprintState::Reading;

    h_flex()
      .w_full()
      .gap_3()
      .px_4()
      .py_3()
      .text_size(FIELD_TEXT_SIZE)
      .rounded_lg()
      .bg(rgba(0xFFFFFF0F))
      .border_1()
      .map(|this| match rejected {
        true => this.border_color(rgb(ERROR_COLOR)),
        false => this.border_color(rgba(0xFFFFFF1F)),
      })
      .when(disabled, |this| this.opacity(FIELD_DISABLED_OPACITY))
      .child(leading)
      .child(input(&self.password).flex_grow().disabled(disabled))
      .when_some(render_fingerprint(fingerprint), |this, indicator| {
        this.child(indicator)
      })
  }

  /// The desktop clock, which the lock surfaces cover up, drawn in the same
  /// corner it would be in if they didn't.
  fn render_clock(&self, cx: &App) -> Div {
    let clock = Clock::global(cx);
    let clock = clock.read(cx);

    status::render_clock_in_corner(
      clock.now,
      clock.battery,
      ConfigState::get(cx).status.opacity,
    )
  }
}

fn render_avatar(avatar: Option<PathBuf>, initial: SharedString) -> AnyElement {
  let frame = div()
    .size(AVATAR_SIZE)
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
        // The frame's `overflow_hidden` doesn't round what the image paints;
        // the radius has to be on the image itself.
        img(ImageSource::Resource(Resource::Path(path.into())))
          .size_full()
          .rounded_full()
          .object_fit(ObjectFit::Cover),
      )
      .into_any_element(),
    None => frame
      .child(
        div()
          .text_size(px(40.))
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
        .size(FIELD_ICON_SIZE)
        .text_color(rgba(0xFFFFFF66))
        .into_any_element(),
    ),
    FingerprintState::Reading => Some(
      Spinner::new()
        .size(FIELD_ICON_SIZE)
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
