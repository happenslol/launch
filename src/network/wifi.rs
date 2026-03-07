use std::collections::{HashMap, HashSet};
use std::sync::{Arc, atomic::AtomicBool};

use futures::StreamExt;
use std::time::Duration;

use gpui::{
  Animation, AnimationExt, App, Context, ElementId, Entity, FocusHandle, Focusable,
  InteractiveElement, IntoElement, KeyBinding, ParentElement, SharedString, Styled, Subscription,
  Task, Window, actions, div, hsla, prelude::*, px, relative, rems, rgb, rgba,
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
  icon::{Icon, IconName},
  input::{
    input,
    state::{InputEvent, InputState},
  },
  matcher::MatcherPool,
  picker::{Category, Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  submenu::{SubMenu, SubMenuEvent},
  util::{ResultExt, v_flex},
};

actions!(wifi, [Refresh, DismissPasswordPopup, ForgetNetwork]);

const CONTEXT: &str = "wifi";
const PASSWORD_POPUP_CONTEXT: &str = "wifi_password_popup";
const PASSWORD_ANIM_ENTER: Duration = Duration::from_millis(150);
const PASSWORD_ANIM_EXIT: Duration = Duration::from_millis(100);

struct PasswordPopup {
  input: Entity<InputState>,
  focus_handle: FocusHandle,
  closing: bool,
  _dismiss_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

enum PasswordPopupEvent {
  Closing,
  Dismiss,
  Submit(SharedString),
}

impl PasswordPopup {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new(
      "escape",
      DismissPasswordPopup,
      Some(PASSWORD_POPUP_CONTEXT),
    )]);

    let input = cx.new(|cx| {
      InputState::new(window, cx)
        .masked(true)
        .placeholder("Password")
    });

    let subscriptions =
      vec![
        cx.subscribe_in(&input, window, |this, _input, event, _window, cx| {
          if let InputEvent::PressEnter { .. } = *event {
            let password = this.input.read(cx).value();
            cx.emit(PasswordPopupEvent::Submit(password));
          }
        }),
      ];

    let focus_handle = cx.focus_handle();

    Self {
      input,
      focus_handle,
      closing: false,
      _dismiss_task: None,
      _subscriptions: subscriptions,
    }
  }

  fn dismiss(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }

    self.closing = true;
    cx.emit(PasswordPopupEvent::Closing);
    cx.notify();

    self._dismiss_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(PASSWORD_ANIM_EXIT).await;

      this
        .update(cx, |_this, cx| {
          cx.emit(PasswordPopupEvent::Dismiss);
        })
        .log_err();
    }));
  }
}

impl Focusable for PasswordPopup {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for PasswordPopup {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    div()
      .track_focus(&self.focus_handle)
      .key_context(PASSWORD_POPUP_CONTEXT)
      .on_action(cx.listener(|this, _: &DismissPasswordPopup, _, cx| {
        this.dismiss(cx);
      }))
      .on_mouse_down_out(cx.listener(|this, _, _, cx| {
        this.dismiss(cx);
      }))
      .w(px(300.))
      .p_2()
      .bg(rgba(0x171717F0))
      .border_1()
      .border_color(rgba(0xFFFFFF15))
      .rounded_md()
      .shadow_lg()
      .child(input(&self.input).w_full())
  }
}

impl gpui::EventEmitter<PasswordPopupEvent> for PasswordPopup {}

#[derive(Clone)]
struct WifiAction {
  id: &'static str,
  label: SharedString,
  icon: IconName,
  search_string: String,
}

struct WifiActionDelegate;

impl PickerDelegate for WifiActionDelegate {
  type ListItem = WifiAction;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    div()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .flex()
      .flex_row()
      .items_center()
      .gap_2()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(
        Icon::new(item.icon)
          .size(rems(1.0))
          .text_color(rgb(0x888888)),
      )
      .child(item.label.clone())
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
      let mut matcher = matchers.get().await.log_err();
      let Some(ref mut matcher) = matcher else {
        return;
      };
      let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
      let mut matches = Vec::new();
      let mut buf = Vec::new();

