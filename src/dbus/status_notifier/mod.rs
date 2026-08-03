mod api;
mod watcher;

use std::collections::HashMap;

use anyhow::Result;
use futures::StreamExt;
use gpui::{App, AppContext, Entity, EventEmitter, Global, SharedString, Task};
use serde::Deserialize;
use tracing::{info, warn};
use zbus::fdo::IntrospectableProxy;
use zvariant::OwnedObjectPath;

pub use api::DBusMenuProxy;

use crate::dbus::GlobalDbusConnection;
use crate::util::ResultExt;
use watcher::Watcher;

#[derive(Clone, Debug)]
pub enum Status {
  Passive,
  Active,
  NeedsAttention,
}

impl std::str::FromStr for Status {
  type Err = String;

  fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
    match value {
      "Passive" => Ok(Status::Passive),
      "Active" => Ok(Status::Active),
      "NeedsAttention" => Ok(Status::NeedsAttention),
      other => Err(format!("invalid status: {:?}", other)),
    }
  }
}

#[derive(Clone)]
pub struct TrayItem {
  pub id: SharedString,
  pub title: SharedString,
  pub status: Status,
  pub icon_name: Option<SharedString>,
  pub icon_theme_path: Option<SharedString>,
  pub icon_pixmap: Option<Vec<(i32, i32, Vec<u8>)>>,
  pub menu_path: Option<OwnedObjectPath>,
  pub item_is_menu: bool,

  address: String,
  proxy: api::StatusNotifierItemProxy<'static>,
}

impl TrayItem {
  async fn from_address(connection: &zbus::Connection, address: &str) -> Result<Self> {
    let (destination, path) = {
      if let Some((addr, path)) = address.split_once('/') {
        (addr.to_owned(), format!("/{}", path))
      } else if address.starts_with(':') {
        let resolved = resolve_pathless_address(connection, address, "/".to_owned())
          .await?
          .ok_or_else(|| anyhow::anyhow!("no StatusNotifierItem found for {}", address))?;
        (address.to_owned(), resolved)
      } else {
        anyhow::bail!("invalid StatusNotifierItem address: {}", address);
      }
    };

    let proxy = api::StatusNotifierItemProxy::builder(connection)
      .destination(destination)?
      .path(path)?
      .build()
      .await?;

    let id = proxy.id().await.unwrap_or_default();
    let title = proxy.title().await.unwrap_or_default();
    let status_str = proxy.status().await.unwrap_or_else(|_| "Active".into());
    let status = status_str.parse().unwrap_or(Status::Active);
    let icon_name = proxy.icon_name().await.ok().filter(|s| !s.is_empty());
    let icon_theme_path = proxy.icon_theme_path().await.ok().filter(|s| !s.is_empty());
    let icon_pixmap = proxy.icon_pixmap().await.ok().filter(|v| !v.is_empty());
    let menu_path = proxy.menu().await.ok();
    let item_is_menu = proxy.item_is_menu().await.unwrap_or(false);

    Ok(TrayItem {
      id: SharedString::from(id),
      title: SharedString::from(title),
      status,
      icon_name: icon_name.map(SharedString::from),
      icon_theme_path: icon_theme_path.map(SharedString::from),
      icon_pixmap,
      menu_path,
      item_is_menu,
      address: address.to_owned(),
      proxy,
    })
  }

  pub fn has_menu(&self) -> bool {
    self.menu_path.is_some()
  }

  pub fn address(&self) -> &str {
    &self.address
  }

  pub async fn menu_proxy(&self) -> Result<Option<api::DBusMenuProxy<'static>>> {
    let path = match &self.menu_path {
      Some(path) => path.clone(),
      None => return Ok(None),
    };

    let proxy = api::DBusMenuProxy::builder(self.proxy.inner().connection())
      .destination(self.proxy.inner().destination().to_owned())?
      .path(path)?
      .build()
      .await?;

    Ok(Some(proxy))
  }

  pub async fn activate(&self, x: i32, y: i32) -> Result<()> {
    self.proxy.activate(x, y).await?;
    Ok(())
  }

  pub async fn secondary_activate(&self, x: i32, y: i32) -> Result<()> {
    self.proxy.secondary_activate(x, y).await?;
    Ok(())
  }

  #[allow(dead_code)]
  pub async fn context_menu(&self, x: i32, y: i32) -> Result<()> {
    self.proxy.context_menu(x, y).await?;
    Ok(())
  }

  #[allow(dead_code)]
  pub async fn scroll(&self, delta: i32, orientation: &str) -> Result<()> {
    self.proxy.scroll(delta, orientation).await?;
    Ok(())
  }
}

