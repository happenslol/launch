use gpui::SharedString;

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
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

#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct SinkInputId(pub u32);

#[derive(Debug, Clone)]
pub struct SinkInputInfo {
  pub id: SinkInputId,
  pub name: Option<SharedString>,
  pub sink_id: SinkId,
  pub volume: ChannelVolumes,
  pub mute: bool,
  pub application_name: Option<SharedString>,
  pub icon_name: Option<SharedString>,
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

impl From<ChannelVolumes> for pulse::volume::ChannelVolumes {
  fn from(v: ChannelVolumes) -> Self {
    let mut result = pulse::volume::ChannelVolumes::default();

    result.set_len(v.channels);
    for (index, volume) in result.get_mut().iter_mut().enumerate() {
      *volume = pulse::volume::Volume(v.volumes[index]);
    }

    result
  }
}

impl ChannelVolumes {
  pub fn set(&mut self, volume: Volume) {
    for v in self.volumes.iter_mut() {
      *v = volume.0;
    }
  }

  pub fn set_percent(&mut self, base: Volume, percent: u32) {
    let vol = (percent as f32 / 100. * base.0 as f32).round() as u32;
    self.set(Volume(vol));
  }

  pub fn add(&mut self, volume: Volume) {
    for v in self.volumes.iter_mut() {
      *v += volume.0;
    }
  }

  pub fn sub(&mut self, volume: Volume) {
    for v in self.volumes.iter_mut() {
      *v = v.saturating_sub(volume.0);
    }
  }

  pub fn add_percent(&mut self, base: Volume, percent: u32) {
    let vol = (percent as f32 / 100. * base.0 as f32).round() as i32;
    self.add(Volume(vol as u32));
  }

  pub fn sub_percent(&mut self, base: Volume, percent: u32) {
    let vol = (percent as f32 / 100. * base.0 as f32).round() as i32;
    self.sub(Volume(vol as u32));
  }

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

#[derive(Debug, Clone)]
pub enum SinkEvent {
  VolumeChanged(ChannelVolumes),
  MuteChanged(bool),
  InfoChanged(SinkInfo),
  BecameDefault,
  NoLongerDefault,
  Removed,
}

#[derive(Debug, Clone)]
pub enum SourceEvent {
  VolumeChanged(ChannelVolumes),
  MuteChanged(bool),
  InfoChanged(SourceInfo),
  BecameDefault,
  NoLongerDefault,
  Removed,
}

#[derive(Debug, Clone)]
pub enum SinkListEvent {
  Added(SinkInfo),
  Removed(SinkId),
  DefaultChanged(Option<SinkId>),
}

#[derive(Debug, Clone)]
pub enum SourceListEvent {
  Added(SourceInfo),
  Removed(SourceId),
  DefaultChanged(Option<SourceId>),
}

#[derive(Debug, Clone)]
pub enum SinkInputEvent {
  VolumeChanged(ChannelVolumes),
  MuteChanged(bool),
  SinkChanged(SinkId),
  InfoChanged(SinkInputInfo),
  Removed,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum SinkInputListEvent {
  Added(SinkInputInfo),
  Removed(SinkInputId),
}