      for (index, item) in items.iter().enumerate() {
        if let Some(score) = needle.score(Utf32Str::new(&item.search_string, &mut buf), matcher) {
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

#[derive(Clone)]
pub struct WifiEntry {
  id: String,
  search_string: String,
  is_known: bool,
  entry: Entity<WifiEntryInner>,
  alternate_paths: HashSet<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnectionState {
  Idle,
  Connecting,
  Failed,
}

pub struct WifiEntryInner {
  access_point: AccessPoint,
  pub is_connected: bool,
  pub is_known: bool,
  pub connection_path: Option<OwnedObjectPath>,
  connection_state: ConnectionState,
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
      connection_state: ConnectionState::Idle,
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
  password_popup: Option<(Entity<PasswordPopup>, Entity<WifiEntryInner>)>,
  action_submenu: Option<(Entity<SubMenu<WifiActionDelegate>>, WifiEntry)>,
  _scan_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl WifiPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-r", Refresh, Some(CONTEXT)),
      KeyBinding::new("ctrl-d", ForgetNetwork, Some(CONTEXT)),
    ]);

    let delegate = WifiDelegate {};
    let picker = cx.new(|cx| {
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search wifi networks...", cx);
      picker
    });

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| match ev {
        PickerEvent::Picked(wifi_entry) => {
          let (access_point, is_connected, is_known, connection_path) = {
            let entry = wifi_entry.entry.read(cx);
            (
              entry.access_point.clone(),
              entry.is_connected,
              entry.is_known,
              entry.connection_path.clone(),
            )
          };

          if is_connected {
            this.disconnect(&wifi_entry.entry, window, cx);
          } else if is_known {
            this.connect_known(&wifi_entry.entry, access_point, connection_path, cx);
          } else if access_point.security.is_secured() {
            this.show_password_popup(&wifi_entry.entry, &access_point, window, cx);
          } else {
            this.connect_open(&wifi_entry.entry, access_point, window, cx);
          }
        }
        PickerEvent::SecondaryPicked(wifi_entry) => {
          this.open_action_submenu(wifi_entry.clone(), window, cx);
        }
        _ => {}
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
      password_popup: None,
      action_submenu: None,
      _scan_task: None,
      _subscriptions: subscriptions,
    };

    if let Some(conn) = GlobalDbusConnection::system(cx) {
      panel.initialize(window, cx, &conn);
    }

    panel
  }

  fn initialize(&mut self, window: &mut Window, cx: &mut Context<Self>, conn: &zbus::Connection) {
    tracing::info!("Initializing wifi panel");
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
        Err(error) => {
          tracing::error!(%error, "Failed to connect to NetworkManager");
          return Some(());
        }
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

              let mut entry = WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
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

      tracing::info!("Starting wifi scan");

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

      tracing::info!(count = access_points.len(), "Wifi scan complete");

      this
        .update_in(cx, |this, window, cx| {
          this.is_scanning = false;

          let deduplicated = deduplicate_access_points(access_points);

          let all_new_paths: HashSet<String> = deduplicated
            .iter()
            .flat_map(|(ap, alternates)| {
              std::iter::once(ap.path.to_string()).chain(alternates.iter().cloned())
            })
            .collect();

          this.entries.retain(|entry| {
            all_new_paths.contains(&entry.id)
              || entry
                .alternate_paths
                .iter()
                .any(|path| all_new_paths.contains(path))
          });

          for (ap, alternate_paths) in deduplicated {
            let existing = this.entries.iter_mut().find(|entry| {
              entry.id == ap.path.as_str() || entry.alternate_paths.contains(ap.path.as_str())
            });

            if let Some(existing) = existing {
              existing.entry.update(cx, |inner, cx| {
                inner.access_point = ap;
                cx.notify();
              });
              existing.alternate_paths = alternate_paths;
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

            let mut entry = WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
            entry.alternate_paths = alternate_paths;
            this.entries.push(entry);
          }

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
          let existing = this.entries.iter_mut().find(|entry| {
            entry.id == ap.path.as_str() || entry.alternate_paths.contains(ap.path.as_str())
          });

          if let Some(existing) = existing {
            existing.entry.update(cx, |inner, cx| {
              inner.access_point = ap;
              cx.notify();
            });
            existing.alternate_paths = alternate_paths;
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

          let mut entry = WifiEntry::new(ap, is_connected, is_known, known_conn, window, cx);
          entry.alternate_paths = alternate_paths;
          this.entries.push(entry);
        }

        picker.update(cx, |picker, cx| {
          picker.set_items(this.entries.clone(), window, cx);
        });

        cx.notify();
      });
    })
    .detach();
  }