impl std::fmt::Debug for TrayItem {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TrayItem")
      .field("id", &self.id)
      .field("title", &self.title)
      .field("status", &self.status)
      .field("icon_name", &self.icon_name)
      .field("address", &self.address)
      .finish()
  }
}

// The `Item` prefix distinguishes these from the host-level registration
// events the SNI spec also defines, so it is kept despite being shared.
#[allow(clippy::enum_variant_names)]
pub enum SystrayEvent {
  ItemAdded(TrayItem),
  ItemRemoved { address: String },
  ItemUpdated { address: String },
}

struct GlobalSystray(Entity<Systray>);

impl Global for GlobalSystray {}

pub struct Systray {
  items: Vec<TrayItem>,
  _item_signal_tasks: HashMap<String, Task<()>>,
  _host_task: Option<Task<()>>,
}

impl EventEmitter<SystrayEvent> for Systray {}

impl Systray {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalSystray>().0.clone()
  }

  pub fn items(&self) -> &[TrayItem] {
    &self.items
  }

  fn add_item(&mut self, item: TrayItem, cx: &mut gpui::Context<Self>) {
    info!(?item, "systray item added");
    cx.emit(SystrayEvent::ItemAdded(item.clone()));
    self.items.push(item);
    cx.notify();
  }

  fn remove_item(&mut self, address: &str, cx: &mut gpui::Context<Self>) {
    if let Some(position) = self.items.iter().position(|i| i.address == address) {
      let item = self.items.remove(position);
      info!(?item, "systray item removed");
      self._item_signal_tasks.remove(address);
      cx.emit(SystrayEvent::ItemRemoved {
        address: address.to_owned(),
      });
      cx.notify();
    }
  }

  fn update_item(&mut self, address: &str, cx: &mut gpui::Context<Self>) {
    cx.emit(SystrayEvent::ItemUpdated {
      address: address.to_owned(),
    });
    cx.notify();
  }
}

pub fn init(cx: &mut App) {
  let entity = cx.new(|_cx| Systray {
    items: Vec::new(),
    _item_signal_tasks: HashMap::new(),
    _host_task: None,
  });

  cx.set_global(GlobalSystray(entity.clone()));

  let host_task = cx.spawn({
    let entity = entity.clone();
    async move |cx| {
      if let Err(error) = run_host(entity, cx).await {
        warn!(?error, "systray host loop exited with error");
      }
    }
  });

  entity.update(cx, |systray, _cx| {
    systray._host_task = Some(host_task);
  });
}

