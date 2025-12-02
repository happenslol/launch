use std::{collections::HashMap, ops::Range, sync::Arc, time::Duration};

use anyhow::Result;
use dbus_networkmanager::{
  device::wireless::WirelessDevice, interface::device::wireless::WirelessDeviceProxy,
  nm::NetworkManager,
};
use futures::{StreamExt as _, stream::FuturesUnordered};
use gpui::{
  ClickEvent, FutureExt, Task, UniformListScrollHandle, Window, div, prelude::*, uniform_list,
};
use zbus::zvariant::ObjectPath;

use crate::{
  network::{
    dbus::{get_wifi_devices, read_access_points},
    types::AccessPoint,
  },
  util::{ResultExt, h_flex, v_flex},
};

const CONTEXT: &str = "wifi";

pub struct WifiPanel {
  conn: Option<zbus::Connection>,
  access_points: HashMap<String, (Arc<str>, Vec<AccessPoint>)>,
  active_access_point: HashMap<String, String>,
  scan_task: Option<Task<Result<()>>>,
  tasks: Vec<Task<Result<()>>>,
  list_scroll_handle: UniformListScrollHandle,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.spawn_in(window, async move |this, cx| {
      let conn = zbus::Connection::system().await?;

      this.update_in(cx, |this, window, cx| {
        this.conn = Some(conn.clone());
        this.listen_ap_changes(window, cx);
      })
    })
    .detach_and_log_err(cx);

    Self {
      conn: None,
      access_points: HashMap::new(),
      active_access_point: HashMap::new(),
      scan_task: None,
      tasks: Vec::new(),
      list_scroll_handle: UniformListScrollHandle::new(),
    }
  }

  fn listen_ap_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(conn) = self.conn.clone() else {
      return;
    };

    let (tx, rx) = flume::unbounded();
    let (active_tx, active_rx) = flume::unbounded();
    let listen_task = cx.background_spawn(async move {
      let nm = NetworkManager::new(&conn).await?;
      let wifi_devices = get_wifi_devices(&nm).await?;

      FuturesUnordered::from_iter(wifi_devices.into_iter().map(|(iface, wifi_device)| {
        let tx = tx.clone();
        let active_tx = active_tx.clone();
        let path: Arc<str> = Arc::from(wifi_device.inner().path().as_str());

        async move {
          let get_aps = {
            let tx = tx.clone();
            let iface = iface.clone();
            let path = path.clone();
            let wifi_device: WirelessDevice<'_> = wifi_device.clone().into();
            async move {
              let access_points = read_access_points(&wifi_device).await?;
              println!("sending aps for {iface}: {access_points:?}");
              tx.send_async((iface.clone(), (path.clone(), access_points)))
                .await?;
              Ok::<_, anyhow::Error>(())
            }
          };

          let receive_aps_changed = {
            let iface = iface.clone();
            let wifi_device: WirelessDevice<'_> = wifi_device.clone().into();
            async move {
              let mut aps_changed = wifi_device.receive_access_points_changed().await;

              while aps_changed.next().await.is_some() {
                let access_points = read_access_points(&wifi_device).await?;
                println!("sending aps for {iface}: {access_points:?}");
                tx.send_async((iface.clone(), (path.clone(), access_points)))
                  .await?;
              }

              Ok::<_, anyhow::Error>(())
            }
          };

          let receive_active_ap_changed = {
            let iface = iface.clone();
            let wifi_device: WirelessDevice<'_> = wifi_device.clone().into();
            async move {
              let mut active_ap_changed = wifi_device.receive_active_access_point_changed().await;

              while active_ap_changed.next().await.is_some() {
                let active_ap = wifi_device.active_access_point().await?;
                let ssid = String::from_utf8_lossy_owned(active_ap.ssid().await?);
                active_tx.send_async((iface.clone(), ssid)).await?;
              }

              Ok::<_, anyhow::Error>(())
            }
          };

          let receive_state_changed = {
            let iface = iface.clone();
            let wifi_device: WirelessDevice<'_> = wifi_device.clone().into();
            async move {
              let device = wifi_device.upcast().await?;
              let mut state_changed = device.receive_state_changed().await;

              while let Some(ev) = state_changed.next().await {
                let state = ev.get().await?;
                println!("state changed {iface}: {state:?}");
              }

              Ok::<_, anyhow::Error>(())
            }
          };

          futures::join!(
            receive_state_changed,
            get_aps,
            receive_aps_changed,
            receive_active_ap_changed
          )
        }
      }))
      .collect::<Vec<_>>()
      .await;

      Ok(())
    });

    let recv_task = cx.spawn_in(window, async move |this, cx| {
      while let Ok((iface, aps)) = rx.recv_async().await {
        this.update(cx, |this, cx| {
          println!("got aps for {iface}: {aps:?}");
          this.access_points.insert(iface, aps);
          cx.notify();
        })?;
      }

      Ok(())
    });

    let active_recv_task = cx.spawn_in(window, async move |this, cx| {
      while let Ok((iface, ssid)) = active_rx.recv_async().await {
        this.update(cx, |this, cx| {
          this.active_access_point.insert(iface, ssid);
          cx.notify();
        })?;
      }

      Ok(())
    });

    self.tasks.push(listen_task);
    self.tasks.push(recv_task);
    self.tasks.push(active_recv_task);
  }

  fn scan(&mut self, window: &mut Window, cx: &mut Context<Self>, path: Arc<str>) {
    let Some(conn) = self.conn.clone() else {
      return;
    };

    if self.scan_task.is_some() {
      return;
    }

    self.scan_task = Some(cx.spawn_in(window, async move |this, cx| {
      cx.background_spawn(async move {
        let wifi_device = WirelessDeviceProxy::builder(&conn)
          .path(ObjectPath::from_str_unchecked(&path))?
          .build()
          .await?;

        let last_scan = wifi_device.last_scan().await?;
        let mut last_scan_changed = wifi_device.receive_last_scan_changed().await;

        while let Some(next_scan) = last_scan_changed.next().await {
          if next_scan.get().await? > last_scan {
            break;
          }
        }

        Ok::<_, anyhow::Error>(())
      })
      .with_timeout(Duration::from_secs(10), cx.background_executor())
      .await
      .log_err();

      this.update(cx, |this, _cx| this.scan_task.take())?;

      Ok(())
    }));
  }
}

impl Render for WifiPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let iface = self.access_points.iter().next();

    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .when_some(iface, |this, (iface, (path, aps))| {
        this
          .child(div().id("refresh").child("refresh").on_click(cx.listener({
            let path = path.clone();
            move |this, _: &ClickEvent, window, cx| this.scan(window, cx, path.clone())
          })))
          .child(
            uniform_list(
              "access_points",
              aps.len(),
              cx.processor({
                let iface = iface.clone();
                move |this, range: Range<usize>, _window, _cx| {
                  range
                    .map(|ix| {
                      let ap = &this.access_points[&iface].1[ix];
                      h_flex().w_full().child(ap.ssid.to_string())
                    })
                    .collect()
                }
              }),
            )
            .track_scroll(&self.list_scroll_handle)
            .flex_1(),
          )
      })
  }
}
