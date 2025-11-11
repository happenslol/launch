use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use thiserror::Error;

use super::{control, pipewire, pulse};

/// Capacity for broadcast channels for device, volume, and profile events
const EVENT_BROADCAST_CAPACITY: usize = 100;

/// Opaque identifier for a sink device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SinkId(pub(crate) u32);

/// Opaque identifier for a source device
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SourceId(pub(crate) u32);

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

impl From<&pulse::DeviceVariant> for DeviceId {
  fn from(variant: &pulse::DeviceVariant) -> Self {
    match variant {
      pulse::DeviceVariant::Alsa { alsa_card } => DeviceId::Alsa(*alsa_card),
      pulse::DeviceVariant::Bluez5 { address } => DeviceId::Bluez5(address.clone()),
    }
  }
}

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

#[derive(Default)]
struct AudioState {
  // Device information
  devices: BTreeMap<DeviceId, Card>,
  card_names: IndexMap<DeviceId, String>,
  card_profiles: IndexMap<DeviceId, Vec<pulse::CardProfile>>,
  active_profiles: IndexMap<DeviceId, Option<String>>,

  // Sink state
  sinks: Vec<String>,
  sink_pw_ids: Vec<u32>,
  active_sink: Option<usize>,
  active_sink_device: Option<DeviceId>,
  active_sink_profile: Option<usize>,
  sink_volume: u32,
  sink_mute: bool,

  // Source state
  sources: Vec<String>,
  source_pw_ids: Vec<u32>,
  active_source: Option<usize>,
  active_source_device: Option<DeviceId>,
  active_source_profile: Option<usize>,
  source_volume: u32,
  source_mute: bool,

  // Default devices
  default_sink: String,
  default_source: String,

  // Profile change tracking
  changing_sink_profile: Option<DeviceId>,
  changing_source_profile: Option<DeviceId>,
}

pub struct Audio {
  // Background thread handles
  pipewire_handle: JoinHandle<()>,
  pulse_handle: JoinHandle<()>,
  event_loop_handle: JoinHandle<()>,

  // Shutdown channels
  pipewire_shutdown: pipewire::ShutdownChannel,
  event_loop_shutdown: flume::Sender<()>,

  // Shared state
  state: Arc<RwLock<AudioState>>,

  // Broadcast channels for events
  device_tx: async_broadcast::Sender<DeviceEvent>,
  volume_tx: async_broadcast::Sender<VolumeEvent>,
  profile_tx: async_broadcast::Sender<ProfileEvent>,

  // Keeping these around keeps the broadcast channels open
  _device_rx: async_broadcast::InactiveReceiver<DeviceEvent>,
  _volume_rx: async_broadcast::InactiveReceiver<VolumeEvent>,
  _profile_rx: async_broadcast::InactiveReceiver<ProfileEvent>,

  // PulseAudio channels for volume control
  sink_channels: Arc<Mutex<Option<pulse::PulseChannels>>>,
}

impl Audio {
  /// Create a new Audio instance and start monitoring audio devices
  pub fn new() -> Result<Self, AudioError> {
    // Create broadcast channels
    let (device_tx, device_rx) = async_broadcast::broadcast(EVENT_BROADCAST_CAPACITY);
    let (volume_tx, volume_rx) = async_broadcast::broadcast(EVENT_BROADCAST_CAPACITY);
    let (profile_tx, profile_rx) = async_broadcast::broadcast(EVENT_BROADCAST_CAPACITY);

    // Create channels for internal communication
    let (pw_tx, pw_rx) = flume::unbounded();
    let (pulse_tx, pulse_rx) = flume::unbounded();
    let (event_loop_shutdown_tx, event_loop_shutdown_rx) = flume::unbounded();

    // Spawn PipeWire and PulseAudio threads
    let (pipewire_handle, pw_shutdown) = pipewire::spawn_thread(pw_tx);
    let pulse_handle = pulse::spawn_thread(pulse_tx);

    // Create shared state
    let state = Arc::new(RwLock::new(AudioState::default()));
    let sink_channels = Arc::new(Mutex::new(None));

    // Spawn event processing loop
    let event_loop_handle = std::thread::spawn({
      let state = state.clone();
      let device_tx = device_tx.clone();
      let volume_tx = volume_tx.clone();
      let profile_tx = profile_tx.clone();
      let sink_channels = sink_channels.clone();

      move || {
        event_loop_main(
          state,
          pw_rx,
          pulse_rx,
          event_loop_shutdown_rx,
          device_tx,
          volume_tx,
          profile_tx,
          sink_channels,
        )
      }
    });

    Ok(Audio {
      pipewire_handle,
      pulse_handle,
      event_loop_handle,
      pipewire_shutdown: pw_shutdown,
      event_loop_shutdown: event_loop_shutdown_tx,
      state,
      device_tx,
      volume_tx,
      profile_tx,
      sink_channels,
      _device_rx: device_rx.deactivate(),
      _volume_rx: volume_rx.deactivate(),
      _profile_rx: profile_rx.deactivate(),
    })
  }

