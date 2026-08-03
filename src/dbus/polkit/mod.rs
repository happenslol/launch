//! polkit authentication agent.
//!
//! Registers a `org.freedesktop.PolicyKit1.AuthenticationAgent` on the system
//! bus for the current login session. When polkit needs to authorize a
//! privileged action it calls [`PolkitAgent::begin_authentication`], which
//! drives the `polkit-agent-helper-1` PAM conversation and forwards prompts to
//! the UI over an [`AgentEvent`] channel. The helper is the only component
//! allowed to talk to PAM's setuid machinery, so the password itself is written
//! straight back to it and never persisted here.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;

use anyhow::Result;
use async_net::unix::UnixStream;
use async_process::Command;
use flume::{Receiver, Sender};
use futures::{
  FutureExt as _, StreamExt as _,
  io::{AsyncBufReadExt as _, AsyncRead, AsyncWrite, AsyncWriteExt as _, BufReader},
  select_biased,
};
use gpui::{App, AsyncApp, SharedString};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use zvariant::Type;

use crate::dbus::GlobalDbusConnection;
use crate::util::ResultExt;

const OBJECT_PATH: &str = "/lol/happens/launch/PolkitAgent";

/// Socket exposed by modern polkit (>=126). Connecting to it makes systemd
/// socket-activate `polkit-agent-helper-1 --socket-activated` as root, avoiding
/// the need for a setuid helper binary. This is what upstream `libpolkit-agent`
/// prefers, falling back to spawning the setuid helper if the socket is absent.
const AGENT_HELPER_SOCKET: &str = "/run/polkit/agent-helper.socket";

/// A message from the agent (running on the D-Bus executor) to the UI event
/// loop on the foreground thread.
pub enum AgentEvent {
  /// A new authentication session started. The `cancel` sender lets the UI
  /// abort the attempt (e.g. the user pressed escape or closed the dialog).
  Begin {
    message: SharedString,
    cancel: Sender<()>,
  },
  /// The helper is asking for a secret. `echo` is true for `PAM_PROMPT_ECHO_ON`
  /// (a visible prompt, e.g. a one-time code) and false for a masked password.
  /// The reply is sent back over `reply`.
  Prompt {
    prompt: SharedString,
    echo: bool,
    reply: Sender<String>,
  },
  /// A PAM error message, such as a failed previous attempt.
  Error { message: SharedString },
  /// A PAM informational message, such as fingerprint reader instructions.
  Info { message: SharedString },
  /// The session ended (success, failure or cancellation); dismiss the dialog.
  Close,
}

#[zbus::proxy(
  default_service = "org.freedesktop.login1",
  interface = "org.freedesktop.login1.Session",
  default_path = "/org/freedesktop/login1/session/auto"
)]
trait LogindSession {
  #[zbus(property)]
  fn id(&self) -> zbus::Result<String>;
}

#[zbus::proxy(
  default_service = "org.freedesktop.PolicyKit1",
  interface = "org.freedesktop.PolicyKit1.Authority",
  default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait PolkitAuthority {
  fn register_authentication_agent(
    &self,
    subject: Subject<'_>,
    locale: &str,
    object_path: &str,
  ) -> zbus::Result<()>;

  fn unregister_authentication_agent(
    &self,
    subject: Subject<'_>,
    object_path: &str,
  ) -> zbus::Result<()>;
}

#[derive(Serialize, Type)]
pub struct Subject<'a> {
  subject_kind: &'a str,
  subject_details: HashMap<&'a str, zvariant::Value<'a>>,
}

#[derive(Debug, Serialize, Deserialize, Type)]
pub struct Identity<'a> {
  identity_kind: &'a str,
  identity_details: HashMap<&'a str, zvariant::Value<'a>>,
}

