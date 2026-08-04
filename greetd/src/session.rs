//! The session worker: one PAM transaction, from `pam_start` to `pam_end`.
//!
//! Runs as root in a process the daemon forked and re-exec'd, talking to it
//! over an inherited datagram socket. Everything here is blocking and strictly
//! sequential - there is no runtime in this process, which is what makes the
//! fork near the end safe.
//!
//! The order of the calls below is not a matter of taste. Each constraint is
//! noted where it applies; violating any of them produces a login that looks
//! like it worked and is subtly broken.

use std::ffi::{CStr, CString};
use std::os::unix::net::UnixDatagram as StdUnixDatagram;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use nix::sys::prctl;
use nix::sys::signal::Signal;
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, Gid, Uid, fork, initgroups, setgid, setsid, setuid};
use pam_client2::{Context as PamContext, ConversationHandler, ErrorCode, Flag};
use tracing::{debug, error, info, warn};
use uzers::os::unix::UserExt as _;

use crate::terminal::Terminal;
use crate::worker::{
  AuthMessageType, ParentToWorker, SessionClass, TerminalMode, WorkerToParent,
  adopt_inherited_socket, has_cloexec,
};

/// Entry point for `--session-worker`.
pub fn main(fd: i32) -> Result<()> {
  // Before anything else: the daemon cleared CLOEXEC so this descriptor would
  // survive execv, and it has to go back on before there is any chance of
  // another exec inheriting it.
  let socket = adopt_inherited_socket(fd)?;

  // A configuring worker that outlives the daemon would sit on the fingerprint
  // reader forever. This is cleared again once a session is open, because from
  // that point the worker owes a `pam_close_session` and must not be killed.
  if let Err(error) = prctl::set_pdeathsig(Signal::SIGTERM) {
    warn!(?error, "Could not set the parent death signal");
  }

  match run(&socket) {
    Ok(()) => Ok(()),
    Err(error) => {
      // The daemon only learns why through this message; after it, the socket
      // closing is all it would see.
      let message = format!("{error:#}");
      WorkerToParent::Error {
        message: message.clone(),
      }
      .send(&socket)
      .ok();

      bail!(message)
    }
  }
}

