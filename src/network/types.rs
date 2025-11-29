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
