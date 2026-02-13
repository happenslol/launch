use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::AtomicBool};

use futures::StreamExt;
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, SharedString, Styled,
  Subscription, Task, Window, actions, div, prelude::*, px, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use zvariant::OwnedObjectPath;

use crate::{
  dbus::{
    GlobalDbusConnection,
    networkmanager::{AccessPoint, NetworkManager, WirelessDevice},
  },
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, v_flex},
};

actions!(wifi, [Refresh]);

const CONTEXT: &str = "wifi";

#[derive(Clone)]
pub struct WifiEntry {
  id: String,
  search_string: String,
  is_known: bool,
  entry: Entity<WifiEntryInner>,
  alternate_paths: HashSet<String>,
}

pub struct WifiEntryInner {
  access_point: AccessPoint,
  pub is_connected: bool,
  pub is_known: bool,
  pub connection_path: Option<OwnedObjectPath>,
  _listeners: Vec<Task<()>>,
}

impl WifiEntryInner {
  pub fn new(
    access_point: AccessPoint,
    is_connected: bool,
    is_known: bool,
    connection_path: Option<OwnedObjectPath>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let listeners = vec![cx.spawn_in(window, {
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
    })];

    WifiEntryInner {
      access_point,
      is_connected,
      is_known,
      connection_path,
      _listeners: listeners,
    }
  }
}

impl WifiEntry {
  pub fn new(
    access_point: AccessPoint,
    is_connected: bool,
    is_known: bool,
    connection_path: Option<OwnedObjectPath>,
    window: &mut Window,
    cx: &mut App,
  ) -> Self {
    let search_string = access_point.ssid.to_string();

    // Use the path as the ID, this is guaranteed to be unique.
    let id = access_point.path.to_string();

    let entity = cx.new(|cx| {
      WifiEntryInner::new(
        access_point,
        is_connected,
        is_known,
        connection_path,
        window,
        cx,
      )
    });

    Self {
      id,
      search_string,
      is_known,
      entry: entity,
      alternate_paths: HashSet::new(),
    }
  }
}

/// Deduplicates access points by SSID, keeping the one with the highest signal strength.
/// Returns the winning access points along with the set of alternate object paths for each.
fn deduplicate_access_points(
  access_points: Vec<AccessPoint>,
) -> Vec<(AccessPoint, HashSet<String>)> {
  let mut best_by_ssid: HashMap<SharedString, (AccessPoint, HashSet<String>)> = HashMap::new();

  for ap in access_points {
    if ap.ssid.is_empty() {
      continue;
    }

    match best_by_ssid.entry(ap.ssid.clone()) {
      std::collections::hash_map::Entry::Occupied(mut entry) => {
        let (existing, alternates): &mut (AccessPoint, HashSet<String>) = entry.get_mut();
        if ap.strength > existing.strength {
          alternates.insert(existing.path.to_string());
          *existing = ap;
        } else {
          alternates.insert(ap.path.to_string());
        }
      }
      std::collections::hash_map::Entry::Vacant(entry) => {
        entry.insert((ap, HashSet::new()));
      }
    }
  }

  best_by_ssid.into_values().collect()
}

pub struct WifiPanel {
  picker: Entity<Picker<WifiDelegate>>,
  network_manager: Option<NetworkManager>,
  device: Option<WirelessDevice>,
  entries: Vec<WifiEntry>,
  is_scanning: bool,
  _scan_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new("ctrl-r", Refresh, Some(CONTEXT))]);

    let delegate = WifiDelegate {};
    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search wifi networks...", cx);
      picker
    });

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(wifi_entry) = ev {
          let entry = wifi_entry.entry.read(cx);
          let access_point = entry.access_point.clone();
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

          let deduplicated = deduplicate_access_points(initial_access_points);

          let entries: Vec<WifiEntry> = deduplicated
            .into_iter()
            .map(|(ap, alternate_paths)| {
              let is_connected = active_hw_address
                .as_ref()
                .is_some_and(|addr| addr == &ap.hw_address);
              let known_conn = known_connections
                .iter()
                .find(|(ssid, _)| ssid == &ap.ssid)
                .map(|(_, path)| path.clone());
              let is_known = known_conn.is_some();

              let mut entry =
                WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
              entry.alternate_paths = alternate_paths;
              entry
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

          let deduplicated = deduplicate_access_points(access_points);

          let entries: Vec<WifiEntry> = deduplicated
            .into_iter()
            .map(|(ap, alternate_paths)| {
              let is_connected = active_hw_address
                .as_ref()
                .is_some_and(|addr| addr == &ap.hw_address);
              let known_conn = known_connections
                .iter()
                .find(|(ssid, _)| ssid == &ap.ssid)
                .map(|(_, path)| path.clone());
              let is_known = known_conn.is_some();

              let mut entry =
                WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
              entry.alternate_paths = alternate_paths;
              entry
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

        let deduplicated = deduplicate_access_points(access_points);

        // Collect all paths that are represented (primary + alternates) in the new scan.
        let all_new_paths: HashSet<String> = deduplicated
          .iter()
          .flat_map(|(ap, alternates)| {
            std::iter::once(ap.path.to_string()).chain(alternates.iter().cloned())
          })
          .collect();

        // Retain entries whose primary or alternate paths still appear in the new scan.
        this.entries.retain(|entry| {
          all_new_paths.contains(&entry.id)
            || entry
              .alternate_paths
              .iter()
              .any(|path| all_new_paths.contains(path))
        });

        for (ap, alternate_paths) in deduplicated {
          let already_exists = this.entries.iter().any(|entry| {
            entry.id == ap.path.as_str()
              || entry.alternate_paths.contains(ap.path.as_str())
          });

          if already_exists {
            continue;
          }

          let is_connected = active_hw_address
            .as_ref()
            .is_some_and(|addr| addr == &ap.hw_address);
          let known_conn = known_connections
            .iter()
            .find(|(ssid, _)| ssid == &ap.ssid)
            .map(|(_, path)| path.clone());
          let is_known = known_conn.is_some();

          let mut entry =
            WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
          entry.alternate_paths = alternate_paths;
          this.entries.push(entry);
        }

        // TODO: Can we somehow not clone the items here?
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
      .child(picker_input(&self.picker).show_back_button(true))
      .child(picker_results(&self.picker))
  }
}

struct WifiDelegate {}

impl PickerDelegate for WifiDelegate {
  type ListItem = WifiEntry;

  fn sort_items(&self, _cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by_key(|(index, score)| (std::cmp::Reverse(items[*index].is_known), std::cmp::Reverse(*score)));
  }

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let entry = &item.entry.read(cx);
    let ap = &entry.access_point;

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
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(
        div()
          .w_full()
          .text_ellipsis()
          .overflow_x_hidden()
          .child(status_text),
      )
      .child(
        div()
          .text_sm()
          .text_color(rgb(0x888888))
          .child(info_text),
      )
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

    let matchers = MatcherPool::global(cx);
    cx.spawn_in(window, async move |cx, window| {
      let mut matcher = matchers.get().await.unwrap();
      let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
      let mut matches = Vec::new();
      let mut buf = Vec::new();

      for (index, item) in items.iter().enumerate() {
        if let Some(score) =
          needle.score(Utf32Str::new(&item.search_string, &mut buf), &mut matcher)
        {
          matches.push((index, score));
        }
      }

      cx.update_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, Some(matches));
      })
      .log_err();
    })
  }
}
