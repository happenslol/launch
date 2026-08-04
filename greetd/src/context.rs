//! The daemon's state: which session is running, which authentication attempts
//! are in flight, and what happens when any of them ends.
//!
//! One mutex guards the whole thing. greetd uses a read-write lock with
//! `drop(guard)` / re-acquire dances between the steps of a transition, and its
//! subtlest bugs live in exactly those windows. Serializing every transition
//! costs microseconds and removes the class.
//!
//! The invariant that makes that safe: no method which takes the lock may be
//! called from inside a locked region.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use greet_ipc::{AuthFailure, AuthSource, Event, IpcUser, PROTOCOL_VERSION, Secret};
use nix::unistd::Pid;
use tokio::sync::{Mutex, mpsc};
use tokio::task::{self, JoinHandle};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::session::{REJECTED, UNAVAILABLE};
use crate::worker::{
  AuthMessageType, ParentToWorker, SessionClass, TerminalMode, Worker, WorkerToParent,
};

/// How soon after a session starts its exit counts as a crash rather than a
/// logout. Restarting a greeter into a compositor that dies immediately would
/// otherwise spin.
const CRASH_LOOP_DEBOUNCE: Duration = Duration::from_secs(1);

/// How long a cancelled worker gets to act on `Cancel` before it is killed.
const CANCEL_GRACE: Duration = Duration::from_millis(250);

/// Failed matches before the reader is given up on for this attempt. The same
/// value as the lock screen's, and for the same reason: a reader that cannot
/// read this finger should stop asking rather than compete with the password
/// field forever.
const MAX_FINGERPRINT_FAILURES: u32 = 5;

/// How long the login screen gets to exit on its own after a session has been
/// scheduled, before it is asked and then made to.
///
/// Not configurable, because there is no machine for which a different number is
/// right. It only has to cover a *cooperative* shutdown - gpui teardown, the
/// compositor quitting, `pam_close_session` - which is well under a second even
/// on a slow disk, and the timer is aborted the moment that happens. Erring long
/// is nearly free: the only cost of a generous value is how long somebody stares
/// at a wedged login screen in a case that should never happen. The cost of a
/// value that is too short is real, so this is not it.
const EVICTION_PATIENCE: Duration = Duration::from_secs(5);

/// How long it gets between being asked to leave and being made to.
///
/// Shorter, because by this point it has ignored both `SessionStarted` and a
/// SIGTERM, while the user reads a screen that says it is logging them in.
const EVICTION_GRACE: Duration = Duration::from_secs(2);

/// Where the last successful login is recorded, so the screen opens on the
/// account most likely to be wanted. Advisory: an unreadable or stale value
/// falls through to the configured default (see [`crate::users`]).
pub const LAST_USER_PATH: &str = "/var/lib/launch-greetd/last-user";

/// A session that is running: the worker holding its PAM transaction, and the
/// process the worker forked.
pub struct RunningSession {
  /// The worker. Never signalled directly - it has a `pam_close_session` to
  /// run, and killing it strands the logind session.
  pub worker: Pid,
  /// The session process, which is what gets signalled to end the session.
  pub session: Pid,
  pub started: Instant,
  pub is_greeter: bool,
}

impl RunningSession {
  /// Asks the session to end, leaving the worker alone so PAM teardown still
  /// happens. greetd's equivalent kills both, which is why greeter sessions
  /// leak there.
  pub fn terminate(&self) {
    self.signal(nix::sys::signal::Signal::SIGTERM);
  }

  /// Ends a session that would not go when asked.
  ///
  /// Still only the session process: the worker survives either way, because it
  /// owes a `pam_close_session` and killing it strands the logind session
  /// whether or not the thing it was hosting was cooperative.
  fn kill(&self) {
    self.signal(nix::sys::signal::Signal::SIGKILL);
  }

  fn signal(&self, signal: nix::sys::signal::Signal) {
    if let Err(error) = nix::sys::signal::kill(self.session, signal) {
      warn!(?error, ?signal, pid = %self.session, "Session was already gone");
    }
  }
}

