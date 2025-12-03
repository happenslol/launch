use zbus::{Result, proxy, zvariant::OwnedObjectPath};

#[proxy(
  interface = "org.freedesktop.NetworkManager.Connection.Active",
  default_service = "org.freedesktop.NetworkManager"
)]
pub trait ActiveConnection {
  /// StateChanged signal
  #[zbus(signal, name = "StateChanged")]
  fn state_changed_signal(&self, state: u32, reason: u32) -> Result<()>;

  /// Connection property
  #[zbus(property)]
  fn connection(&self) -> Result<OwnedObjectPath>;

  /// Controller property
  #[zbus(property)]
  fn controller(&self) -> Result<OwnedObjectPath>;

  /// Default property
  #[zbus(property)]
  fn default(&self) -> Result<bool>;

  /// Default6 property
  #[zbus(property)]
  fn default6(&self) -> Result<bool>;

  /// Devices property
  #[zbus(property)]
  fn devices(&self) -> Result<Vec<OwnedObjectPath>>;

  /// Dhcp4Config property
  #[zbus(property)]
  fn dhcp4_config(&self) -> Result<OwnedObjectPath>;

  /// Dhcp6Config property
  #[zbus(property)]
  fn dhcp6_config(&self) -> Result<OwnedObjectPath>;

  /// Id property
  #[zbus(property)]
  fn id(&self) -> Result<String>;

  /// Ip4Config property
  #[zbus(property)]
  fn ip4_config(&self) -> Result<OwnedObjectPath>;

  /// Ip6Config property
  #[zbus(property)]
  fn ip6_config(&self) -> Result<OwnedObjectPath>;

  /// Master property
  #[zbus(property)]
  fn master(&self) -> Result<OwnedObjectPath>;

  /// SpecificObject property
  #[zbus(property)]
  fn specific_object(&self) -> Result<OwnedObjectPath>;

  /// State property
  #[zbus(property, name = "State")]
  fn state_property(&self) -> Result<u32>;

  /// StateFlags property
  #[zbus(property)]
  fn state_flags(&self) -> Result<u32>;

  /// Type property
  #[zbus(property)]
  fn type_(&self) -> Result<String>;

  /// Uuid property
  #[zbus(property)]
  fn uuid(&self) -> Result<String>;

  /// Vpn property
  #[zbus(property)]
  fn vpn(&self) -> Result<bool>;
}
