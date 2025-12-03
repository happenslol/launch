use std::collections::HashMap;

use zbus::{
  Result, proxy,
  zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

#[proxy(
  interface = "org.freedesktop.NetworkManager.Device",
  default_service = "org.freedesktop.NetworkManager"
)]
pub trait Device {
  /// Delete method
  fn delete(&self) -> Result<()>;

  /// Disconnect method
  fn disconnect(&self) -> Result<()>;

  /// GetAppliedConnection method
  fn get_applied_connection(
    &self,
    flags: u32,
  ) -> Result<(HashMap<String, HashMap<String, OwnedValue>>, u64)>;

  /// Reapply method
  fn reapply(
    &self,
    connection: HashMap<&str, HashMap<&str, &Value<'_>>>,
    version_id: u64,
    flags: u32,
  ) -> Result<()>;

  /// StateChanged signal
  #[zbus(signal, name = "StateChanged")]
  fn state_changed_(&self, new_state: u32, old_state: u32, reason: u32) -> Result<()>;

  /// ActiveConnection property
  #[zbus(property)]
  fn active_connection(&self) -> Result<OwnedObjectPath>;

  /// Autoconnect property
  #[zbus(property)]
  fn autoconnect(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_autoconnect(&self, value: bool) -> Result<()>;

  /// AvailableConnections property
  #[zbus(property)]
  fn available_connections(&self) -> Result<Vec<OwnedObjectPath>>;

  /// Capabilities property
  #[zbus(property)]
  fn capabilities(&self) -> Result<u32>;

  /// DeviceType property
  #[zbus(property)]
  fn device_type(&self) -> Result<u32>;

  /// Dhcp4Config property
  #[zbus(property)]
  fn dhcp4_config(&self) -> Result<OwnedObjectPath>;

  /// Dhcp6Config property
  #[zbus(property)]
  fn dhcp6_config(&self) -> Result<OwnedObjectPath>;

  /// Driver property
  #[zbus(property)]
  fn driver(&self) -> Result<String>;

  /// DriverVersion property
  #[zbus(property)]
  fn driver_version(&self) -> Result<String>;

  /// FirmwareMissing property
  #[zbus(property)]
  fn firmware_missing(&self) -> Result<bool>;

  /// FirmwareVersion property
  #[zbus(property)]
  fn firmware_version(&self) -> Result<String>;

  /// HwAddress property
  #[zbus(property)]
  fn hw_address(&self) -> Result<String>;

  /// Interface property
  #[zbus(property)]
  fn interface(&self) -> Result<String>;

  /// InterfaceFlags property
  #[zbus(property)]
  fn interface_flags(&self) -> Result<u32>;

  /// Ip4Address property
  #[zbus(property)]
  fn ip4_address(&self) -> Result<u32>;

  /// Ip4Config property
  #[zbus(property)]
  fn ip4_config(&self) -> Result<OwnedObjectPath>;

  /// Ip4Connectivity property
  #[zbus(property)]
  fn ip4_connectivity(&self) -> Result<u32>;

  /// Ip6Config property
  #[zbus(property)]
  fn ip6_config(&self) -> Result<OwnedObjectPath>;

  /// Ip6Connectivity property
  #[zbus(property)]
  fn ip6_connectivity(&self) -> Result<u32>;

  /// IpInterface property
  #[zbus(property)]
  fn ip_interface(&self) -> Result<String>;

  /// LldpNeighbors property
  #[zbus(property)]
  fn lldp_neighbors(&self) -> Result<Vec<HashMap<String, OwnedValue>>>;

  /// Managed property
  #[zbus(property)]
  fn managed(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_managed(&self, value: bool) -> Result<()>;

  /// Metered property
  #[zbus(property)]
  fn metered(&self) -> Result<u32>;

  /// Mtu property
  #[zbus(property)]
  fn mtu(&self) -> Result<u32>;

  /// NmPluginMissing property
  #[zbus(property)]
  fn nm_plugin_missing(&self) -> Result<bool>;

  /// Path property
  #[zbus(property)]
  fn path(&self) -> Result<String>;

  /// PhysicalPortId property
  #[zbus(property)]
  fn physical_port_id(&self) -> Result<String>;

  /// Ports property
  #[zbus(property)]
  fn ports(&self) -> Result<Vec<OwnedObjectPath>>;

  /// Real property
  #[zbus(property)]
  fn real(&self) -> Result<bool>;

  /// State property
  #[zbus(property)]
  fn state(&self) -> Result<u32>;

  /// StateReason property
  #[zbus(property)]
  fn state_reason(&self) -> Result<(u32, u32)>;

  /// Udi property
  #[zbus(property)]
  fn udi(&self) -> Result<String>;
}

#[proxy(
  interface = "org.freedesktop.NetworkManager.Device.Wireless",
  default_service = "org.freedesktop.NetworkManager"
)]
pub trait WirelessDevice {
  /// GetAccessPoints method
  fn get_access_points(&self) -> Result<Vec<OwnedObjectPath>>;

  /// GetAllAccessPoints method
  fn get_all_access_points(&self) -> Result<Vec<OwnedObjectPath>>;

  /// RequestScan method
  fn request_scan(&self, options: HashMap<&str, &Value<'_>>) -> Result<()>;

  /// AccessPointAdded signal
  #[zbus(signal)]
  fn access_point_added(&self, access_point: ObjectPath<'_>) -> Result<()>;

  /// AccessPointRemoved signal
  #[zbus(signal)]
  fn access_point_removed(&self, access_point: ObjectPath<'_>) -> Result<()>;

  /// AccessPoints property
  #[zbus(property)]
  fn access_points(&self) -> Result<Vec<OwnedObjectPath>>;

  /// ActiveAccessPoint property
  #[zbus(property)]
  fn active_access_point(&self) -> Result<OwnedObjectPath>;

  /// Bitrate property
  #[zbus(property)]
  fn bitrate(&self) -> Result<u32>;

  /// HwAddress property
  #[zbus(property)]
  fn hw_address(&self) -> Result<String>;

  /// LastScan property
  #[zbus(property)]
  fn last_scan(&self) -> Result<i64>;

  /// Mode property
  #[zbus(property)]
  fn mode(&self) -> Result<u32>;

  /// PermHwAddress property
  #[zbus(property)]
  fn perm_hw_address(&self) -> Result<String>;

  /// WirelessCapabilities property
  #[zbus(property)]
  fn wireless_capabilities(&self) -> Result<u32>;
}

#[proxy(
  interface = "org.freedesktop.NetworkManager.Device.Statistics",
  default_service = "org.freedesktop.NetworkManager"
)]
pub trait DeviceStatistics {
  /// RefreshRateMs property
  #[zbus(property)]
  fn refresh_rate_ms(&self) -> Result<u32>;
  #[zbus(property)]
  fn set_refresh_rate_ms(&self, value: u32) -> Result<()>;

  /// RxBytes property
  #[zbus(property)]
  fn rx_bytes(&self) -> Result<u64>;

  /// TxBytes property
  #[zbus(property)]
  fn tx_bytes(&self) -> Result<u64>;
}
