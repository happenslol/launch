mod api;

use std::collections::HashMap;

use futures::{Stream, StreamExt, stream::FuturesUnordered, try_join};
use gpui::SharedString;
use zbus::Result;
use zvariant::OwnedObjectPath;

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
#[derive(Clone)]
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

#[allow(dead_code)]
impl WirelessDevice {
  pub async fn scan(&self) -> Result<()> {
    let last_scan = self.wireless_proxy.last_scan().await?;
    let mut last_scan_changed = self.wireless_proxy.receive_last_scan_changed().await;
    self.wireless_proxy.request_scan(HashMap::new()).await?;

    while let Some(ev) = last_scan_changed.next().await {
      let next_scan = ev.get().await?;
      if next_scan > last_scan {
        break;
      }
    }

    Ok(())
  }

  pub async fn get_access_points(&self) -> Result<Vec<AccessPoint>> {
    let access_points = self.wireless_proxy.get_access_points().await?;
    let access_points = access_points
      .into_iter()
      .map(|path| async move {
        AccessPoint::new(self.device_proxy.inner().connection(), path)
          .await
          .ok()
      })
      .collect::<FuturesUnordered<_>>()
      .filter_map(|access_point| async { access_point })
      .collect::<Vec<_>>()
      .await;

    Ok(access_points)
  }

  pub async fn access_point_changes(&self) -> impl Stream<Item = Vec<AccessPoint>> {
    self
      .wireless_proxy
      .receive_access_points_changed()
      .await
      .filter_map(move |ev| {
        let conn = self.device_proxy.inner().connection().clone();
        async move {
          let access_points = ev.get().await.ok()?;

          let access_points = access_points
            .into_iter()
            .map(|path| {
              let conn = conn.clone();
              async move { AccessPoint::new(&conn, path).await.ok() }
            })
            .collect::<FuturesUnordered<_>>()
            .filter_map(|access_point| async { access_point })
            .collect::<Vec<_>>()
            .await;

          Some(access_points)
        }
      })
  }
}

#[allow(dead_code)]
pub struct AccessPoint {
  pub ssid: SharedString,
  pub strength: u8,
  pub frequency: u32,

  proxy: api::AccessPointProxy<'static>,
}

impl AccessPoint {
  pub async fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Result<Self> {
    let access_point = api::AccessPointProxy::builder(conn)
      .path(path)?
      .build()
      .await?;

    let (ssid, strength, frequency) = try_join!(
      access_point.ssid(),
      access_point.strength(),
      access_point.frequency(),
    )?;

    let ssid = String::from_utf8_lossy(&ssid).to_string();
    let ssid = SharedString::from(ssid);

    Ok(AccessPoint {
      ssid,
      strength,
      frequency,
      proxy: access_point,
    })
  }
}

impl std::fmt::Debug for AccessPoint {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AccessPoint")
      .field("ssid", &self.ssid)
      .field("strength", &self.strength)
      .field("frequency", &self.frequency)
      .finish()
  }
}