  fn show_password_popup(
    &mut self,
    entry: &Entity<WifiEntryInner>,
    access_point: &AccessPoint,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let popup = cx.new(|cx| PasswordPopup::new(window, cx));

    let search_input = self.picker.read(cx).search_input.clone();
    let entry_handle = entry.clone();
    let access_point = access_point.clone();

    cx.subscribe_in(
      &popup,
      window,
      move |this, popup, event: &PasswordPopupEvent, window, cx| match event {
        PasswordPopupEvent::Closing => {
          cx.focus_view(&search_input, window);
          cx.notify();
        }
        PasswordPopupEvent::Dismiss => {
          tracing::debug!(ssid = %access_point.ssid, "Password popup dismissed");
          this.password_popup = None;
          cx.notify();
        }
        PasswordPopupEvent::Submit(password) => {
          this.connect_with_password(&entry_handle, &access_point, password, window, cx);
          popup.update(cx, |popup, cx| popup.dismiss(cx));
        }
      },
    )
    .detach();

    let input_focus = popup.read(cx).input.read(cx).focus_handle.clone();

    self.password_popup = Some((popup, entry.clone()));
    cx.notify();

    window.focus(&input_focus, cx);
  }

  fn open_action_submenu(
    &mut self,
    wifi_entry: WifiEntry,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let (is_connected, is_known, ssid) = {
      let entry_inner = wifi_entry.entry.read(cx);
      (
        entry_inner.is_connected,
        entry_inner.is_known,
        entry_inner.access_point.ssid.clone(),
      )
    };

    let mut actions = Vec::new();

    actions.push(WifiAction {
      id: "connect",
      label: if is_connected {
        "Disconnect".into()
      } else {
        "Connect".into()
      },
      icon: if is_connected {
        IconName::PlugOff
      } else {
        IconName::PlugConnected
      },
      search_string: if is_connected {
        "disconnect".into()
      } else {
        "connect".into()
      },
    });

    if is_known {
      actions.push(WifiAction {
        id: "forget",
        label: "Forget Network".into(),
        icon: IconName::Trash,
        search_string: "forget network".into(),
      });
    }

    let picker = cx.new(|cx| Picker::new(WifiActionDelegate, Arc::new(actions), window, cx));

    let submenu = cx.new(|cx| {
      SubMenu::new(picker, window, cx)
        .height(px(154.))
        .header(move |_window, _cx| {
          div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .px_3()
            .py_2()
            .overflow_x_hidden()
            .bg(rgb(0x1D1D1D))
            .border_1()
            .border_color(rgba(0xFFFFFF15))
            .rounded_lg()
            .child(
              Icon::new(IconName::Wifi)
                .size(rems(1.0))
                .text_color(rgb(0x888888)),
            )
            .child(
              div()
                .flex_1()
                .text_ellipsis()
                .overflow_x_hidden()
                .child(ssid.clone()),
            )
            .into_any_element()
        })
    });

    let wifi_entry_clone = wifi_entry.clone();
    self._subscriptions.push(cx.subscribe_in(
      &submenu,
      window,
      move |this, _submenu, ev: &PickerEvent<WifiActionDelegate>, window, cx| {
        if let PickerEvent::Picked(action) = ev {
          match action.id {
            "connect" => {
              let (access_point, is_connected, is_known, connection_path) = {
                let entry = wifi_entry_clone.entry.read(cx);
                (
                  entry.access_point.clone(),
                  entry.is_connected,
                  entry.is_known,
                  entry.connection_path.clone(),
                )
              };

              if is_connected {
                this.disconnect(&wifi_entry_clone.entry, window, cx);
              } else if is_known {
                this.connect_known(&wifi_entry_clone.entry, access_point, connection_path, cx);
              } else if access_point.security.is_secured() {
                this.show_password_popup(&wifi_entry_clone.entry, &access_point, window, cx);
              } else {
                this.connect_open(&wifi_entry_clone.entry, access_point, window, cx);
              }
            }
            "forget" => {
              this.forget_selected_entry(&wifi_entry_clone, window, cx);
            }
            _ => {}
          }
        }
      },
    ));

    self._subscriptions.push(cx.subscribe_in(
      &submenu,
      window,
      |this, _submenu, _ev: &SubMenuEvent, window, cx| {
        this.action_submenu = None;
        cx.focus_view(&this.picker.read(cx).search_input.clone(), window);
        cx.notify();
      },
    ));

    self.action_submenu = Some((submenu, wifi_entry));
    cx.notify();
  }