fn run(socket: &StdUnixDatagram) -> Result<()> {
  let ParentToWorker::InitiateLogin {
    service,
    class,
    user,
    authenticate,
    terminal,
    source_profile,
    seat,
    session_type,
    listener_path,
    command,
  } = ParentToWorker::recv(socket)?
  else {
    bail!("cancelled before the transaction started");
  };

  let conversation = SocketConversation {
    socket: socket.try_clone().context("cloning the daemon socket")?,
  };

  let mut context = PamContext::new(&service, Some(&user), conversation)
    .with_context(|| format!("starting the PAM transaction for service {service:?}"))?;

  // `authenticate: false` is how the greeter's own session starts. It is
  // reachable only from the daemon's configuration, never from the IPC
  // protocol.
  if authenticate {
    context.authenticate(Flag::NONE).map_err(pam_failure)?;
  }

  if let Err(error) = context.acct_mgmt(Flag::NONE) {
    if error.code() != ErrorCode::NEW_AUTHTOK_REQD {
      return Err(pam_failure(error));
    }

    context
      .chauthtok(Flag::CHANGE_EXPIRED_AUTHTOK)
      .map_err(pam_failure)?;
  }

  // Authenticated and authorized. For a worker racing a sibling, this is the
  // moment it wins; the daemon cancels the other one on seeing it.
  WorkerToParent::Ready.send(socket)?;

  match ParentToWorker::recv(socket)? {
    ParentToWorker::Start => {}
    ParentToWorker::Cancel => return Ok(()),
    other => bail!("expected a start message, got {other:?}"),
  }

  // PAM may have mapped the name onto a different account, so the passwd entry
  // comes from what PAM ended up with rather than what was asked for.
  let pam_username = context.user().context("reading the authenticated user")?;
  let account = uzers::get_user_by_name(&pam_username)
    .with_context(|| format!("no passwd entry for {pam_username:?}"))?;

  let home = account.home_dir().to_path_buf();
  let shell = account.shell().to_path_buf();
  let uid = Uid::from_raw(account.uid());
  let gid = Gid::from_raw(account.primary_group_id());

  // Must precede any terminal work: without a new session there is nothing for
  // TIOCSCTTY to attach the terminal to.
  setsid().context("starting a new session")?;

  if let TerminalMode::Terminal { path, vt, switch } = &terminal {
    // logind reads PAM_TTY and XDG_VTNR during open_session, so both have to be
    // in place before it runs.
    context
      .set_tty(Some(&format!("tty{vt}")))
      .context("setting the PAM tty")?;

    context
      .putenv(format!("XDG_VTNR={vt}"))
      .context("setting XDG_VTNR")?;

    let target = Terminal::open(Path::new(path))?;

    // Text rather than graphics: a graphics-mode console can't run textual
    // sessions, which greetd deliberately supports and there is no reason to
    // drop.
    target.kd_set_text_mode()?;

    // Cleared before the switch, so the previous session's output never flashes
    // past on the way in.
    target.term_clear()?;

    if *switch && *vt != target.vt_get_current()? {
      target.vt_setactivate(*vt)?;
    }

    target.term_connect_pipes()?;
    target.term_take_ctty()?;
  }

  // Everything pam_systemd reads has to be set before open_session.
  let mut environment = vec![
    format!("XDG_SESSION_CLASS={}", class.as_str()),
    format!("XDG_SESSION_TYPE={session_type}"),
    format!("USER={}", account.name().to_string_lossy()),
    format!("LOGNAME={}", account.name().to_string_lossy()),
    format!("HOME={}", home.to_string_lossy()),
    format!("SHELL={}", shell.to_string_lossy()),
    format!(
      "TERM={}",
      std::env::var("TERM").unwrap_or_else(|_| "linux".to_owned())
    ),
  ];

  // Only meaningful with a VT. Claiming a seat without one makes logind
  // describe a session that isn't on any console.
  if let (Some(seat), TerminalMode::Terminal { .. }) = (&seat, &terminal) {
    environment.push(format!("XDG_SEAT={seat}"));
  }

  // Only the greeter is told where the socket is. User sessions are not given
  // the value at all, so this cannot leak into one by mistake.
  if class == SessionClass::Greeter
    && let Some(path) = &listener_path
  {
    environment.push(format!("{}={path}", greet_ipc::SOCKET_ENV_VAR));
  }

  for entry in &environment {
    context
      .putenv(entry)
      .with_context(|| format!("setting {entry}"))?;
  }

  let session = context.open_session(Flag::NONE).map_err(pam_failure)?;

  // `Session` closes the PAM session when it drops. The child forked below must
  // never unwind through that - it would tear down the session it is about to
  // become - so the guard is defused here and restored in the parent after the
  // session ends.
  let token = session.leak();

  // Steered pam_systemd; leaking "greeter" into the session's environment would
  // only confuse whatever reads it.
  if let Err(error) = context.putenv("XDG_SESSION_CLASS") {
    warn!(?error, "Could not unset XDG_SESSION_CLASS");
  }

  // After open_session, so pam_systemd's XDG_RUNTIME_DIR, XDG_SESSION_ID and
  // DBUS_SESSION_BUS_ADDRESS are included.
  let environment: Vec<CString> = context.envlist().into();
  let environment: Vec<&CStr> = environment.iter().map(CString::as_c_str).collect();

  let command_line = match source_profile {
    true => format!(
      "[ -f /etc/profile ] && . /etc/profile; [ -f \"$HOME/.profile\" ] && . \"$HOME/.profile\"; exec {command}"
    ),
    false => format!("exec {command}"),
  };

  // Built before the fork: allocating between fork and exec is only safe
  // because this process is single-threaded, and there is no reason to lean on
  // that any harder than necessary.
  let shell_path = CString::new("/bin/sh").context("building the session argv")?;
  let dash_c = CString::new("-c").context("building the session argv")?;
  let script = CString::new(command_line).context("building the session command")?;
  let account_name =
    CString::new(account.name().to_string_lossy().as_bytes()).context("building the user name")?;

  if !has_cloexec(socket) {
    error!("The daemon socket would be inherited by the session; refusing to start it");
    bail!("the worker socket lost CLOEXEC");
  }

  // SAFETY: this process has no runtime and no threads, and the child arm
  // touches nothing but the calls listed in it.
  let child = match unsafe { fork() }.context("forking the session")? {
    ForkResult::Parent { child } => child,
    ForkResult::Child => {
      // fork-child contract: no `?`, no panicking API, no unwinding. Every
      // failure ends in _exit, because returning from here would resume the
      // worker's own control flow in a second process.
      //
      // The order is load-bearing: initgroups needs root, setgid must precede
      // setuid, and the parent death signal has to come after both because
      // changing credentials clears it.
      if initgroups(&account_name, gid).is_err() {
        fail_child(b"launch-greetd: unable to initialise groups\n");
      }

      if setgid(gid).is_err() {
        fail_child(b"launch-greetd: unable to set the group id\n");
      }

      if setuid(uid).is_err() {
        fail_child(b"launch-greetd: unable to set the user id\n");
      }

      if prctl::set_pdeathsig(Signal::SIGTERM).is_err() {
        fail_child(b"launch-greetd: unable to set the parent death signal\n");
      }

      // Not fatal: a home directory on an unavailable mount shouldn't stop the
      // session from starting.
      let _ = nix::unistd::chdir(&home);

      let _ = nix::unistd::execve(&shell_path, &[&shell_path, &dash_c, &script], &environment);

      fail_child(b"launch-greetd: unable to execute the session\n")
    }
  };

  WorkerToParent::FinalChildPid {
    pid: child.as_raw(),
  }
  .send(socket)?;

  // Nothing more will be said to the daemon, and the session must not find an
  // open channel to it.
  socket
    .shutdown(std::net::Shutdown::Both)
    .context("closing the daemon socket")?;

  // From here the worker owes a pam_close_session, so it has to survive the
  // daemon dying rather than being taken down with it.
  if let Err(error) = prctl::set_pdeathsig(None) {
    warn!(?error, "Could not clear the parent death signal");
  }

  info!(pid = %child, user = %pam_username, "Session started");

  loop {
    match waitpid(child, None) {
      Ok(_) => break,
      Err(nix::errno::Errno::EINTR) => continue,
      Err(error) => {
        warn!(?error, "Failed to wait for the session");
        break;
      }
    }
  }

  // Still root, which is the point: this unmounts home directories, tells
  // logind the session ended, and releases the runtime directory.
  let session = context.unleak_session(token);
  if let Err(error) = session.close(Flag::NONE) {
    warn!(?error, "Failed to close the PAM session");
  }

  info!(user = %pam_username, "Session ended");
  Ok(())
}

