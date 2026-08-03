//! Daemon configuration, read once at startup from a root-owned TOML file.
//!
//! Everything the greeter is allowed to influence arrives over the socket; what
//! is in here - which command becomes the session, which PAM stacks are used -
//! is deliberately out of its reach.

use std::fmt;
use std::path::Path;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Deserializer, de};
use tracing::warn;

pub const DEFAULT_PATH: &str = "/etc/launch/greetd.toml";

/// Where PAM looks for service files, in its own lookup order. A service with
/// no file falls through to `other`, which denies everything, so a missing file
/// has to be caught before it turns into a login screen that rejects the
/// correct password.
const PAM_SERVICE_DIRS: &[&str] = &["/etc/pam.d", "/usr/lib/pam.d"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
  pub terminal: TerminalConfig,
  pub greeter: GreeterConfig,
  pub session: SessionConfig,
  #[serde(default)]
  pub general: GeneralConfig,
  #[serde(default)]
  pub users: UsersConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalConfig {
  /// Which VT to run on: `"next"`, `"current"`, `"none"`, or a number.
  pub vt: VtSelection,
  /// Whether to switch to that VT, or to wait until something else does.
  #[serde(default = "yes")]
  pub switch: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GreeterConfig {
  /// Unprivileged user the greeter session runs as. Also owns the IPC socket -
  /// that ownership is the entire access control on it.
  pub user: String,
  /// Shell command line that starts the compositor hosting the login screen.
  pub command: String,
  /// PAM service for the greeter session. Its auth stack is never run, so it
  /// only needs an account and session half.
  pub service: String,
  /// Output the prompt is drawn on, e.g. `"eDP-1"`. The greeter has no config
  /// of its own to read this from, so it is told. `None` leaves the choice to
  /// the greeter, which takes the first output.
  #[serde(default)]
  pub primary_output: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
  /// Shell command line run as the user who logs in.
  pub command: String,
  /// PAM service for password authentication.
  pub service: String,
  /// PAM service whose auth stack is `pam_fprintd` and nothing else. Run in a
  /// second worker so it can block on the reader without holding up the
  /// password.
  #[serde(default = "fingerprint_service")]
  pub fingerprint_service: String,
  /// Turned off automatically when [`Self::fingerprint_service`] has no file.
  #[serde(default = "yes")]
  pub fingerprint: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralConfig {
  /// Whether to source `/etc/profile` and `~/.profile` before the session
  /// command, the way a login shell would.
  #[serde(default = "yes")]
  pub source_profile: bool,
  /// Seat the session is on. greetd hardcodes `seat0`; it is a setting here
  /// because multi-seat machines exist. Omitted entirely when there is no VT.
  #[serde(default = "seat0")]
  pub seat: String,
  /// `XDG_SESSION_TYPE` for the session. greetd never sets it, which leaves
  /// logind reporting `tty` for a Wayland session and portals picking the wrong
  /// backend.
  #[serde(default = "wayland")]
  pub session_type: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsersConfig {
  /// Range of uids treated as human accounts.
  #[serde(default = "min_uid")]
  pub minimum_uid: u32,
  #[serde(default = "max_uid")]
  pub maximum_uid: u32,
  /// Accounts to offer regardless of their uid.
  #[serde(default)]
  pub include: Vec<String>,
  /// Accounts to hide even when their uid is in range.
  #[serde(default)]
  pub exclude: Vec<String>,
  /// Account selected when the login screen appears. Falls back to the first
  /// one on offer when unset or not among them.
  #[serde(default)]
  pub default: Option<String>,
}

fn yes() -> bool {
  true
}

fn seat0() -> String {
  "seat0".to_owned()
}

fn wayland() -> String {
  "wayland".to_owned()
}

fn fingerprint_service() -> String {
  "launch-greeter-fingerprint".to_owned()
}

fn min_uid() -> u32 {
  1000
}

fn max_uid() -> u32 {
  60000
}

impl Default for GeneralConfig {
  fn default() -> Self {
    Self {
      source_profile: true,
      seat: seat0(),
      session_type: wayland(),
    }
  }
}

impl Default for UsersConfig {
  fn default() -> Self {
    Self {
      minimum_uid: min_uid(),
      maximum_uid: max_uid(),
      include: Vec::new(),
      exclude: Vec::new(),
      default: None,
    }
  }
}

/// Which VT the session gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtSelection {
  /// The first unused one, via `VT_OPENQRY`.
  Next,
  /// Whichever is active now.
  Current,
  /// Don't touch VTs at all; the session inherits our stdio.
  None,
  Specific(u32),
}

impl fmt::Display for VtSelection {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Next => formatter.write_str("next"),
      Self::Current => formatter.write_str("current"),
      Self::None => formatter.write_str("none"),
      Self::Specific(vt) => write!(formatter, "{vt}"),
    }
  }
}

impl std::str::FromStr for VtSelection {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "next" => Ok(Self::Next),
      "current" => Ok(Self::Current),
      "none" => Ok(Self::None),
      other => other.parse::<u32>().map(Self::Specific).map_err(|_| {
        format!("expected \"next\", \"current\", \"none\" or a number, got {other:?}")
      }),
    }
  }
}