/// An authenticated worker waiting for the greeter to get out of the way.
///
/// It is parked in `recv` having already said it is ready, so the session does
/// not start until it is sent `Start`. That ordering is what keeps the greeter
/// and the session from fighting over the same VT.
struct PendingSession {
  worker: Rc<Worker>,
  /// When it was scheduled, which is what the eviction timer measures from.
  ///
  /// greetd measures from when the *attempt* was created instead, so any login
  /// where the user took more than ten seconds to type went straight to
  /// SIGKILL without ever being asked politely.
  scheduled_at: Instant,
}

/// How far along one worker's PAM conversation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotState {
  /// Inside PAM, with nothing owed to it.
  Running,
  /// It asked something and is blocked until it gets an answer.
  AwaitingResponse,
  /// Authenticated and authorized, parked waiting for `Start`.
  Ready,
}

struct Slot {
  worker: Rc<Worker>,
  state: SlotState,
  reader: JoinHandle<()>,
}

impl Slot {
  /// Ends a worker that has not opened a session.
  ///
  /// Safe precisely because it hasn't: a worker still in its PAM conversation
  /// owes no teardown. The same must never be done to one past `open_session` -
  /// it has a `pam_close_session` to run, and skipping that leaks a logind
  /// session and strands the user's runtime directory.
  ///
  /// The kill happens on a detached task so cancelling a loser never blocks the
  /// winner's session from being scheduled. Nothing waits on it: the worker's
  /// exit arrives as SIGCHLD and is reaped like any other.
  fn cancel(self) {
    self.reader.abort();
    Self::end(self.worker);
  }

  /// Cancels a slot from inside its own reader task.
  ///
  /// Aborting the reader here would mean aborting the caller. Dropping the
  /// handle instead just detaches it, and the reader is returning anyway.
  fn cancel_from_reader(self) {
    Self::end(self.worker);
  }

  fn end(worker: Rc<Worker>) {
    task::spawn_local(async move { worker.cancel_configuring(CANCEL_GRACE).await });
  }

  /// Ends a worker now, for shutdown, when there is no runtime left to run a
  /// detached task on.
  fn kill(self) {
    self.reader.abort();
    self.worker.kill_configuring();
  }
}

/// The authentication attempt for one user.
struct Attempt {
  username: String,
  password: Option<Slot>,
  /// The second worker, running a stack whose auth half is `pam_fprintd` and
  /// nothing else. Absent when the configuration disables it or the fork failed;
  /// a reader that turns out to be unusable is discovered by the worker itself.
  fingerprint: Option<Slot>,
  /// Which worker got there first. Set once and never overwritten, so two
  /// workers reporting success cannot both win.
  winner: Option<AuthSource>,
  /// Failed matches so far, counted across the retries `pam_fprintd` does
  /// within one `pam_authenticate` call.
  fingerprint_failures: u32,
}

impl Attempt {
  fn slot(&mut self, source: AuthSource) -> Option<&mut Slot> {
    match source {
      AuthSource::Password => self.password.as_mut(),
      AuthSource::Fingerprint => self.fingerprint.as_mut(),
    }
  }

  fn take(&mut self, source: AuthSource) -> Option<Slot> {
    match source {
      AuthSource::Password => self.password.take(),
      AuthSource::Fingerprint => self.fingerprint.take(),
    }
  }

  fn is_empty(&self) -> bool {
    self.password.is_none() && self.fingerprint.is_none()
  }

  fn cancel(self) {
    for slot in [self.password, self.fingerprint].into_iter().flatten() {
      slot.cancel();
    }
  }

  fn kill(self) {
    for slot in [self.password, self.fingerprint].into_iter().flatten() {
      slot.kill();
    }
  }
}

struct ContextInner {
  /// The greeter, or the user session that replaced it.
  current: Option<RunningSession>,
  /// Authenticated and waiting for the greeter to exit.
  scheduled: Option<PendingSession>,
  /// Makes the greeter leave if it will not go on its own. Aborted the moment
  /// the session it was waiting for is promoted.
  eviction: Option<JoinHandle<()>>,
  attempt: Option<Attempt>,
  /// Where pushed events go. Only one greeter may be connected at a time.
  client: Option<mpsc::UnboundedSender<Event>>,
}

