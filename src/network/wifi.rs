use std::sync::{Arc, atomic::AtomicBool};

use futures::StreamExt;
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, Styled, Subscription,
  Task, Window, actions, div, prelude::*, rgb,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use zvariant::OwnedObjectPath;

use crate::{
  dbus::{
    GlobalDbusConnection,
    networkmanager::{AccessPoint, NetworkManager, WirelessDevice},
  },
  picker::{Picker, PickerDelegate, PickerEvent},
  util::v_flex,
};

actions!(wifi, [Refresh]);

const CONTEXT: &str = "wifi";

pub struct WifiEntry {
  access_point: AccessPoint,
  pub is_connected: bool,
  pub is_known: bool,
  pub connection_path: Option<OwnedObjectPath>,
  _property_listeners: Vec<Task<()>>,
}

impl WifiEntry {
  pub fn new(
    access_point: AccessPoint,
    is_connected: bool,
    is_known: bool,
    connection_path: Option<OwnedObjectPath>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let mut entry = Self {
      access_point: access_point.clone(),
      is_connected,
      is_known,
      connection_path,
      _property_listeners: Vec::new(),
    };

    entry.spawn_property_listeners(&access_point, window, cx);
    entry
  }

  pub fn access_point(&self) -> &AccessPoint {
    &self.access_point
  }

  fn spawn_property_listeners(
    &mut self,
    access_point: &AccessPoint,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let strength_listener = cx.spawn_in(window, {
      let access_point = access_point.clone();
      async move |this, cx| {
        let strength_stream = cx
          .background_spawn({
            let access_point = access_point.clone();
            async move { access_point.listen_strength_changed().await }
          })
          .await;

        let Ok(strength_stream) = strength_stream else {
          return;
        };

        futures::pin_mut!(strength_stream);

        while let Some(new_strength) = strength_stream.next().await {
          let _ = this.update(cx, |this, cx| {
            this.access_point.strength = new_strength;
            cx.notify();
          });
        }
      }
    });

    self._property_listeners.push(strength_listener);
  }
}

