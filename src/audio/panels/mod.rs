mod sinks;
mod sources;
mod streams;

use std::sync::Arc;

use gpui::prelude::*;

pub use sinks::{AudioSinksPanel, VolumeBar};
use sources::AudioSourcesPanel;
use streams::AudioStreamsPanel;

use crate::{icon::IconName, launcher::RootItem};

pub fn get_items() -> Vec<RootItem> {
  vec![
    RootItem::Panel {
      id: "sinks".into(),
      icon: IconName::Volume,
      name: "Volume".into(),
      description: "Manage audio output devices and volume".into(),
      terms: vec!["sinks".into(), "audio".into(), "volume".into()],
      view: Arc::new(|window, cx| cx.new(|cx| AudioSinksPanel::new(window, cx)).into()),
    },
    RootItem::Panel {
      id: "sources".into(),
      icon: IconName::Microphone,
      name: "Microphone".into(),
      description: "Manage audio input devices".into(),
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
      icon: IconName::Headphones,
      name: "Playback Streams".into(),
      description: "Manage per-application audio playback".into(),
      terms: vec!["streams".into(), "playback".into(), "applications".into()],
      view: Arc::new(|window, cx| cx.new(|cx| AudioStreamsPanel::new(window, cx)).into()),
    },
  ]
}
