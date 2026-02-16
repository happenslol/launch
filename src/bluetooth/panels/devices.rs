use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

use futures::StreamExt;
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, KeyBinding, Render, SharedString,
  Styled, Subscription, Task, Window, actions, div, hsla, prelude::*, px, relative, rems, rgb,
  rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use tracing::error;

use crate::{
  dbus::{
    GlobalDbusConnection,
    bluez::{Adapter, BlueZ, Device},
  },
  icon::{Icon, IconName},
  matcher::MatcherPool,
  picker::{Category, Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, v_flex},
};

fn device_category(icon: Option<&str>) -> Option<(&'static str, IconName)> {
  let icon = icon?;
  match icon {
    "audio-headphones" => Some(("Headphones", IconName::Headphones)),
    "audio-headset" => Some(("Headset", IconName::Headset)),
    "audio-card" | "audio-speakers" => Some(("Speaker", IconName::Volume)),
    "input-keyboard" => Some(("Keyboard", IconName::Keyboard)),
    "input-mouse" => Some(("Mouse", IconName::Mouse)),
    "input-gaming" => Some(("Gamepad", IconName::DeviceGamepad)),
    "phone" => Some(("Phone", IconName::Phone)),
    "computer" => Some(("Computer", IconName::DeviceDesktop)),
    "printer" => Some(("Printer", IconName::Printer)),
    "camera-photo" | "camera-video" => Some(("Camera", IconName::AppWindow)),
    _ => None,
  }
}

#[derive(Clone)]
pub struct BluetoothEntry {
  id: String,
  search_string: String,
  is_connected: Arc<AtomicBool>,
  is_paired: Arc<AtomicBool>,
  entry: Entity<DeviceEntryInner>,
}

impl BluetoothEntry {
  pub fn new(device: Device, window: &mut Window, cx: &mut App) -> Self {
    let id = device.address.to_string();
    let search_string = format!("{} {}", device.name, device.address);
    let is_connected = Arc::new(AtomicBool::new(device.connected));
    let is_paired = Arc::new(AtomicBool::new(device.paired));
    let entry = cx.new({
      let is_connected = is_connected.clone();
      let is_paired = is_paired.clone();
      |cx| DeviceEntryInner::new(device, is_connected, is_paired, window, cx)
    });

    Self {
      id,
      search_string,
      is_connected,
      is_paired,
      entry,
    }
  }
}

pub struct DeviceEntryInner {
  device: Device,
  is_connected: Arc<AtomicBool>,
  is_paired: Arc<AtomicBool>,
  _property_listeners: Vec<Task<()>>,
}

impl DeviceEntryInner {
  pub fn new(
    device: Device,
    is_connected: Arc<AtomicBool>,
    is_paired: Arc<AtomicBool>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    let mut entry = Self {
      device: device.clone(),
      is_connected,
      is_paired,
      _property_listeners: Vec::new(),
    };

    entry.spawn_property_listeners(&device, window, cx);
    entry
  }

  fn spawn_property_listeners(
    &mut self,
    device: &Device,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let alias_listener = cx.spawn_in(window, {
      let device = device.clone();
      async move |this, cx| {
        let alias_stream = cx
          .background_spawn({
            let device = device.clone();
            async move { device.listen_alias_changed().await }
          })
          .await;

        let Ok(alias_stream) = alias_stream else {
          return;
        };

        futures::pin_mut!(alias_stream);

        while let Some(new_alias) = alias_stream.next().await {
          let _ = this.update(cx, |this, cx| {
            this.device.name = SharedString::from(new_alias.clone());
            cx.notify();
          });
        }
      }
    });

    let connected_listener = cx.spawn_in(window, {
      let device = device.clone();
      async move |this, cx| {
        let connected_stream = cx
          .background_spawn({
            let device = device.clone();
            async move { device.listen_connected_changed().await }
          })
          .await;

        let Ok(connected_stream) = connected_stream else {
          return;
        };

        futures::pin_mut!(connected_stream);

        while let Some(new_connected) = connected_stream.next().await {
          let _ = this.update(cx, |this, cx| {
            this.device.connected = new_connected;
            this.is_connected.store(new_connected, Ordering::Relaxed);
            cx.notify();
          });
        }
      }
    });

    let battery_listener = cx.spawn_in(window, {
      let device = device.clone();
      async move |this, cx| {
        let battery_stream = cx
          .background_spawn({
            let device = device.clone();
            async move { device.listen_battery_changed().await }
          })
          .await;

        let Ok(battery_stream) = battery_stream else {
          return;
        };

        futures::pin_mut!(battery_stream);

        while let Some(new_battery) = battery_stream.next().await {
          let _ = this.update(cx, |this, cx| {
            this.device.battery = new_battery;
            cx.notify();
          });
        }
      }
    });

    let rssi_listener = cx.spawn_in(window, {
      let device = device.clone();
      async move |this, cx| {
        let rssi_stream = cx
          .background_spawn({
            let device = device.clone();
            async move { device.listen_rssi_changed().await }
          })
          .await;

        let Ok(rssi_stream) = rssi_stream else {
          return;
        };

        futures::pin_mut!(rssi_stream);

        while let Some(new_rssi) = rssi_stream.next().await {
          let _ = this.update(cx, |this, cx| {
            this.device.rssi = new_rssi;
            cx.notify();
          });
        }
      }
    });

    self._property_listeners.push(alias_listener);
    self._property_listeners.push(connected_listener);
    self._property_listeners.push(battery_listener);
    self._property_listeners.push(rssi_listener);
  }
}

