//! fprintd fingerprint reader.
//!
//! Wraps `net.reactivated.Fprint` so the lock screen can verify a fingerprint
//! while the user could just as well type their password. Verification is driven
//! directly instead of through `pam_fprintd` because a PAM stack is sequential:
//! the module holds the conversation until the reader gives up, which would
//! block password entry for as long as it runs.
//!
//! Only the calling user's own prints are ever touched, which is what fprintd
//! allows an active session to do without further authorization.

mod api;

use anyhow::{Context as _, Result};
use futures::{Stream, StreamExt as _};
use gpui::SharedString;
use tracing::{debug, warn};

use crate::util::ResultExt;

/// Matches against every enrolled finger rather than a specific one.
const ANY_FINGER: &str = "any";

/// fprintd resolves an empty username to the user the calling client runs as.
/// Naming a user explicitly would require the `setusername` authorization, which
/// only root has by default.
const CURRENT_USER: &str = "";

/// How a reader wants to be given a finger, which is all the difference between
/// the two prompts we can show.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ScanType {
  Press,
  Swipe,
}

impl ScanType {
  fn parse(scan_type: &str) -> Self {
    match scan_type {
      "swipe" => Self::Swipe,
      "press" => Self::Press,
      other => {
        warn!(scan_type = other, "Unknown fingerprint scan type");
        Self::Press
      }
    }
  }

  pub fn prompt(self) -> SharedString {
    match self {
      Self::Press => "Place your finger on the reader".into(),
      Self::Swipe => "Swipe your finger on the reader".into(),
    }
  }
}

/// What the reader reported about a running verification.
pub enum VerifyStatus {
  /// The finger matched one of the enrolled prints.
  Match,
  /// The finger did not match any enrolled print.
  NoMatch,
  /// The scan was unusable and the user should try again. Carries the reason,
  /// phrased for display.
  Retry(SharedString),
  /// The reader errored out or went away; it must not be used again.
  Failed(SharedString),
}

/// One status update of a running verification.
pub struct VerifyUpdate {
  pub status: VerifyStatus,
  /// Set once the verification has ended. The reader then needs a
  /// [`FingerprintReader::stop_verification`] before it accepts another attempt,
  /// and no further updates will arrive for this one.
  pub done: bool,
}

impl VerifyUpdate {
  fn parse(result: &str, done: bool) -> Self {
    let status = match result {
      "verify-match" => VerifyStatus::Match,
      "verify-no-match" => VerifyStatus::NoMatch,
      "verify-retry-scan" => VerifyStatus::Retry("Couldn't read that, try again".into()),
      "verify-swipe-too-short" => VerifyStatus::Retry("Swipe was too short, try again".into()),
      "verify-finger-not-centered" => {
        VerifyStatus::Retry("Center your finger on the reader".into())
      }
      "verify-remove-and-retry" => VerifyStatus::Retry("Remove your finger and try again".into()),
      "verify-too-fast" => VerifyStatus::Retry("That was too fast, try again".into()),
      "verify-disconnected" => {
        VerifyStatus::Failed("The fingerprint reader was disconnected".into())
      }
      other => {
        // Covers `verify-unknown-error`, which fprintd sends for driver
        // problems, as well as statuses added after this was written.
        warn!(status = other, "Fingerprint verification failed");
        VerifyStatus::Failed("The fingerprint reader failed".into())
      }
    };

    Self { status, done }
  }
}

/// The system's default fingerprint reader, known to have prints enrolled for
/// the current user.
#[derive(Clone)]
pub struct FingerprintReader {
  proxy: api::DeviceProxy<'static>,
  pub name: SharedString,
  pub scan_type: ScanType,
}

impl FingerprintReader {
  /// Looks up the default reader. `Ok(None)` covers the ordinary reasons there
  /// is nothing to verify against - no fprintd, no reader, no enrolled prints -
  /// so callers can treat fingerprint support as simply absent.
  pub async fn find(connection: &zbus::Connection) -> Result<Option<Self>> {
    let manager = api::ManagerProxy::new(connection).await?;

    let path = match manager.get_default_device().await {
      Ok(path) => path,
      Err(error) => {
        debug!(?error, "No fingerprint reader available");
        return Ok(None);
      }
    };

    let proxy = api::DeviceProxy::builder(connection)
      .path(path)?
      .build()
      .await?;

    match proxy.list_enrolled_fingers(CURRENT_USER).await {
      // fprintd answers with `NoEnrolledPrints` rather than an empty list, but
      // handle both so either shape means "nothing to verify against".
      Ok(fingers) if fingers.is_empty() => {
        debug!("Fingerprint reader has no enrolled prints");
        return Ok(None);
      }
      Ok(fingers) => debug!(?fingers, "Found enrolled fingerprints"),
      Err(error) => {
        debug!(?error, "Could not list enrolled fingerprints");
        return Ok(None);
      }
    }

    let name = proxy.name().await.log_err().unwrap_or_default();
    let scan_type = proxy
      .scan_type()
      .await
      .log_err()
      .map_or(ScanType::Press, |scan_type| ScanType::parse(&scan_type));

    Ok(Some(Self {
      proxy,
      name: name.into(),
      scan_type,
    }))
  }

  /// Takes the reader for the current user. Fails while another client holds it,
  /// which is what happens when a `pam_fprintd` conversation is running.
  pub async fn claim(&self) -> Result<()> {
    self
      .proxy
      .claim(CURRENT_USER)
      .await
      .context("claiming the fingerprint reader")?;
    Ok(())
  }

  pub async fn release(&self) -> Result<()> {
    self
      .proxy
      .release()
      .await
      .context("releasing the fingerprint reader")?;
    Ok(())
  }

  /// Starts one verification attempt and returns its status updates. The stream
  /// ends when the connection goes away; a finished attempt is signalled by
  /// [`VerifyUpdate::done`] instead.
  pub async fn start_verification(&self) -> Result<impl Stream<Item = VerifyUpdate> + use<>> {
    // Subscribe before starting the attempt, so an update can't slip through
    // between the two calls.
    let updates = self.proxy.receive_verify_status().await?;

    self
      .proxy
      .verify_start(ANY_FINGER)
      .await
      .context("starting fingerprint verification")?;

    Ok(updates.filter_map(|signal| async move {
      let args = signal.args().log_err()?;
      Some(VerifyUpdate::parse(args.result(), *args.done()))
    }))
  }

  pub async fn stop_verification(&self) -> Result<()> {
    self
      .proxy
      .verify_stop()
      .await
      .context("stopping fingerprint verification")?;
    Ok(())
  }
}
