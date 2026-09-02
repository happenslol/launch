#[zbus::proxy(
  interface = "org.freedesktop.systemd1.Manager",
  default_service = "org.freedesktop.systemd1",
  default_path = "/org/freedesktop/systemd1",
  gen_blocking = false
)]
pub trait Manager {
  /// Creates a unit that exists only for as long as it is needed and starts it.
  ///
  /// The returned path is the queued job, not the unit: the call is answered as
  /// soon as the job is enqueued, so a program that fails to execute does so
  /// after this has already returned successfully.
  fn start_transient_unit(
    &self,
    name: &str,
    mode: &str,
    properties: &[(&str, zvariant::Value<'_>)],
    aux: &[(&str, &[(&str, zvariant::Value<'_>)])],
  ) -> zbus::Result<zvariant::OwnedObjectPath>;
}
