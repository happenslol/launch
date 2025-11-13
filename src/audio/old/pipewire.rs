// Adapted from cosmic-settings
// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: MPL-2.0

use std::{
  cell::RefCell,
  collections::HashMap,
  rc::Rc,
  thread::{self, JoinHandle},
};

use pipewire::{
  context::ContextRc as PwContext,
  main_loop::MainLoopRc as PwMainLoop,
  node::{Node, NodeInfoRef, NodeState},
  proxy::{Listener, ProxyT},
  types::ObjectType,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PipewireError {
  #[error(transparent)]
  Pipewire(#[from] pipewire::Error),
}

/// Device event
#[derive(Debug)]
pub enum Event {
  /// The pipewire thread unexpectedly exited
  Exited(PipewireError),
  /// A device was added or removed
  Device(DeviceEvent),
}

#[derive(Clone, Debug)]
pub enum DeviceEvent {
  /// A new device was detected.
  Add(Device),
  /// A device with the given object_id was removed.
  Remove(u32),
}

/// Device information
#[must_use]
#[derive(Clone, Debug)]
pub struct Device {
  pub object_id: u32,
  pub variant: DeviceVariant,
  pub media_class: MediaClass,
  pub product_name: String,
  pub node_name: String,
  pub state: DeviceState,
}

#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub enum DeviceVariant {
  Alsa { alsa_card: u32 },
  Bluez5 { address: String },
  Unknown {},
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceState {
  Idle,
  Running,
  Creating,
  Suspended,
  Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaClass {
  Source,
  Sink,
}

impl Device {
  /// Attains process info from a pipewire info node.
  #[must_use]
  pub fn from_node(info: &NodeInfoRef) -> Option<Self> {
    let props = info.props()?;

    let (variant, product_name) =
      if let Some(alsa_card) = props.get("alsa.card").and_then(|v| v.parse::<u32>().ok()) {
        let device_profile_description = props.get("device.profile.description")?.to_owned();

        let description = props.get("node.description")?;

        let description = description
          .strip_suffix(&device_profile_description)
          .map(str::trim_end)
          .unwrap_or(description)
          .replace("High Definition Audio", "HD Audio");

        (DeviceVariant::Alsa { alsa_card }, description)
      } else if let Some(address) = props
        .get("api.bluez5.address")
        .and_then(|v| v.parse::<String>().ok())
      {
        (
          DeviceVariant::Bluez5 {
            address: address.to_owned(),
          },
          props.get("node.description")?.to_owned(),
        )
      } else {
        (
          DeviceVariant::Unknown {},
          props.get("node.description")?.to_owned(),
        )
      };

    Some(Device {
      object_id: props.get("object.id")?.parse::<u32>().ok()?,
      variant,
      media_class: match props.get("media.class")? {
        "Audio/Sink" => MediaClass::Sink,
        "Audio/Source" => MediaClass::Source,
        _ => return None,
      },
      product_name,
      node_name: props.get("node.name")?.to_owned(),
      state: match info.state() {
        NodeState::Idle => DeviceState::Idle,
        NodeState::Running => DeviceState::Running,
        NodeState::Creating => DeviceState::Creating,
        NodeState::Suspended => DeviceState::Suspended,
        NodeState::Error(why) => DeviceState::Error(why.to_owned()),
      },
    })
  }
}

pub fn spawn_thread(tx: flume::Sender<Event>) -> JoinHandle<()> {
  thread::spawn(move || {
    if let Err(err) = thread_main(tx.clone()) {
      let _ = tx.send(Event::Exited(err));
    }
  })
}

fn thread_main(tx: flume::Sender<Event>) -> Result<(), PipewireError> {
  let main_loop = PwMainLoop::new(None)?;
  let context = PwContext::new(&main_loop, None)?;
  let core = context.connect_rc(None)?;

  let registry = Rc::new(core.get_registry_rc()?);
  let registry_weak = Rc::downgrade(&registry);

  let proxies = Rc::new(RefCell::new(HashMap::new()));
  let managed = Rc::new(RefCell::new(std::collections::BTreeMap::new()));
  let tx = Rc::new(tx);

  let main_loop_clone = main_loop.clone();

  let _registry_listener = registry
    .add_listener_local()
    .global(move |obj| {
      let Some(registry) = registry_weak.upgrade() else {
        return;
      };

      let attached_proxy: Option<(Box<dyn ProxyT>, Box<dyn Listener>)> = match obj.type_ {
        ObjectType::Node => {
          let Ok(node): Result<Node, _> = registry.bind(obj) else {
            return;
          };

          let listener = node
            .add_listener_local()
            .info({
              let managed = Rc::downgrade(&managed);
              let tx = Rc::downgrade(&tx);
              let main_loop = main_loop_clone.clone();
              let id = node.upcast_ref().id();

              move |info| {
                let (Some(managed), Some(tx)) = (managed.upgrade(), tx.upgrade()) else {
                  return;
                };

                let Some(device) = Device::from_node(info) else {
                  // Not a device we're interested in
                  return;
                };

                if managed.borrow_mut().insert(id, device.object_id).is_some() {
                  // We already know about this device
                  return;
                }

                if tx.send(Event::Device(DeviceEvent::Add(device))).is_err() {
                  main_loop.quit();
                }
              }
            })
            .register();

          Some((Box::new(node), Box::new(listener)))
        }

        _ => None,
      };

      if let Some((proxy_spe, listener)) = attached_proxy {
        let proxy = proxy_spe.upcast_ref();
        let id = proxy.id();
        let (object_type, _object_version) = proxy.get_type();

        let remove_listener = proxy
          .add_listener_local()
          .removed({
            let proxies = Rc::downgrade(&proxies);
            let managed = Rc::downgrade(&managed);
            let tx = Rc::downgrade(&tx);
            let main_loop = main_loop_clone.clone();

            move || {
              if object_type != ObjectType::Node {
                return;
              }

              let (Some(managed), Some(tx)) = (managed.upgrade(), tx.upgrade()) else {
                return;
              };

              if managed.borrow_mut().remove(&id).is_none() {
                // Already removed
                return;
              }

              if tx.send(Event::Device(DeviceEvent::Remove(id))).is_err() {
                main_loop.quit();
              }

              if let Some(proxies) = proxies.upgrade() {
                proxies.borrow_mut().remove(&id);
              }
            }
          })
          .register();

        proxies
          .borrow_mut()
          .insert(id, (proxy_spe, listener, remove_listener));
      }
    })
    .register();

  main_loop.run();
  Ok(())
}