/// Accepts both `vt = 1` and `vt = "next"`, since the two spellings are equally
/// natural in TOML and getting it wrong is a boot-time failure.
impl<'de> Deserialize<'de> for VtSelection {
  fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
      Number(u32),
      Text(String),
    }

    match Raw::deserialize(deserializer)? {
      Raw::Number(vt) => Ok(VtSelection::Specific(vt)),
      Raw::Text(text) => text.parse().map_err(de::Error::custom),
    }
  }
}

impl Config {
  pub fn load(path: &Path) -> Result<Self> {
    let text = std::fs::read_to_string(path)
      .with_context(|| format!("reading the configuration at {}", path.display()))?;

    toml::from_str(&text)
      .with_context(|| format!("parsing the configuration at {}", path.display()))
  }

  /// Rejects a configuration that would produce a login screen nobody can log
  /// in through. Runs before the greeter starts, so the failure lands on the
  /// console rather than behind a surface that never appears.
  ///
  /// Turns the fingerprint path off rather than failing when only its PAM
  /// service is missing: password login still works, and that is what the
  /// greeter is told through `Welcome`.
  pub fn validate(&mut self) -> Result<()> {
    if self.greeter.command.trim().is_empty() {
      bail!("greeter.command is empty");
    }

    if self.session.command.trim().is_empty() {
      bail!("session.command is empty");
    }

    if uzers::get_user_by_name(&self.greeter.user).is_none() {
      bail!("greeter.user {:?} does not exist", self.greeter.user);
    }

    if !pam_service_exists(&self.session.service) {
      bail!(
        "no PAM service file for session.service {:?}; PAM would deny every login",
        self.session.service
      );
    }

    // greetd falls back to the session stack here rather than failing, and the
    // same reasoning applies: the greeter's auth stack is never run, so the
    // session's works for it.
    if !pam_service_exists(&self.greeter.service) {
      warn!(
        service = self.greeter.service,
        fallback = self.session.service,
        "No PAM service file for the greeter, falling back"
      );

      self.greeter.service = self.session.service.clone();
    }

    if self.session.fingerprint && !pam_service_exists(&self.session.fingerprint_service) {
      warn!(
        service = self.session.fingerprint_service,
        "No PAM service file for fingerprint authentication, disabling it"
      );

      self.session.fingerprint = false;
    }

    Ok(())
  }
}

fn pam_service_exists(service: &str) -> bool {
  PAM_SERVICE_DIRS
    .iter()
    .any(|directory| Path::new(directory).join(service).exists())
}

#[cfg(test)]
mod tests {
  use super::*;

  const MINIMAL: &str = r#"
    [terminal]
    vt = 1

    [greeter]
    user = "launch-greeter"
    command = "niri -c /etc/launch/greeter.kdl"
    service = "launch-greeter"

    [session]
    command = "niri --session"
    service = "launch-login"
  "#;

  #[test]
  fn parses_a_minimal_configuration() {
    let config: Config = toml::from_str(MINIMAL).expect("parses");

    assert_eq!(config.terminal.vt, VtSelection::Specific(1));
    assert!(config.terminal.switch);
    assert_eq!(config.general.seat, "seat0");
    assert_eq!(config.general.session_type, "wayland");
    assert!(config.general.source_profile);
    assert_eq!(config.users.minimum_uid, 1000);
  }

  #[test]
  fn accepts_both_spellings_of_vt() {
    for (text, expected) in [
      ("\"next\"", VtSelection::Next),
      ("\"current\"", VtSelection::Current),
      ("\"none\"", VtSelection::None),
      ("2", VtSelection::Specific(2)),
      ("\"2\"", VtSelection::Specific(2)),
    ] {
      let document = MINIMAL.replace("vt = 1", &format!("vt = {text}"));
      let config: Config = toml::from_str(&document).expect("parses");
      assert_eq!(config.terminal.vt, expected, "for {text}");
    }
  }

  #[test]
  fn rejects_an_unparseable_vt() {
    let document = MINIMAL.replace("vt = 1", "vt = \"sideways\"");
    assert!(toml::from_str::<Config>(&document).is_err());
  }

  #[test]
  fn rejects_unknown_keys() {
    let document = format!("{MINIMAL}\n[nonsense]\nkey = 1\n");
    assert!(toml::from_str::<Config>(&document).is_err());
  }
}
