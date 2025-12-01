// from cosmic-settings
use std::sync::Arc;

use dbus_networkmanager::interface::enums::{ActiveConnectionState, DeviceState, DeviceType};
use zbus::zvariant::ObjectPath;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeviceInfo {
  pub path: ObjectPath<'static>,
  pub device_type: DeviceType,
  pub interface: String,
  pub state: DeviceState,
  pub active_connection: Option<(DeviceConnection, ActiveConnectionState)>,
  pub available_connections: Vec<DeviceConnection>,
  pub known_connections: Vec<KnownDeviceConnection>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeviceConnection {
  pub path: ObjectPath<'static>,
  pub id: String,
  pub uuid: Arc<str>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct KnownDeviceConnection {
  pub id: String,
  pub uuid: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessPoint {
  pub ssid: Arc<str>,
  pub strength: u8,
  pub state: DeviceState,
  pub working: bool,
  pub path: ObjectPath<'static>,
  pub hw_address: HwAddress,
  pub secured: bool,
  pub wps_push: bool,
  pub network_type: NetworkType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkType {
  Open,
  PSK,
  EAP,
}

#[derive(Copy, Clone, PartialEq, Eq, Default, Debug, PartialOrd, Ord)]
pub struct HwAddress {
  address: u64,
}

impl HwAddress {
  pub fn from_str(arg: &str) -> Option<Self> {
    let columnless_vec = arg.split(":").collect::<Vec<&str>>();
    if columnless_vec.len() * 3 - 1 != arg.len() {
      return None;
    }
    for byte in &columnless_vec {
      if byte.len() != 2 {
        return None;
      }
    }
    u64::from_str_radix(columnless_vec.join("").as_str(), 16)
      .ok()
      .map(|address| HwAddress { address })
  }
  pub fn from_string(arg: &String) -> Option<Self> {
    HwAddress::from_str(arg.as_str())
  }
}

impl std::fmt::Display for HwAddress {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let hex = format!("{:#x}", self.address)
      .trim_start_matches("0x")
      .chars()
      .collect::<Vec<_>>()
      .chunks(2)
      .map(|chunk| chunk.iter().cloned().collect::<String>())
      .collect::<Vec<String>>()
      .join(":");
    write!(f, "{}", hex)
  }
}
