use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use gpui::{App, Entity, Global, Task, prelude::*};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use tracing::{error, info, warn};

use crate::util::ResultExt;

/// User configuration loaded from `~/.config/launch/config.toml`.
///
/// The file is parsed with `toml_edit` so that a future settings GUI can edit
/// it while preserving comments and formatting. All fields default so a missing
/// or partial file still produces a usable config.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
  /// Name of the output that carries whatever there is only one of, e.g.
  /// `"eDP-1"` or `"HDMI-A-1"`. Sections that place something on a screen fall
  /// back to this when they name no output of their own. Being a plain key, it
  /// has to appear above the first `[section]` in the file.
  pub primary_display: Option<String>,
  pub notifications: NotificationsConfig,
  pub status: StatusConfig,
  pub lock: LockConfig,
  pub system: SystemConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
  /// Name of the output that notifications are shown on, e.g. `"DP-1"` or
  /// `"HDMI-A-1"`. When unset, [`Config::primary_display`] is used, and failing
  /// that the first available display.
  pub display: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct StatusConfig {
  /// Whether to show the clock at all - both the desktop overlay and the copy
  /// the lock screen draws while it covers that overlay up.
  pub enabled: bool,
  /// Name of the output the clock is shown on. When unset,
  /// [`Config::primary_display`] is used, and failing that the first available
  /// display.
  pub display: Option<String>,
  /// How strongly the clock is drawn, from invisible at `0.0` to solid white at
  /// `1.0`. It sits over whatever is on screen, so it is meant to be faint.
  pub opacity: f32,
}

