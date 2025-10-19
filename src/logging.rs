use tracing::level_filters::LevelFilter;

#[cfg(debug_assertions)]
const DEFAULT_LOG_LEVEL: &str = "debug";

#[cfg(not(debug_assertions))]
const DEFAULT_LOG_LEVEL: &str = "info";

pub fn init() {
  let env = format!("{}_LOG", env!("CARGO_CRATE_NAME").to_uppercase());

  let filter = tracing_subscriber::EnvFilter::try_from_env(env).unwrap_or_else(|_| {
    tracing_subscriber::EnvFilter::default()
      .add_directive(
        format!("{}={}", env!("CARGO_CRATE_NAME"), DEFAULT_LOG_LEVEL)
          .parse()
          .unwrap(),
      )
      .add_directive(LevelFilter::WARN.into())
  });

  tracing_subscriber::fmt().with_env_filter(filter).init();
}