  fn disconnect(
    &mut self,
    entry: &Entity<WifiEntryInner>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(nm) = self.network_manager.clone() else {
      return;
    };
    let Some(device) = self.device.clone() else {
      return;
    };

    let ssid = entry.read(cx).access_point.ssid.clone();
    tracing::info!(%ssid, "Disconnecting from wifi network");

    cx.spawn_in(window, {
      let entry = entry.clone();
      async move |_this, cx| {
        let result = cx
          .background_spawn(async move {
            let active_conn = device.get_active_connection_path().await?;
            if let Some(active_conn) = active_conn {
              nm.deactivate_connection(&active_conn).await?;
            }
            Ok::<(), anyhow::Error>(())
          })
          .await;

        match result {
          Ok(()) => {
            tracing::info!(%ssid, "Disconnected from wifi network");
            entry.update(cx, |entry, cx| {
              entry.is_connected = false;
              cx.notify();
            });
          }
          Err(error) => {
            tracing::error!(%ssid, %error, "Failed to disconnect from wifi network");
            entry.update(cx, |entry, cx| {
              entry.connection_state = ConnectionState::Failed;
              cx.notify();
            });
          }
        }
      }
    })
    .detach();
  }

  fn forget_network(&mut self, _: &ForgetNetwork, window: &mut Window, cx: &mut Context<Self>) {
    let Some(selected) = self.picker.read(cx).get_selected_item().cloned() else {
      return;
    };

    self.forget_selected_entry(&selected, window, cx);
  }

  fn forget_selected_entry(
    &mut self,
    selected: &WifiEntry,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if !selected.is_known {
      return;
    }

    let connection_path = {
      let entry = selected.entry.read(cx);
      let Some(path) = entry.connection_path.clone() else {
        return;
      };
      path
    };

    let Some(nm) = self.network_manager.clone() else {
      return;
    };

    let ssid = selected.entry.read(cx).access_point.ssid.clone();
    tracing::info!(%ssid, "Forgetting wifi network");

    let entry = selected.entry.clone();
    let selected_id = selected.id.clone();
    let picker = self.picker.clone();

    cx.spawn_in(window, {
      async move |this, cx| {
        let result = cx
          .background_spawn({
            let nm = nm.clone();
            let connection_path = connection_path.clone();
            async move { nm.delete_connection(&connection_path).await }
          })
          .await;

        match result {
          Ok(()) => {
            tracing::info!(%ssid, "Forgot wifi network");
            let _ = this.update_in(cx, |this, window, cx| {
              entry.update(cx, |inner, cx| {
                inner.is_known = false;
                inner.connection_path = None;
                inner.is_connected = false;
                cx.notify();
              });
              for wifi_entry in &mut this.entries {
                if wifi_entry.id == selected_id {
                  wifi_entry.is_known = false;
                }
              }
              picker.update(cx, |picker, cx| {
                picker.set_items(this.entries.clone(), window, cx);
              });
            });
          }
          Err(error) => {
            tracing::error!(%ssid, %error, "Failed to forget wifi network");
          }
        }
      }
    })
    .detach();
  }

