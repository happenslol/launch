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

/// The caller's own session; logind resolves the `auto` path to it, so no
/// session id has to be looked up first.
#[zbus::proxy(
  interface = "org.freedesktop.login1.Session",
  default_service = "org.freedesktop.login1",
  default_path = "/org/freedesktop/login1/session/auto"
)]
pub trait Session {
  /// Records whether the session is locked. This is what `loginctl` and idle
  /// daemons read back; it is purely informational and locks nothing itself.
  fn set_locked_hint(&self, locked: bool) -> zbus::Result<()>;

  /// Sent when something asks the session to lock, e.g. `loginctl lock-session`
  /// or an idle daemon.
  #[zbus(signal)]
  fn lock(&self) -> zbus::Result<()>;

  /// Sent when something asks the session to unlock, e.g.
  /// `loginctl unlock-session`.
  #[zbus(signal)]
  fn unlock(&self) -> zbus::Result<()>;
}