pub struct Context {
  config: Config,
  terminal: TerminalMode,
  listener_path: String,
  /// Resolved once at startup: the greeter cannot read other users' homes, so
  /// it is told rather than looking for itself.
  users: Vec<IpcUser>,
  default_user: String,
  inner: Mutex<ContextInner>,
}

impl Context {
  pub fn new(
    config: Config,
    terminal: TerminalMode,
    listener_path: String,
    users: Vec<IpcUser>,
    default_user: String,
  ) -> Rc<Self> {
    Rc::new(Self {
      config,
      terminal,
      listener_path,
      users,
      default_user,
      inner: Mutex::new(ContextInner {
        current: None,
        scheduled: None,
        eviction: None,
        attempt: None,
        client: None,
      }),
    })
  }

  /// Everything the greeter needs to draw itself.
  pub fn welcome(&self) -> Event {
    Event::Welcome {
      version: PROTOCOL_VERSION,
      users: self.users.clone(),
      default_user: self.default_user.clone(),
      fingerprint: self.config.session.fingerprint,
      primary_output: self.config.greeter.primary_output.clone(),
    }
  }

  /// Registers the connected greeter, refusing a second one.
  ///
  /// Only one client is allowed: two would share one attempt and be able to
  /// cancel each other's.
  pub async fn attach_client(&self, sink: mpsc::UnboundedSender<Event>) -> Result<()> {
    let mut inner = self.inner.lock().await;

    if inner.client.is_some() {
      bail!("a greeter is already connected");
    }

    inner.client = Some(sink);
    Ok(())
  }

  /// Drops the client and abandons whatever it had in flight.
  ///
  /// A greeter that dies mid-authentication must not leave an authenticated
  /// worker parked and startable by whoever connects next.
  pub async fn detach_client(&self) {
    let mut inner = self.inner.lock().await;
    inner.client = None;
    let attempt = inner.attempt.take();
    drop(inner);

    if let Some(attempt) = attempt {
      debug!("Greeter disconnected, cancelling its attempt");
      attempt.cancel();
    }
  }

  /// Sends one event to the connected greeter, if there is one.
  pub async fn notify(&self, event: Event) {
    Self::push(&*self.inner.lock().await, event);
  }

  fn push(inner: &ContextInner, event: Event) {
    let Some(client) = &inner.client else {
      debug!("No greeter connected, dropping an event");
      return;
    };

    // Unbounded, so this only fails once the receiver is gone.
    if client.send(event).is_err() {
      debug!("Greeter went away before an event could be delivered");
    }
  }

  /// Starts, or restarts, authentication for one user.
  pub async fn authenticate(self: &Rc<Self>, username: String) -> Result<()> {
    if username.len() > greet_ipc::MAX_USERNAME_LEN {
      bail!("the user name is too long");
    }

    // The greeter can only ask about accounts it was offered, so a compromised
    // one cannot go fishing for which other names exist.
    if !self.users.iter().any(|user| user.name == username) {
      bail!("{username:?} is not one of the accounts on offer");
    }

    let mut inner = self.inner.lock().await;

    // Nothing is hosting the login screen, so there is nothing sensible to do
    // with the result.
    if inner.current.is_none() {
      bail!("no session is active");
    }

    if inner.scheduled.is_some() {
      bail!("a session is already starting");
    }

    // Switching users abandons whatever was in flight.
    let previous = inner.attempt.take();
    drop(inner);

    if let Some(previous) = previous {
      previous.cancel();
    }

    let password = self
      .spawn_slot(
        AuthSource::Password,
        &self.config.session.service,
        &username,
      )
      .await?;

    // Forked second, and strictly after the first: `Worker::spawn` clears
    // CLOEXEC on a descriptor it then drops, and two overlapping forks would let
    // one worker inherit the other's socket.
    //
    // A failure here is not fatal. The password worker is already up, and a
    // login screen that works one way is worth more than one that refuses to
    // appear because a reader is broken.
    // Started whenever the stack exists, without first asking whether this user
    // has any prints enrolled. `pam_fprintd` answers that itself with
    // `PAM_AUTHINFO_UNAVAIL`, which arrives here as the path going quiet rather
    // than as a failure - so there is nothing to gain from reading fprintd's
    // private storage layout to guess the same answer less reliably. GDM does it
    // this way too.
    let fingerprint = match self.config.session.fingerprint {
      false => None,
      true => match self
        .spawn_slot(
          AuthSource::Fingerprint,
          &self.config.session.fingerprint_service,
          &username,
        )
        .await
      {
        Ok(slot) => Some(slot),
        Err(error) => {
          warn!(?error, "Could not start fingerprint authentication");
          None
        }
      },
    };

    let mut inner = self.inner.lock().await;

    // Tells the login screen whether to show the indicator at all, now that it
    // is known for this user rather than guessed from the configuration.
    Self::push(
      &inner,
      Event::Fingerprint {
        state: match fingerprint.is_some() {
          true => greet_ipc::FingerprintState::Starting,
          false => greet_ipc::FingerprintState::Off,
        },
      },
    );

    inner.attempt = Some(Attempt {
      username,
      password: Some(password),
      fingerprint,
      winner: None,
      fingerprint_failures: 0,
    });

    Ok(())
  }

