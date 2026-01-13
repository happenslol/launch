mod api;

use std::{collections::HashMap, fmt};

use futures::{Stream, StreamExt, stream::FuturesUnordered, try_join};
use gpui::SharedString;
use zbus::Result;
use zvariant::{ObjectPath, OwnedObjectPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityType {
  Open,
  WEP,
  WPA,
  WPA2,
  WPA3,
}

impl SecurityType {
  fn from_flags(flags: u32, wpa_flags: u32, rsn_flags: u32) -> Self {
    if rsn_flags & 0x200 != 0 {
      SecurityType::WPA3
    } else if rsn_flags != 0 {
      SecurityType::WPA2
    } else if wpa_flags != 0 {
      SecurityType::WPA
    } else if flags & 0x1 != 0 {
      SecurityType::WEP
    } else {
      SecurityType::Open
    }
  }

  pub fn is_secured(&self) -> bool {
    !matches!(self, SecurityType::Open)
  }
}

impl fmt::Display for SecurityType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      SecurityType::Open => write!(f, "Open"),
      SecurityType::WEP => write!(f, "WEP"),
      SecurityType::WPA => write!(f, "WPA"),
      SecurityType::WPA2 => write!(f, "WPA2"),
      SecurityType::WPA3 => write!(f, "WPA3"),
    }
  }
}

#[derive(Clone)]
pub struct NetworkManager {
  conn: zbus::Connection,
  proxy: api::NetworkManagerProxy<'static>,
}

impl NetworkManager {
  pub async fn new(conn: &zbus::Connection) -> Result<Self> {
    let proxy = api::NetworkManagerProxy::new(conn).await?;
    Ok(Self {
      conn: conn.clone(),
      proxy,
    })
  }

  pub fn connection(&self) -> &zbus::Connection {
    &self.conn
  }

  pub async fn activate_connection(
    &self,
    connection_path: &OwnedObjectPath,
    device_path: &OwnedObjectPath,
    ap_path: &OwnedObjectPath,
  ) -> Result<OwnedObjectPath> {
    self
      .proxy
      .activate_connection(
        &ObjectPath::try_from(connection_path.as_str())?,
        &ObjectPath::try_from(device_path.as_str())?,
        &ObjectPath::try_from(ap_path.as_str())?,
      )
      .await
  }

  pub async fn add_and_activate_connection(
    &self,
    device_path: &OwnedObjectPath,
    ap_path: &OwnedObjectPath,
  ) -> Result<(OwnedObjectPath, OwnedObjectPath)> {
    self
      .proxy
      .add_and_activate_connection(
        HashMap::new(),
        &ObjectPath::try_from(device_path.as_str())?,
        &ObjectPath::try_from(ap_path.as_str())?,
      )
      .await
  }

  pub async fn deactivate_connection(
    &self,
    active_connection_path: &OwnedObjectPath,
  ) -> Result<()> {
    self
      .proxy
      .deactivate_connection(&ObjectPath::try_from(active_connection_path.as_str())?)
      .await
  }

  pub async fn get_known_wifi_connections(&self) -> Result<Vec<(SharedString, OwnedObjectPath)>> {
    let settings = api::SettingsProxy::builder(&self.conn)
      .path("/org/freedesktop/NetworkManager/Settings")?
      .build()
      .await?;

    let connections = settings.list_connections().await?;

    let results = connections
      .into_iter()
      .map(|path| {
        let conn = self.conn.clone();
        async move {
          let connection = api::settings_connection::ConnectionProxy::builder(&conn)
            .path(path.clone())
            .ok()?
            .build()
            .await
            .ok()?;

          let settings = connection.get_settings().await.ok()?;

          let wifi_settings = settings.get("802-11-wireless")?;
          let ssid_value = wifi_settings.get("ssid")?;

          let ssid_bytes: Vec<u8> = ssid_value.try_to_owned().ok()?.try_into().ok()?;
          let ssid = String::from_utf8_lossy(&ssid_bytes).to_string();

          Some((SharedString::from(ssid), path))
        }
      })
      .collect::<FuturesUnordered<_>>()
      .filter_map(|result| async { result })
      .collect::<Vec<_>>()
      .await;

    Ok(results)
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
          .path(path.clone())
          .ok()?
          .build()
          .await
          .ok()?;

        Some(WirelessDevice {
          name,
          path,
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

#[derive(Clone)]
pub struct WirelessDevice {
  pub name: SharedString,

  path: OwnedObjectPath,
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

impl WirelessDevice {
  pub fn device_path(&self) -> &OwnedObjectPath {
    &self.path
  }

  pub fn connection(&self) -> &zbus::Connection {
    self.device_proxy.inner().connection()
  }

  pub async fn get_active_access_point(&self) -> Result<Option<AccessPoint>> {
    let active_ap_path = self.wireless_proxy.active_access_point().await?;

    if active_ap_path.as_str() == "/" {
      return Ok(None);
    }

    let ap = AccessPoint::new(self.device_proxy.inner().connection(), active_ap_path).await?;
    Ok(Some(ap))
  }

  pub async fn get_active_connection_path(&self) -> Result<Option<OwnedObjectPath>> {
    let active_conn_path = self.device_proxy.active_connection().await?;

    if active_conn_path.as_str() == "/" {
      return Ok(None);
    }

    Ok(Some(active_conn_path))
  }

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

#[derive(Clone)]
pub struct AccessPoint {
  pub ssid: SharedString,
  pub hw_address: SharedString,
  pub strength: u8,
  pub frequency: u32,
  pub security: SecurityType,
  pub path: OwnedObjectPath,

  proxy: api::AccessPointProxy<'static>,
}

impl AccessPoint {
  pub async fn new(conn: &zbus::Connection, path: OwnedObjectPath) -> Result<Self> {
    let access_point = api::AccessPointProxy::builder(conn)
      .path(path.clone())?
      .build()
      .await?;

    let (ssid, hw_address, strength, frequency, flags, wpa_flags, rsn_flags) = try_join!(
      access_point.ssid(),
      access_point.hw_address(),
      access_point.strength(),
      access_point.frequency(),
      access_point.flags(),
      access_point.wpa_flags(),
      access_point.rsn_flags(),
    )?;

    let ssid = String::from_utf8_lossy(&ssid).to_string();
    let ssid = SharedString::from(ssid);
    let hw_address = SharedString::from(hw_address);
    let security = SecurityType::from_flags(flags, wpa_flags, rsn_flags);

    Ok(AccessPoint {
      ssid,
      hw_address,
      strength,
      frequency,
      security,
      path,
      proxy: access_point,
    })
  }

  pub async fn listen_strength_changed(&self) -> Result<impl Stream<Item = u8> + Send + use<>> {
    let proxy = self.proxy.clone();
    let stream = proxy.receive_strength_changed().await;
    Ok(stream.filter_map(|ev| async move { ev.get().await.ok() }))
  }
}

impl std::fmt::Debug for AccessPoint {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AccessPoint")
      .field("ssid", &self.ssid)
      .field("strength", &self.strength)
      .field("frequency", &self.frequency)
      .field("security", &self.security)
      .finish()
  }
}
