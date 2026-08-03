//! The daemon's side of a session worker: the fork, the exec, and the datagram
//! protocol they speak.
//!
//! A worker is this same binary, re-exec'd through `/proc/self/exe` with
//! `--session-worker`. It exists because PAM cannot be `exec`ed out of - that
//! registers as a logout - and because `pam_close_session` needs root, so
//! privileges can only be dropped in a further child. The re-exec also gives
//! the worker a provably single-threaded address space, which is what makes
//! that second fork safe.

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixDatagram as StdUnixDatagram;

use anyhow::{Context as _, Result, bail};
use nix::fcntl::{F_GETFD, F_SETFD, FdFlag, fcntl};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{ForkResult, Pid, fork};
use serde::{Deserialize, Serialize};
use tokio::net::UnixDatagram;
use tracing::{debug, warn};
use zeroize::Zeroizing;

use crate::config::VtSelection;

/// Largest datagram either side will send.
///
/// greetd uses a fixed 10 KiB buffer and lets `recv` truncate anything larger,
/// which turns an oversized message into a confusing parse failure. Every input
/// that reaches a worker is length-bounded at the socket (see `greet_ipc`'s
/// `MAX_SECRET_LEN` and `MAX_USERNAME_LEN`), so exceeding this can only be a
/// bug - and it is reported as one rather than silently truncating.
const WORKER_MSG_MAX: usize = 64 * 1024;

/// Whether a session is the login screen or a real user session.
///
/// `pam_systemd` reads the matching `XDG_SESSION_CLASS` and treats the two
/// differently, and it decides whether the worker is told the socket path at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionClass {
  Greeter,
  User,
}

impl SessionClass {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Greeter => "greeter",
      Self::User => "user",
    }
  }
}

/// Where the session's terminal comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TerminalMode {
  /// A real VT, which the worker opens, clears and possibly switches to.
  Terminal {
    path: String,
    vt: u32,
    /// Whether to switch to it, or to assume something else will.
    switch: bool,
  },
  /// No VT handling at all; the session inherits our stdio. Used when the
  /// daemon runs under a supervisor with no console of its own.
  Stdin,
}