  fn connect_known(
    &mut self,
    entry: &Entity<WifiEntryInner>,
    access_point: AccessPoint,
    connection_path: Option<OwnedObjectPath>,
    cx: &mut Context<Self>,
  ) {
    let Some(nm) = self.network_manager.clone() else {
      return;
    };
    let Some(device) = self.device.clone() else {
      return;
    };
    let Some(conn_path) = connection_path else {
      return;
    };

    tracing::info!(ssid = %access_point.ssid, "Connecting to known wifi network");

    entry.update(cx, |entry, cx| {
      entry.connection_state = ConnectionState::Connecting;
      cx.notify();
    });

    cx.spawn({
      let entry = entry.clone();
      let ssid = access_point.ssid.clone();
      async move |_this, cx| {
        let executor = cx.background_executor().clone();
        let ap_path = access_point.path.clone();
        let device_path = device.device_path().clone();
        let result = cx
          .background_spawn(async move {
            let active_path = nm
              .activate_connection(&conn_path, &device_path, &ap_path)
              .await?;
            nm.wait_for_connection_active(&active_path, &executor)
              .await?;
            Ok::<(), anyhow::Error>(())
          })
          .await;

        match result {
          Ok(()) => {
            tracing::info!(%ssid, "Connected to known wifi network");
            entry.update(cx, |entry, cx| {
              entry.connection_state = ConnectionState::Idle;
              entry.is_connected = true;
              cx.notify();
            });
          }
          Err(error) => {
            tracing::error!(%ssid, %error, "Failed to connect to known wifi network");
            entry.update(cx, |entry, cx| {
              entry.connection_state = ConnectionState::Failed;
              cx.notify();
            });
          }
        }
      }
    })
    .detach();
  }

  fn connect_open(
    &mut self,
    entry: &Entity<WifiEntryInner>,
    access_point: AccessPoint,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(nm) = self.network_manager.clone() else {
      return;
    };
    let Some(device) = self.device.clone() else {
      return;
    };

    tracing::info!(ssid = %access_point.ssid, "Connecting to open wifi network");

    entry.update(cx, |entry, cx| {
      entry.connection_state = ConnectionState::Connecting;
      cx.notify();
    });

    let picker = self.picker.clone();

    cx.spawn_in(window, {
      let entry = entry.clone();
      let ssid = access_point.ssid.clone();
      async move |this, cx| {
        let executor = cx.background_executor().clone();
        let ap_path = access_point.path.clone();
        let device_path = device.device_path().clone();
        let result = cx
          .background_spawn(async move {
            let (_connection_path, active_path) = nm
              .add_and_activate_connection(&device_path, &ap_path)
              .await?;
            nm.wait_for_connection_active(&active_path, &executor)
              .await?;
            Ok::<(), anyhow::Error>(())
          })
          .await;

        match result {
          Ok(()) => {
            tracing::info!(%ssid, "Connected to open wifi network");
            let _ = this.update_in(cx, |this, window, cx| {
              entry.update(cx, |inner, cx| {
                inner.connection_state = ConnectionState::Idle;
                inner.is_connected = true;
                inner.is_known = true;
                cx.notify();
              });
              let entry_id = entry.entity_id();
              for wifi_entry in &mut this.entries {
                if wifi_entry.entry.entity_id() == entry_id {
                  wifi_entry.is_known = true;
                }
              }
              picker.update(cx, |picker, cx| {
                picker.set_items(this.entries.clone(), window, cx);
              });
            });
          }
          Err(error) => {
            tracing::error!(%ssid, %error, "Failed to connect to open wifi network");
            entry.update(cx, |entry, cx| {
              entry.connection_state = ConnectionState::Failed;
              cx.notify();
            });
          }
        }
      }
    })
    .detach();
  }