async fn run_host(entity: Entity<Systray>, cx: &mut gpui::AsyncApp) -> Result<()> {
  let conn_task = cx.update(GlobalDbusConnection::session);
  let connection: zbus::Connection = conn_task
    .await
    .ok_or_else(|| anyhow::anyhow!("failed to get session bus connection"))?;

  let watcher = Watcher::new();
  let watcher_items = watcher.items.clone();
  let watcher_hosts = watcher.hosts.clone();
  watcher.attach_to(&connection).await?;

  // Claim a well-known name for the host
  let pid = std::process::id();
  let mut counter = 0;
  let wellknown = loop {
    use zbus::fdo::RequestNameReply::*;
    counter += 1;
    let name = format!("org.freedesktop.StatusNotifierHost-{}-{}", pid, counter);
    let wellknown: zbus::names::WellKnownName = name
      .try_into()
      .map_err(|error| anyhow::anyhow!("invalid well-known name: {}", error))?;

    let flags = [zbus::fdo::RequestNameFlags::DoNotQueue];
    match connection
      .request_name_with_flags(&wellknown, flags.into_iter().collect())
      .await?
    {
      PrimaryOwner => break wellknown,
      Exists | AlreadyOwner => {}
      InQueue => unreachable!("DoNotQueue was specified"),
    };
  };

  let snw = api::StatusNotifierWatcherProxy::new(&connection).await?;
  snw.register_status_notifier_host(&wellknown).await?;

  info!("systray host registered as {}", wellknown);

  // Subscribe to signal streams before fetching existing items to avoid races
  let new_items = snw.receive_status_notifier_item_registered().await?;
  let gone_items = snw.receive_status_notifier_item_unregistered().await?;

  let dbus_proxy = zbus::fdo::DBusProxy::new(&connection).await?;
  let name_owner_changed = dbus_proxy.receive_name_owner_changed().await?;

  // Fetch existing items
  let existing = snw.registered_status_notifier_items().await?;
  for address in &existing {
    match TrayItem::from_address(&connection, address).await {
      Ok(item) => {
        let item_address = item.address.clone();
        let item_proxy = item.proxy.clone();
        entity.update(cx, |systray, cx| {
          systray.add_item(item, cx);
        });
        spawn_item_signal_listener(&entity, &item_address, &item_proxy, cx);
      }
      Err(error) => {
        warn!(?error, address, "could not create TrayItem from address");
      }
    }
  }

  enum Event {
    NewItem(api::StatusNotifierItemRegistered),
    GoneItem(api::StatusNotifierItemUnregistered),
    NameOwnerChanged(zbus::fdo::NameOwnerChanged),
  }

  let mut events = futures::stream::select_all([
    new_items.map(Event::NewItem).boxed(),
    gone_items.map(Event::GoneItem).boxed(),
    name_owner_changed.map(Event::NameOwnerChanged).boxed(),
  ]);

  while let Some(event) = events.next().await {
    match event {
      Event::NewItem(signal) => {
        let args = signal.args()?;
        let address = args.service;
        info!(address, "systray: item registered signal");

        let already_tracked = entity.read_with(cx, |systray, _cx| {
          systray.items.iter().any(|i| i.address == address)
        });

        if already_tracked {
          info!(address, "systray: duplicate item, skipping");
          continue;
        }

        match TrayItem::from_address(&connection, address).await {
          Ok(item) => {
            let item_address = item.address.clone();
            let item_proxy = item.proxy.clone();
            entity.update(cx, |systray, cx| {
              systray.add_item(item, cx);
            });
            spawn_item_signal_listener(&entity, &item_address, &item_proxy, cx);
          }
          Err(error) => {
            warn!(?error, address, "could not create TrayItem");
          }
        }
      }

      Event::GoneItem(signal) => {
        let args = signal.args()?;
        let address = args.service;
        info!(address, "systray: item unregistered signal");
        entity.update(cx, |systray, cx| {
          systray.remove_item(address, cx);
        });
      }

      Event::NameOwnerChanged(signal) => {
        let args = signal.args()?;
        if args.new_owner.is_some() {
          continue;
        }

        let gone_name = args.name.as_str();

        // Remove from watcher's shared state
        let removed_items: Vec<String> = {
          let mut items = watcher_items.lock().expect("mutex poisoned");
          let to_remove: Vec<String> = items
            .iter()
            .filter(|item| item.starts_with(gone_name))
            .cloned()
            .collect();
          for item in &to_remove {
            items.remove(item);
          }
          to_remove
        };

        let removed_host = {
          let mut hosts = watcher_hosts.lock().expect("mutex poisoned");
          hosts.remove(gone_name)
        };

        // Emit watcher signals for removed items/hosts
        let watcher_path = "/StatusNotifierWatcher";
        if let Ok(iface_ref) = connection
          .object_server()
          .interface::<_, Watcher>(watcher_path)
          .await
        {
          let emitter = iface_ref.signal_emitter();

          for item in &removed_items {
            Watcher::emit_item_unregistered(emitter, item)
              .await
              .log_err();
          }

          if !removed_items.is_empty() {
            Watcher::registered_status_notifier_items_refresh(emitter)
              .await
              .log_err();
          }

          if removed_host {
            Watcher::emit_host_unregistered(emitter).await.log_err();

            let hosts_empty = watcher_hosts.lock().expect("mutex poisoned").is_empty();
            if hosts_empty {
              Watcher::is_status_notifier_host_registered_refresh(emitter)
                .await
                .log_err();
            }
          }
        }

        // Remove from entity
        for item in &removed_items {
          entity.update(cx, |systray, cx| {
            systray.remove_item(item, cx);
          });
        }
      }
    }
  }

  Ok(())
}

fn spawn_item_signal_listener(
  entity: &Entity<Systray>,
  address: &str,
  proxy: &api::StatusNotifierItemProxy<'static>,
  cx: &mut gpui::AsyncApp,
) {
  let address = address.to_owned();
  let proxy = proxy.clone();
  let entity = entity.clone();

  let task = cx.spawn({
    let entity = entity.clone();
    let address = address.clone();
    async move |cx| {
      if let Err(error) = listen_item_signals(&entity, &address, &proxy, cx).await {
        warn!(?error, address, "item signal listener exited");
      }
    }
  });

  entity.update(cx, |systray, _cx| {
    systray._item_signal_tasks.insert(address, task);
  });
}