pub struct BluetoothDevicesPanel {
  picker: Entity<Picker<DevicesDelegate>>,
  bluez: Option<BlueZ>,
  adapter: Option<Adapter>,
  devices: Vec<BluetoothEntry>,
  adapter_powered: Option<bool>,
  is_discovering: bool,
  _device_updates_task: Option<Task<()>>,
  _discovering_listener: Option<Task<()>>,
  _powered_listener: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

actions!(bluetooth, [TogglePower]);

const CONTEXT: &str = "bluetooth";

impl BluetoothDevicesPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new("ctrl-t", TogglePower, Some(CONTEXT))]);

    let picker = cx.new(|cx| {
      let mut picker = Picker::new(DevicesDelegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search bluetooth devices...", cx);
      picker
    });

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(bluetooth_entry) = ev {
          let device = bluetooth_entry.entry.read(cx).device.clone();
          this.handle_device_picked(device, window, cx);
        }
      }),
      cx.observe_global_in::<GlobalDbusConnection>(window, |this, window, cx| {
        if this.bluez.is_none()
          && let Some(conn) = GlobalDbusConnection::system(cx)
        {
          this.initialize(window, cx, &conn);
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let mut panel = Self {
      picker,
      bluez: None,
      adapter: None,
      devices: Vec::new(),
      adapter_powered: None,
      is_discovering: false,
      _device_updates_task: None,
      _discovering_listener: None,
      _powered_listener: None,
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

    self.is_discovering = true;
    cx.notify();

    cx.spawn_in(window, async move |this, cx| {
      let bluez = cx
        .background_spawn(async move { BlueZ::new(&conn).await })
        .await;

      let bluez = match bluez {
        Ok(bluez) => bluez,
        Err(_) => {
          let _ = this.update(cx, |this, cx| {
            this.is_discovering = false;
            cx.notify();
          });
          return Some(());
        }
      };

      let adapter = match cx
        .background_spawn({
          let bluez = bluez.clone();
          async move { bluez.get_adapter().await }
        })
        .await
      {
        Ok(Some(adapter)) => adapter,
        _ => {
          error!("No Bluetooth adapter found");
          let _ = this.update(cx, |this, cx| {
            this.is_discovering = false;
            cx.notify();
          });
          return Some(());
        }
      };

      let _ = cx
        .background_spawn({
          let adapter = adapter.clone();
          async move { adapter.start_discovery().await }
        })
        .await;

      let devices = cx
        .background_spawn({
          let bluez = bluez.clone();
          async move { bluez.get_devices().await }
        })
        .await
        .unwrap_or_default();

      let (is_discovering, is_powered) = cx
        .background_spawn({
          let adapter = adapter.clone();
          async move {
            let discovering = adapter.discovering().await.unwrap_or(false);
            let powered = adapter.powered().await.unwrap_or(false);
            (discovering, powered)
          }
        })
        .await;

      this
        .update_in(cx, |this, window, cx| {
          this.bluez = Some(bluez.clone());
          this.adapter = Some(adapter.clone());
          this.adapter_powered = Some(is_powered);
          this.is_discovering = is_discovering;

          this.devices = devices
            .into_iter()
            .map(|device| BluetoothEntry::new(device, window, cx))
            .collect();

          picker.update(cx, |picker, cx| {
            picker.set_items(this.devices.clone(), window, cx);
          });

          this._device_updates_task = Some(this.listen_for_device_updates(window, cx));
          this._discovering_listener = Some(this.listen_for_discovering_changes(window, cx));
          this._powered_listener = Some(this.listen_for_powered_changes(window, cx));

          cx.notify();
        })
        .ok()?;

      Some(())
    })
    .detach();
  }

  fn listen_for_device_updates(&self, window: &mut Window, cx: &mut Context<Self>) -> Task<()> {
    use futures::pin_mut;

    let Some(bluez) = self.bluez.clone() else {
      return Task::ready(());
    };

    cx.spawn_in(window, async move |this, cx| {
      let conn = bluez.conn.clone();
      let interfaces_added_result = cx
        .background_spawn({
          let bluez = bluez.clone();
          async move { bluez.interfaces_added().await }
        })
        .await;

      let Ok(interfaces_added) = interfaces_added_result else {
        return;
      };

      pin_mut!(interfaces_added);

      while let Some((path, is_device)) = interfaces_added.next().await {
        if !is_device {
          continue;
        }

        let device_result = cx
          .background_spawn({
            let conn = conn.clone();
            async move { Device::new(&conn, path).await }
          })
          .await;

        if let Ok(device) = device_result {
          let _ = this.update_in(cx, |this, window, cx| {
            let device_address = device.address.to_string();

            let already_exists = this
              .devices
              .iter()
              .any(|entry| entry.id == device_address);

            if !already_exists {
              let new_entry = BluetoothEntry::new(device, window, cx);
              this.devices.push(new_entry);

              let picker = this.picker.clone();
              picker.update(cx, |picker, cx| {
                picker.set_items(this.devices.clone(), window, cx);
              });

              cx.notify();
            }
          });
        }
      }
    })
  }

  fn listen_for_discovering_changes(
    &self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    let Some(adapter) = self.adapter.clone() else {
      return Task::ready(());
    };

    cx.spawn_in(window, async move |this, cx| {
      let stream = cx
        .background_spawn({
          let adapter = adapter.clone();
          async move { adapter.listen_discovering_changed().await }
        })
        .await;

      let Ok(stream) = stream else {
        return;
      };

      futures::pin_mut!(stream);

      while let Some(discovering) = stream.next().await {
        let _ = this.update(cx, |this, cx| {
          this.is_discovering = discovering;
          cx.notify();
        });
      }
    })
  }

  fn listen_for_powered_changes(
    &self,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    let Some(adapter) = self.adapter.clone() else {
      return Task::ready(());
    };

    cx.spawn_in(window, async move |this, cx| {
      let stream = cx
        .background_spawn({
          let adapter = adapter.clone();
          async move { adapter.listen_powered_changed().await }
        })
        .await;

      let Ok(stream) = stream else {
        return;
      };

      futures::pin_mut!(stream);

      while let Some(powered) = stream.next().await {
        let _ = this.update(cx, |this, cx| {
          this.adapter_powered = Some(powered);
          cx.notify();
        });
      }
    })
  }

  fn toggle_power(&mut self, _: &TogglePower, _window: &mut Window, cx: &mut Context<Self>) {
    let Some(adapter) = self.adapter.clone() else {
      return;
    };

    let new_powered = !self.adapter_powered.unwrap_or(false);

    cx.background_spawn(async move {
      adapter.set_powered(new_powered).await?;
      if new_powered {
        adapter.start_discovery().await?;
      }
      Ok::<(), anyhow::Error>(())
    })
    .detach_and_log_err(cx);
  }

  fn handle_device_picked(&mut self, device: Device, _window: &mut Window, cx: &mut Context<Self>) {
    cx.background_spawn(async move {
      if !device.paired {
        device.pair().await?;
      }

      if device.connected {
        device.disconnect().await?;
      } else {
        device.connect().await?;
      }

      Ok::<(), anyhow::Error>(())
    })
    .detach_and_log_err(cx);
  }
}

impl Focusable for BluetoothDevicesPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for BluetoothDevicesPanel {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let mut input = picker_input(&self.picker).show_back_button(true);

    let mut suffix = div()
      .flex()
      .flex_row()
      .items_center()
      .gap_3()
      .flex_shrink_0();

    if self.is_discovering {
      suffix = suffix.child(
        div()
          .text_sm()
          .text_color(rgb(0x888888))
          .child("Scanning"),
      );
    }

    if let Some(powered) = self.adapter_powered {
      let (color, label) = if powered {
        (rgb(0x44AA44), "On")
      } else {
        (rgb(0xCC4444), "Off")
      };

      suffix = suffix.child(
        div()
          .flex()
          .flex_row()
          .items_center()
          .gap_1p5()
          .child(
            div()
              .w(px(8.))
              .h(px(8.))
              .rounded_full()
              .bg(color),
          )
          .child(
            div()
              .text_sm()
              .text_color(color)
              .child(label),
          ),
      );
    }

    input = input.suffix(suffix);

    v_flex()
      .key_context(CONTEXT)
      .on_action(cx.listener(Self::toggle_power))
      .size_full()
      .child(input)
      .child(picker_results(&self.picker))
  }
}

struct DevicesDelegate;

impl PickerDelegate for DevicesDelegate {
  type ListItem = BluetoothEntry;

  fn sort_items(&self, cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by_key(|(index, score)| {
      let device = &items[*index].entry.read(cx).device;
      (
        std::cmp::Reverse(*score),
        std::cmp::Reverse(device.connected),
        std::cmp::Reverse(device.paired),
      )
    });
  }

  fn categories(&self) -> Option<Vec<Category<Self::ListItem>>> {
    Some(vec![
      Category::new("Connected", |entry: &BluetoothEntry| {
        entry.is_connected.load(Ordering::Relaxed)
      }),
      Category::new("Paired", |entry: &BluetoothEntry| {
        !entry.is_connected.load(Ordering::Relaxed)
          && entry.is_paired.load(Ordering::Relaxed)
      }),
      Category::new("New Devices", |entry: &BluetoothEntry| {
        !entry.is_connected.load(Ordering::Relaxed)
          && !entry.is_paired.load(Ordering::Relaxed)
      }),
    ])
  }

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let device = &item.entry.read(cx).device;
    let name: SharedString = device.name.clone();
    let address: SharedString = device.address.clone();

    let status = if device.connected {
      Some("Connected")
    } else if !device.paired {
      Some("New")
    } else {
      None
    };

    let status_color = if device.connected {
      rgb(0x44AA44)
    } else {
      rgb(0x888888)
    };

    let category = device_category(device.icon.as_ref().map(|s| s.as_ref()));
    let battery_text = device.battery.map(|b| format!("{}%", b));

    // RSSI typically ranges from -100 (weak) to -40 (strong)
    let signal_strength = device.rssi.map(|rssi| {
      ((rssi.clamp(-100, -40) + 100) as f32 / 60.0).clamp(0.0, 1.0)
    });

    v_flex()
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
            // Signal strength bar when RSSI available, device icon otherwise
            if let Some(strength) = signal_strength {
              let signal_color = hsla(strength * 0.33, 0.8, 0.5, 1.0);
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
                )
                .into_any_element()
            } else {
              div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .w(px(5.))
                .child(
                  div()
                    .w(px(5.))
                    .h(px(5.))
                    .rounded_full()
                    .bg(rgb(0x555555)),
                )
                .into_any_element()
            },
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
                  .child(div().text_ellipsis().overflow_x_hidden().child(name))
                  .child(
                    div()
                      .text_sm()
                      .text_color(rgb(0x666666))
                      .flex_shrink_0()
                      .child(address),
                  )
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
                  .gap_2()
                  .text_sm()
                  .text_color(rgb(0x888888))
                  .when_some(category, |this, (label, icon)| {
                    this.child(
                      div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(
                          Icon::new(icon)
                            .custom_size(rems(0.75))
                            .color(rgb(0x888888).into()),
                        )
                        .child(label),
                    )
                  })
                  .when_some(battery_text, |this, battery| {
                    this.child(
                      div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .child(battery),
                    )
                  }),
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
