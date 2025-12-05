use std::collections::HashMap;

use anyhow::Result;
use gpui::{SharedString, Subscription, Task, UniformListScrollHandle, Window, prelude::*};

use crate::{
  dbus::{
    GlobalDbusConnection,
    networkmanager::{NetworkManager, WirelessDevice},
  },
  util::v_flex,
};

const CONTEXT: &str = "wifi";

pub struct WifiPanel {
  devices: HashMap<SharedString, WirelessDevice>,
  listen_task: Option<Task<Result<()>>>,
  _list_scroll_handle: UniformListScrollHandle,
  _subscriptions: Vec<Subscription>,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let listen_task = GlobalDbusConnection::system(cx).map(|conn| {
      println!("dbus conn found on spawn");
      Self::listen(window, cx, &conn)
    });

    let subscriptions =
      vec![
        cx.observe_global_in::<GlobalDbusConnection>(window, |this, window, cx| {
          let Some(conn) = GlobalDbusConnection::system(cx) else {
            return;
          };

          this.listen_task = Some(Self::listen(window, cx, &conn));
        }),
      ];

    Self {
      devices: HashMap::new(),
      listen_task,
      _list_scroll_handle: UniformListScrollHandle::new(),
      _subscriptions: subscriptions,
    }
  }

  fn listen(
    window: &mut Window,
    cx: &mut Context<Self>,
    conn: &zbus::Connection,
  ) -> Task<Result<()>> {
    let conn = conn.clone();
    cx.spawn_in(window, async move |this, cx| {
      let devices = cx
        .background_spawn(async move {
          let nm = NetworkManager::new(&conn).await?;
          let devices = nm
            .get_wireless_devices()
            .await?
            .into_iter()
            .map(|device| {
              let name = device.name.clone();
              (name, device)
            })
            .collect();

          Ok::<_, anyhow::Error>(devices)
        })
        .await?;

      this.update(cx, |this, cx| {
        println!("got devices: {:?}", devices);
        this.devices = devices;
        cx.notify();
      })?;
      Ok(())
    })
  }
}

impl Render for WifiPanel {
  fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    v_flex().key_context(CONTEXT)
  }
}
