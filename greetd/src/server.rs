//! Startup, the greeter socket, and the signal loop.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use nix::sys::stat::Mode;
use nix::unistd::{Gid, Uid, chown, getpid};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};
use uzers::os::unix::UserExt as _;

use crate::config::{Config, VtSelection};
use crate::context::Context;
use crate::terminal::Terminal;
use crate::worker::{TerminalMode, terminal_mode};

/// The greeter socket, unlinked when the daemon goes away.
struct Listener {
  path: PathBuf,
  listener: UnixListener,
}

impl Listener {
  /// Creates the socket owned by the greeter user and readable by nobody else.
  ///
  /// greetd sets no mode at all, leaving it at `0777 & ~umask` - 0755 under
  /// systemd's default, but group-writable under a 0002 umask, which would let
  /// anyone in the greeter's group drive the login flow. The umask is narrowed
  /// around the bind so the socket is never briefly more permissive than it
  /// should be, and the explicit chmod covers a platform that ignores it.
  fn create(uid: Uid, gid: Gid) -> Result<Self> {
    let path = PathBuf::from(format!("/run/launch-greetd-{}.sock", getpid()));

    if path.exists() {
      std::fs::remove_file(&path)
        .with_context(|| format!("removing a stale socket at {}", path.display()))?;
    }

    let previous = nix::sys::stat::umask(Mode::from_bits_truncate(0o177));
    let listener = UnixListener::bind(&path);
    nix::sys::stat::umask(previous);

    let listener =
      listener.with_context(|| format!("binding the greeter socket at {}", path.display()))?;

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
      .context("restricting the greeter socket")?;

    chown(&path, Some(uid), Some(gid)).context("handing the greeter socket to the greeter user")?;

    Ok(Self { path, listener })
  }
}

impl Drop for Listener {
  fn drop(&mut self) {
    if let Err(error) = std::fs::remove_file(&self.path) {
      warn!(?error, path = %self.path.display(), "Could not remove the greeter socket");
    }
  }
}

pub async fn main(mut config: Config) -> Result<()> {
  // Locks the daemon's pages into RAM so a password can't reach swap. Not fatal
  // if it fails - greetd exits here, but refusing to provide a login screen
  // over it is the wrong trade.
  if let Err(error) = nix::sys::mman::mlockall(
    nix::sys::mman::MlockAllFlags::MCL_CURRENT | nix::sys::mman::MlockAllFlags::MCL_FUTURE,
  ) {
    warn!(?error, "Could not lock memory; secrets may reach swap");
  }

  config.validate()?;

  let greeter = uzers::get_user_by_name(&config.greeter.user)
    .with_context(|| format!("looking up greeter.user {:?}", config.greeter.user))?;
  let uid = Uid::from_raw(greeter.uid());
  let gid = Gid::from_raw(greeter.primary_group_id());

  let listener = Listener::create(uid, gid)?;
  let listener_path = listener.path.to_string_lossy().into_owned();
  info!(socket = %listener_path, "Listening for the greeter");

  let accounts = crate::users::list(&config.users);
  if accounts.is_empty() {
    warn!("No accounts match the configured uid range; the login screen will have nobody to offer");
  } else {
    info!(count = accounts.len(), "Accounts available to log in");
  }

  // Done before the greeter starts, so the portraits are already in place by
  // the time it draws. A failure here costs portraits, not logins.
  let avatars = crate::avatars::sync(&accounts, greeter.home_dir(), uid, gid)
    .inspect_err(|error| warn!(?error, "Could not publish avatars for the greeter"))
    .unwrap_or_default();

  let users = crate::users::to_ipc(&accounts, &avatars);

  let terminal = terminal_mode(config.terminal.vt, config.terminal.switch)?;
  if let TerminalMode::Terminal { vt, .. } = &terminal {
    info!(vt, switch = config.terminal.switch, "Using VT");
  }

  // Told not to switch: something else owns the switch, so wait until it lands
  // rather than painting a login screen onto a console nobody is looking at.
  if !config.terminal.switch {
    wait_for_vt(&terminal)?;
  }

  let context = Context::new(config, terminal.clone(), listener_path, users);

  if let Err(error) = context.greet().await {
    error!(?error, "Could not start the greeter");
    reset_vt(&terminal);
    return Err(error);
  }

  run_signal_loop(&context, listener).await;

  context.terminate().await;
  Ok(())
}

async fn run_signal_loop(context: &std::rc::Rc<Context>, listener: Listener) {
  let mut child = match signal(SignalKind::child()) {
    Ok(stream) => stream,
    Err(error) => {
      error!(?error, "Could not listen for SIGCHLD");
      return;
    }
  };

  let mut terminate = match signal(SignalKind::terminate()) {
    Ok(stream) => stream,
    Err(error) => {
      error!(?error, "Could not listen for SIGTERM");
      return;
    }
  };

  let mut interrupt = match signal(SignalKind::interrupt()) {
    Ok(stream) => stream,
    Err(error) => {
      error!(?error, "Could not listen for SIGINT");
      return;
    }
  };

  loop {
    tokio::select! {
      _ = child.recv() => {
        if let Err(error) = context.check_children().await {
          error!(?error, "Failed to handle a child exit");
          return;
        }
      }
      _ = terminate.recv() => {
        info!("Terminating");
        return;
      }
      _ = interrupt.recv() => {
        info!("Interrupted");
        return;
      }
      accepted = listener.listener.accept() => {
        match accepted {
          // Serving the greeter is not wired up yet; the connection is closed
          // so a client fails fast rather than waiting on a silent socket.
          Ok((stream, _)) => {
            warn!("Rejecting a greeter connection: the protocol is not implemented yet");
            drop(stream);
          }
          Err(error) => warn!(?error, "Failed to accept a greeter connection"),
        }
      }
    }
  }
}

/// Waits until the target VT is the active one.
fn wait_for_vt(terminal: &TerminalMode) -> Result<()> {
  let TerminalMode::Terminal { path, vt, .. } = terminal else {
    return Ok(());
  };

  Terminal::open(Path::new(path))?.vt_waitactive(*vt)
}

/// Puts the console back to text mode after a failed start, so the machine is
/// left with a usable terminal rather than a blank screen.
fn reset_vt(terminal: &TerminalMode) {
  let TerminalMode::Terminal { path, vt, .. } = terminal else {
    return;
  };

  let restore = || -> Result<()> {
    let terminal = Terminal::open(Path::new(path))?;
    terminal.kd_set_text_mode()?;
    terminal.vt_setactivate(*vt)
  };

  if let Err(error) = restore() {
    warn!(?error, "Could not restore the console");
  }
}

/// Applied on top of the file, for `--vt`.
pub fn override_vt(config: &mut Config, vt: VtSelection) {
  config.terminal.vt = vt;
}
