mod active_connection;
mod device;
mod settings;
mod settings2;

use std::collections::HashMap;

use zbus::{
  Result, proxy,
  zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

#[proxy(
  interface = "org.freedesktop.NetworkManager",
  default_service = "org.freedesktop.NetworkManager",
  default_path = "/org/freedesktop/NetworkManager"
)]
pub trait NetworkManager {
  /// ActivateConnection method
  fn activate_connection(
    &self,
    connection: &ObjectPath<'_>,
    device: &ObjectPath<'_>,
    specific_object: &ObjectPath<'_>,
  ) -> Result<OwnedObjectPath>;

  /// AddAndActivateConnection method
  fn add_and_activate_connection(
    &self,
    connection: HashMap<&str, HashMap<&str, &Value<'_>>>,
    device: &ObjectPath<'_>,
    specific_object: &ObjectPath<'_>,
  ) -> Result<(OwnedObjectPath, OwnedObjectPath)>;

  /// AddAndActivateConnection2 method
  fn add_and_activate_connection2(
    &self,
    connection: HashMap<&str, HashMap<&str, &Value<'_>>>,
    device: &ObjectPath<'_>,
    specific_object: &ObjectPath<'_>,
    options: HashMap<&str, &Value<'_>>,
  ) -> Result<(
    OwnedObjectPath,
    OwnedObjectPath,
    HashMap<String, OwnedValue>,
  )>;

  /// CheckConnectivity method
  fn check_connectivity(&self) -> Result<u32>;

  /// CheckpointAdjustRollbackTimeout method
  fn checkpoint_adjust_rollback_timeout(
    &self,
    checkpoint: &ObjectPath<'_>,
    add_timeout: u32,
  ) -> Result<()>;

  /// CheckpointCreate method
  fn checkpoint_create(
    &self,
    devices: &[&ObjectPath<'_>],
    rollback_timeout: u32,
    flags: u32,
  ) -> Result<OwnedObjectPath>;

  /// CheckpointDestroy method
  fn checkpoint_destroy(&self, checkpoint: &ObjectPath<'_>) -> Result<()>;

  /// CheckpointRollback method
  fn checkpoint_rollback(&self, checkpoint: &ObjectPath<'_>) -> Result<HashMap<String, u32>>;

  /// DeactivateConnection method
  fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> Result<()>;

  /// Enable method
  fn enable(&self, enable: bool) -> Result<()>;

  /// GetAllDevices method
  fn get_all_devices(&self) -> Result<Vec<OwnedObjectPath>>;

  /// GetDeviceByIpIface method
  fn get_device_by_ip_iface(&self, iface: &str) -> Result<OwnedObjectPath>;

  /// GetDevices method
  fn get_devices(&self) -> Result<Vec<OwnedObjectPath>>;

  /// GetLogging method
  fn get_logging(&self) -> Result<(String, String)>;

  /// GetPermissions method
  fn get_permissions(&self) -> Result<HashMap<String, String>>;

  /// Reload method
  fn reload(&self, flags: u32) -> Result<()>;

  /// SetLogging method
  fn set_logging(&self, level: &str, domains: &str) -> Result<()>;

  /// Sleep method
  fn sleep(&self, sleep: bool) -> Result<()>;

  /// state method
  #[zbus(name = "state")]
  fn state_method(&self) -> Result<u32>;

  /// CheckPermissions signal
  #[zbus(signal)]
  fn check_permissions(&self) -> Result<()>;

  /// DeviceAdded signal
  #[zbus(signal)]
  fn device_added(&self, device_path: ObjectPath<'_>) -> Result<()>;

  /// DeviceRemoved signal
  #[zbus(signal)]
  fn device_removed(&self, device_path: ObjectPath<'_>) -> Result<()>;

  /// StateChanged signal
  #[zbus(signal, name = "StateChanged")]
  fn state_changed_signal(&self, state: u32) -> Result<()>;

  /// ActivatingConnection property
  #[zbus(property)]
  fn activating_connection(&self) -> Result<OwnedObjectPath>;

  /// ActiveConnections property
  #[zbus(property)]
  fn active_connections(&self) -> Result<Vec<OwnedObjectPath>>;

  /// AllDevices property
  #[zbus(property)]
  fn all_devices(&self) -> Result<Vec<OwnedObjectPath>>;

  /// Capabilities property
  #[zbus(property)]
  fn capabilities(&self) -> Result<Vec<u32>>;

  /// Checkpoints property
  #[zbus(property)]
  fn checkpoints(&self) -> Result<Vec<OwnedObjectPath>>;

  /// Connectivity property
  #[zbus(property)]
  fn connectivity(&self) -> Result<u32>;

  /// ConnectivityCheckAvailable property
  #[zbus(property)]
  fn connectivity_check_available(&self) -> Result<bool>;

  /// ConnectivityCheckEnabled property
  #[zbus(property)]
  fn connectivity_check_enabled(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_connectivity_check_enabled(&self, value: bool) -> Result<()>;

  /// ConnectivityCheckUri property
  #[zbus(property)]
  fn connectivity_check_uri(&self) -> Result<String>;

  /// Devices property
  #[zbus(property)]
  fn devices(&self) -> Result<Vec<OwnedObjectPath>>;

  /// GlobalDnsConfiguration property
  #[zbus(property)]
  fn global_dns_configuration(&self) -> Result<HashMap<String, OwnedValue>>;

  // TODO: This is a readwrite property. Maybe we need the newest zbus version?
  // #[zbus(property)]
  // fn set_global_dns_configuration(&self, value: HashMap<&str, &Value<'_>>) -> Result<()>;

  /// Metered property
  #[zbus(property)]
  fn metered(&self) -> Result<u32>;

  /// NetworkingEnabled property
  #[zbus(property)]
  fn networking_enabled(&self) -> Result<bool>;

  /// PrimaryConnection property
  #[zbus(property)]
  fn primary_connection(&self) -> Result<OwnedObjectPath>;

  /// PrimaryConnectionType property
  #[zbus(property)]
  fn primary_connection_type(&self) -> Result<String>;

  /// RadioFlags property
  #[zbus(property)]
  fn radio_flags(&self) -> Result<u32>;

  /// Startup property
  #[zbus(property)]
  fn startup(&self) -> Result<bool>;

  /// State property
  #[zbus(property, name = "State")]
  fn state_property(&self) -> Result<u32>;

  /// Version property
  #[zbus(property)]
  fn version(&self) -> Result<String>;

  /// VersionInfo property
  #[zbus(property)]
  fn version_info(&self) -> Result<Vec<u32>>;

  /// WimaxEnabled property
  #[zbus(property)]
  fn wimax_enabled(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_wimax_enabled(&self, value: bool) -> Result<()>;

  /// WimaxHardwareEnabled property
  #[zbus(property)]
  fn wimax_hardware_enabled(&self) -> Result<bool>;

  /// WirelessEnabled property
  #[zbus(property)]
  fn wireless_enabled(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_wireless_enabled(&self, value: bool) -> Result<()>;

  /// WirelessHardwareEnabled property
  #[zbus(property)]
  fn wireless_hardware_enabled(&self) -> Result<bool>;

  /// WwanEnabled property
  #[zbus(property)]
  fn wwan_enabled(&self) -> Result<bool>;
  #[zbus(property)]
  fn set_wwan_enabled(&self, value: bool) -> Result<()>;

  /// WwanHardwareEnabled property
  #[zbus(property)]
  fn wwan_hardware_enabled(&self) -> Result<bool>;
}