async fn listen_item_signals(
  entity: &Entity<Systray>,
  address: &str,
  proxy: &api::StatusNotifierItemProxy<'static>,
  cx: &mut gpui::AsyncApp,
) -> Result<()> {
  let new_icon = proxy.receive_new_icon().await?;
  let new_title = proxy.receive_new_title().await?;
  let new_status = proxy.receive_new_status().await?;

  // Variants mirror the SNI signal names and their generated `api` types.
  #[allow(clippy::enum_variant_names)]
  enum ItemSignal {
    NewIcon(#[allow(dead_code)] api::NewIcon),
    NewTitle(#[allow(dead_code)] api::NewTitle),
    NewStatus(api::NewStatus),
  }

  let mut signals = futures::stream::select_all([
    new_icon.map(ItemSignal::NewIcon).boxed(),
    new_title.map(ItemSignal::NewTitle).boxed(),
    new_status.map(ItemSignal::NewStatus).boxed(),
  ]);

  while let Some(signal) = signals.next().await {
    match signal {
      ItemSignal::NewIcon(_) => {
        let icon_name = proxy.icon_name().await.ok().filter(|s| !s.is_empty());
        let icon_theme_path = proxy.icon_theme_path().await.ok().filter(|s| !s.is_empty());
        let icon_pixmap = proxy.icon_pixmap().await.ok().filter(|v| !v.is_empty());
        let address = address.to_owned();

        entity.update(cx, |systray, cx| {
          if let Some(item) = systray.items.iter_mut().find(|i| i.address == address) {
            item.icon_name = icon_name.map(SharedString::from);
            item.icon_theme_path = icon_theme_path.map(SharedString::from);
            item.icon_pixmap = icon_pixmap;
            systray.update_item(&address, cx);
          }
        });
      }
      ItemSignal::NewTitle(_) => {
        let title = proxy.title().await.unwrap_or_default();
        let address = address.to_owned();

        entity.update(cx, |systray, cx| {
          if let Some(item) = systray.items.iter_mut().find(|i| i.address == address) {
            item.title = SharedString::from(title);
            systray.update_item(&address, cx);
          }
        });
      }
      ItemSignal::NewStatus(signal) => {
        let args = signal.args()?;
        let status: Status = args.status.parse().unwrap_or(Status::Active);
        let address = address.to_owned();

        entity.update(cx, |systray, cx| {
          if let Some(item) = systray.items.iter_mut().find(|i| i.address == address) {
            item.status = status;
            systray.update_item(&address, cx);
          }
        });
      }
    }
  }

  Ok(())
}

// Introspection helpers for resolving non-conforming item addresses

#[derive(Deserialize)]
struct DBusNode {
  #[serde(default)]
  interface: Vec<DBusInterface>,

  #[serde(default)]
  node: Vec<DBusNode>,

  #[serde(rename = "@name")]
  name: Option<String>,
}

#[derive(Deserialize)]
struct DBusInterface {
  #[serde(rename = "@name")]
  name: String,
}

async fn resolve_pathless_address(
  connection: &zbus::Connection,
  service: &str,
  path: String,
) -> Result<Option<String>> {
  let introspection_xml = IntrospectableProxy::builder(connection)
    .destination(service)?
    .path(path.as_str())?
    .build()
    .await?
    .introspect()
    .await?;

  let dbus_node = quick_xml::de::from_str::<DBusNode>(&introspection_xml)?;

  if dbus_node
    .interface
    .iter()
    .any(|iface| iface.name == "org.kde.StatusNotifierItem")
  {
    return Ok(Some(path));
  }

  for node in dbus_node.node {
    if let Some(name) = node.name {
      if name == "StatusNotifierItem" {
        return Ok(Some(join_to_path(&path, &name)));
      }

      let resolved = Box::pin(resolve_pathless_address(
        connection,
        service,
        join_to_path(&path, &name),
      ))
      .await?;

      if resolved.is_some() {
        return Ok(resolved);
      }
    }
  }

  Ok(None)
}

fn join_to_path(path: &str, name: &str) -> String {
  if path == "/" {
    format!("/{}", name)
  } else {
    format!("{}/{}", path, name)
  }
}
