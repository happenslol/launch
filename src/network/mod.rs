mod types;

use std::sync::{Arc, atomic::AtomicBool};

use anyhow::Result;
use dbus_networkmanager::{interface::enums::DeviceType, nm::NetworkManager};
use futures::{StreamExt as _, stream::FuturesOrdered};
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, SharedString, Task, Window,
  prelude::*, rgb,
};

use crate::{
  launcher::RootItem,
  network::types::{DeviceConnection, DeviceInfo, KnownDeviceConnection},
  picker::{Picker, PickerDelegate},
  util::{h_flex, v_flex},
};

pub fn get_items() -> Result<Vec<RootItem>> {
  Ok(vec![RootItem::Panel {
    id: "networks".into(),
    name: "Networks".into(),
    icon: None,
    terms: vec!["net".into(), "network".into(), "ethernet".into()],
    view: Arc::new(|window, cx| cx.new(|cx| NetworkPanel::new(window, cx)).into()),
  }])
}

pub struct NetworkPanel {
  picker: Entity<Picker<NetworkDelegate>>,
  _dbus_task: Task<Result<()>>,
}

const CONTEXT: &str = "network";

impl NetworkPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let picker = cx.new(|cx| Picker::new(NetworkDelegate {}, vec![], window, cx));
    cx.focus_view(&picker, window);

    let dbus_task = cx.spawn_in(window, async move |_window, _cx| {
      let conn = zbus::Connection::system().await?;
      let nm = NetworkManager::new(&conn).await?;

      let (devices, nm_settings) = futures::try_join!(nm.devices(), nm.settings())?;
      let conn_settings: &Vec<_> = &FuturesOrdered::from_iter(
        nm_settings
          .list_connections()
          .await?
          .into_iter()
          .map(|conn| async move { conn.get_settings().await }),
      )
      .filter_map(|res| async move { res.ok() })
      .collect()
      .await;

      let device_iter = devices.into_iter().map(|device| async move {
        let device_type = device.device_type().await.ok()?;
        if !matches!(device_type, DeviceType::Ethernet | DeviceType::Wifi) {
          return None;
        }

        let (interface, hw_address, state, available_connections) = futures::try_join!(
          device.interface(),
          device.hw_address(),
          device.state(),
          device.available_connections()
        )
        .ok()?;

        if hw_address.is_empty() {
          return None;
        }

        let (active_connection, available_connections) = futures::join!(
          async {
            let conn = device.active_connection().await?;
            let (id, uuid, state) = futures::try_join!(conn.id(), conn.uuid(), conn.state())?;

            Ok::<_, zbus::Error>((
              DeviceConnection {
                id,
                uuid: Arc::from(uuid),
                path: conn.inner().path().to_owned(),
              },
              state,
            ))
          },
          FuturesOrdered::from_iter(available_connections.into_iter().map(|conn| async move {
            let path = conn.inner().path().to_owned();
            let settings = conn.get_settings().await.ok()?;

            let id = settings
              .get("connection")?
              .get("id")?
              .downcast_ref::<String>()
              .ok()?;

            let uuid = settings["connection"]
              .get("uuid")?
              .downcast_ref::<String>()
              .ok()?;

            Some(DeviceConnection {
              id,
              uuid: Arc::from(uuid),
              path,
            })
          }),)
          .filter_map(|res| async move { res })
          .collect::<Vec<_>>()
        );

        let known_connections = conn_settings
          .iter()
          .flat_map(|conn_settings| {
            let connection = conn_settings.get("connection")?;

            let interface_name = connection
              .get("interface-name")?
              .downcast_ref::<String>()
              .ok()?;

            if interface_name != interface {
              return None;
            }

            let id = connection.get("id")?.downcast_ref::<String>().ok()?;
            let uuid = connection.get("uuid")?.downcast_ref::<String>().ok()?;

            Some(KnownDeviceConnection {
              uuid: Arc::from(uuid),
              id,
            })
          })
          .collect();

        Some(DeviceInfo {
          path: device.inner().path().to_owned(),
          device_type,
          interface,
          state,
          active_connection: active_connection.ok(),
          known_connections,
          available_connections,
        })
      });

      let devices_info = FuturesOrdered::from_iter(device_iter)
        .filter_map(|res| async move { res })
        .collect::<Vec<DeviceInfo>>()
        .await;

      println!("{devices_info:#?}");

      Ok(())
    });

    Self {
      picker,
      _dbus_task: dbus_task,
    }
  }
}

impl Focusable for NetworkPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for NetworkPanel {
  fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .child(self.picker.clone())
  }
}

#[derive(Debug, Clone)]
pub struct Network {
  name: SharedString,
}

struct NetworkDelegate {}

impl PickerDelegate for NetworkDelegate {
  type ListItem = Network;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    h_flex()
      .w_full()
      .when_else(
        is_selected,
        |div| div.bg(rgb(0xDDDDDD)),
        |div| div.bg(rgb(0xFFFFFF)),
      )
      .child(item.name.clone())
  }

  fn update_matches(
    &mut self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    _query: String,
    _cancel_flag: Arc<AtomicBool>,
    _search_id: usize,
    _items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    Task::ready(())
  }
}