pub struct WifiPanel {
  picker: Entity<Picker<WifiDelegate>>,
  network_manager: Option<NetworkManager>,
  device: Option<WirelessDevice>,
  entries: Vec<Entity<WifiEntry>>,
  is_scanning: bool,
  _scan_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new("ctrl-r", Refresh, Some(CONTEXT))]);

    let delegate = WifiDelegate {};
    let picker = cx.new(|cx| Picker::new(delegate, Arc::new(vec![]), window, cx));

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(wifi_entry) = ev {
          let entry = wifi_entry.read(cx);
          let access_point = entry.access_point().clone();
          let is_connected = entry.is_connected;
          let is_known = entry.is_known;
          let connection_path = entry.connection_path.clone();
          this.handle_entry_picked(
            access_point,
            is_connected,
            is_known,
            connection_path,
            window,
            cx,
          );
        }
      }),
      cx.observe_global_in::<GlobalDbusConnection>(window, |this, window, cx| {
        if this.network_manager.is_none()
          && let Some(conn) = GlobalDbusConnection::system(cx)
        {
          this.initialize(window, cx, &conn);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let mut panel = Self {
      picker,
      network_manager: None,
      device: None,
      entries: Vec::new(),
      is_scanning: false,
      _scan_task: None,
      _subscriptions: subscriptions,
    };

    if let Some(conn) = GlobalDbusConnection::system(cx) {
      panel.initialize(window, cx, &conn);
    }

    panel
  }

  fn initialize(&mut self, window: &mut Window, cx: &mut Context<Self>, conn: &zbus::Connection) {
    let conn = conn.clone();
    let picker = self.picker.clone();

    cx.spawn_in(window, async move |this, cx| {
      let nm = cx
        .background_spawn({
          let conn = conn.clone();
          async move { NetworkManager::new(&conn).await }
        })
        .await;

      let nm = match nm {
        Ok(nm) => nm,
        Err(_) => return Some(()),
      };

      let devices = cx
        .background_spawn({
          let nm = nm.clone();
          async move { nm.get_wireless_devices().await }
        })
        .await
        .unwrap_or_default();

      let device = match devices.into_iter().next() {
        Some(device) => device,
        None => {
          tracing::error!("No WiFi adapter found");
          return Some(());
        }
      };

      let known_connections = cx
        .background_spawn({
          let nm = nm.clone();
          async move { nm.get_known_wifi_connections().await }
        })
        .await
        .unwrap_or_default();

      let initial_access_points = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_access_points().await }
        })
        .await
        .unwrap_or_default();

      let active_ap = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_active_access_point().await }
        })
        .await
        .ok()
        .flatten();

      let active_hw_address = active_ap.as_ref().map(|ap| ap.hw_address.clone());

      this
        .update_in(cx, |this, window, cx| {
          this.network_manager = Some(nm.clone());
          this.device = Some(device.clone());

          let entries: Vec<Entity<WifiEntry>> = initial_access_points
            .into_iter()
            .filter(|ap| !ap.ssid.is_empty())
            .map(|ap| {
              let is_connected = active_hw_address
                .as_ref()
                .is_some_and(|addr| addr == &ap.hw_address);
              let known_conn = known_connections
                .iter()
                .find(|(ssid, _)| ssid == &ap.ssid)
                .map(|(_, path)| path.clone());
              let is_known = known_conn.is_some();

              cx.new(|cx| WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx))
            })
            .collect();

          this.entries = entries;

          picker.update(cx, |picker, cx| {
            picker.set_items(this.entries.clone(), window, cx);
          });

          cx.notify();
        })
        .ok()?;

      this
        .update_in(cx, |this, _window, cx| {
          this.is_scanning = true;
          cx.notify();
        })
        .ok()?;

      let _ = cx
        .background_spawn({
          let device = device.clone();
          async move { device.scan().await }
        })
        .await;

      let access_points = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_access_points().await }
        })
        .await
        .unwrap_or_default();

      let active_ap = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_active_access_point().await }
        })
        .await
        .ok()
        .flatten();

      let active_hw_address = active_ap.as_ref().map(|ap| ap.hw_address.clone());

      this
        .update_in(cx, |this, window, cx| {
          this.is_scanning = false;

          let entries: Vec<Entity<WifiEntry>> = access_points
            .into_iter()
            .filter(|ap| !ap.ssid.is_empty())
            .map(|ap| {
              let is_connected = active_hw_address
                .as_ref()
                .is_some_and(|addr| addr == &ap.hw_address);
              let known_conn = known_connections
                .iter()
                .find(|(ssid, _)| ssid == &ap.ssid)
                .map(|(_, path)| path.clone());
              let is_known = known_conn.is_some();

              cx.new(|cx| WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx))
            })
            .collect();

          this.entries = entries;

          picker.update(cx, |picker, cx| {
            picker.set_items(this.entries.clone(), window, cx);
          });

          cx.notify();
        })
        .ok()?;

      Some(())
    })
    .detach();
  }

  fn refresh(&mut self, _: &Refresh, window: &mut Window, cx: &mut Context<Self>) {
    let Some(device) = self.device.clone() else {
      return;
    };
    let Some(nm) = self.network_manager.clone() else {
      return;
    };

    let picker = self.picker.clone();
    self.is_scanning = true;
    cx.notify();

    cx.spawn_in(window, async move |this, cx| {
      let _ = cx
        .background_spawn({
          let device = device.clone();
          async move { device.scan().await }
        })
        .await;

      let known_connections = cx
        .background_spawn({
          let nm = nm.clone();
          async move { nm.get_known_wifi_connections().await }
        })
        .await
        .unwrap_or_default();

      let access_points = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_access_points().await }
        })
        .await
        .unwrap_or_default();

      let active_ap = cx
        .background_spawn({
          let device = device.clone();
          async move { device.get_active_access_point().await }
        })
        .await
        .ok()
        .flatten();

      let active_hw_address = active_ap.as_ref().map(|ap| ap.hw_address.clone());

      let _ = this.update_in(cx, |this, window, cx| {
        this.is_scanning = false;

        let entries: Vec<Entity<WifiEntry>> = access_points
          .into_iter()
          .filter(|ap| !ap.ssid.is_empty())
          .map(|ap| {
            let is_connected = active_hw_address
              .as_ref()
              .is_some_and(|addr| addr == &ap.hw_address);
            let known_conn = known_connections
              .iter()
              .find(|(ssid, _)| ssid == &ap.ssid)
              .map(|(_, path)| path.clone());
            let is_known = known_conn.is_some();

            cx.new(|cx| WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx))
          })
          .collect();

        this.entries = entries;

        picker.update(cx, |picker, cx| {
          picker.set_items(this.entries.clone(), window, cx);
        });

        cx.notify();
      });
    })
    .detach();
  }

  fn handle_entry_picked(
    &mut self,
    access_point: AccessPoint,
    is_connected: bool,
    is_known: bool,
    connection_path: Option<OwnedObjectPath>,
    _window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(nm) = self.network_manager.clone() else {
      return;
    };
    let Some(device) = self.device.clone() else {
      return;
    };

    if is_connected {
      cx.background_spawn(async move {
        let active_conn = device.get_active_connection_path().await?;
        if let Some(active_conn) = active_conn {
          nm.deactivate_connection(&active_conn).await?;
        }
        Ok::<(), anyhow::Error>(())
      })
      .detach_and_log_err(cx);
    } else if is_known {
      if let Some(conn_path) = connection_path {
        let ap_path = access_point.path.clone();
        let device_path = device.device_path().clone();
        cx.background_spawn(async move {
          nm.activate_connection(&conn_path, &device_path, &ap_path)
            .await?;
          Ok::<(), anyhow::Error>(())
        })
        .detach_and_log_err(cx);
      }
    } else if !access_point.security.is_secured() {
      let ap_path = access_point.path.clone();
      let device_path = device.device_path().clone();
      cx.background_spawn(async move {
        nm.add_and_activate_connection(&device_path, &ap_path)
          .await?;
        Ok::<(), anyhow::Error>(())
      })
      .detach_and_log_err(cx);
    }
  }
}