// The full error set mirrors the polkit spec so the D-Bus error names map
// correctly; we only ever construct `Failed` and `Cancelled` ourselves.
#[allow(dead_code)]
#[derive(Clone, Debug, zbus::DBusError)]
#[zbus(prefix = "org.freedesktop.PolicyKit1.Error")]
pub enum PolkitError {
  Failed,
  Cancelled,
  NotSupported,
  NotAuthorized,
  CancellationIdNotUnique,
}

/// The currently running authentication attempt. Stored so a concurrent
/// `CancelAuthentication` call can find the matching attempt and abort it.
struct Attempt {
  cookie: String,
  cancel: Sender<()>,
}

struct PolkitAgent {
  events: Sender<AgentEvent>,
  attempt: Mutex<Option<Attempt>>,
}

#[zbus::interface(name = "org.freedesktop.PolicyKit1.AuthenticationAgent")]
impl PolkitAgent {
  async fn begin_authentication(
    &self,
    _action_id: String,
    message: String,
    _icon_name: String,
    _details: HashMap<String, String>,
    cookie: String,
    identities: Vec<Identity<'_>>,
  ) -> Result<(), PolkitError> {
    info!(%message, "polkit authentication requested");

    if self.attempt().is_some() {
      error!("polkit authentication already in progress");
      return Err(PolkitError::Failed);
    }

    let Some(username) = select_username_from_identities(&identities) else {
      error!("no usable identity for polkit authentication");
      return Err(PolkitError::Failed);
    };

    let (cancel, cancel_rx) = flume::unbounded::<()>();
    self.set_attempt(Some(Attempt {
      cookie: cookie.clone(),
      cancel: cancel.clone(),
    }));

    self
      .events
      .send(AgentEvent::Begin {
        message: message.into(),
        cancel,
      })
      .log_err();

    let result = self.authenticate(&cookie, &username, &cancel_rx).await;

    self.events.send(AgentEvent::Close).log_err();
    self.set_attempt(None);

    match &result {
      Ok(()) => info!("polkit authentication succeeded"),
      Err(error) => info!(?error, "polkit authentication ended"),
    }

    result
  }

  async fn cancel_authentication(&self, cookie: String) -> Result<(), PolkitError> {
    info!("polkit authentication cancellation requested");

    if let Some(attempt) = self.attempt().as_ref() {
      if attempt.cookie == cookie {
        attempt.cancel.send(()).log_err();
      } else {
        warn!("cancel request cookie did not match the active attempt");
      }
    }

    Ok(())
  }
}

