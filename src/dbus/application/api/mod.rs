#[zbus::proxy(interface = "org.freedesktop.Application", gen_blocking = false)]
pub trait Application {
  fn activate(
    &self,
    platform_data: std::collections::HashMap<&str, zvariant::Value<'_>>,
  ) -> zbus::Result<()>;
}
