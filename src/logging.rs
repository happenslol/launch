use std::fs;
use std::io;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(debug_assertions)]
const DEFAULT_LOG_LEVEL: &str = "debug";

#[cfg(not(debug_assertions))]
const DEFAULT_LOG_LEVEL: &str = "info";

/// Sets up logging to `$XDG_STATE_HOME/launch/log` and stderr.
///
/// Writes go out synchronously. `tracing_appender::non_blocking` would hand them
/// to a worker thread, and threads don't survive the `fork` that daemonizes us -
/// which silently swallowed everything the daemon ever logged.
pub fn init() {
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

  let log_dir = dirs::state_dir()
    .expect("Failed to get state dir")
    .join(env!("CARGO_CRATE_NAME"))
    .join("log");

  fs::create_dir_all(&log_dir).expect("Failed to create log directory");

  let appender = tracing_appender::rolling::daily(&log_dir, "launch.log");
  let file_layer = fmt::layer().with_writer(appender).with_ansi(false);
  let stderr_layer = fmt::layer().with_writer(io::stderr).with_ansi(true);

  tracing_subscriber::registry()
    .with(filter)
    .with(file_layer)
    .with(stderr_layer)
    .init();
}
