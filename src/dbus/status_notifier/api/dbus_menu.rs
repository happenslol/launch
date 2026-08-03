use std::collections::HashMap;

use zbus::proxy;
use zvariant::{OwnedValue, Value};

#[proxy(
  interface = "com.canonical.dbusmenu",
  assume_defaults = false,
  default_path = "/com/canonical/dbusmenu"
)]
pub trait DBusMenu {
  fn get_layout(
    &self,
    parent_id: i32,
    recursion_depth: i32,
    property_names: Vec<&str>,
  ) -> zbus::Result<(u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>))>;

  fn event(&self, id: i32, event_id: &str, data: &Value<'_>, timestamp: u32) -> zbus::Result<()>;

  fn about_to_show(&self, id: i32) -> zbus::Result<bool>;
}
