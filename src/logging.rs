use std::fs;
use std::io;
use std::path::PathBuf;

use tracing::level_filters::LevelFilter;
use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::{fmt, layer::SubscriberExt, reload, util::SubscriberInitExt};

#[cfg(debug_assertions)]
const DEFAULT_LOG_LEVEL: &str = "debug";

#[cfg(not(debug_assertions))]
const DEFAULT_LOG_LEVEL: &str = "info";

/// Must be held for the entire runtime to ensure all logs are flushed before exit.
#[must_use]
pub struct Guard {
  _file: WorkerGuard,
  _stderr: WorkerGuard,
}

/// Replaces the writer of one layer, boxed so callers never have to name the
/// layer and subscriber types a [`reload::Handle`] is generic over.
type ReloadWriter = Box<dyn Fn(NonBlocking) -> Result<(), reload::Error>>;

/// Swaps out the writers the subscriber logs through, which is what
/// [`reinit_after_fork`] needs to give them new worker threads.
pub struct Reload {
  file: ReloadWriter,
  stderr: ReloadWriter,
}

pub fn init() -> (Guard, Reload) {
  let filter = tracing_subscriber::EnvFilter::try_from_env(format!(
    "{}_LOG",
    env!("CARGO_CRATE_NAME").to_uppercase()
  ))
  .unwrap_or_else(|_| {
    tracing_subscriber::EnvFilter::default()
      .add_directive(
        format!("{}={}", env!("CARGO_CRATE_NAME"), DEFAULT_LOG_LEVEL)
          .parse()
          .expect("Failed to parse log directive"),
      )
      // Hide warnings when invalid SVGs are parsed
      .add_directive("usvg=error".parse().expect("Failed to parse log directive"))
      .add_directive(
        "resvg=error"
          .parse()
          .expect("Failed to parse log directive"),
      )
      // Hide irrelevant warnings from vulkan
      .add_directive(
        "wgpu_hal::vulkan::instance=error"
          .parse()
          .expect("Failed to parse log directive"),
      )
      .add_directive(LevelFilter::WARN.into())
  });

  let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender());
  let (file_layer, file_handle) =
    reload::Layer::new(fmt::layer().with_writer(file_writer).with_ansi(false));

  let (stderr_writer, stderr_guard) = tracing_appender::non_blocking(io::stderr());
  let (stderr_layer, stderr_handle) =
    reload::Layer::new(fmt::layer().with_writer(stderr_writer).with_ansi(true));

  tracing_subscriber::registry()
    .with(filter)
    .with(file_layer)
    .with(stderr_layer)
    .init();

  let reload = Reload {
    file: Box::new(move |writer| {
      file_handle.reload(fmt::layer().with_writer(writer).with_ansi(false))
    }),
    stderr: Box::new(move |writer| {
      stderr_handle.reload(fmt::layer().with_writer(writer).with_ansi(true))
    }),
  };

  let guard = Guard {
    _file: file_guard,
    _stderr: stderr_guard,
  };

  (guard, reload)
}

/// Gives the log writers fresh worker threads, to be called in the child right
/// after daemonizing.
///
/// `fork` carries only the calling thread into the child, so the threads
/// `tracing_appender` started in the parent don't exist there and everything the
/// daemon logs would queue up unwritten. The subscriber itself is plain data and
/// comes across fine; only the writers underneath it have to be replaced, and
/// the parent's [`Guard`] has to be forgotten rather than dropped, since it
/// would wait on threads that were never here.
pub fn reinit_after_fork(reload: &Reload) -> Guard {
  let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender());
  let (stderr_writer, stderr_guard) = tracing_appender::non_blocking(io::stderr());

  // Nothing can be logged about a failure to install the log writers, so this
  // is one of the few places that has to write to stderr directly.
  if let Err(error) = (reload.file)(file_writer) {
    eprintln!("Failed to reinstall the log file writer: {error}");
  }

  if let Err(error) = (reload.stderr)(stderr_writer) {
    eprintln!("Failed to reinstall the stderr writer: {error}");
  }

  Guard {
    _file: file_guard,
    _stderr: stderr_guard,
  }
}

/// The daily log file, or a sink when there is nowhere to put one.
///
/// The greeter runs as its own system user, whose home may be unwritable or, if
/// `HOME` is unset, absent entirely - and this runs before anything has been
/// drawn. Panicking there would take down the login screen and leave no log
/// saying why, so a missing log directory degrades to stderr-only instead.
///
/// Boxed so the caller gets one type either way, which keeps the reload handles
/// and [`Guard`] below unchanged.
fn file_appender() -> Box<dyn io::Write + Send> {
  let Some(log_dir) = log_dir() else {
    // Too early for `tracing` - the subscriber this feeds isn't installed yet.
    eprintln!("No state directory available, logging to stderr only");
    return Box::new(io::sink());
  };

  if let Err(error) = fs::create_dir_all(&log_dir) {
    eprintln!(
      "Failed to create log directory {}, logging to stderr only: {error}",
      log_dir.display()
    );

    return Box::new(io::sink());
  }

  Box::new(tracing_appender::rolling::daily(&log_dir, "launch.log"))
}

fn log_dir() -> Option<PathBuf> {
  Some(
    dirs::state_dir()?
      .join(env!("CARGO_CRATE_NAME"))
      .join("log"),
  )
}
