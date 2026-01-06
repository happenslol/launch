mod devices;

pub use devices::BluetoothDevicesPanel;

use std::sync::Arc;

use gpui::prelude::*;

use crate::launcher::RootItem;

pub fn get_items() -> Vec<RootItem> {
  vec![RootItem::Panel {
    id: "bluetooth".into(),
    icon: None,
    name: "Bluetooth".into(),
    terms: vec!["bluetooth".into(), "bt".into(), "devices".into()],
    view: Arc::new(|window, cx| cx.new(|cx| BluetoothDevicesPanel::new(window, cx)).into()),
  }]
}