  fn connect_with_password(
    &mut self,
    entry: &Entity<WifiEntryInner>,
    access_point: &AccessPoint,
    password: &str,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(nm) = self.network_manager.clone() else {
      return;
    };
    let Some(device) = self.device.clone() else {
      return;
    };

    tracing::info!(ssid = %access_point.ssid, "Connecting to secured wifi network with password");

    entry.update(cx, |entry, cx| {
      entry.connection_state = ConnectionState::Connecting;
      cx.notify();
    });

    let ap_path = access_point.path.clone();
    let ssid = access_point.ssid.clone();
    let password = password.to_string();
    let picker = self.picker.clone();

    cx.spawn_in(window, {
      let entry = entry.clone();
      async move |this, cx| {
        let executor = cx.background_executor().clone();
        let device_path = device.device_path().clone();
        let result = cx
          .background_spawn(async move {
            let (connection_path, active_path) = nm
              .add_and_activate_connection_with_password(&device_path, &ap_path, &password)
              .await?;
            match nm.wait_for_connection_active(&active_path, &executor).await {
              Ok(()) => Ok(()),
              Err(error) => {
                nm.delete_connection(&connection_path).await.log_err();
                Err(error)
              }
            }
          })
          .await;

        match result {
          Ok(()) => {
            tracing::info!(%ssid, "Connected to secured wifi network");
            let _ = this.update_in(cx, |this, window, cx| {
              entry.update(cx, |inner, cx| {
                inner.connection_state = ConnectionState::Idle;
                inner.is_connected = true;
                inner.is_known = true;
                cx.notify();
              });
              let entry_id = entry.entity_id();
              for wifi_entry in &mut this.entries {
                if wifi_entry.entry.entity_id() == entry_id {
                  wifi_entry.is_known = true;
                }
              }
              picker.update(cx, |picker, cx| {
                picker.set_items(this.entries.clone(), window, cx);
              });
            });
          }
          Err(error) => {
            tracing::error!(%ssid, %error, "Failed to connect to secured wifi network");
            entry.update(cx, |entry, cx| {
              entry.connection_state = ConnectionState::Failed;
              cx.notify();
            });
          }
        }
      }
    })
    .detach();
  }
}

impl Focusable for WifiPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    if let Some((popup, _)) = &self.password_popup {
      if !popup.read(cx).closing {
        return popup.read(cx).focus_handle(cx);
      }
    }
    if let Some((submenu, _)) = &self.action_submenu {
      submenu.read(cx).focus_handle(cx)
    } else {
      self.picker.read(cx).focus_handle(cx)
    }
  }
}

impl Render for WifiPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let password_popup = self.password_popup.as_ref().map(|(popup, _)| popup.clone());
    let password_closing = self
      .password_popup
      .as_ref()
      .is_some_and(|(popup, _)| popup.read(cx).closing);
    let action_submenu = self
      .action_submenu
      .as_ref()
      .map(|(submenu, _)| submenu.clone());
    let dismiss_backdrop = cx.listener(|this, _: &gpui::ClickEvent, _window, cx| {
      if let Some((popup, _)) = &this.password_popup {
        popup.update(cx, |popup, cx| popup.dismiss(cx));
      }
    });

    v_flex()
      .key_context(CONTEXT)
      .on_action(cx.listener(Self::refresh))
      .on_action(cx.listener(Self::forget_network))
      .size_full()
      .relative()
      .child(
        picker_input(&self.picker)
          .show_back_button(true)
          .loading(self.is_scanning),
      )
      .child(picker_results(&self.picker))
      .when_some(password_popup, |this, popup| {
        let closing = password_closing;
        let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);

        this.child(
          div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
              div()
                .id("password-backdrop")
                .occlude()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .rounded_xl()
                .bg(rgba(0x00000088))
                .on_click(dismiss_backdrop)
                .with_animation(
                  ElementId::NamedInteger("password-backdrop-fade".into(), closing as u64),
                  Animation::new(if closing {
                    PASSWORD_ANIM_EXIT
                  } else {
                    PASSWORD_ANIM_ENTER
                  })
                  .with_easing(easing),
                  move |this, delta| {
                    let opacity = if closing { 1.0 - delta } else { delta };
                    this.opacity(opacity)
                  },
                ),
            )
            .child(
              div()
                .id("password-popup-content")
                .occlude()
                .child(popup)
                .with_animation(
                  ElementId::NamedInteger("password-popup-fade".into(), closing as u64),
                  Animation::new(if closing {
                    PASSWORD_ANIM_EXIT
                  } else {
                    PASSWORD_ANIM_ENTER
                  })
                  .with_easing(easing),
                  move |this, delta| {
                    let opacity = if closing { 1.0 - delta } else { delta };
                    this.opacity(opacity)
                  },
                ),
            ),
        )
      })
      .when_some(action_submenu, |this, submenu| this.child(submenu))
  }
}