  /// Forks one worker and starts pumping its messages.
  async fn spawn_slot(
    self: &Rc<Self>,
    source: AuthSource,
    service: &str,
    username: &str,
  ) -> Result<Slot> {
    let worker = Rc::new(Worker::spawn()?);

    worker
      .send(&self.initiate(
        SessionClass::User,
        service,
        username,
        &self.config.session.command,
        true,
      ))
      .await?;

    let reader = task::spawn_local({
      let context = Rc::clone(self);
      let worker = Rc::clone(&worker);

      async move {
        loop {
          match worker.recv().await {
            Ok(message) => {
              if context.on_worker_message(source, message).await {
                break;
              }
            }
            Err(error) => {
              debug!(?source, ?error, "Worker channel closed");
              context.on_worker_gone(source).await;
              break;
            }
          }
        }
      }
    });

    Ok(Slot {
      worker,
      state: SlotState::Running,
      reader,
    })
  }

  fn initiate(
    &self,
    class: SessionClass,
    service: &str,
    user: &str,
    command: &str,
    authenticate: bool,
  ) -> ParentToWorker {
    ParentToWorker::InitiateLogin {
      service: service.to_owned(),
      class,
      user: user.to_owned(),
      authenticate,
      terminal: self.terminal.clone(),
      source_profile: self.config.general.source_profile,
      seat: Some(self.config.general.seat.clone()),
      session_type: self.config.general.session_type.clone(),
      // Only a greeter is told where the socket is.
      listener_path: match class {
        SessionClass::Greeter => Some(self.listener_path.clone()),
        SessionClass::User => None,
      },
      command: command.to_owned(),
    }
  }