impl Default for StatusConfig {
  fn default() -> Self {
    Self {
      enabled: true,
      display: None,
      opacity: 0.35,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct LockConfig {
  /// Name of the PAM service the lock screen authenticates against, i.e. the
  /// file in `/etc/pam.d`. When that file is missing a known interactive service
  /// is used instead, since PAM denies services it has no configuration for.
  pub pam_service: String,
  /// Whether to verify fingerprints through fprintd next to the password. The
  /// PAM service should not include `pam_fprintd` when this is on, as only one
  /// client at a time can use the reader.
  pub fingerprint: bool,
}

impl Default for LockConfig {
  fn default() -> Self {
    Self {
      pam_service: "launch".to_owned(),
      fingerprint: true,
    }
  }
}

/// Which column the process list is ordered by. Lives here rather than next to
/// the panel so that `config` stays a leaf module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortKey {
  Cpu,
  Memory,
  Name,
  Pid,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct SystemConfig {
  /// How often the system panel resamples while it is open. This is the
  /// expensive tier: it walks every entry in `/proc`.
  pub interval_ms: u64,
  /// How often CPU, memory and network are sampled while no panel is open.
  /// Processes are skipped in this tier, which leaves three small file reads, so
  /// it can run continuously to keep the graphs populated for the next time the
  /// panel is opened.
  pub idle_interval_ms: u64,
  /// How many samples the graphs keep. At the default interval this is a little
  /// under four minutes of idle history.
  pub history_samples: usize,
  /// Whether to list kernel threads next to real processes. They are numerous
  /// and rarely what someone opening a process list is looking for.
  pub show_kernel_threads: bool,
  /// Whether process CPU usage is divided across all cores. When off (the
  /// default, matching htop and btop) a thread saturating one core reads 100%;
  /// when on the same thread reads `100 / core_count`.
  pub normalize_cpu: bool,
  /// Whether processes sharing a name are collapsed into one expandable row.
  pub group_processes: bool,
  pub default_sort: SortKey,
}

impl Default for SystemConfig {
  fn default() -> Self {
    Self {
      interval_ms: 1000,
      idle_interval_ms: 2000,
      history_samples: 120,
      show_kernel_threads: false,
      normalize_cpu: false,
      group_processes: true,
      default_sort: SortKey::Cpu,
    }
  }
}

struct GlobalConfig(Entity<ConfigState>);

impl Global for GlobalConfig {}

pub fn init(cx: &mut App) {
  let entity = cx.new(ConfigState::new);
  cx.set_global(GlobalConfig(entity));
}

/// Holds the live config and keeps it in sync with the file on disk.
///
/// The file is watched via inotify (through the `notify` crate) rather than
/// polled. Reads of the changed file happen on a background thread; the parsed
/// result is applied on the foreground and triggers `cx.notify()` so observers
/// can react to live edits.
pub struct ConfigState {
  config: Config,
  path: Option<PathBuf>,
  _watcher: Option<RecommendedWatcher>,
  _watch_task: Task<()>,
}

impl ConfigState {
  fn new(cx: &mut Context<Self>) -> Self {
    let Some(path) = config_path() else {
      error!("Could not determine config directory; using default config");
      return Self {
        config: Config::default(),
        path: None,
        _watcher: None,
        _watch_task: Task::ready(()),
      };
    };

    if let Err(error) = ensure_file(&path) {
      error!(?error, "Failed to create config file");
    }

    let config = load(&path).log_err().unwrap_or_default();
    let (watcher, watch_task) = watch(path.clone(), cx);

    Self {
      config,
      path: Some(path),
      _watcher: watcher,
      _watch_task: watch_task,
    }
  }

  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalConfig>().0.clone()
  }

  /// Returns a snapshot of the current config.
  pub fn get(cx: &App) -> Config {
    cx.global::<GlobalConfig>().0.read(cx).config.clone()
  }
}

/// Watches the config file's parent directory for changes and reloads on edit.
///
/// The parent directory is watched rather than the file itself so that atomic
/// saves (write-to-temp then rename), which most editors perform, are still
/// observed after the original inode is replaced.
fn watch(path: PathBuf, cx: &mut Context<ConfigState>) -> (Option<RecommendedWatcher>, Task<()>) {
  let (sender, receiver) = flume::unbounded::<()>();

  let watcher = build_watcher(&path, sender).log_err();

  let task = cx.spawn(async move |this, cx| {
    while receiver.recv_async().await.is_ok() {
      let Ok(Some(path)) = this.read_with(cx, |this, _| this.path.clone()) else {
        break;
      };

      match cx.background_spawn(async move { load(&path) }).await {
        Ok(config) => {
          // Editors emit several write events per save, so only apply and log
          // when the parsed config actually differs from what we already hold.
          let result = this.update(cx, |this, cx| {
            if this.config == config {
              return false;
            }
            this.config = config;
            cx.notify();
            true
          });

          match result {
            Ok(true) => info!("Reloaded config"),
            Ok(false) => {}
            Err(_) => break,
          }
        }
        Err(error) => warn!(?error, "Failed to reload config"),
      }
    }
  });

  (watcher, task)
}

fn build_watcher(path: &Path, sender: flume::Sender<()>) -> Result<RecommendedWatcher> {
  let target = path.to_path_buf();
  let directory = path
    .parent()
    .context("config path has no parent directory")?
    .to_path_buf();

  let mut watcher =
    notify::recommended_watcher(move |result: notify::Result<Event>| match result {
      Ok(event) => {
        // Only react to writes. Reading the file to reload it emits inotify access
        // events, so reacting to those as well would trigger an endless loop.
        let is_write = matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_));
        if is_write
          && event.paths.iter().any(|changed| changed == &target)
          && sender.send(()).is_err()
        {
          // The receiver has been dropped, which only happens once the app is
          // shutting down. Nothing left to notify, so the failure is expected.
        }
      }
      Err(error) => error!(?error, "Config watch error"),
    })?;

  watcher.watch(&directory, RecursiveMode::NonRecursive)?;
  Ok(watcher)
}

fn load(path: &Path) -> Result<Config> {
  let content = std::fs::read_to_string(path)
    .with_context(|| format!("reading config file {}", path.display()))?;
  let config = toml_edit::de::from_str(&content)
    .with_context(|| format!("parsing config file {}", path.display()))?;
  Ok(config)
}

fn ensure_file(path: &Path) -> Result<()> {
  if path.exists() {
    return Ok(());
  }

  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)
      .with_context(|| format!("creating config directory {}", parent.display()))?;
  }

  std::fs::write(path, "").with_context(|| format!("creating config file {}", path.display()))?;
  Ok(())
}

/// The directory the config file lives in. It doubles as a drop-off point for
/// things the user supplies as files rather than settings, such as the profile
/// picture the lock screen shows.
pub fn config_dir() -> Option<PathBuf> {
  dirs::config_dir().map(|dir| dir.join("launch"))
}

