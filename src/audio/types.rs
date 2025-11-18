use gpui::SharedString;

#[derive(Debug, Clone)]
pub struct ChannelVolumes(
  // TODO: SmallVec[2]?
  pub Vec<u32>,
);

impl PartialEq for ChannelVolumes {
  fn eq(&self, other: &Self) -> bool {
    if self.0.len() != other.0.len() {
      return false;
    }

    self.0.iter().zip(other.0.iter()).all(|(a, b)| a == b)
  }
}

impl Eq for ChannelVolumes {}

impl PartialEq<pulse::volume::ChannelVolumes> for ChannelVolumes {
  fn eq(&self, other: &pulse::volume::ChannelVolumes) -> bool {
    let other = other.get();
    self.0.len() == other.len() && self.0.iter().zip(other.iter()).all(|(a, b)| *a == b.0)
  }
}

impl From<pulse::volume::ChannelVolumes> for ChannelVolumes {
  fn from(v: pulse::volume::ChannelVolumes) -> Self {
    let channels = v.get();
    let mut result = Vec::with_capacity(channels.len());

    for channel in channels {
      result.push(channel.0);
    }

    Self(result)
  }
}

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct SinkId(pub u32);

#[derive(Debug, Clone)]
pub struct SinkInfo {
  pub id: SinkId,
  pub name: Option<SharedString>,
  pub volume: ChannelVolumes,
  pub mute: bool,
}

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct SourceId(pub u32);

#[derive(Debug, Clone)]
pub struct SourceInfo {
  pub id: SourceId,
  pub name: Option<SharedString>,
  pub volume: ChannelVolumes,
  pub mute: bool,
}