  /// Applies one message from a worker. Returns whether its reader should stop.
  ///
  /// Written so that no borrow of the attempt is alive across a push or a send:
  /// everything needed is copied out first, which is what keeps this a single
  /// locked region rather than a sequence of them.
  async fn on_worker_message(self: &Rc<Self>, source: AuthSource, message: WorkerToParent) -> bool {
    let mut inner = self.inner.lock().await;

    let Some(attempt) = inner.attempt.as_mut() else {
      debug!(?source, "Message from a worker with no attempt");
      return true;
    };

    if attempt.slot(source).is_none() {
      debug!(?source, "Message from a slot that is gone");
      return true;
    }

    match message {
      WorkerToParent::PamMessage { style, message } => match style {
        // Something to answer: park the slot until the greeter replies.
        AuthMessageType::Visible | AuthMessageType::Secret => {
          if let Some(slot) = attempt.slot(source) {
            slot.state = SlotState::AwaitingResponse;
          }

          Self::push(
            &inner,
            Event::Prompt {
              source,
              echo: style == AuthMessageType::Visible,
            },
          );

          false
        }
        // Nothing to answer, but the worker is blocked until it gets the
        // acknowledgement that keeps the datagram socket in lockstep.
        AuthMessageType::Info | AuthMessageType::Error => {
          // A reader that cannot read this finger should stop asking rather than
          // compete with the password field indefinitely. `pam_fprintd` has a
          // retry limit of its own, but it is configured elsewhere and says
          // nothing to the user when it trips.
          //
          // Counted here, before any push, because the attempt may not be
          // borrowed across one.
          let exhausted = source == AuthSource::Fingerprint && style == AuthMessageType::Error && {
            attempt.fingerprint_failures += 1;
            attempt.fingerprint_failures >= MAX_FINGERPRINT_FAILURES
          };

          let cancelled = match exhausted {
            true => attempt.take(AuthSource::Fingerprint),
            false => None,
          };

          // Nothing is acknowledged when the slot is being cancelled: it is
          // being ended, not let on to the next round of its conversation.
          let worker = match exhausted {
            true => None,
            false => attempt.slot(source).map(|slot| Rc::clone(&slot.worker)),
          };

          let ended = exhausted && attempt.is_empty();

          if exhausted {
            debug!("Giving up on the fingerprint reader for this attempt");

            // Worded exactly as the lock screen words it.
            Self::push(
              &inner,
              Event::Error {
                source,
                message: "Too many attempts, use your password".to_owned(),
              },
            );

            Self::push(
              &inner,
              Event::Fingerprint {
                state: greet_ipc::FingerprintState::Off,
              },
            );

            if ended {
              inner.attempt = None;
            }

            drop(inner);

            if let Some(slot) = cancelled {
              slot.cancel_from_reader();
            }

            return true;
          }

          // `pam_fprintd` only speaks when the reader is armed and listening, so
          // its first word is the signal that the indicator should go live.
          // There is no equivalent for the lock screen's `Reading`: libpam has
          // no way to report a finger mid-swipe, which fprintd's own D-Bus
          // signals do.
          //
          // Only the *arrival* is used. `pam_fprintd`'s own wording is an
          // instruction ("Swipe your finger across the fingerprint reader") which
          // reads as the thing to do next, when it is really an alternative to the
          // password sitting right above it - so the greeter writes its own line
          // from the state instead, and the text is dropped here. GDM substitutes
          // it in the same place for the same reason.
          let event = match (source, style) {
            (AuthSource::Fingerprint, AuthMessageType::Info) => Event::Fingerprint {
              state: greet_ipc::FingerprintState::Waiting,
            },
            (_, AuthMessageType::Error) => Event::Error { source, message },
            _ => Event::Info { source, message },
          };

          Self::push(&inner, event);
          drop(inner);

          if let Some(worker) = worker
            && let Err(error) = worker
              .send(&ParentToWorker::PamResponse { response: None })
              .await
          {
            warn!(?source, ?error, "Failed to acknowledge a PAM message");
          }

          false
        }
      },

      WorkerToParent::Ready => {
        // Exactly one winner, ever. A second worker reporting success is a
        // genuine race, and it is the late one that gets cancelled.
        if let Some(winner) = attempt.winner {
          debug!(?source, ?winner, "A worker won the race already");
          let slot = attempt.take(source);
          drop(inner);

          if let Some(slot) = slot {
            slot.cancel_from_reader();
          }

          return true;
        }

        attempt.winner = Some(source);
        if let Some(slot) = attempt.slot(source) {
          slot.state = SlotState::Ready;
        }

        info!(?source, "Authenticated");
        Self::push(&inner, Event::Authenticated { via: source });
        false
      }

      WorkerToParent::Error { message } => {
        let rejected = message.contains(REJECTED);
        let unavailable = message.contains(UNAVAILABLE);
        debug!(?source, message, "Attempt turned down");

        // Only this slot fails. A fingerprint that gives up must never take a
        // live password worker with it, or the other way round.
        //
        // The worker exits immediately after sending this, so its reader is all
        // there is left to stop.
        if let Some(slot) = attempt.take(source) {
          slot.reader.abort();
        }

        // The login already succeeded on the other path; the loser's complaint
        // is not something the user needs to read on the way out.
        if let Some(winner) = attempt.winner {
          debug!(
            ?source,
            ?winner,
            "Ignoring a failure after the race was won"
          );
          return true;
        }

        // Read out before the pushes below, for the same reason as above.
        let empty = attempt.is_empty();

        if source == AuthSource::Fingerprint {
          Self::push(
            &inner,
            Event::Fingerprint {
              state: greet_ipc::FingerprintState::Off,
            },
          );
        }

        // The stack could not be attempted, so there is nothing to report: the
        // indicator going away above is the whole message. Saying more would mean
        // a machine with no enrolled prints greeting every user with an error
        // about a reader they were never going to use.
        if unavailable {
          debug!(?source, "Path unavailable, offering it no further");

          if empty {
            inner.attempt = None;
          }

          return true;
        }

        let failure = match (source, rejected) {
          (_, false) => AuthFailure::Error { message },
          (AuthSource::Password, true) => AuthFailure::Rejected,
          // An outlined password field means "that password was wrong", so a
          // finger that did not match has to say so in words instead.
          (AuthSource::Fingerprint, true) => AuthFailure::Error {
            message: "Fingerprint not recognised, use your password".to_owned(),
          },
        };

        Self::push(&inner, Event::Failed { source, failure });

        if empty {
          inner.attempt = None;
        }

        true
      }

      other => {
        warn!(?source, ?other, "Unexpected message from a worker");
        true
      }
    }
  }