fn config_path() -> Option<PathBuf> {
  config_dir().map(|dir| dir.join("config.toml"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn parse(content: &str) -> Config {
    toml_edit::de::from_str(content).expect("config should parse")
  }

  #[test]
  fn empty_config_uses_defaults() {
    let config = parse("");
    assert_eq!(config.primary_display, None);
    assert_eq!(config.notifications.display, None);
    assert_eq!(config.status, StatusConfig::default());
    assert_eq!(config.lock, LockConfig::default());
    assert_eq!(config.system, SystemConfig::default());
  }

  #[test]
  fn reads_system_section() {
    let config = parse(
      "[system]\ninterval_ms = 500\nidle_interval_ms = 5000\nhistory_samples = 60\nshow_kernel_threads = true\nnormalize_cpu = true\ngroup_processes = false\ndefault_sort = \"memory\"\n",
    );
    assert_eq!(config.system.interval_ms, 500);
    assert_eq!(config.system.idle_interval_ms, 5000);
    assert_eq!(config.system.history_samples, 60);
    assert!(config.system.show_kernel_threads);
    assert!(config.system.normalize_cpu);
    assert!(!config.system.group_processes);
    assert_eq!(config.system.default_sort, SortKey::Memory);
  }

  #[test]
  fn partial_system_section_keeps_other_defaults() {
    let defaults = SystemConfig::default();
    let config = parse("[system]\ninterval_ms = 250\n");
    assert_eq!(config.system.interval_ms, 250);
    assert_eq!(config.system.idle_interval_ms, defaults.idle_interval_ms);
    assert_eq!(config.system.default_sort, defaults.default_sort);
    assert_eq!(config.system.group_processes, defaults.group_processes);
  }

  #[test]
  fn reads_status_section() {
    let config = parse("[status]\nenabled = false\ndisplay = \"DP-1\"\nopacity = 0.5\n");
    assert!(!config.status.enabled);
    assert_eq!(config.status.display.as_deref(), Some("DP-1"));
    assert_eq!(config.status.opacity, 0.5);
  }

  #[test]
  fn partial_status_section_keeps_other_defaults() {
    let config = parse("[status]\nopacity = 0.1\n");
    assert!(config.status.enabled);
    assert_eq!(config.status.opacity, 0.1);
  }

  #[test]
  fn reads_primary_display() {
    let config = parse("primary_display = \"eDP-1\"\n[notifications]\n");
    assert_eq!(config.primary_display.as_deref(), Some("eDP-1"));
    assert_eq!(config.notifications.display, None);
  }

  #[test]
  fn reads_lock_section() {
    let config = parse("[lock]\npam_service = \"swaylock\"\nfingerprint = false\n");
    assert_eq!(config.lock.pam_service, "swaylock");
    assert!(!config.lock.fingerprint);
  }

  #[test]
  fn partial_lock_section_keeps_other_defaults() {
    let config = parse("[lock]\nfingerprint = false\n");
    assert_eq!(config.lock.pam_service, LockConfig::default().pam_service);
    assert!(!config.lock.fingerprint);
  }

  #[test]
  fn partial_section_uses_defaults() {
    let config = parse("[notifications]\n");
    assert_eq!(config.notifications.display, None);
  }

  #[test]
  fn reads_notification_display() {
    let config = parse("[notifications]\ndisplay = \"DP-1\"\n");
    assert_eq!(config.notifications.display.as_deref(), Some("DP-1"));
  }

  #[test]
  fn ensure_file_creates_missing_file_and_parents() {
    let dir = std::env::temp_dir().join(format!("launch-config-test-{}", std::process::id()));
    let path = dir.join("nested").join("config.toml");

    let _ = std::fs::remove_dir_all(&dir);
    ensure_file(&path).expect("file should be created");
    assert!(path.exists());

    // Calling again on an existing file leaves it untouched.
    std::fs::write(&path, "[notifications]\ndisplay = \"eDP-1\"\n").unwrap();
    ensure_file(&path).expect("existing file should be left alone");

    let config = load(&path).expect("written config should load");
    assert_eq!(config.notifications.display.as_deref(), Some("eDP-1"));

    std::fs::remove_dir_all(&dir).unwrap();
  }
}