  /// Get all available sinks
  pub fn sinks(&self) -> Vec<Sink> {
    let state = self.state.read().unwrap();
    state
      .sink_pw_ids
      .iter()
      .enumerate()
      .filter_map(|(idx, &pw_id)| {
        let name = state.sinks.get(idx)?.clone();

        // Find the device this sink belongs to
        let (device_id, device_type, profiles, active_profile) = state
          .devices
          .iter()
          .find_map(|(dev_id, card)| {
            card.ports.iter().find_map(|(&port_pw_id, _port)| {
              if port_pw_id == pw_id {
                let profiles = get_device_profiles(&state, dev_id);
                let active = state
                  .active_profiles
                  .get(dev_id)
                  .and_then(|p| p.as_ref().map(|s| ProfileId(s.clone())));
                Some((dev_id.clone(), DeviceType::from(dev_id), profiles, active))
              } else {
                None
              }
            })
          })
          .unwrap_or_else(|| (DeviceId::Unknown, DeviceType::Unknown, Vec::new(), None));

        let is_default = state
          .devices
          .get(&device_id)
          .and_then(|card| {
            card
              .ports
              .iter()
              .find(|(id, _)| **id == pw_id)
              .map(|(_, port)| port.identifier == state.default_sink)
          })
          .unwrap_or(false);

        Some(Sink {
          id: SinkId(pw_id),
          name: name.clone(),
          description: name,
          volume: if is_default { state.sink_volume } else { 0 },
          muted: if is_default { state.sink_mute } else { false },
          is_default,
          device_type,
          available_profiles: profiles,
          active_profile,
        })
      })
      .collect()
  }

  /// Get all available sources
  pub fn sources(&self) -> Vec<Source> {
    let state = self.state.read().unwrap();
    state
      .source_pw_ids
      .iter()
      .enumerate()
      .filter_map(|(idx, &pw_id)| {
        let name = state.sources.get(idx)?.clone();

        // Find the device this source belongs to
        let (device_id, device_type, profiles, active_profile) = state
          .devices
          .iter()
          .find_map(|(dev_id, card)| {
            card.ports.iter().find_map(|(&port_pw_id, _port)| {
              if port_pw_id == pw_id {
                let profiles = get_device_profiles(&state, dev_id);
                let active = state
                  .active_profiles
                  .get(dev_id)
                  .and_then(|p| p.as_ref().map(|s| ProfileId(s.clone())));
                Some((dev_id.clone(), DeviceType::from(dev_id), profiles, active))
              } else {
                None
              }
            })
          })
          .unwrap_or_else(|| (DeviceId::Unknown, DeviceType::Unknown, Vec::new(), None));

        let is_default = state
          .devices
          .get(&device_id)
          .and_then(|card| {
            card
              .ports
              .iter()
              .find(|(id, _)| **id == pw_id)
              .map(|(_, port)| port.identifier == state.default_source)
          })
          .unwrap_or(false);

        Some(Source {
          id: SourceId(pw_id),
          name: name.clone(),
          description: name,
          volume: if is_default { state.source_volume } else { 0 },
          muted: if is_default { state.source_mute } else { false },
          is_default,
          device_type,
          available_profiles: profiles,
          active_profile,
        })
      })
      .collect()
  }

