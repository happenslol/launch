#![allow(unused)]

//! fprintd D-Bus interface proxies
//!
//! Transcribed from the interface definitions shipped with fprintd
//! (`share/dbus-1/interfaces/net.reactivated.Fprint.{Manager,Device}.xml`).

use zbus::proxy;

#[proxy(
  interface = "net.reactivated.Fprint.Manager",
  default_service = "net.reactivated.Fprint",
  default_path = "/net/reactivated/Fprint/Manager"
)]
pub trait Manager {
  /// Enumerates all fingerprint readers attached to the system.
  fn get_devices(&self) -> zbus::Result<Vec<zvariant::OwnedObjectPath>>;

  /// The default fingerprint reader. Fails with
  /// `net.reactivated.Fprint.Error.NoSuchDevice` if the system has none.
  fn get_default_device(&self) -> zbus::Result<zvariant::OwnedObjectPath>;
}

#[proxy(
  interface = "net.reactivated.Fprint.Device",
  default_service = "net.reactivated.Fprint"
)]
pub trait Device {
  /// Lists the enrolled fingers of `username`. An empty username means the user
  /// the calling client runs as, which is the only one we may access without
  /// further polkit authorization.
  fn list_enrolled_fingers(&self, username: &str) -> zbus::Result<Vec<String>>;

  /// Claims the reader for `username`, which must be held for the duration of a
  /// verification. Fails with `net.reactivated.Fprint.Error.AlreadyInUse` if
  /// another client (e.g. `pam_fprintd`) holds the claim.
  fn claim(&self, username: &str) -> zbus::Result<()>;

  fn release(&self) -> zbus::Result<()>;

  /// Starts one verification against the enrolled prints. `finger_name` may be
  /// `"any"` to match against every enrolled finger. Progress is reported via
  /// [`DeviceProxy::receive_verify_status`].
  fn verify_start(&self, finger_name: &str) -> zbus::Result<()>;

  fn verify_stop(&self) -> zbus::Result<()>;

  /// One status update of a running verification. `done` is set once the
  /// verification has finished and `verify_stop` should be called.
  #[zbus(signal)]
  fn verify_status(&self, result: String, done: bool) -> zbus::Result<()>;

  #[zbus(signal)]
  fn verify_finger_selected(&self, finger_name: String) -> zbus::Result<()>;

  /// The product name of the reader.
  #[zbus(property, name = "name")]
  fn name(&self) -> zbus::Result<String>;

  /// Either `"press"` or `"swipe"`.
  #[zbus(property, name = "scan-type")]
  fn scan_type(&self) -> zbus::Result<String>;

  /// Whether a finger is on the sensor right now. Reported during an operation,
  /// and only by drivers that track finger status at all.
  #[zbus(property, name = "finger-present")]
  fn finger_present(&self) -> zbus::Result<bool>;
}
