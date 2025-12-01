mod sinks;

use std::sync::Arc;

use gpui::prelude::*;

use crate::{audio::panels::sinks::AudioSinksPanel, launcher::RootItem};

pub fn get_items() -> Vec<RootItem> {
  vec![RootItem::Panel {
    id: "sinks".into(),
    icon: None,
    name: "Volume".into(),
    terms: vec!["sinks".into(), "audio".into(), "volume".into()],
    view: Arc::new(|window, cx| cx.new(|cx| AudioSinksPanel::new(window, cx)).into()),
  }]
}
