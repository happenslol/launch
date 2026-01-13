mod sinks;
mod sources;
mod streams;

use std::sync::Arc;

use gpui::prelude::*;

use sinks::AudioSinksPanel;
pub use sinks::VolumeBar;
use sources::AudioSourcesPanel;
use streams::AudioStreamsPanel;

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
    RootItem::Panel {
      id: "streams".into(),
      icon: None,
      name: "Playback Streams".into(),
      terms: vec!["streams".into(), "playback".into(), "applications".into()],
      view: Arc::new(|window, cx| cx.new(|cx| AudioStreamsPanel::new(window, cx)).into()),
    },
  ]
}