/// PAM message styles, one-to-one with libpam's four.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMessageType {
  Visible,
  Secret,
  Info,
  Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentToWorker {
  InitiateLogin {
    service: String,
    class: SessionClass,
    user: String,
    /// `false` skips `pam_authenticate` entirely, for the greeter's own
    /// session. Never reachable from the IPC protocol: only the daemon decides
    /// this, and only for a session it starts itself.
    authenticate: bool,
    terminal: TerminalMode,
    source_profile: bool,
    seat: Option<String>,
    session_type: String,
    /// Path of the greeter socket. `None` for user sessions, so the worker
    /// physically cannot leak it into one - greetd relies on a runtime check
    /// instead.
    listener_path: Option<String>,
    /// Shell command line the session runs.
    command: String,
  },
  /// Answer to a PAM prompt, or `None` to acknowledge an informational message.
  PamResponse {
    response: Option<String>,
  },
  Start,
  Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerToParent {
  /// PAM authenticated and authorized this user. This is the point at which a
  /// worker has won the race against its sibling.
  Ready,
  /// Acknowledges a message that needed no answer.
  Success,
  Error {
    message: String,
  },
  PamMessage {
    style: AuthMessageType,
    message: String,
  },
  /// The session process was forked; this is its pid.
  FinalChildPid(i32),
}

impl ParentToWorker {
  pub fn recv(socket: &StdUnixDatagram) -> Result<Self> {
    let mut buffer = Zeroizing::new(vec![0u8; WORKER_MSG_MAX]);
    let read = socket
      .recv(&mut buffer)
      .context("reading from the daemon")?;
    serde_json::from_slice(&buffer[..read]).context("decoding a message from the daemon")
  }
}

impl WorkerToParent {
  pub fn send(&self, socket: &StdUnixDatagram) -> Result<()> {
    let encoded = serde_json::to_vec(self).context("encoding a worker message")?;
    send_datagram(socket, &encoded)
  }
}

fn send_datagram(socket: &StdUnixDatagram, encoded: &[u8]) -> Result<()> {
  // Checked rather than truncated: a datagram longer than the peer's buffer is
  // silently cut short by the kernel, and the resulting parse error says
  // nothing useful about what went wrong.
  if encoded.len() > WORKER_MSG_MAX {
    bail!(
      "worker message of {} bytes exceeds the {WORKER_MSG_MAX} byte limit",
      encoded.len()
    );
  }

  socket.send(encoded).context("writing to the socket")?;
  Ok(())
}

/// The daemon's handle on one worker.
pub struct Worker {
  /// The worker process itself, which holds the PAM transaction.
  pub pid: Pid,
  pub socket: UnixDatagram,
}

impl Worker {
  /// Forks a worker and re-execs this binary into it.
  pub fn spawn() -> Result<Self> {
    let (parent_end, child_end) =
      StdUnixDatagram::pair().context("creating a worker socket pair")?;

    // The child end has to survive execv, so CLOEXEC comes off. It goes back on
    // inside the worker (see `session::main`) before anything else, so it can
    // never reach the session the worker eventually starts.
    let child_fd = OwnedFd::from(child_end);
    clear_cloexec(&child_fd)?;

    // Everything the child arm needs is built here. Allocating between fork and
    // exec is only safe in a single-threaded process, and this is the one place
    // that assumption would be easy to break by accident.
    let program = CString::new("/proc/self/exe").context("building the worker path")?;
    let argv0 = CString::new("launch-greetd").context("building the worker argv")?;
    let flag = CString::new("--session-worker").context("building the worker argv")?;
    let fd_argument =
      CString::new(child_fd.as_raw_fd().to_string()).context("building the worker argv")?;

    // SAFETY: the daemon is single-threaded by construction (see main.rs), and
    // the child arm below only calls execv and _exit.
    let forked = unsafe { fork() }.context("forking a session worker")?;

    let pid = match forked {
      ForkResult::Parent { child } => child,
      ForkResult::Child => {
        // fork-child contract: no `?`, no panicking API, no allocation. A
        // `return` here would leave a second daemon running.
        let _ = nix::unistd::execv(&program, &[&argv0, &flag, &fd_argument]);

        // Only reachable if execv failed, and there is no way to report it
        // except the exit status; the parent sees the socket close.
        unsafe { libc::_exit(127) }
      }
    };

    // Dropped explicitly, not at end of scope. The next worker forks while this
    // one is alive, and a descriptor that still has CLOEXEC cleared would be
    // inherited by it - a leak greetd never has to think about, because it only
    // ever has one worker under construction.
    drop(child_fd);

    parent_end
      .set_nonblocking(true)
      .context("making the worker socket non-blocking")?;

    let socket = UnixDatagram::from_std(parent_end).context("registering the worker socket")?;

    Ok(Self { pid, socket })
  }

  pub async fn send(&self, message: &ParentToWorker) -> Result<()> {
    let encoded = Zeroizing::new(serde_json::to_vec(message).context("encoding a worker message")?);

    if encoded.len() > WORKER_MSG_MAX {
      bail!(
        "worker message of {} bytes exceeds the {WORKER_MSG_MAX} byte limit",
        encoded.len()
      );
    }

    self
      .socket
      .send(&encoded)
      .await
      .context("writing to the worker")?;
    Ok(())
  }

  /// Ends a worker that has not opened a session.
  ///
  /// Safe precisely because it hasn't: a worker still in its PAM conversation
  /// owes no teardown. A `Cancel` message alone is not enough - a worker blocked
  /// inside `pam_fprintd` is in libpam, not reading its socket - so this is the
  /// escalation that always works.
  pub fn kill_configuring(&self) {
    if let Err(error) = kill(self.pid, Signal::SIGKILL) {
      debug!(?error, pid = %self.pid, "Worker was already gone");
    }
  }

  pub async fn recv(&self) -> Result<WorkerToParent> {
    let mut buffer = vec![0u8; WORKER_MSG_MAX];
    let read = self
      .socket
      .recv(&mut buffer)
      .await
      .context("reading from the worker")?;

    if read == 0 {
      bail!("the worker exited");
    }

    serde_json::from_slice(&buffer[..read]).context("decoding a message from the worker")
  }
}

fn clear_cloexec(fd: &OwnedFd) -> Result<()> {
  let current = fcntl(fd, F_GETFD).context("reading the worker socket flags")?;
  let mut flags = FdFlag::from_bits_retain(current);
  flags.remove(FdFlag::FD_CLOEXEC);
  fcntl(fd, F_SETFD(flags)).context("clearing CLOEXEC on the worker socket")?;
  Ok(())
}

/// Adopts the socket the worker inherited and puts `FD_CLOEXEC` back on it.
///
/// The parent had to take it off so the descriptor would survive `execv`.
/// Leaving it off would hand the logged-in user an open control channel to a
/// root process, so this runs before anything else the worker does.
///
/// The descriptor is checked to be open first, so a stray `--session-worker
/// 9999` fails here rather than at the first read.
pub fn adopt_inherited_socket(fd: i32) -> Result<StdUnixDatagram> {
  if fd < 0 {
    bail!("{fd} is not a descriptor");
  }

  // Cheapest way to ask "is this open?" without changing anything about it.
  let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) };
  let current = fcntl(borrowed, F_GETFD).with_context(|| format!("descriptor {fd} is not open"))?;

  let mut flags = FdFlag::from_bits_retain(current);
  flags.insert(FdFlag::FD_CLOEXEC);
  fcntl(borrowed, F_SETFD(flags)).context("restoring CLOEXEC on the inherited socket")?;

  // SAFETY: checked open above, and the parent handed ownership over through
  // exec, so nothing else in this process holds it.
  let owned = unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
  Ok(StdUnixDatagram::from(owned))
}