/// Reports a failure from the fork child and exits.
///
/// Uses a raw `write` because it is async-signal-safe; `eprintln!` allocates
/// and locks, neither of which is sound between fork and exec.
fn fail_child(message: &[u8]) -> ! {
  // SAFETY: writing a fixed byte string to fd 2 is async-signal-safe.
  unsafe {
    libc::write(2, message.as_ptr().cast(), message.len());
    // _exit, not process::exit: the latter runs atexit handlers registered by
    // the parent, which is not fork-safe.
    libc::_exit(127)
  }
}

/// Turns a PAM error into something worth showing, keeping the distinction the
/// UI needs between "wrong password" and "something is broken".
fn pam_failure(error: pam_client2::Error) -> anyhow::Error {
  // The stack could not even try - no reader attached, nothing enrolled for this
  // user, fprintd not running. Distinct from a rejection because there is nothing
  // for the user to do differently, and distinct from a broken stack because
  // nothing is broken. GDM makes the same distinction from the same two codes,
  // and it is what lets this daemon offer a fingerprint worker without first
  // asking whether the user has any prints.
  if matches!(
    error.code(),
    ErrorCode::AUTHINFO_UNAVAIL | ErrorCode::MODULE_UNKNOWN
  ) {
    debug!(?error, "Authentication service unavailable");
    return anyhow::anyhow!(UNAVAILABLE);
  }

  let rejected = matches!(
    error.code(),
    ErrorCode::AUTH_ERR
      | ErrorCode::CRED_INSUFFICIENT
      | ErrorCode::MAXTRIES
      | ErrorCode::USER_UNKNOWN
      | ErrorCode::PERM_DENIED
  );

  if rejected {
    debug!(?error, "Authentication rejected");
    return anyhow::anyhow!(REJECTED);
  }

  warn!(?error, "Authentication failed");
  anyhow::anyhow!("{error}")
}

