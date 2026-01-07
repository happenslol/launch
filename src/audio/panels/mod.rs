mod sinks;
mod sources;

use std::sync::Arc;

use gpui::prelude::*;

pub use sinks::VolumeBar;
use sinks::AudioSinksPanel;
use sources::AudioSourcesPanel;

use crate::launcher::RootItem;

pub fn get_items() -> Vec<RootItem> {
  vec![
    RootItem::Panel {
      id: "sinks".into(),
      icon: None,
      name: "Volume".into(),
      terms: vec!["sinks".into(), "audio".into(), "volume".into()],
      view: Arc::new(|window, cx| cx.new(|cx| AudioSinksPanel::new(window, cx)).into()),
    },
    RootItem::Panel {
      id: "sources".into(),
      icon: None,
      name: "Microphone".into(),
      terms: vec![
        "sources".into(),
        "microphone".into(),
        "recording".into(),
        "input".into(),
      ],
      view: Arc::new(|window, cx| cx.new(|cx| AudioSourcesPanel::new(window, cx)).into()),
    },
  ]
}
