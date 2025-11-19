use gpui::SharedString;

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct SinkId(pub u32);

#[derive(Debug, Clone)]
pub struct SinkInfo {
  pub id: SinkId,
  pub name: Option<SharedString>,
  pub description: Option<SharedString>,
  pub volume: ChannelVolumes,
  pub base_volume: Volume,
  pub mute: bool,
}

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct SourceId(pub u32);

#[derive(Debug, Clone)]
pub struct SourceInfo {
  pub id: SourceId,
  pub name: Option<SharedString>,
  pub description: Option<SharedString>,
  pub volume: ChannelVolumes,
  pub base_volume: Volume,
  pub mute: bool,
}

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
pub struct Volume(pub u32);

impl From<pulse::volume::Volume> for Volume {
  fn from(v: pulse::volume::Volume) -> Self {
    Self(v.0)
  }
}

#[derive(Debug, Clone)]
pub struct ChannelVolumes {
  pub channels: u8,
  pub volumes: [u32; pulse::volume::ChannelVolumes::CHANNELS_MAX as usize],
}

impl PartialEq for ChannelVolumes {
  fn eq(&self, other: &Self) -> bool {
    self.channels == other.channels
      && self
        .volumes
        .iter()
        .zip(other.volumes.iter())
        .all(|(a, b)| a == b)
  }
}

impl Eq for ChannelVolumes {}

impl PartialEq<pulse::volume::ChannelVolumes> for ChannelVolumes {
  fn eq(&self, other: &pulse::volume::ChannelVolumes) -> bool {
    let other = other.get();
    self.volumes.len() == other.len()
      && self
        .volumes
        .iter()
        .zip(other.iter())
        .all(|(a, b)| *a == b.0)
  }
}

impl From<pulse::volume::ChannelVolumes> for ChannelVolumes {
  fn from(v: pulse::volume::ChannelVolumes) -> Self {
    let channels = v.get();
    let mut result = Self {
      channels: channels.len() as u8,
      volumes: [0; pulse::volume::ChannelVolumes::CHANNELS_MAX as usize],
    };

    for (index, channel) in channels.iter().enumerate() {
      result.volumes[index] = channel.0;
    }

    result
  }
}

impl ChannelVolumes {
  pub fn as_percent(&self, base: Volume) -> u32 {
    let vol = self
      .volumes
      .iter()
      .take(self.channels as usize)
      .max()
      .copied()
      .unwrap_or(0) as f32;

    let base = base.0.max(1) as f32;

    (vol / base * 100.).round() as u32
  }
}