impl Focusable for WifiPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for WifiPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .on_action(cx.listener(Self::refresh))
      .size_full()
      .when(self.is_scanning, |this| {
        this.child(div().child("Scanning..."))
      })
      .child(self.picker.clone())
  }
}

struct WifiDelegate {}

impl PickerDelegate for WifiDelegate {
  type ListItem = Entity<WifiEntry>;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let entry = item.read(cx);
    let ap = entry.access_point();

    let mut status_text = String::new();
    if entry.is_connected {
      status_text.push_str("CONNECTED ");
    } else if entry.is_known {
      status_text.push_str("KNOWN ");
    }
    status_text.push_str(&ap.ssid);

    let info_text = format!("Signal: {} - {}", ap.strength, ap.security);

    v_flex()
      .w_full()
      .when(is_selected, |this| this.bg(rgb(0x444444)))
      .child(
        div()
          .w_full()
          .text_ellipsis()
          .overflow_x_hidden()
          .child(status_text),
      )
      .child(div().child(info_text))
  }

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    query: String,
    _cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    if query.is_empty() {
      cx.defer_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, None);
      });

      return Task::ready(());
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = Vec::new();
    let mut buf = Vec::new();

    for (index, item) in items.iter().enumerate() {
      let ap = item.read(cx).access_point();

      if let Some(score) = needle.score(Utf32Str::new(&ap.ssid, &mut buf), &mut matcher) {
        matches.push((index, score));
      }
    }

    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, Some(matches));
    });

    Task::ready(())
  }

  fn sort_items(&self, cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by(|(idx_a, score_a), (idx_b, score_b)| {
      let entry_a = items[*idx_a].read(cx);
      let entry_b = items[*idx_b].read(cx);

      let a_data = (
        entry_a.is_connected,
        entry_a.is_known,
        entry_a.access_point.strength,
      );
      let b_data = (
        entry_b.is_connected,
        entry_b.is_known,
        entry_b.access_point.strength,
      );

      match (a_data.0, b_data.0) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => match (a_data.1, b_data.1) {
          (true, false) => std::cmp::Ordering::Less,
          (false, true) => std::cmp::Ordering::Greater,
          _ => {
            if score_a == score_b {
              b_data.2.cmp(&a_data.2)
            } else {
              score_b.cmp(score_a)
            }
          }
        },
      }
    });
  }
}
