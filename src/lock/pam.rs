//! Password verification against PAM.
//!
//! libpam is blocking and a stack can take a while to answer (key derivation,
//! network modules), so every attempt runs on its own thread and reports back
//! over a channel. The password is moved into that thread, handed to PAM's
//! conversation callback and dropped with it.

use std::ffi::{CStr, CString};
use std::path::Path;

use flume::{Receiver, Sender};
use gpui::SharedString;
use pam_client2::{Context, ConversationHandler, ErrorCode, Flag};
use tracing::{debug, error, warn};

use crate::util::ResultExt;

/// PAM services we fall back to when the configured one has no file in
/// `/etc/pam.d`. An unknown service falls through to the `other` stack, which
/// denies everything, so without a fallback a missing file would mean a lock
/// screen that rejects the correct password. Both of these are stacks meant for
/// interactive authentication of the local user.
const FALLBACK_SERVICES: &[&str] = &["swaylock", "hyprlock", "login"];

/// Directories PAM reads service files from, in its own lookup order.
const SERVICE_DIRS: &[&str] = &["/etc/pam.d", "/usr/lib/pam.d"];

/// What a PAM stack reports while it verifies a password.
pub enum AuthEvent {
  /// An informational message, e.g. "Your password expires in 3 days".
  Info(SharedString),
  /// An error message from a module, e.g. a failed previous attempt.
  Error(SharedString),
  /// The attempt ended. `Ok(())` means the password was accepted and the account
  /// is in good standing; the error carries the reason, phrased for display.
  Finished(Result<(), SharedString>),
}

/// Verifies `password` for `username`, reporting progress over the returned
/// channel. Dropping the receiver doesn't cancel the attempt: PAM has no way to
/// interrupt a running conversation, but the thread ends on its own once the
/// stack answers.
pub fn authenticate(service: &str, username: &str, password: String) -> Receiver<AuthEvent> {
  let (events, receiver) = flume::unbounded();
  let service = service.to_owned();
  let username = username.to_owned();

  // A plain thread rather than the background executor: the conversation blocks
  // for as long as the stack takes, and executor threads are shared with work
  // that shouldn't wait behind it.
  std::thread::spawn({
    let events = events.clone();
    move || {
      let result = verify(&service, &username, password, events.clone());
      if events.send(AuthEvent::Finished(result)).is_err() {
        debug!("Lock screen went away before authentication finished");
      }
    }
  });

  receiver
}

fn verify(
  service: &str,
  username: &str,
  password: String,
  events: Sender<AuthEvent>,
) -> Result<(), SharedString> {
  let conversation = Conversation {
    username: username.to_owned(),
    password,
    events,
  };

  let mut context = Context::new(service, Some(username), conversation).map_err(|error| {
    error!(?error, service, "Failed to start PAM transaction");
    SharedString::from("Authentication is unavailable")
  })?;

  context.authenticate(Flag::NONE).map_err(failure_message)?;
  context.acct_mgmt(Flag::NONE).map_err(failure_message)?;

  Ok(())
}

/// Turns a PAM failure into something worth putting on a lock screen.
fn failure_message(error: pam_client2::Error) -> SharedString {
  // A rejected password is the one failure that isn't worth spelling out: PAM's
  // own wording for it ("Authentication failure") reads like a malfunction.
  let rejected = matches!(
    error.code(),
    ErrorCode::AUTH_ERR | ErrorCode::CRED_INSUFFICIENT | ErrorCode::MAXTRIES
  );

  if rejected {
    debug!(?error, "Password rejected");
    return "Incorrect password".into();
  }

  warn!(?error, "Authentication failed");
  error.to_string().into()
}

/// Picks the service to authenticate against: the configured one when PAM has a
/// file for it, otherwise the first known-good fallback. See
/// [`FALLBACK_SERVICES`].
pub fn resolve_service(configured: &str) -> SharedString {
  if service_exists(configured) {
    return configured.to_owned().into();
  }

  match FALLBACK_SERVICES
    .iter()
    .find(|service| service_exists(service))
  {
    Some(fallback) => {
      warn!(
        configured,
        fallback, "No PAM service file for the configured service, falling back"
      );
      (*fallback).into()
    }
    None => {
      error!(
        configured,
        "No PAM service file for the configured service and no fallback available"
      );
      configured.to_owned().into()
    }
  }
}

fn service_exists(service: &str) -> bool {
  SERVICE_DIRS
    .iter()
    .any(|dir| Path::new(dir).join(service).exists())
}

/// Answers the prompts of the PAM stack. Password prompts are answered from the
/// single password this attempt was created with; a stack that asks twice (a
/// second factor, say) gets the same answer and will reject it, which is the
/// same behaviour as any other single-prompt lock screen.
struct Conversation {
  username: String,
  password: String,
  events: Sender<AuthEvent>,
}

impl Conversation {
  fn send(&self, event: AuthEvent) {
    if self.events.send(event).is_err() {
      debug!("Lock screen went away before a PAM message could be shown");
    }
  }
}

impl ConversationHandler for Conversation {
  fn prompt_echo_on(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
    // Anything the stack wants echoed is the user name; a lock screen has
    // nowhere else to ask.
    CString::new(self.username.clone()).map_err(|_| ErrorCode::CONV_ERR)
  }

  fn prompt_echo_off(&mut self, _prompt: &CStr) -> Result<CString, ErrorCode> {
    CString::new(self.password.clone()).map_err(|_| ErrorCode::CONV_ERR)
  }

  fn text_info(&mut self, message: &CStr) {
    if let Some(message) = message_text(message) {
      self.send(AuthEvent::Info(message));
    }
  }

  fn error_msg(&mut self, message: &CStr) {
    if let Some(message) = message_text(message) {
      self.send(AuthEvent::Error(message));
    }
  }
}

fn message_text(message: &CStr) -> Option<SharedString> {
  let message = message.to_str().log_err()?.trim();
  if message.is_empty() {
    return None;
  }

  Some(message.to_owned().into())
}
