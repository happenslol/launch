//! UPower battery readout.
//!
//! Only the composite "display device" is used: it folds however many batteries
//! the machine has into the single figure a UI shows, so nothing here has to
//! enumerate or pick between devices.

mod api;

use anyhow::{Context as _, Result};
use futures::{Stream, StreamExt as _};
use tracing::debug;

use crate::util::ResultExt;

/// The machine's battery, known to exist.
pub struct Battery {
  device: api::DeviceProxy<'static>,
}

impl Battery {
  /// Looks the battery up. `Ok(None)` when the machine has none, which is the
  /// ordinary state of affairs on a desktop.
  pub async fn find(connection: &zbus::Connection) -> Result<Option<Self>> {
    let upower = api::UPowerProxy::new(connection).await?;

    let path = upower
      .get_display_device()
      .await
      .context("looking up the composite power device")?;

    let device = api::DeviceProxy::builder(connection)
      .path(path)?
      .build()
      .await?;

    if !device.is_present().await? {
      debug!("Machine has no battery");
      return Ok(None);
    }

    Ok(Some(Self { device }))
  }

  /// Charge left, in percent.
  pub async fn percentage(&self) -> Result<f64> {
    Ok(self.device.percentage().await?)
  }

  /// Follows the charge as it changes.
  pub async fn listen(&self) -> Result<impl Stream<Item = f64> + use<>> {
    let changes = self.device.receive_percentage_changed().await;
    Ok(changes.filter_map(|change| async move { change.get().await.log_err() }))
  }
}
