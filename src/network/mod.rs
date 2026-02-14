mod wifi;

use std::sync::{Arc, atomic::AtomicBool};

use anyhow::Result;
use gpui::{
  App, Context, Entity, FocusHandle, Focusable, IntoElement, SharedString, Task, Window,
  prelude::*, rgba,
};

use crate::{
  icon::IconName,
  launcher::RootItem,
  picker::{Picker, PickerDelegate, picker_input, picker_results},
  util::{h_flex, v_flex},
};

pub fn get_items() -> Vec<RootItem> {
  vec![
    RootItem::Panel {
      id: "networks".into(),
      name: "Networks".into(),
      icon: IconName::Network,
      terms: vec!["net".into(), "network".into(), "ethernet".into()],
      view: Arc::new(|window, cx| cx.new(|cx| NetworkPanel::new(window, cx)).into()),
    },
    RootItem::Panel {
      id: "wifi".into(),
      icon: IconName::Wifi,
      name: "Wifi".into(),
      terms: vec!["wifi".into()],
      view: Arc::new(|window, cx| cx.new(|cx| wifi::WifiPanel::new(window, cx)).into()),
    },
  ]
}

pub struct NetworkPanel {
  picker: Entity<Picker<NetworkDelegate>>,
  _dbus_task: Task<Result<()>>,
}

const CONTEXT: &str = "network";

impl NetworkPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let picker = cx.new(|cx| {
      let mut picker = Picker::new(NetworkDelegate {}, Arc::new(vec![]), window, cx);
      picker.placeholder("Search networks...", cx);
      picker
    });
    cx.focus_view(&picker, window);

    let dbus_task = cx.spawn_in(window, async move |_this, _cx| Ok(()));

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
      .child(picker_input(&self.picker).show_back_button(true))
      .child(picker_results(&self.picker))
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
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
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
