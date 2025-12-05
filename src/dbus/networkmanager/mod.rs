mod api;

use futures::{StreamExt, stream::FuturesUnordered};
use gpui::SharedString;
use zbus::Result;

pub struct NetworkManager {
  proxy: api::NetworkManagerProxy<'static>,
}

impl NetworkManager {
  pub async fn new(conn: &zbus::Connection) -> Result<Self> {
    let proxy = api::NetworkManagerProxy::new(conn).await?;
    Ok(Self { proxy })
  }

  pub async fn get_wireless_devices(&self) -> Result<Vec<WirelessDevice>> {
    let devices = self.proxy.get_devices().await?;

    let devices = devices
      .into_iter()
      .map(|path| async move {
        let device = api::DeviceProxy::builder(self.proxy.inner().connection())
          .path(path.clone())
          .ok()?
          .build()
          .await
          .ok()?;

        let device_type = device.device_type().await.ok()?.into();
        if !matches!(device_type, DeviceType::Wifi) {
          return None;
        }

        let name = SharedString::from(device.interface().await.ok()?);

        let wireless = api::WirelessProxy::builder(self.proxy.inner().connection())
          .path(path)
          .ok()?
          .build()
          .await
          .ok()?;

        Some(WirelessDevice {
          name,
          device_proxy: device,
          wireless_proxy: wireless,
        })
      })
      .collect::<FuturesUnordered<_>>()
      .filter_map(|device| async move { device })
      .collect::<Vec<_>>()
      .await;

    Ok(devices)
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
  Unknown,
  Ethernet,
  Wifi,
}

impl From<u32> for DeviceType {
  fn from(value: u32) -> Self {
    match value {
      1 => Self::Ethernet,
      2 => Self::Wifi,
      _ => Self::Unknown,
    }
  }
}

#[allow(dead_code)]
pub struct WirelessDevice {
  pub name: SharedString,
  device_proxy: api::DeviceProxy<'static>,
  wireless_proxy: api::WirelessProxy<'static>,
}

impl std::fmt::Debug for WirelessDevice {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("WirelessDevice")
      .field("name", &self.name)
      .finish()
  }
}
