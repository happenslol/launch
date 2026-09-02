mod api;

use std::{
  path::Path,
  sync::atomic::{AtomicU64, Ordering},
  time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context as _, Result};
use tracing::debug;
use zvariant::Value;

/// How much of an app id a unit name may carry.
///
/// Unit names are capped at 255 bytes and the rest of what is built below is
/// well under a hundred, so this only ever truncates an id far longer than any
/// real one.
const MAX_ID_LENGTH: usize = 128;

/// Starts a program as a transient user service.
///
/// The point of going through the manager rather than forking the program here
/// is that it then belongs to the manager instead of to us: it lands in its own
/// cgroup under `app.slice` rather than inside the launcher's unit, so stopping
/// or replacing the launcher no longer takes down every app started from it,
/// and it is executed with the session environment the manager holds rather
/// than whatever the launcher was started with.
///
/// `program` has to be the absolute path to execute, since systemd resolves
/// nothing itself. `arguments` is the whole `argv`, its first entry included,
/// which is how the program name can stay as the entry wrote it while the path
/// it resolved to is what gets executed.
pub async fn start_app(
  connection: &zbus::Connection,
  app_id: &str,
  program: &Path,
  arguments: &[String],
  working_directory: Option<&str>,
) -> Result<()> {
  let program = program
    .to_str()
    .with_context(|| format!("Program path {program:?} is not valid UTF-8"))?;

  let proxy = api::ManagerProxy::new(connection).await?;
  let unit = unit_name(app_id);

  let mut properties = vec![
    ("Description", Value::from(format!("launch: {app_id}"))),
    // Where the manager expects units that run something on the user's behalf,
    // as opposed to the session's own services.
    ("Slice", Value::from("app.slice")),
    ("ExecStart", exec_start(program, arguments)),
    // A transient unit is not forgotten on its own once it has run: without
    // this, every app ever started would stay listed, and a second launch of
    // one would collide with the leftover of the first.
    ("CollectMode", Value::from("inactive-or-failed")),
    // Plenty of apps fork and let the process that was executed exit. Judging
    // the unit by its cgroup instead of by that one process keeps systemd from
    // treating such an app as finished and killing what it left behind.
    ("ExitType", Value::from("cgroup")),
  ];

  if let Some(directory) = working_directory.filter(|directory| !directory.is_empty()) {
    properties.push(("WorkingDirectory", Value::from(directory.to_string())));
  }

  let job = proxy
    .start_transient_unit(&unit, "fail", &properties, &[])
    .await
    .with_context(|| format!("Starting transient unit `{unit}`"))?;

  debug!(unit, %job, app_id, "Started app as a transient unit");

  Ok(())
}

/// The one command the unit runs, as the `a(sasb)` systemd expects: what to
/// execute, the `argv` to execute it with, and whether a failure to start it may
/// be ignored - which it may not, since it is the only thing the unit is for.
fn exec_start(program: &str, arguments: &[String]) -> Value<'static> {
  Value::from(vec![(program.to_string(), arguments.to_vec(), false)])
}

/// The unit name for one launch of `app_id`.
///
/// `app-` is the prefix systemd reserves for units running something on the
/// user's behalf, and the launcher's own name after it says who started this
/// one. Two things share the suffix: a counter, so launching an app that is
/// already running does not collide with it, and the time, so that neither does
/// launching one whose still-running copy came from an earlier launcher - which
/// is the whole point of starting apps this way.
fn unit_name(app_id: &str) -> String {
  static LAUNCHES: AtomicU64 = AtomicU64::new(0);

  let launch = LAUNCHES.fetch_add(1, Ordering::Relaxed);
  let now = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map_or(0, |since_epoch| since_epoch.as_nanos());

  // A desktop file's id is a file name and can hold anything; a unit name takes
  // a restricted alphabet. Nothing here has to be read back, so a character
  // that is not allowed is simply stood in for.
  let id: String = app_id
    .chars()
    .take(MAX_ID_LENGTH)
    .map(|character| match character {
      character if character.is_ascii_alphanumeric() => character,
      '_' | '.' => character,
      _ => '_',
    })
    .collect();

  format!("app-launch-{id}-{now:x}-{launch:x}.service")
}

#[cfg(test)]
mod tests {
  use super::*;

  /// systemd rejects the whole call if any property is not of the type it
  /// expects, and `ExecStart` is the only one here that is not a plain string.
  #[test]
  fn exec_start_is_built_with_the_signature_systemd_expects() {
    let value = exec_start("/run/current-system/sw/bin/sh", &["sh".to_string()]);
    assert_eq!(value.value_signature().to_string(), "a(sasb)");
  }

  #[test]
  fn unit_names_keep_an_app_id_that_is_already_allowed() {
    let name = unit_name("org.gnome.Nautilus");
    assert!(name.starts_with("app-launch-org.gnome.Nautilus-"), "{name}");
    assert!(name.ends_with(".service"), "{name}");
  }

  #[test]
  fn unit_names_stand_in_for_characters_a_unit_name_cannot_hold() {
    let name = unit_name("my app/v2-final");
    assert!(name.starts_with("app-launch-my_app_v2_final-"), "{name}");
  }

  #[test]
  fn unit_names_of_one_app_differ_between_launches() {
    let app_id = "org.gnome.Nautilus";
    assert_ne!(unit_name(app_id), unit_name(app_id));
  }

  #[test]
  fn unit_names_stay_within_what_systemd_accepts() {
    let name = unit_name(&"a".repeat(4096));
    assert!(name.len() < 256, "{} bytes", name.len());
  }
}