  /// A worker's socket closed without it saying why.
  async fn on_worker_gone(&self, source: AuthSource) {
    let mut inner = self.inner.lock().await;

    let Some(attempt) = &mut inner.attempt else {
      return;
    };

    // A worker that won is parked and its reader was stopped deliberately; this
    // is only for one that died.
    if attempt.winner == Some(source) {
      return;
    }

    if attempt.take(source).is_none() {
      return;
    }

    let ended = attempt.is_empty();

    if source == AuthSource::Fingerprint {
      Self::push(
        &inner,
        Event::Fingerprint {
          state: greet_ipc::FingerprintState::Off,
        },
      );
    }

    Self::push(
      &inner,
      Event::Failed {
        source,
        failure: AuthFailure::Error {
          message: "Authentication stopped unexpectedly".to_owned(),
        },
      },
    );

    if ended {
      inner.attempt = None;
    }
  }

  /// Answers the password worker's prompt.
  pub async fn password(&self, secret: Secret) -> Result<()> {
    let mut inner = self.inner.lock().await;

    let Some(attempt) = &mut inner.attempt else {
      bail!("nothing is being authenticated");
    };

    let Some(slot) = attempt.slot(AuthSource::Password) else {
      bail!("there is no password worker");
    };

    if slot.state != SlotState::AwaitingResponse {
      bail!("the password worker is not asking for anything");
    }

    slot.state = SlotState::Running;
    let worker = Rc::clone(&slot.worker);
    drop(inner);

    worker
      .send(&ParentToWorker::PamResponse {
        response: Some(secret),
      })
      .await
      .context("handing the password to the worker")
  }

  /// Abandons the current attempt.
  pub async fn cancel(&self) {
    let attempt = self.inner.lock().await.attempt.take();

    if let Some(attempt) = attempt {
      attempt.cancel();
    }
  }

  /// Schedules the authenticated session.
  ///
  /// Nothing starts here. The winning worker is already parked having said it is
  /// ready, and it stays parked until the greeter exits - otherwise the two
  /// would fight over the same VT. The greeter is told to quit, and
  /// [`Self::check_children`] does the rest.
  pub async fn start_session(self: &Rc<Self>) -> Result<()> {
    let mut inner = self.inner.lock().await;

    if inner.scheduled.is_some() {
      bail!("a session is already starting");
    }

    let Some(attempt) = &mut inner.attempt else {
      bail!("nothing has been authenticated");
    };

    let Some(winner) = attempt.winner else {
      bail!("nothing has been authenticated");
    };

    let Some(slot) = attempt.take(winner) else {
      bail!("the authenticated worker is gone");
    };

    // Its reader would otherwise race the promotion for the same socket.
    slot.reader.abort();

    let username = attempt.username.clone();
    let loser = inner.attempt.take();
    let scheduled_at = Instant::now();

    inner.scheduled = Some(PendingSession {
      worker: slot.worker,
      scheduled_at,
    });

    // Armed here rather than when the greeter is asked to go, so the clock starts
    // at the schedule and not at whatever the greeter does next.
    inner.eviction = Some(task::spawn_local({
      let context = Rc::clone(self);
      async move { context.evict_greeter().await }
    }));

    Self::push(&inner, Event::SessionStarted);
    drop(inner);

    // Whichever worker lost has nothing left to do.
    if let Some(loser) = loser {
      loser.cancel();
    }

    // Recorded at the point authentication succeeded and a session is committed,
    // not once it is running: a session that fails to start was still this
    // machine's last successful login, and is still the right account to offer
    // next time.
    self.record_last_user(&username);

    info!(user = username, ?winner, "Session scheduled");
    Ok(())
  }

