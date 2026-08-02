//! Login manager for `launch`.
//!
//! This binary has two personalities, chosen by `--session-worker`. Without it
//! we are the daemon: we own a VT, start a greeter session, and serve the
//! greeter over a unix socket. With it we are a session worker, a child the
//! daemon forked and `execv`ed to hold one PAM transaction.
//!
//! The split exists because PAM will not tolerate being `exec`ed out of - it
//! registers that as a logout - and because `pam_close_session` needs root, so
//! privileges can only be dropped in a further child. `execv`ing
//! `/proc/self/exe` also gets us a fresh, provably single-threaded address
//! space, which is what makes the second fork safe.
//!
//! Adapted from greetd (<https://git.sr.ht/~kennylevinsen/greetd>).

mod avatars;
mod config;
mod context;
mod server;
mod session;
mod terminal;
mod users;
mod worker;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::config::{Config, VtSelection};

#[derive(Debug, Parser)]
#[command(name = "launch-greetd", about = "Login manager for launch")]
struct Args {
  /// Path to the daemon configuration.
  #[arg(short, long, default_value = config::DEFAULT_PATH)]
  config: PathBuf,

  /// Override the VT named in the configuration.
  #[arg(long)]
  vt: Option<VtSelection>,

  /// Internal: run as the session worker on this inherited socket. Not meant
  /// to be passed by hand.
  #[arg(long, hide = true)]
  session_worker: Option<i32>,
}

fn main() -> Result<()> {
  let args = Args::parse();

  // Checked before anything else builds a runtime. The worker forks again to
  // start the session, and doing that from a process with a reactor thread
  // would put the child one malloc away from a deadlock.
  if let Some(fd) = args.session_worker {
    logging::init("launch-greetd-worker");
    return session::main(fd);
  }

  logging::init("launch-greetd");

  let mut config = Config::load(&args.config)?;
  if let Some(vt) = args.vt {
    server::override_vt(&mut config, vt);
  }

  run_daemon(config)
}

/// The daemon must stay single-threaded: it forks, and between `fork` and
/// `execve` a child may only call async-signal-safe functions. A
/// `current_thread` runtime spawns no threads of its own, which is what keeps
/// that true. Never switch this to `multi_thread`, and never call
/// `spawn_blocking`.
fn run_daemon(config: Config) -> Result<()> {
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()?;

  let local = tokio::task::LocalSet::new();
  local.block_on(&runtime, server::main(config))
}

mod logging {
  use tracing_subscriber::EnvFilter;

  /// Logs to stderr only. systemd captures it into the journal, and a login
  /// manager has nowhere of its own to write before any filesystem is
  /// necessarily writable.
  pub fn init(name: &str) {
    let filter = EnvFilter::try_from_env("LAUNCH_GREETD_LOG")
      .unwrap_or_else(|_| EnvFilter::new("launch_greetd=info,warn"));

    tracing_subscriber::fmt()
      .with_env_filter(filter)
      .with_writer(std::io::stderr)
      .with_ansi(false)
      .with_target(false)
      .init();

    tracing::debug!(personality = name, "Starting");
  }
}
