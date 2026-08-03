//! The protocol between `launch-greetd` and the `launch greet` login screen.
//!
//! The daemon runs two PAM workers side by side - one for the password, one for
//! the fingerprint reader - and either can produce a message at any time. A
//! request/response protocol can't express that, so this one is one-directional
//! in both directions: the greeter sends [`Request`]s and never waits for a
//! reply, and every outcome, including the failure of a request, comes back as
//! an [`Event`].
//!
//! Frames are a little-endian `u32` length followed by JSON. The length is
//! checked against [`MAX_FRAME_LEN`] before anything is allocated, since the
//! peer is only as trustworthy as the socket permissions make it.

pub mod codec;
pub mod user;

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// Bumped when a change would make an old greeter misread a new daemon. The
/// two ship in the same package, so this only catches a stale binary left
/// running across an upgrade.
pub const PROTOCOL_VERSION: u32 = 1;

/// Environment variable naming the daemon's socket, set for the greeter session
/// and nothing else.
///
/// Deliberately not `GREETD_SOCK`: this is a different protocol, and a greeter
/// that picked up a leftover greetd socket would fail in a thoroughly confusing
/// way.
pub const SOCKET_ENV_VAR: &str = "LAUNCH_GREETD_SOCK";

/// Longest frame either side will send or accept.
///
/// Every message is a handful of short strings plus, in `Welcome`, one entry
/// per user, so this is orders of magnitude more than a real frame needs. It
/// exists to bound the allocation made from a length prefix that hasn't been
/// validated yet.
pub const MAX_FRAME_LEN: u32 = 64 * 1024;

/// Longest password accepted. PAM stacks reject far shorter than this; the
/// bound is here so an oversized secret can never reach the fixed-size datagram
/// the daemon uses to talk to its workers.
pub const MAX_SECRET_LEN: usize = 1024;

/// Longest username accepted, for the same reason.
pub const MAX_USERNAME_LEN: usize = 256;

/// A password on its way to a PAM worker.
///
/// Wipes itself on drop and refuses to print itself, so it can be put in a
/// `tracing` field or a `Debug` derive without leaking. Serializes as a plain
/// string, since that is what it is on the wire.
#[derive(Clone, Serialize, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
  /// Fails when the value is longer than [`MAX_SECRET_LEN`].
  pub fn new(value: String) -> Result<Self, TooLong> {
    if value.len() > MAX_SECRET_LEN {
      return Err(TooLong);
    }

    Ok(Self(value))
  }

  pub fn expose(&self) -> &str {
    &self.0
  }
}

impl fmt::Debug for Secret {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("Secret(<redacted>)")
  }
}

#[derive(Debug, thiserror::Error)]
#[error("value is too long")]
pub struct TooLong;

/// A user the greeter can offer to log in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcUser {
  /// Login name, i.e. what PAM authenticates.
  pub name: String,
  pub display_name: String,
  /// Path to a copy of the user's avatar inside the greeter's own home, or
  /// `None` when they have none or it couldn't be read. The greeter cannot read
  /// other users' homes, so the daemon puts a copy somewhere it can.
  pub avatar: Option<PathBuf>,
}

/// Which of the two concurrent PAM workers a message came from, or which one
/// a response is meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
  Password,
  Fingerprint,
}

/// What the fingerprint reader is doing. Mirrors the lock screen's own states
/// so both screens can render from the same value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FingerprintState {
  /// Not in use: no reader, no enrolled prints, or it has given up.
  Off,
  /// Being claimed. Nothing to show the user yet.
  Starting,
  /// Armed and waiting for a finger.
  Waiting,
  /// A finger is on the sensor.
  Reading,
}

/// Why an attempt didn't go through.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthFailure {
  /// The credentials were wrong. There is nothing to say about it that the
  /// field they were typed into doesn't already show.
  Rejected,
  /// Something else stood in the way - an expired account, a stack that can't
  /// run. Carries the reason, phrased for display.
  Error { message: String },
}

/// Greeter to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
  /// First message on the connection.
  Hello { version: u32 },
  /// Begin authenticating this user, starting both workers. Sent again when the
  /// user switches, which abandons whatever was in flight.
  Authenticate { username: String },
  /// Answer the password worker's prompt.
  Password { value: Secret },
  /// Abandon the current attempt without starting another.
  Cancel,
  /// Hand off: start the session for whichever worker authenticated. Only
  /// meaningful after [`Event::Authenticated`].
  StartSession,
}

/// Daemon to greeter.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
  /// Answer to `Hello`, and everything the greeter needs to draw itself.
  Welcome {
    version: u32,
    users: Vec<IpcUser>,
    /// Who to select on arrival. Always one of `users`.
    default_user: String,
    /// Whether a fingerprint worker will be started at all.
    fingerprint: bool,
    /// Output that carries the prompt, e.g. `"eDP-1"`. `None` leaves the choice
    /// to the greeter.
    primary_output: Option<String>,
  },
  /// A worker is asking for input. `echo` distinguishes a username-style prompt
  /// from a password one.
  Prompt {
    source: AuthSource,
    echo: bool,
  },
  /// An informational message from a PAM stack, e.g. "Swipe your finger".
  Info {
    source: AuthSource,
    message: String,
  },
  /// An error message from a PAM module.
  Error {
    source: AuthSource,
    message: String,
  },
  Fingerprint {
    state: FingerprintState,
  },
  /// This worker turned the attempt down. The other one, if any, is unaffected.
  Failed {
    source: AuthSource,
    failure: AuthFailure,
    /// Whether this path is armed again and will send a fresh [`Event::Prompt`].
    ///
    /// A rejected password re-arms in place, so the greeter waits for the new
    /// prompt; anything else leaves the path dead, and the greeter has to ask
    /// for a whole new attempt. Without this the greeter would have to infer
    /// which happened from the failure kind, and guessing wrong means either a
    /// login screen waiting for a prompt nobody will send, or one that discards
    /// a live fingerprint worker on every typo.
    retry: bool,
  },
  /// PAM accepted. The greeter should send [`Request::StartSession`].
  Authenticated {
    via: AuthSource,
  },
  /// The session is starting and the greeter is about to be torn down; it
  /// should quit so the daemon doesn't have to evict it.
  SessionStarted,
  /// The session command could not be started. The greeter stays up, because a
  /// dead login screen is worse than a failed login.
  SessionFailed {
    message: String,
  },
  /// A request could not be carried out - it arrived in a state that made no
  /// sense, or named something that does not exist. Surfaced rather than only
  /// logged, so a greeter that has got out of step says so instead of sitting
  /// there looking ready.
  RequestFailed {
    message: String,
  },
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn secret_redacts_itself() {
    let secret = Secret::new("hunter2".to_owned()).expect("within the length bound");
    assert_eq!(format!("{secret:?}"), "Secret(<redacted>)");
    assert!(!format!("{secret:?}").contains("hunter2"));
  }

  #[test]
  fn secret_rejects_oversized_values() {
    assert!(Secret::new("a".repeat(MAX_SECRET_LEN)).is_ok());
    assert!(Secret::new("a".repeat(MAX_SECRET_LEN + 1)).is_err());
  }

  #[test]
  fn secret_is_transparent_on_the_wire() {
    let secret = Secret::new("hunter2".to_owned()).expect("within the length bound");
    let encoded = serde_json::to_string(&secret).expect("serializes");
    assert_eq!(encoded, "\"hunter2\"");
  }
}
