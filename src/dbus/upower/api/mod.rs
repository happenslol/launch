#![allow(unused)]

//! UPower D-Bus interface proxies
//!
//! Transcribed from the interface definitions shipped with UPower
//! (`share/dbus-1/interfaces/org.freedesktop.UPower.{,Device.}xml`).

use zbus::proxy;

#[proxy(
  interface = "org.freedesktop.UPower",
  default_service = "org.freedesktop.UPower",
  default_path = "/org/freedesktop/UPower"
)]
pub trait UPower {
  /// The composite device every power source in the machine is aggregated into.
  /// It exists even on machines that have none.
  fn get_display_device(&self) -> zbus::Result<zvariant::OwnedObjectPath>;

  /// Whether the machine is running off a battery rather than mains power.
  #[zbus(property)]
  fn on_battery(&self) -> zbus::Result<bool>;
}

#[proxy(
  interface = "org.freedesktop.UPower.Device",
  default_service = "org.freedesktop.UPower"
)]
pub trait Device {
  /// Charge left, in percent.
  #[zbus(property)]
  fn percentage(&self) -> zbus::Result<f64>;

  /// Whether the device is really there. False for the display device of a
  /// machine without any battery.
  #[zbus(property)]
  fn is_present(&self) -> zbus::Result<bool>;
}
