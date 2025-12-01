use std::collections::HashMap;

use anyhow::Result;
use dbus_networkmanager::{device::SpecificDevice, nm::NetworkManager};
use futures::{StreamExt as _, stream::FuturesUnordered};
use gpui::{ClickEvent, Task, Window, prelude::*};

use crate::{
  network::dbus::read_access_points,
  util::{h_flex, v_flex},
};

const CONTEXT: &str = "wifi";

pub struct WifiPanel {
  conn: Option<zbus::Connection>,
  scan_task: Option<Task<Result<()>>>,
  tasks: Vec<Task<Result<()>>>,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.spawn_in(window, async move |this, cx| {
      let conn = zbus::Connection::system().await?;

      this.update_in(cx, |this, window, cx| {
        this.conn = Some(conn.clone());
        this.listen_changes(window, cx);
      })
    })
    .detach_and_log_err(cx);

    Self {
      conn: None,
      scan_task: None,
      tasks: Vec::new(),
    }
  }

  fn listen_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(conn) = self.conn.clone() else {
      return;
    };

    self
      .tasks
      .push(cx.spawn_in(window, async move |_this, _cx| {
        let nm = NetworkManager::new(&conn).await?;
        let devices = nm.devices().await?;
        let wifi_devices = FuturesUnordered::from_iter(devices.iter().map(|device| async move {
          let iface = device.interface().await.ok()?;
          match device.downcast_to_device().await {
            Ok(Some(SpecificDevice::Wireless(wifi))) => Some((iface, wifi)),
            _ => None,
          }
        }))
        .filter_map(|res| async move { res })
        .collect::<Vec<_>>()
        .await;

        FuturesUnordered::from_iter(wifi_devices.iter().map(|(iface, wifi_device)| async move {
          let mut aps_changed = wifi_device.receive_access_points_changed().await;

          while aps_changed.next().await.is_some() {
            let access_points = read_access_points(wifi_device).await?;
            println!("changed ({iface}): {access_points:?}");
          }

          Ok::<_, anyhow::Error>(())
        }))
        .collect::<Vec<_>>()
        .await;

        Ok(())
      }));
  }

  fn refresh(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
    let Some(conn) = self.conn.clone() else {
      return;
    };

    if self.scan_task.is_some() {
      println!("already scanning");
      return;
    }

    println!("scanning");
    self.scan_task = Some(cx.spawn_in(window, async move |this, cx| {
      let nm = NetworkManager::new(&conn).await?;
      let devices = nm.devices().await?;
      let wifi_devices = FuturesUnordered::from_iter(devices.iter().map(|device| async move {
        let iface = device.interface().await.ok()?;
        match device.downcast_to_device().await {
          Ok(Some(SpecificDevice::Wireless(wifi))) => Some((iface, wifi)),
          _ => None,
        }
      }))
      .filter_map(|res| async move { res })
      .collect::<Vec<_>>()
      .await;

      FuturesUnordered::from_iter(wifi_devices.iter().map(|(iface, wifi_device)| async move {
        let last_scan = wifi_device.last_scan().await?;
        println!("Waiting for scan after {last_scan} in {iface}");
        let mut last_scan_changed = wifi_device.receive_last_scan_changed().await;

        // actually request a scan
        wifi_device.request_scan(HashMap::new()).await?;

        while let Some(next_scan) = last_scan_changed.next().await {
          let next_scan = next_scan.get().await?;
          println!("Scanned {next_scan} in {iface}");
          if next_scan > last_scan {
            println!("Scan finished for {iface}");
            break;
          }
        }

        Ok::<_, anyhow::Error>(())
      }))
      .collect::<Vec<_>>()
      .await;

      println!("Scanning finished");
      this.update(cx, |this, _cx| this.scan_task.take())?;

      Ok(())
    }));
  }
}

impl Render for WifiPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex().key_context(CONTEXT).size_full().child(
      h_flex()
        .id("test-refresh")
        .child("Wifi")
        .on_click(cx.listener(Self::refresh)),
    )
  }
}