  /// Makes the login screen leave so the authenticated session can have the VT.
  ///
  /// Only ever reached by a greeter that ignored `SessionStarted`. Left alone,
  /// that is a machine which authenticated you and then sat there, so the
  /// escalation is worth having even though it should never fire.
  ///
  /// A timer task rather than greetd's `alarm()` + SIGALRM: this is an async
  /// daemon with a timer wheel already running, and a signal handler racing the
  /// state it would have to inspect is the harder thing to get right.
  async fn evict_greeter(self: Rc<Self>) {
    // Taken from the session it is protecting rather than passed in, so the
    // schedule time lives in exactly one place.
    let Some(scheduled_at) = self
      .inner
      .lock()
      .await
      .scheduled
      .as_ref()
      .map(|pending| pending.scheduled_at)
    else {
      return;
    };

    // Measured from the schedule, which is the whole point. greetd measures from
    // when the attempt was created, so a user who took longer than the patience
    // to type their password had the SIGTERM step skipped entirely.
    tokio::time::sleep(EVICTION_PATIENCE.saturating_sub(scheduled_at.elapsed())).await;

    let inner = self.inner.lock().await;

    // Gone on its own in the meantime, which is the normal path.
    let Some(current) = &inner.current else {
      return;
    };

    if !current.is_greeter {
      return;
    }

    warn!(
      seconds = scheduled_at.elapsed().as_secs(),
      "The login screen has not exited, asking it to"
    );

    current.terminate();
    drop(inner);

    tokio::time::sleep(EVICTION_GRACE).await;

    let inner = self.inner.lock().await;

    let Some(current) = &inner.current else {
      return;
    };

    if !current.is_greeter {
      return;
    }

    warn!(
      seconds = scheduled_at.elapsed().as_secs(),
      "The login screen ignored SIGTERM, killing it"
    );

    current.kill();
  }

  /// Remembers who logged in, so the login screen opens on them next time.
  ///
  /// Advisory in both directions: a failure to write costs the next login its
  /// pre-selected account and nothing else, so it is logged rather than
  /// propagated.
  fn record_last_user(&self, username: &str) {
    let path = std::path::Path::new(LAST_USER_PATH);

    let Some(directory) = path.parent() else {
      return;
    };

    if let Err(error) = std::fs::create_dir_all(directory) {
      warn!(?error, ?directory, "Could not create the state directory");
      return;
    }

    if let Err(error) = std::fs::write(path, username) {
      warn!(?error, ?path, "Could not record the last user");
    }
  }

  /// Sends `Start` to the parked worker and waits for the session to be forked.
  async fn promote(&self, pending: PendingSession) -> Result<RunningSession> {
    pending.worker.send(&ParentToWorker::Start).await?;

    let session = loop {
      match pending.worker.recv().await? {
        WorkerToParent::FinalChildPid { pid } => break Pid::from_raw(pid),
        // A module can still speak during open_session - pam_motd and the like.
        // Nothing is listening by then, so they are acknowledged and dropped.
        WorkerToParent::PamMessage { .. } => {
          pending
            .worker
            .send(&ParentToWorker::PamResponse { response: None })
            .await?;
        }
        WorkerToParent::Error { message } => bail!("{message}"),
        other => bail!("expected the session pid, got {other:?}"),
      }
    };

    Ok(RunningSession {
      worker: pending.worker.pid,
      session,
      started: Instant::now(),
      is_greeter: false,
    })
  }

  /// Starts the login screen.
  pub async fn greet(&self) -> Result<()> {
    let session = self.start_greeter().await.context("starting the greeter")?;

    info!(worker = %session.worker, session = %session.session, "Greeter started");
    self.inner.lock().await.current = Some(session);

    Ok(())
  }

