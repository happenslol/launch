use std::sync::{Arc, atomic::AtomicBool};

use futures::StreamExt;
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Styled, Subscription, Task,
  Window, div, prelude::*, rgb,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use tracing::error;

use crate::{
  dbus::{GlobalDbusConnection, bluez::{Adapter, BlueZ, Device}},
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{h_flex, v_flex},
};

pub struct BluetoothDevicesPanel {
  picker: Entity<Picker<DevicesDelegate>>,
  bluez: Option<BlueZ>,
  adapter: Option<Adapter>,
  devices: Vec<Device>,
  _device_updates_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

const CONTEXT: &str = "bluetooth";

impl BluetoothDevicesPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let delegate = DevicesDelegate {};
    let picker = cx.new(|cx| Picker::new(delegate, Arc::new(vec![]), window, cx));

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, ev, window, cx| {
        if let PickerEvent::Picked(device) = ev {
          this.handle_device_picked(device.clone(), window, cx);
        }
      }),
      cx.observe_global_in::<GlobalDbusConnection>(window, |this, window, cx| {
        if this.bluez.is_none() {
          if let Some(conn) = GlobalDbusConnection::system(cx) {
            this.initialize(window, cx, &conn);
          }
        }
      }),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let mut panel = Self {
      picker,
      bluez: None,
      adapter: None,
      devices: Vec::new(),
      _device_updates_task: None,
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
      let bluez = cx
        .background_spawn(async move { BlueZ::new(&conn).await })
        .await;

      let bluez = match bluez {
        Ok(bluez) => bluez,
        Err(_) => return Some(()),
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
          return Some(());
        }
      };

      let _ = cx
        .background_spawn({
          let adapter = adapter.clone();
          async move { adapter.start_discovery().await }
        })
        .await;

      let devices = match cx
        .background_spawn({
          let bluez = bluez.clone();
          async move { bluez.get_devices().await }
        })
        .await
      {
        Ok(devices) => devices,
        Err(_) => Vec::new(),
      };

      this
        .update_in(cx, |this, window, cx| {
          this.bluez = Some(bluez.clone());
          this.adapter = Some(adapter.clone());
          this.devices = devices;

          picker.update(cx, |picker, cx| {
            picker.set_items(this.devices.clone(), window, cx);
          });

          this._device_updates_task = Some(this.listen_for_device_updates(window, cx));

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
      let interfaces_added_result = cx.background_spawn({
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
            let device_address = device.address.clone();
            if let Some(existing) = this.devices.iter_mut().find(|d| d.address == device_address) {
              *existing = device.clone();
            } else {
              this.devices.push(device.clone());
            }

            let picker = this.picker.clone();
            picker.update(cx, |picker, cx| {
              picker.set_items(this.devices.clone(), window, cx);
            });

            cx.notify();
          });
        }
      }
    })
  }


  fn handle_device_picked(&mut self, device: Device, window: &mut Window, cx: &mut Context<Self>) {
    cx.spawn_in(window, async move |_this, cx| {
      let _ = cx
        .background_spawn(async move {
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
        .await;
    })
    .detach();
  }
}

impl Focusable for BluetoothDevicesPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for BluetoothDevicesPanel {
  fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .child(self.picker.clone())
  }
}

struct DevicesDelegate {}

impl PickerDelegate for DevicesDelegate {
  type ListItem = Device;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let mut status_text = String::new();
    if item.connected {
      status_text.push_str("CONNECTED ");
    }
    if !item.paired {
      status_text.push_str("NEW ");
    }
    status_text.push_str(&item.name);

    let battery_text = item
      .battery
      .map(|b| format!("Battery: {}%", b))
      .unwrap_or_else(|| String::from(" "));

    v_flex()
      .w_full()
      .when(is_selected, |this| this.bg(rgb(0x444444)))
      .child(
        // First row: device name with status
        div()
          .w_full()
          .text_ellipsis()
          .overflow_x_hidden()
          .child(status_text),
      )
      .child(
        // Second row: battery info (or empty space)
        div().child(battery_text),
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

    let mut matcher = Matcher::new(Config::DEFAULT);
    let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
    let mut matches = Vec::new();
    let mut buf = Vec::new();

    for (index, item) in items.iter().enumerate() {
      let mut max_score: Option<u32> = None;

      if let Some(score) = needle.score(Utf32Str::new(&item.name, &mut buf), &mut matcher) {
        max_score = Some(score);
      }

      if let Some(score) = needle.score(Utf32Str::new(&item.address, &mut buf), &mut matcher) {
        max_score = Some(max_score.map_or(score, |s| s.max(score)));
      }

      if let Some(score) = max_score {
        matches.push((index, score));
      }
    }

    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, Some(matches));
    });

    Task::ready(())
  }
}