  /// Get a specific sink by ID
  pub fn get_sink(&self, id: SinkId) -> Option<Sink> {
    self.sinks().into_iter().find(|s| s.id == id)
  }

  /// Get a specific source by ID
  pub fn get_source(&self, id: SourceId) -> Option<Source> {
    self.sources().into_iter().find(|s| s.id == id)
  }

  /// Get the default sink ID
  pub fn default_sink(&self) -> Option<SinkId> {
    let state = self.state.read().unwrap();
    state
      .active_sink
      .and_then(|idx| state.sink_pw_ids.get(idx).map(|&id| SinkId(id)))
  }

  /// Get the default source ID
  pub fn default_source(&self) -> Option<SourceId> {
    let state = self.state.read().unwrap();
    state
      .active_source
      .and_then(|idx| state.source_pw_ids.get(idx).map(|&id| SourceId(id)))
  }

  /// Set the volume of a sink (0-100)
  pub async fn set_sink_volume(&self, id: SinkId, volume: u32) -> Result<(), AudioError> {
    if volume > 100 {
      return Err(AudioError::InvalidVolume);
    }

    // Update internal state
    {
      let mut state = self.state.write().unwrap();
      state.sink_volume = volume;
    }

    // Use wpctl for sink volume
    control::wpctl_set_volume(id.0, volume).await?;

    Ok(())
  }

  /// Set the volume of a source (0-100)
  pub async fn set_source_volume(&self, id: SourceId, volume: u32) -> Result<(), AudioError> {
    if volume > 100 {
      return Err(AudioError::InvalidVolume);
    }

    // Update internal state
    {
      let mut state = self.state.write().unwrap();
      state.source_volume = volume;
    }

    // Use wpctl for source volume
    control::wpctl_set_volume(id.0, volume).await?;

    Ok(())
  }

  /// Set the mute state of a sink
  pub async fn set_sink_mute(&self, id: SinkId, muted: bool) -> Result<(), AudioError> {
    {
      let mut state = self.state.write().unwrap();
      state.sink_mute = muted;
    }

    control::wpctl_set_mute(id.0, muted).await
  }

  /// Set the mute state of a source
  pub async fn set_source_mute(&self, id: SourceId, muted: bool) -> Result<(), AudioError> {
    {
      let mut state = self.state.write().unwrap();
      state.source_mute = muted;
    }

    control::wpctl_set_mute(id.0, muted).await
  }

  /// Set the default sink
  pub async fn set_default_sink(&self, id: SinkId) -> Result<(), AudioError> {
    control::wpctl_set_default(id.0).await
  }

  /// Set the default source
  pub async fn set_default_source(&self, id: SourceId) -> Result<(), AudioError> {
    control::wpctl_set_default(id.0).await
  }

  /// Set the profile for a device
  pub async fn set_profile(
    &self,
    device_type: DeviceType,
    profile_id: ProfileId,
  ) -> Result<(), AudioError> {
    let device_id = match device_type {
      DeviceType::Alsa { card } => DeviceId::Alsa(card),
      DeviceType::Bluetooth { address } => DeviceId::Bluez5(address),
      DeviceType::Unknown => return Err(AudioError::DeviceNotFound),
    };

    let card_name = {
      let state = self.state.read().unwrap();
      state
        .card_names
        .get(&device_id)
        .cloned()
        .ok_or(AudioError::DeviceNotFound)?
    };

    control::pactl_set_card_profile(card_name, profile_id.0).await
  }

  /// Subscribe to device events
  pub fn device_events(&self) -> async_broadcast::Receiver<DeviceEvent> {
    self.device_tx.new_receiver()
  }

  /// Subscribe to volume events
  pub fn volume_events(&self) -> async_broadcast::Receiver<VolumeEvent> {
    self.volume_tx.new_receiver()
  }