/// Sentinel the daemon matches on to tell an ordinary rejection from a broken
/// stack. A rejection needs no message: the field it was typed into says it.
pub const REJECTED: &str = "__rejected__";

/// Sentinel for a stack that could not be attempted at all. Reported to the
/// greeter as the path going away rather than as a failure, so a machine with no
/// enrolled prints simply shows no fingerprint indicator.
pub const UNAVAILABLE: &str = "__unavailable__";

/// Answers PAM's prompts by asking the daemon, which asks the greeter.
///
/// Every message gets exactly one reply, including the informational ones, so
/// the datagram socket stays in lockstep - a missed reply would desynchronise
/// every message after it.
struct SocketConversation {
  socket: StdUnixDatagram,
}

impl SocketConversation {
  fn ask(
    &self,
    style: AuthMessageType,
    message: &CStr,
  ) -> Result<Option<greet_ipc::Secret>, ErrorCode> {
    let message = message.to_string_lossy().trim().to_owned();

    WorkerToParent::PamMessage { style, message }
      .send(&self.socket)
      .map_err(|_| ErrorCode::CONV_ERR)?;

    match ParentToWorker::recv(&self.socket).map_err(|_| ErrorCode::CONV_ERR)? {
      ParentToWorker::PamResponse { response } => Ok(response),
      // A cancelled attempt aborts the PAM call cleanly, which is how the
      // losing worker of a race is unwound.
      ParentToWorker::Cancel => Err(ErrorCode::CONV_ERR),
      _ => Err(ErrorCode::CONV_ERR),
    }
  }

  /// The one place the secret leaves our control: `CString` has no wiping drop,
  /// and `pam-client2` takes one by value. It lives from here until libpam has
  /// copied it, which is as small a window as this API allows.
  fn prompt(&self, style: AuthMessageType, message: &CStr) -> Result<CString, ErrorCode> {
    let response = self.ask(style, message)?.ok_or(ErrorCode::CONV_ERR)?;
    CString::new(response.expose()).map_err(|_| ErrorCode::CONV_ERR)
  }
}

impl ConversationHandler for SocketConversation {
  fn prompt_echo_on(&mut self, message: &CStr) -> Result<CString, ErrorCode> {
    self.prompt(AuthMessageType::Visible, message)
  }

  fn prompt_echo_off(&mut self, message: &CStr) -> Result<CString, ErrorCode> {
    self.prompt(AuthMessageType::Secret, message)
  }

  fn text_info(&mut self, message: &CStr) {
    let _ = self.ask(AuthMessageType::Info, message);
  }

  fn error_msg(&mut self, message: &CStr) {
    let _ = self.ask(AuthMessageType::Error, message);
  }
}
