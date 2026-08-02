//! The daemon's state: which session is running, and what happens when it ends.

use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use nix::unistd::Pid;
use tokio::sync::Mutex;
use tracing::{info, warn};

use greet_ipc::IpcUser;

use crate::config::Config;
use crate::worker::{ParentToWorker, SessionClass, TerminalMode, Worker, WorkerToParent};

/// How soon after a session starts its exit counts as a crash rather than a
/// logout. Restarting a greeter into a compositor that dies immediately would
/// otherwise spin.
const CRASH_LOOP_DEBOUNCE: Duration = Duration::from_secs(1);

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

struct ContextInner {
  current: Option<RunningSession>,
}

pub struct Context {
  config: Config,
  terminal: TerminalMode,
  listener_path: String,
  /// Resolved once at startup: the greeter cannot read other users' homes, so
  /// it is told rather than looking for itself.
  users: Vec<IpcUser>,
  inner: Mutex<ContextInner>,
}

impl Context {
  pub fn new(
    config: Config,
    terminal: TerminalMode,
    listener_path: String,
    users: Vec<IpcUser>,
  ) -> Rc<Self> {
    Rc::new(Self {
      config,
      terminal,
      listener_path,
      users,
      inner: Mutex::new(ContextInner { current: None }),
    })
  }

  /// The accounts the login screen offers, in the order it should show them.
  pub fn users(&self) -> &[IpcUser] {
    &self.users
  }

  /// Starts the login screen.
  pub async fn greet(&self) -> Result<()> {
    let session = self
      .start_session(
        SessionClass::Greeter,
        &self.config.greeter.service,
        &self.config.greeter.user,
        &self.config.greeter.command,
        false,
      )
      .await
      .context("starting the greeter")?;

    info!(worker = %session.worker, session = %session.session, "Greeter started");
    self.inner.lock().await.current = Some(session);

    Ok(())
  }

  /// Runs one session through a worker, from `pam_start` to the fork.
  ///
  /// `authenticate` is false only for the greeter's own session, whose PAM
  /// stack has no auth half to run. Nothing reachable from the IPC protocol can
  /// set it.
  async fn start_session(
    &self,
    class: SessionClass,
    service: &str,
    user: &str,
    command: &str,
    authenticate: bool,
  ) -> Result<RunningSession> {
    let worker = Worker::spawn()?;

    worker
      .send(&ParentToWorker::InitiateLogin {
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
      })
      .await?;

    // An unauthenticated stack should ask nothing, but a module that does gets
    // an empty answer rather than being left to block forever.
    loop {
      match worker.recv().await? {
        WorkerToParent::Ready => break,
        WorkerToParent::PamMessage { style, message } => {
          warn!(
            ?style,
            message, "Unexpected prompt while starting a session"
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
        // A module can still speak during open_session - pam_motd and the like.
        // Nothing is listening by then, so they are acknowledged and dropped.
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
      is_greeter: class == SessionClass::Greeter,
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

      drop(inner);

      let lifetime = finished.started.elapsed();
      info!(
        greeter = finished.is_greeter,
        seconds = lifetime.as_secs(),
        "Session ended"
      );

      if lifetime < CRASH_LOOP_DEBOUNCE {
        warn!("Session ended immediately, pausing before restarting the greeter");
        tokio::time::sleep(CRASH_LOOP_DEBOUNCE).await;
      }

      self.greet().await?;
    }
  }

  /// Ends whatever is running, for shutdown.
  pub async fn terminate(&self) {
    if let Some(current) = self.inner.lock().await.current.take() {
      current.terminate();
    }
  }
}
