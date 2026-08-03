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
use crate::session::REJECTED;
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

/// Where fprintd keeps enrolled prints, one directory per user.
///
/// Read only to decide whether forking a fingerprint worker is worth it, and
/// deliberately fail-open (see [`Context::has_enrolled_prints`]): this is
/// fprintd's private storage layout, not an interface it promises to keep.
const FPRINT_STORAGE: &str = "/var/lib/fprint";

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
    if let Err(error) = nix::sys::signal::kill(self.session, nix::sys::signal::Signal::SIGTERM) {
      warn!(?error, pid = %self.session, "Session was already gone");
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
  /// When it was scheduled, which is what the eviction timer measures.
  ///
  /// greetd measures from when the *attempt* was created instead, so any login
  /// where the user took more than ten seconds to type went straight to
  /// SIGKILL without ever being asked politely.
  #[allow(dead_code)]
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
  /// nothing else. Absent when the reader is unavailable or unenrolled.
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
    let fingerprint = match self.fingerprint_eligible(&username) {
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

  /// Whether a fingerprint worker is worth forking for this user.
  ///
  /// The service file having been checked at startup is what `config.session
  /// .fingerprint` already means, so only the per-user question is left.
  fn fingerprint_eligible(&self, username: &str) -> bool {
    self.config.session.fingerprint && Self::has_enrolled_prints(username)
  }

  /// Whether fprintd has prints stored for this user.
  ///
  /// Purely an optimisation, and deliberately fail-open: `true` on anything
  /// unexpected. Asking fprintd properly would mean a D-Bus round trip on the
  /// login path, and reading its storage directly couples us to a layout it does
  /// not promise to keep - so a wrong answer here must cost a wasted fork, never
  /// a reader that silently stops working. The worker itself is the real check:
  /// `pam_fprintd` returns immediately when there is nothing enrolled.
  fn has_enrolled_prints(username: &str) -> bool {
    Self::prints_stored_in(std::path::Path::new(FPRINT_STORAGE), username)
  }

  fn prints_stored_in(storage: &std::path::Path, username: &str) -> bool {
    let directory = storage.join(username);

    match std::fs::read_dir(&directory) {
      Ok(mut entries) => entries.next().is_some(),
      // The one case we can conclude from: fprintd has no storage for this user.
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
      // Anything else - no permission, no fprintd, a layout that moved - is not
      // ours to conclude from, so let the worker decide.
      Err(error) => {
        debug!(?error, ?directory, "Could not tell whether prints exist");
        true
      }
    }
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
          if source == AuthSource::Fingerprint {
            Self::push(
              &inner,
              Event::Fingerprint {
                state: greet_ipc::FingerprintState::Waiting,
              },
            );
          }

          let event = match style {
            AuthMessageType::Error => Event::Error { source, message },
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

        // A wrong password re-arms in place. Sending the greeter back through
        // `Authenticate` instead would tear down a live fingerprint worker and
        // re-claim the reader on every typo.
        let retry = source == AuthSource::Password && rejected;

        // Read out before the pushes below, for the same reason as above.
        let username = attempt.username.clone();
        let empty = attempt.is_empty();

        let failure = match (source, rejected) {
          (_, false) => AuthFailure::Error { message },
          (AuthSource::Password, true) => AuthFailure::Rejected,
          // An outlined password field means "that password was wrong", so a
          // finger that did not match has to say so in words instead.
          (AuthSource::Fingerprint, true) => AuthFailure::Error {
            message: "Fingerprint not recognised, use your password".to_owned(),
          },
        };

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
            failure,
            retry,
          },
        );

        // Kept alive across the re-arm: the slot it is about to be given back is
        // for this attempt, and dropping it here would orphan that worker.
        if empty && !retry {
          inner.attempt = None;
        }

        drop(inner);

        if retry {
          self.rearm_password(username).await;
        }

        true
      }

      other => {
        warn!(?source, ?other, "Unexpected message from a worker");
        true
      }
    }
  }

  /// Replaces a rejected password worker with a fresh one, leaving any
  /// fingerprint worker exactly as it was.
  async fn rearm_password(self: &Rc<Self>, username: String) {
    let slot = match self
      .spawn_slot(
        AuthSource::Password,
        &self.config.session.service,
        &username,
      )
      .await
    {
      Ok(slot) => slot,
      Err(error) => {
        error!(?error, "Could not restart password authentication");

        let mut inner = self.inner.lock().await;
        Self::push(
          &inner,
          Event::Error {
            source: AuthSource::Password,
            message: "Password authentication is unavailable".to_owned(),
          },
        );

        // Nothing will prompt again, so the attempt is over. The greeter finds
        // out by asking for a new one.
        if inner.attempt.as_ref().is_some_and(Attempt::is_empty) {
          inner.attempt = None;
        }

        return;
      }
    };

    let mut inner = self.inner.lock().await;

    // The attempt can have been replaced while the fork was in progress - a user
    // switch, a cancel, or the fingerprint worker winning - in which case this
    // worker is already obsolete.
    let stale = match inner.attempt.as_ref() {
      None => true,
      Some(attempt) => {
        attempt.username != username || attempt.winner.is_some() || attempt.password.is_some()
      }
    };

    if stale {
      drop(inner);
      debug!("Discarding a password worker for an attempt that moved on");
      slot.cancel();
      return;
    }

    if let Some(attempt) = inner.attempt.as_mut() {
      attempt.password = Some(slot);
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
        // A worker that died without a word is not something to retry blindly.
        retry: false,
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
        response: Some(secret.expose().to_owned()),
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
  pub async fn start_session(&self) -> Result<()> {
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

    inner.scheduled = Some(PendingSession {
      worker: slot.worker,
      scheduled_at: Instant::now(),
    });

    Self::push(&inner, Event::SessionStarted);
    drop(inner);

    // Whichever worker lost has nothing left to do.
    if let Some(loser) = loser {
      loser.cancel();
    }

    info!(user = username, ?winner, "Session scheduled");
    Ok(())
  }

  /// Sends `Start` to the parked worker and waits for the session to be forked.
  async fn promote(&self, pending: PendingSession) -> Result<RunningSession> {
    pending.worker.send(&ParentToWorker::Start).await?;

    let session = loop {
      match pending.worker.recv().await? {
        WorkerToParent::FinalChildPid(pid) => break Pid::from_raw(pid),
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
        WorkerToParent::FinalChildPid(pid) => break Pid::from_raw(pid),
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

    if let Some(current) = inner.current.take() {
      current.terminate();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fixture(name: &str) -> std::path::PathBuf {
    let directory = std::env::temp_dir().join(format!(
      "launch-greetd-prints-{}-{name}",
      std::process::id()
    ));

    std::fs::remove_dir_all(&directory).ok();
    std::fs::create_dir_all(&directory).expect("creates the fixture");
    directory
  }

  #[test]
  fn finds_stored_prints() {
    let storage = fixture("enrolled");
    let user = storage.join("ada");
    std::fs::create_dir_all(&user).expect("creates the user directory");
    std::fs::write(user.join("right-index-finger"), b"print").expect("writes a print");

    assert!(Context::prints_stored_in(&storage, "ada"));
  }

  #[test]
  fn an_empty_directory_is_not_enrolled() {
    let storage = fixture("empty");
    std::fs::create_dir_all(storage.join("ada")).expect("creates the user directory");

    assert!(!Context::prints_stored_in(&storage, "ada"));
  }

  #[test]
  fn a_missing_directory_is_not_enrolled() {
    let storage = fixture("missing");
    assert!(!Context::prints_stored_in(&storage, "ada"));
  }

  /// The check is an optimisation, so anything it cannot answer has to come back
  /// `true` and leave the decision to the worker. Answering `false` here would
  /// silently disable the fingerprint reader.
  #[test]
  fn an_unreadable_directory_falls_open() {
    use std::os::unix::fs::PermissionsExt as _;

    let storage = fixture("unreadable");
    let user = storage.join("ada");
    std::fs::create_dir_all(&user).expect("creates the user directory");
    std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o000))
      .expect("removes permissions");

    // Root ignores the mode, which would make this assert the opposite of what
    // it is checking.
    if nix::unistd::Uid::current().is_root() {
      return;
    }

    let answer = Context::prints_stored_in(&storage, "ada");
    std::fs::set_permissions(&user, std::fs::Permissions::from_mode(0o700)).ok();

    assert!(answer);
  }
}