impl PolkitAgent {
  fn attempt(&self) -> std::sync::MutexGuard<'_, Option<Attempt>> {
    self.attempt.lock().unwrap_or_else(|error| error.into_inner())
  }

  fn set_attempt(&self, attempt: Option<Attempt>) {
    *self.attempt() = attempt;
  }

  /// Runs the PAM conversation to completion. Prefers the socket-activated
  /// helper, falling back to spawning the setuid helper, mirroring what
  /// `libpolkit-agent` does. Either transport consumes one polkit cookie, so a
  /// wrong password ends the attempt with [`PolkitError::Failed`]; polkit then
  /// decides whether to reissue a fresh cookie for another try.
  async fn authenticate(
    &self,
    cookie: &str,
    username: &str,
    cancel_rx: &Receiver<()>,
  ) -> Result<(), PolkitError> {
    match UnixStream::connect(AGENT_HELPER_SOCKET).await {
      Ok(stream) => {
        self
          .authenticate_via_socket(stream, cookie, username, cancel_rx)
          .await
      }
      Err(error) => {
        debug!(?error, "agent helper socket unavailable, spawning setuid helper");
        self
          .authenticate_via_helper(cookie, username, cancel_rx)
          .await
      }
    }
  }

  /// Socket transport: systemd runs `polkit-agent-helper-1 --socket-activated`
  /// as root when we connect, and identifies us via the connection's peer
  /// credentials, so no setuid binary is involved. The username is sent first,
  /// then [`Self::converse`] sends the cookie and drives PAM.
  async fn authenticate_via_socket(
    &self,
    stream: UnixStream,
    cookie: &str,
    username: &str,
    cancel_rx: &Receiver<()>,
  ) -> Result<(), PolkitError> {
    let mut writer = stream.clone();
    write_line(&mut writer, username).await?;
    self.converse(stream, &mut writer, cookie, cancel_rx).await
  }

  /// Legacy transport: spawn the setuid helper with the username as an argument
  /// and drive PAM over its stdio.
  async fn authenticate_via_helper(
    &self,
    cookie: &str,
    username: &str,
    cancel_rx: &Receiver<()>,
  ) -> Result<(), PolkitError> {
    let mut child = Command::new("polkit-agent-helper-1")
      .arg(username)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::null())
      .spawn()
      .map_err(|error| {
        error!(?error, "failed to spawn polkit-agent-helper-1");
        PolkitError::Failed
      })?;

    let mut stdin = child.stdin.take().ok_or(PolkitError::Failed)?;
    let stdout = child.stdout.take().ok_or(PolkitError::Failed)?;

    let result = self.converse(stdout, &mut stdin, cookie, cancel_rx).await;

    // Sending EOF lets a helper that is still blocked reading a prompt unwind
    // cleanly; a cancelled attempt is killed outright. Helpers that reached a
    // verdict have already exited and are reaped by async-process.
    drop(stdin);
    if matches!(result, Err(PolkitError::Cancelled)) {
      child.kill().log_err();
    }

    result
  }

  /// Drives the PAM conversation over an already-established transport: sends the
  /// cookie, then relays prompts to the UI and responses back until the helper
  /// reports `SUCCESS`/`FAILURE`.
  async fn converse<R, W>(
    &self,
    reader: R,
    writer: &mut W,
    cookie: &str,
    cancel_rx: &Receiver<()>,
  ) -> Result<(), PolkitError>
  where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
  {
    write_line(writer, cookie).await?;

    let mut lines = BufReader::new(reader).lines();

    loop {
      let line = select_biased! {
        _ = cancel_rx.recv_async().fuse() => return Err(PolkitError::Cancelled),
        line = lines.next().fuse() => line,
      };

      let Some(line) = line else {
        warn!("polkit helper closed the connection without a verdict");
        return Err(PolkitError::Failed);
      };

      let line = line.map_err(|error| {
        error!(?error, "failed to read from polkit helper");
        PolkitError::Failed
      })?;

      // The helper C-escapes message text (matching g_strescape); undo it before
      // display. The leading keyword is plain ASCII and unaffected.
      let line = unescape(&line);
      let (keyword, text) = line.split_once(' ').unwrap_or((line.as_str(), ""));

      match keyword {
        "PAM_PROMPT_ECHO_OFF" | "PAM_PROMPT_ECHO_ON" => {
          let echo = keyword == "PAM_PROMPT_ECHO_ON";
          let secret = self.request_secret(text, echo, cancel_rx).await?;
          write_line(writer, &secret).await?;
        }
        "PAM_ERROR_MSG" => {
          if !text.is_empty() {
            self
              .events
              .send(AgentEvent::Error {
                message: text.to_owned().into(),
              })
              .log_err();
          }
        }
        "PAM_TEXT_INFO" => {
          if !text.is_empty() {
            self
              .events
              .send(AgentEvent::Info {
                message: text.to_owned().into(),
              })
              .log_err();
          }
        }
        "SUCCESS" => return Ok(()),
        "FAILURE" => return Err(PolkitError::Failed),
        other => debug!(other, "ignoring unknown polkit helper line"),
      }
    }
  }

  /// Asks the UI for a secret and waits for the reply, aborting if the attempt
  /// is cancelled or the UI drops the reply channel (dialog closed).
  async fn request_secret(
    &self,
    prompt: &str,
    echo: bool,
    cancel_rx: &Receiver<()>,
  ) -> Result<String, PolkitError> {
    let (reply, reply_rx) = flume::bounded::<String>(1);
    self
      .events
      .send(AgentEvent::Prompt {
        prompt: prompt.to_owned().into(),
        echo,
        reply,
      })
      .map_err(|_| PolkitError::Failed)?;

    select_biased! {
      _ = cancel_rx.recv_async().fuse() => Err(PolkitError::Cancelled),
      secret = reply_rx.recv_async().fuse() => secret.map_err(|_| PolkitError::Cancelled),
    }
  }
}

