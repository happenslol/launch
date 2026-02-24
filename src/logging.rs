use std::fs;

use tracing::level_filters::LevelFilter;
use tracing_appender::{non_blocking, non_blocking::WorkerGuard};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(debug_assertions)]
const DEFAULT_LOG_LEVEL: &str = "debug";

#[cfg(not(debug_assertions))]
const DEFAULT_LOG_LEVEL: &str = "info";

/// Must be held for the entire runtime to ensure all logs are flushed before exit.
pub struct Guard {
  _file: WorkerGuard,
  _stderr: WorkerGuard,
}

#[must_use]
pub fn init() -> Guard {
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
      .add_directive("resvg=error".parse().expect("Failed to parse log directive"))
      // Hide irrelevant warnings from vulkan
      .add_directive(
        "wgpu_hal::vulkan::instance=error"
          .parse()
          .expect("Failed to parse log directive"),
      )
      .add_directive(LevelFilter::WARN.into())
  });

  let log_dir = dirs::state_dir()
    .expect("Failed to get state dir")
    .join(env!("CARGO_CRATE_NAME"))
    .join("log");

  fs::create_dir_all(&log_dir).expect("Failed to create log directory");

  let appender = tracing_appender::rolling::daily(&log_dir, "launch.log");
  let (file_writer, file_guard) = non_blocking(appender);
  let file_layer = fmt::layer().with_writer(file_writer).with_ansi(false);

  let (stderr_writer, stderr_guard) = non_blocking(std::io::stderr());
  let stderr_layer = fmt::layer().with_writer(stderr_writer).with_ansi(true);

  tracing_subscriber::registry()
    .with(filter)
    .with(file_layer)
    .with(stderr_layer)
    .init();

  Guard {
    _file: file_guard,
    _stderr: stderr_guard,
  }
}