  /// Subscribe to profile events
  pub fn profile_events(&self) -> async_broadcast::Receiver<ProfileEvent> {
    self.profile_tx.new_receiver()
  }

  pub fn quit(self) {
    // Signal shutdown
    let _ = self.pipewire_shutdown.send(());
    let _ = self.event_loop_shutdown.send(());

    // Quit PulseAudio channels
    if let Some(channels) = self.sink_channels.lock().unwrap().take() {
      channels.quit();
    }

    // Wait for threads to join
    let _ = self.pulse_handle.join();
    let _ = self.pipewire_handle.join();
    let _ = self.event_loop_handle.join();
  }
}

fn get_device_profiles(state: &AudioState, device_id: &DeviceId) -> Vec<Profile> {
  state
    .card_profiles
    .get(device_id)
    .map(|profiles| {
      profiles
        .iter()
        .filter(|p| p.available && p.name != "off")
        .map(|p| Profile {
          id: ProfileId(p.name.clone()),
          name: p.name.clone(),
          description: p.description.clone(),
          available: p.available,
        })
        .collect()
    })
    .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn event_loop_main(
  state: Arc<RwLock<AudioState>>,
  pw_rx: flume::Receiver<pipewire::Event>,
  pulse_rx: flume::Receiver<pulse::Event>,
  shutdown_rx: flume::Receiver<()>,
  device_tx: async_broadcast::Sender<DeviceEvent>,
  volume_tx: async_broadcast::Sender<VolumeEvent>,
  profile_tx: async_broadcast::Sender<ProfileEvent>,
  sink_channels: Arc<Mutex<Option<pulse::PulseChannels>>>,
) {
  loop {
    enum Event {
      Pipewire(pipewire::Event),
      Pulse(pulse::Event),
      Shutdown,
    }

    // Poll all channels
    let event = flume::Selector::new()
      .recv(&pw_rx, |res| {
        res.map(Event::Pipewire).unwrap_or(Event::Shutdown)
      })
      .recv(&pulse_rx, |res| {
        res.map(Event::Pulse).unwrap_or(Event::Shutdown)
      })
      .recv(&shutdown_rx, |_| Event::Shutdown)
      .wait();

    match event {
      Event::Shutdown => break,
      Event::Pipewire(pw_event) => handle_pipewire_event(pw_event, &state, &device_tx),
      Event::Pulse(pulse_event) => {
        handle_pulse_event(pulse_event, &state, &volume_tx, &profile_tx, &sink_channels)
      }
    }
  }
}

fn handle_pipewire_event(
  event: pipewire::Event,
  state: &Arc<RwLock<AudioState>>,
  device_tx: &async_broadcast::Sender<DeviceEvent>,
) {
  let mut state = state.write().unwrap();

  match event {
    pipewire::Event::Exited(err) => {
      // TODO
      panic!("Pipewire thread unexpectedly exited: {err}");
    }
    pipewire::Event::Device(pipewire::DeviceEvent::Add(device)) => {
      let device_id = DeviceId::from(&device.variant);

      match device.media_class {
        pipewire::MediaClass::Sink => {
          let product_name = device.product_name.clone();
          state.sinks.push(product_name.clone());
          state.sink_pw_ids.push(device.object_id);

          // Sort devices by name
          let mut tmp: Vec<(String, u32)> = std::mem::take(&mut state.sinks)
            .into_iter()
            .zip(std::mem::take(&mut state.sink_pw_ids))
            .collect();
          tmp.sort_unstable_by(|(ak, _), (bk, _)| ak.cmp(bk));
          (state.sinks, state.sink_pw_ids) = tmp.into_iter().unzip();

          if state.default_sink == device.node_name {
            state.active_sink_device = Some(device_id.clone());
            state.active_sink = state.sinks.iter().position(|s| *s == product_name);
          }

          // Emit device added event
          let profiles = get_device_profiles(&state, &device_id);
          let active_profile = state
            .active_profiles
            .get(&device_id)
            .and_then(|p| p.as_ref().map(|s| ProfileId(s.clone())));

          let sink = Sink {
            id: SinkId(device.object_id),
            name: product_name.clone(),
            description: product_name.clone(),
            volume: state.sink_volume,
            muted: state.sink_mute,
            is_default: state.default_sink == device.node_name,
            device_type: DeviceType::from(&device_id),
            available_profiles: profiles,
            active_profile,
          };

          let _ = device_tx.try_broadcast(DeviceEvent::SinkAdded(sink));
        }

        pipewire::MediaClass::Source => {
          let product_name = device.product_name.clone();
          state.sources.push(product_name.clone());
          state.source_pw_ids.push(device.object_id);

          // Sort devices by name
          let mut tmp: Vec<(String, u32)> = std::mem::take(&mut state.sources)
            .into_iter()
            .zip(std::mem::take(&mut state.source_pw_ids))
            .collect();
          tmp.sort_unstable_by(|(ak, _), (bk, _)| ak.cmp(bk));
          (state.sources, state.source_pw_ids) = tmp.into_iter().unzip();

          if state.default_source == device.node_name {
            state.active_source = state.sources.iter().position(|s| *s == product_name);
            state.active_source_device = Some(device_id.clone());
          }

          // Emit device added event
          let profiles = get_device_profiles(&state, &device_id);
          let active_profile = state
            .active_profiles
            .get(&device_id)
            .and_then(|p| p.as_ref().map(|s| ProfileId(s.clone())));

          let source = Source {
            id: SourceId(device.object_id),
            name: product_name.clone(),
            description: product_name.clone(),
            volume: state.source_volume,
            muted: state.source_mute,
            is_default: state.default_source == device.node_name,
            device_type: DeviceType::from(&device_id),
            available_profiles: profiles,
            active_profile,
          };

          let _ = device_tx.try_broadcast(DeviceEvent::SourceAdded(source));
        }
      }

      // Update card ports
      let description = match device.media_class {
        pipewire::MediaClass::Sink | pipewire::MediaClass::Source => device.product_name,
      };

      let card = state.devices.entry(device_id).or_insert_with(|| Card {
        ports: IndexMap::new(),
      });

      card.ports.insert(
        device.object_id,
        CardPort {
          class: device.media_class,
          identifier: device.node_name,
          description,
        },
      );

      card
        .ports
        .sort_unstable_by(|_, av, _, bv| av.description.cmp(&bv.description));
    }

    pipewire::Event::Device(pipewire::DeviceEvent::Remove(node_id)) => {
      // Remove from devices map
      let mut remove_device = None;
      for (card_id, card) in &mut state.devices {
        if card.ports.shift_remove(&node_id).is_some() {
          if card.ports.is_empty() {
            remove_device = Some(card_id.clone());
          }
          break;
        }
      }

      if let Some(card_id) = remove_device {
        let _ = state.devices.remove(&card_id);
      }

      // Remove from sinks
      if let Some(pos) = state.sink_pw_ids.iter().position(|&id| id == node_id) {
        let _ = state.sink_pw_ids.remove(pos);
        let _ = state.sinks.remove(pos);

        if state.active_sink == Some(pos) {
          state.active_sink = None;
          state.active_sink_device = None;
          state.active_sink_profile = None;
        } else {
          state.active_sink = state.active_sink.map(|active_pos| {
            if active_pos > pos {
              active_pos - 1
            } else {
              active_pos
            }
          });
        }

        let _ = device_tx.try_broadcast(DeviceEvent::SinkRemoved(SinkId(node_id)));
      }
      // Remove from sources
      else if let Some(pos) = state.source_pw_ids.iter().position(|&id| id == node_id) {
        let _ = state.source_pw_ids.remove(pos);
        let _ = state.sources.remove(pos);

        if state.active_source == Some(pos) {
          state.active_source = None;
          state.active_source_device = None;
          state.active_source_profile = None;
        }

        let _ = device_tx.try_broadcast(DeviceEvent::SourceRemoved(SourceId(node_id)));
      }
    }
  }
}

fn handle_pulse_event(
  event: pulse::Event,
  state: &Arc<RwLock<AudioState>>,
  volume_tx: &async_broadcast::Sender<VolumeEvent>,
  profile_tx: &async_broadcast::Sender<ProfileEvent>,
  sink_channels: &Arc<Mutex<Option<pulse::PulseChannels>>>,
) {
  let mut state = state.write().unwrap();

  match event {
    pulse::Event::Exited(err) => {
      // TODO
      panic!("Pulse thread unexpectedly exited: {err}");
    }
    pulse::Event::CardInfo(card) => {
      let device_id = DeviceId::from(&card.variant);

      state.card_names.insert(device_id.clone(), card.name);
      state
        .card_profiles
        .insert(device_id.clone(), card.profiles.clone());
      state
        .active_profiles
        .insert(device_id.clone(), card.active_profile.map(|p| p.name));

      // Emit profile event
      let profiles = get_device_profiles(&state, &device_id);
      let _ = profile_tx.try_broadcast(ProfileEvent::ProfilesChanged {
        device_type: DeviceType::from(&device_id),
        profiles,
      });
    }

    pulse::Event::DefaultSink(sink_name) => {
      if state.changing_sink_profile.is_none() {
        set_default_sink(&mut state, sink_name);
      }
    }

    pulse::Event::DefaultSource(source_name) => {
      if state.changing_source_profile.is_none() {
        set_default_source(&mut state, source_name);
      }
    }

    pulse::Event::SinkVolume(volume) => {
      state.sink_volume = volume;
      if let Some(active_sink_id) = state
        .active_sink
        .and_then(|idx| state.sink_pw_ids.get(idx).copied())
      {
        let _ = volume_tx.try_broadcast(VolumeEvent::SinkVolume {
          id: SinkId(active_sink_id),
          volume,
        });
      }
    }

    pulse::Event::SinkMute(muted) => {
      state.sink_mute = muted;
      if let Some(active_sink_id) = state
        .active_sink
        .and_then(|idx| state.sink_pw_ids.get(idx).copied())
      {
        let _ = volume_tx.try_broadcast(VolumeEvent::SinkMute {
          id: SinkId(active_sink_id),
          muted,
        });
      }
    }

    pulse::Event::SourceVolume(volume) => {
      state.source_volume = volume;
      if let Some(active_source_id) = state
        .active_source
        .and_then(|idx| state.source_pw_ids.get(idx).copied())
      {
        let _ = volume_tx.try_broadcast(VolumeEvent::SourceVolume {
          id: SourceId(active_source_id),
          volume,
        });
      }
    }

    pulse::Event::SourceMute(muted) => {
      state.source_mute = muted;
      if let Some(active_source_id) = state
        .active_source
        .and_then(|idx| state.source_pw_ids.get(idx).copied())
      {
        let _ = volume_tx.try_broadcast(VolumeEvent::SourceMute {
          id: SourceId(active_source_id),
          muted,
        });
      }
    }

    pulse::Event::Channels(channels) => {
      *sink_channels.lock().unwrap() = Some(channels);
    }
  }
}

fn set_default_sink(state: &mut AudioState, sink: String) {
  if state.default_sink == sink {
    return;
  }

  state.default_sink = sink;

  for (device_id, card) in &state.devices {
    for (&node_id, card_port) in &card.ports {
      if let pipewire::MediaClass::Sink = card_port.class
        && card_port.identifier == state.default_sink
      {
        state.active_sink = state.sink_pw_ids.iter().position(|&id| id == node_id);
        state.active_sink_device = Some(device_id.clone());
        return;
      }
    }
  }
}

fn set_default_source(state: &mut AudioState, source: String) {
  if state.default_source == source {
    return;
  }

  state.default_source = source;

  for (device_id, card) in &state.devices {
    for (&node_id, card_port) in &card.ports {
      if let pipewire::MediaClass::Source = card_port.class
        && card_port.identifier == state.default_source
      {
        state.active_source = state.source_pw_ids.iter().position(|&id| id == node_id);
        state.active_source_device = Some(device_id.clone());
        return;
      }
    }
  }
}
