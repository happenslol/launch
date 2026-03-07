#[zbus::proxy(
  interface = "org.freedesktop.login1.Manager",
  default_service = "org.freedesktop.login1",
  default_path = "/org/freedesktop/login1"
)]
pub trait Manager {
  fn power_off(&self, interactive: bool) -> zbus::Result<()>;
  fn reboot(&self, interactive: bool) -> zbus::Result<()>;
  fn suspend(&self, interactive: bool) -> zbus::Result<()>;
}