async fn write_line<W: AsyncWrite + Unpin>(
  writer: &mut W,
  line: &str,
) -> Result<(), PolkitError> {
  let write = async {
    writer.write_all(line.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await
  };

  write.await.map_err(|error| {
    error!(?error, "failed to write to polkit helper");
    PolkitError::Failed
  })
}

/// Reverses the C-style escaping the polkit helper applies to PAM message text
/// (the inverse of glib's `g_strescape`, which the agent side undoes with
/// `g_strcompress`). Only affects display text; control keywords never contain
/// escapes.
fn unescape(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  let mut chars = input.chars();

  while let Some(ch) = chars.next() {
    if ch != '\\' {
      out.push(ch);
      continue;
    }

    match chars.next() {
      Some('n') => out.push('\n'),
      Some('r') => out.push('\r'),
      Some('t') => out.push('\t'),
      Some('b') => out.push('\u{8}'),
      Some('f') => out.push('\u{c}'),
      Some('v') => out.push('\u{b}'),
      Some('\\') => out.push('\\'),
      Some('"') => out.push('"'),
      Some(first @ '0'..='7') => {
        let mut value = first as u32 - '0' as u32;
        for _ in 0..2 {
          match chars.clone().next() {
            Some(digit @ '0'..='7') => {
              value = value * 8 + (digit as u32 - '0' as u32);
              chars.next();
            }
            _ => break,
          }
        }
        if let Some(decoded) = char::from_u32(value) {
          out.push(decoded);
        }
      }
      Some(other) => out.push(other),
      None => out.push('\\'),
    }
  }

  out
}

/// Picks the username the helper should authenticate against. Mirrors GNOME
/// Shell: prefer our own uid, then root, then the first offered identity.
fn select_username_from_identities(identities: &[Identity]) -> Option<String> {
  let uids = identities
    .iter()
    .filter(|identity| identity.identity_kind == "unix-user")
    .filter_map(|identity| identity.identity_details.get("uid"))
    .filter_map(|value| match value {
      zvariant::Value::U32(uid) => Some(*uid),
      _ => None,
    })
    .collect::<Vec<_>>();

  let uid = uids
    .iter()
    .find(|uid| **uid == uzers::get_current_uid())
    .or_else(|| uids.iter().find(|uid| **uid == 0))
    .or_else(|| uids.first())?;

  let user = uzers::get_user_by_uid(*uid)?;
  Some(user.name().to_str()?.to_owned())
}

pub fn init(cx: &mut App, events: Sender<AgentEvent>) {
  cx.spawn(async move |cx| {
    if let Err(error) = register(events, cx).await {
      error!(?error, "failed to register polkit agent");
    }
  })
  .detach();
}

async fn register(events: Sender<AgentEvent>, cx: &mut AsyncApp) -> Result<()> {
  let connection = cx
    .update(GlobalDbusConnection::system)
    .await
    .ok_or_else(|| anyhow::anyhow!("system bus connection unavailable"))?;

  let agent = PolkitAgent {
    events,
    attempt: Mutex::new(None),
  };

  connection.object_server().at(OBJECT_PATH, agent).await?;

  let session = LogindSessionProxy::new(&connection).await?;
  let session_id = session.id().await?;

  let mut subject_details = HashMap::new();
  subject_details.insert("session-id", session_id.into());
  let subject = Subject {
    subject_kind: "unix-session",
    subject_details,
  };

  let authority = PolkitAuthorityProxy::new(&connection).await?;
  authority
    .register_authentication_agent(subject, "en_US.UTF-8", OBJECT_PATH)
    .await?;

  info!("polkit authentication agent registered");
  Ok(())
}