  /// Runs the greeter's own session through a worker.
  ///
  /// `authenticate: false` skips `pam_authenticate` entirely. It is reachable
  /// only from here, never from the IPC protocol, which is what keeps it from
  /// being an authentication bypass.
  async fn start_greeter(&self) -> Result<RunningSession> {
    let worker = Worker::spawn()?;

    worker
      .send(&self.initiate(
        SessionClass::Greeter,
        &self.config.greeter.service,
        &self.config.greeter.user,
        &self.config.greeter.command,
        false,
      ))
      .await?;

    // An unauthenticated stack should ask nothing, but a module that does gets
    // an empty answer rather than being left to block forever.
    loop {
      match worker.recv().await? {
        WorkerToParent::Ready => break,
        WorkerToParent::PamMessage { style, message } => {
          warn!(
            ?style,
            message, "Unexpected prompt while starting the greeter"
          );
          worker
            .send(&ParentToWorker::PamResponse { response: None })
            .await?;
        }
        WorkerToParent::Error { message } => bail!("{message}"),
        other => bail!("expected the worker to be ready, got {other:?}"),
      }
    }

    worker.send(&ParentToWorker::Start).await?;

    let session = loop {
      match worker.recv().await? {
        WorkerToParent::FinalChildPid { pid } => break Pid::from_raw(pid),
        WorkerToParent::PamMessage { .. } => {
          worker
            .send(&ParentToWorker::PamResponse { response: None })
            .await?;
        }
        WorkerToParent::Error { message } => bail!("{message}"),
        other => bail!("expected the session pid, got {other:?}"),
      }
    };

    Ok(RunningSession {
      worker: worker.pid,
      session,
      started: Instant::now(),
      is_greeter: true,
    })
  }

  /// Called on SIGCHLD. Reaps whatever exited and decides what replaces it.
  pub async fn check_children(&self) -> Result<()> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};

    loop {
      let status = match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
        Ok(status) => status,
        Err(nix::errno::Errno::ECHILD) => return Ok(()),
        Err(nix::errno::Errno::EINTR) => continue,
        Err(error) => return Err(error).context("reaping a child"),
      };

      let pid = match status {
        WaitStatus::StillAlive => return Ok(()),
        WaitStatus::Exited(pid, _) | WaitStatus::Signaled(pid, _, _) => pid,
        // Stopped or continued; not an exit.
        _ => continue,
      };

      let mut inner = self.inner.lock().await;

      // Only the worker's exit ends a session. The session process itself is a
      // grandchild, reaped by its worker, so it never shows up here.
      let Some(finished) = inner.current.take_if(|current| current.worker == pid) else {
        continue;
      };

      let lifetime = finished.started.elapsed();
      info!(
        greeter = finished.is_greeter,
        seconds = lifetime.as_secs(),
        "Session ended"
      );

      // The greeter is out of the way, so the authenticated worker can have the
      // VT.
      let pending = inner.scheduled.take();

      // Nothing left to evict. Left running, it would go on to signal whatever
      // became `current` next - which is the session it was protecting.
      if let Some(eviction) = inner.eviction.take() {
        eviction.abort();
      }

      drop(inner);

      if let Some(pending) = pending {
        match self.promote(pending).await {
          Ok(session) => {
            info!(session = %session.session, "Session started");
            self.inner.lock().await.current = Some(session);
          }
          Err(error) => {
            error!(
              ?error,
              "Failed to start the session, returning to the greeter"
            );

            let inner = self.inner.lock().await;
            Self::push(
              &inner,
              Event::SessionFailed {
                message: format!("{error:#}"),
              },
            );
            drop(inner);

            self.greet().await?;
          }
        }

        continue;
      }

      if lifetime < CRASH_LOOP_DEBOUNCE {
        warn!("Session ended immediately, pausing before restarting the greeter");
        tokio::time::sleep(CRASH_LOOP_DEBOUNCE).await;
      }

      self.greet().await?;
    }
  }

  /// Ends whatever is running, for shutdown.
  pub async fn terminate(&self) {
    let mut inner = self.inner.lock().await;

    if let Some(attempt) = inner.attempt.take() {
      attempt.kill();
    }

    inner.scheduled = None;

    if let Some(eviction) = inner.eviction.take() {
      eviction.abort();
    }

    if let Some(current) = inner.current.take() {
      current.terminate();
    }
  }
}