struct WifiDelegate {}

impl PickerDelegate for WifiDelegate {
  type ListItem = WifiEntry;

  fn sort_items(&self, cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by_key(|(index, score)| {
      let entry = items[*index].entry.read(cx);
      (
        std::cmp::Reverse(*score),
        std::cmp::Reverse(entry.is_known),
        std::cmp::Reverse(entry.access_point.strength),
      )
    });
  }

  fn categories(&self) -> Option<Vec<Category<Self::ListItem>>> {
    Some(vec![
      Category::new("Known Networks", |entry: &WifiEntry| entry.is_known),
      Category::new("Available Networks", |entry: &WifiEntry| !entry.is_known),
    ])
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
    let connection_state = entry.connection_state;

    let status = match connection_state {
      ConnectionState::Connecting => Some("Connecting..."),
      ConnectionState::Failed => Some("Connection failed"),
      _ if entry.is_connected => Some("Connected"),
      _ if entry.is_known => Some("Saved"),
      _ => None,
    };

    let security = ap.security;
    let ssid: SharedString = ap.ssid.clone();
    let strength = ap.strength.min(100) as f32 / 100.0;

    let status_color = match connection_state {
      ConnectionState::Connecting => rgb(0xCCAA33),
      ConnectionState::Failed => rgb(0xCC4444),
      _ => rgb(0x888888),
    };

    // Hue from 0.0 (red) to 0.33 (green) based on signal strength.
    let signal_color = hsla(strength * 0.33, 0.8, 0.5, 1.0);

    v_flex()
      .relative()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(
        div()
          .flex()
          .flex_row()
          .gap_3()
          .items_center()
          .w_full()
          .child(
            v_flex()
              .w(px(5.))
              .h(px(24.))
              .flex_shrink_0()
              .rounded_sm()
              .bg(rgba(0xFFFFFF11))
              .justify_end()
              .child(
                div()
                  .w_full()
                  .h(relative(strength))
                  .rounded_sm()
                  .bg(signal_color),
              ),
          )
          .child(
            v_flex()
              .flex_grow()
              .overflow_x_hidden()
              .child(
                div()
                  .flex()
                  .flex_row()
                  .items_center()
                  .gap_2()
                  .w_full()
                  .child(div().text_ellipsis().overflow_x_hidden().child(ssid))
                  .when_some(status, |this, status| {
                    this.child(
                      div()
                        .text_sm()
                        .text_color(status_color)
                        .flex_shrink_0()
                        .child(status),
                    )
                  }),
              )
              .child(
                div()
                  .flex()
                  .flex_row()
                  .items_center()
                  .gap_1()
                  .text_sm()
                  .text_color(rgb(0x888888))
                  .when(security.is_secured(), |this| {
                    this.child(
                      Icon::new(IconName::Lock)
                        .size(rems(0.85))
                        .text_color(rgb(0x888888)),
                    )
                  })
                  .child(security.to_string()),
              ),
          ),
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