/// Whether the inherited socket still has `FD_CLOEXEC`, checked just before the
/// session is forked. If this is ever false the daemon's control channel is
/// about to be inherited by the user's session.
pub fn has_cloexec(socket: &StdUnixDatagram) -> bool {
  match fcntl(socket, F_GETFD) {
    Ok(flags) => FdFlag::from_bits_retain(flags).contains(FdFlag::FD_CLOEXEC),
    Err(error) => {
      warn!(?error, "Could not read the worker socket flags");
      false
    }
  }
}

/// Builds the terminal mode from the configured VT selection.
pub fn terminal_mode(selection: VtSelection, switch: bool) -> Result<TerminalMode> {
  use crate::terminal::Terminal;

  match selection {
    VtSelection::None => Ok(TerminalMode::Stdin),
    VtSelection::Specific(vt) => Ok(TerminalMode::Terminal {
      path: format!("/dev/tty{vt}"),
      vt,
      switch,
    }),
    VtSelection::Next => {
      let console = Terminal::open(std::path::Path::new("/dev/tty0"))?;
      let vt = console.vt_get_next()?;

      Ok(TerminalMode::Terminal {
        path: format!("/dev/tty{vt}"),
        vt,
        switch,
      })
    }
    VtSelection::Current => {
      // Started from a terminal: use it, and don't switch, because we are
      // already there.
      if let Ok(terminal) = Terminal::stdin()
        && let Ok(name) = terminal.ttyname()
      {
        if let Some(number) = name.strip_prefix("/dev/tty")
          && let Ok(vt) = number.parse::<u32>()
        {
          return Ok(TerminalMode::Terminal {
            path: name,
            vt,
            switch: false,
          });
        }

        // A pty has no VT to claim, and silently falling back would put the
        // login screen somewhere nobody is looking.
        if name.starts_with("/dev/pts/") {
          bail!("terminal.vt is \"current\", but this was started from a pseudo terminal");
        }
      }

      let console = Terminal::open(std::path::Path::new("/dev/tty0"))?;
      let vt = console.vt_get_current()?;

      Ok(TerminalMode::Terminal {
        path: format!("/dev/tty{vt}"),
        vt,
        switch: false,
      })
    }
  }
}
