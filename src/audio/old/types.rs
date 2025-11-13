use indexmap::IndexMap;
use thiserror::Error;

use super::{pipewire, pulse};

/// Opaque identifier for a sink device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SinkId(pub u32);

/// Opaque identifier for a source device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub u32);

/// Identifier for a device profile
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileId(pub String);

/// Type of audio device
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceType {
  /// ALSA device with card number
  Alsa { card: u32 },
  /// Bluetooth device with address
  Bluetooth { address: String },
  /// Unknown device type
  Unknown,
}

/// Information about a sink (output) device
#[derive(Debug, Clone)]
pub struct Sink {
  pub id: SinkId,
  pub name: String,
  pub description: String,
  pub volume: u32,
  pub muted: bool,
  pub is_default: bool,
  pub device_type: DeviceType,
  pub available_profiles: Vec<Profile>,
  pub active_profile: Option<ProfileId>,
}

/// Information about a source (input) device
#[derive(Debug, Clone)]
pub struct Source {
  pub id: SourceId,
  pub name: String,
  pub description: String,
  pub volume: u32,
  pub muted: bool,
  pub is_default: bool,
  pub device_type: DeviceType,
  pub available_profiles: Vec<Profile>,
  pub active_profile: Option<ProfileId>,
}

/// Audio device profile
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
  pub id: ProfileId,
  pub name: String,
  pub description: String,
  pub available: bool,
}

/// Events related to device changes
#[derive(Debug, Clone)]
pub enum DeviceEvent {
  SinkAdded(Sink),
  SinkRemoved(SinkId),
  SourceAdded(Source),
  SourceRemoved(SourceId),
}

/// Events related to volume changes
#[derive(Debug, Clone)]
pub enum VolumeEvent {
  SinkVolume { id: SinkId, volume: u32 },
  SinkMute { id: SinkId, muted: bool },
  SourceVolume { id: SourceId, volume: u32 },
  SourceMute { id: SourceId, muted: bool },
}

/// Events related to profile changes
#[derive(Debug, Clone)]
pub enum ProfileEvent {
  ProfilesChanged {
    device_type: DeviceType,
    profiles: Vec<Profile>,
  },
  ActiveProfileChanged {
    device_type: DeviceType,
    profile: ProfileId,
  },
}

/// Errors that can occur in audio operations
#[derive(Debug, Clone, Error)]
pub enum AudioError {
  #[error("Command failed: {0}")]
  CommandFailed(String),
  #[error("Device not found")]
  DeviceNotFound,
  #[error("Invalid volume (must be 0-100)")]
  InvalidVolume,
  #[error("Init failed: {0}")]
  InitFailed(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
enum DeviceId {
  Alsa(u32),
  Bluez5(String),
  Unknown,
}

impl From<&pipewire::DeviceVariant> for DeviceId {
  fn from(variant: &pipewire::DeviceVariant) -> Self {
    match variant {
      pipewire::DeviceVariant::Alsa { alsa_card } => DeviceId::Alsa(*alsa_card),
      pipewire::DeviceVariant::Bluez5 { address } => DeviceId::Bluez5(address.clone()),
      pipewire::DeviceVariant::Unknown {} => DeviceId::Unknown,
    }
  }
}

// impl From<&pulse::DeviceVariant> for DeviceId {
//   fn from(variant: &pulse::DeviceVariant) -> Self {
//     match variant {
//       pulse::DeviceVariant::Alsa { alsa_card } => DeviceId::Alsa(*alsa_card),
//       pulse::DeviceVariant::Bluez5 { address } => DeviceId::Bluez5(address.clone()),
//     }
//   }
// }

impl From<&DeviceId> for DeviceType {
  fn from(id: &DeviceId) -> Self {
    match id {
      DeviceId::Alsa(card) => DeviceType::Alsa { card: *card },
      DeviceId::Bluez5(address) => DeviceType::Bluetooth {
        address: address.clone(),
      },
      DeviceId::Unknown => DeviceType::Unknown,
    }
  }
}

struct Card {
  ports: IndexMap<u32, CardPort>,
}

struct CardPort {
  class: pipewire::MediaClass,
  identifier: String,
  description: String,
}
