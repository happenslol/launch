use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tracing::{info, warn};
use zbus::object_server::Interface;

const WATCHER_BUS: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_OBJECT: &str = "/StatusNotifierWatcher";
const ITEM_OBJECT: &str = "/StatusNotifierItem";

pub struct Watcher {
  pub(crate) hosts: Arc<Mutex<HashSet<String>>>,
  pub(crate) items: Arc<Mutex<HashSet<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
  async fn register_status_notifier_host(
    &mut self,
    service: &str,
    #[zbus(header)] header: zbus::message::Header<'_>,
    #[zbus(connection)] connection: &zbus::Connection,
    #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::fdo::Result<()> {
    let (service, _) = parse_service(service, header, connection).await?;
    info!("new host: {}", service);

    let added_first = {
      let mut hosts = self.hosts.lock().expect("mutex poisoned");
      if !hosts.insert(service.to_string()) {
        return Ok(());
      }
      hosts.len() == 1
    };

    if added_first {
      self
        .is_status_notifier_host_registered_changed(&emitter)
        .await?;
    }
    Watcher::status_notifier_host_registered(&emitter).await?;

    Ok(())
  }

  #[zbus(signal)]
  async fn status_notifier_host_registered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::Result<()>;

  #[zbus(signal)]
  async fn status_notifier_host_unregistered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::Result<()>;

  #[zbus(property)]
  async fn is_status_notifier_host_registered(&self) -> bool {
    let hosts = self.hosts.lock().expect("mutex poisoned");
    !hosts.is_empty()
  }

  async fn register_status_notifier_item(
    &mut self,
    service: &str,
    #[zbus(header)] header: zbus::message::Header<'_>,
    #[zbus(connection)] connection: &zbus::Connection,
    #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::fdo::Result<()> {
    let (service, object_path) = parse_service(service, header, connection).await?;
    let service = zbus::names::BusName::Unique(service);

    let item = format!("{}{}", service, object_path);

    {
      let mut items = self.items.lock().expect("mutex poisoned");
      if !items.insert(item.clone()) {
        info!("new item: {} (duplicate)", item);
        return Ok(());
      }
    }
    info!("new item: {}", item);

    self
      .registered_status_notifier_items_changed(&emitter)
      .await?;
    Watcher::status_notifier_item_registered(&emitter, item.as_ref()).await?;

    Ok(())
  }

  #[zbus(signal)]
  async fn status_notifier_item_registered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    service: &str,
  ) -> zbus::Result<()>;

  #[zbus(signal)]
  async fn status_notifier_item_unregistered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    service: &str,
  ) -> zbus::Result<()>;

  #[zbus(property)]
  async fn registered_status_notifier_items(&self) -> Vec<String> {
    let items = self.items.lock().expect("mutex poisoned");
    items.iter().cloned().collect()
  }

  #[zbus(property)]
  fn protocol_version(&self) -> i32 {
    0
  }
}

impl Watcher {
  pub fn new() -> Watcher {
    Watcher {
      hosts: Arc::new(Mutex::new(HashSet::new())),
      items: Arc::new(Mutex::new(HashSet::new())),
    }
  }

  pub async fn emit_item_unregistered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    service: &str,
  ) -> zbus::Result<()> {
    Self::status_notifier_item_unregistered(emitter, service).await
  }

  pub async fn emit_host_unregistered(
    emitter: &zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::Result<()> {
    Self::status_notifier_host_unregistered(emitter).await
  }

  pub async fn is_status_notifier_host_registered_refresh(
    emitter: &zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::Result<()> {
    zbus::fdo::Properties::properties_changed(
      emitter,
      <Self as Interface>::name(),
      HashMap::new(),
      Cow::Borrowed(&["IsStatusNotifierHostRegistered"]),
    )
    .await
  }

  pub async fn registered_status_notifier_items_refresh(
    emitter: &zbus::object_server::SignalEmitter<'_>,
  ) -> zbus::Result<()> {
    zbus::fdo::Properties::properties_changed(
      emitter,
      <Self as Interface>::name(),
      HashMap::new(),
      Cow::Borrowed(&["RegisteredStatusNotifierItems"]),
    )
    .await
  }

  pub async fn attach_to(self, connection: &zbus::Connection) -> zbus::Result<()> {
    if !connection
      .object_server()
      .at(WATCHER_OBJECT, self)
      .await?
    {
      return Err(zbus::Error::Failure(format!(
        "Object already exists at {} -- is StatusNotifierWatcher already running?",
        WATCHER_OBJECT
      )));
    }

    let flags: [zbus::fdo::RequestNameFlags; 0] = [];
    match connection
      .request_name_with_flags(WATCHER_BUS, flags.into_iter().collect())
      .await
    {
      Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Ok(()),
      Ok(_) | Err(zbus::Error::NameTaken) => Ok(()),
      Err(error) => Err(error),
    }
  }
}

/// Decode the service name into bus name + object path.
///
/// The spec says the format should be just the bus name, but some items
/// pass non-conforming values (e.g. just an object path).
async fn parse_service<'a>(
  service: &'a str,
  header: zbus::message::Header<'_>,
  connection: &zbus::Connection,
) -> zbus::fdo::Result<(zbus::names::UniqueName<'static>, &'a str)> {
  if service.starts_with('/') {
    if let Some(sender) = header.sender() {
      Ok((sender.to_owned(), service))
    } else {
      warn!("unknown sender");
      Err(zbus::fdo::Error::InvalidArgs("Unknown bus address".into()))
    }
  } else {
    let bus_name: zbus::names::BusName = match service.try_into() {
      Ok(name) => name,
      Err(error) => {
        warn!("received invalid bus name {:?}: {}", service, error);
        return Err(zbus::fdo::Error::InvalidArgs(error.to_string()));
      }
    };

    if let zbus::names::BusName::Unique(unique) = bus_name {
      Ok((unique.to_owned(), ITEM_OBJECT))
    } else {
      let dbus = zbus::fdo::DBusProxy::new(connection).await?;
      match dbus.get_name_owner(bus_name).await {
        Ok(owner) => Ok((owner.into_inner(), ITEM_OBJECT)),
        Err(error) => {
          warn!("failed to get owner of {:?}: {}", service, error);
          Err(error)
        }
      }
    }
  }
}
