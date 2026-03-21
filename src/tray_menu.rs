use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use gpui::{
  Animation, AnimationExt, App, Context, ElementId, EventEmitter, FocusHandle, Focusable,
  IntoElement, KeyBinding, MouseButton, Render, SharedString, Task, Window, actions, div,
  prelude::*, px, rgba,
};
use zvariant::Value;

use crate::dbus::status_notifier::DBusMenuProxy;
use crate::icon::{Icon, IconName};
use crate::util::ResultExt;

actions!(
  tray_menu,
  [
    Dismiss,
    SelectNext,
    SelectPrev,
    Activate,
    OpenSubmenu,
    CloseSubmenu,
  ]
);

const CONTEXT: &str = "tray_menu";
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(100);

pub enum TrayMenuEvent {
  Dismissed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleType {
  None,
  Checkmark,
  Radio,
}

#[derive(Debug, Clone)]
pub struct MenuItem {
  id: i32,
  label: SharedString,
  enabled: bool,
  visible: bool,
  is_separator: bool,
  toggle_type: ToggleType,
  toggle_state: i32,
  children: Vec<MenuItem>,
}

impl MenuItem {
  fn has_submenu(&self) -> bool {
    !self.children.is_empty()
  }
}

pub fn parse_layout(
  raw: (i32, HashMap<String, zvariant::OwnedValue>, Vec<zvariant::OwnedValue>),
) -> Result<Vec<MenuItem>> {
  let (_id, _props, children) = raw;
  let mut items = Vec::new();
  for child in children {
    if let Some(item) = parse_menu_item(&child) {
      items.push(item);
    }
  }
  Ok(items)
}

fn prop_string(props: &HashMap<String, zvariant::OwnedValue>, key: &str) -> Option<String> {
  let value = props.get(key)?;
  String::try_from(value.clone()).ok()
}

fn prop_bool(props: &HashMap<String, zvariant::OwnedValue>, key: &str) -> Option<bool> {
  let value = props.get(key)?;
  bool::try_from(value.clone()).ok()
}

fn prop_i32(props: &HashMap<String, zvariant::OwnedValue>, key: &str) -> Option<i32> {
  let value = props.get(key)?;
  i32::try_from(value.clone()).ok()
}

fn parse_menu_item(value: &zvariant::OwnedValue) -> Option<MenuItem> {
  let value_ref: Value = value.downcast_ref().ok()?;
  let structure = match value_ref {
    Value::Structure(s) => s,
    _ => return None,
  };
  let fields = structure.fields();

  let id = fields.first().and_then(|v| i32::try_from(v).ok())?;

  let props: HashMap<String, zvariant::OwnedValue> = fields
    .get(1)
    .and_then(|v| {
      if let Value::Dict(dict) = v {
        let mut map = HashMap::new();
        for (key, value) in dict.iter() {
          if let Some(key_str) = <&str>::try_from(key).ok() {
            // Dict values from a{sv} are variant-wrapped, unwrap them
            let unwrapped = match value {
              Value::Value(inner) => inner.try_to_owned().ok()?,
              other => other.try_to_owned().ok()?,
            };
            map.insert(key_str.to_owned(), unwrapped);
          }
        }
        Some(map)
      } else {
        None
      }
    })
    .unwrap_or_default();

  let raw_children: Vec<&Value> = fields
    .get(2)
    .and_then(|v| {
      if let Value::Array(arr) = v {
        Some(arr.iter().collect())
      } else {
        None
      }
    })
    .unwrap_or_default();

  let label = prop_string(&props, "label")
    .map(|s| strip_mnemonic(&s))
    .unwrap_or_default();

  let enabled = prop_bool(&props, "enabled").unwrap_or(true);
  let visible = prop_bool(&props, "visible").unwrap_or(true);

  let item_type = prop_string(&props, "type").unwrap_or_default();
  let is_separator = item_type == "separator";

  let toggle_type = match prop_string(&props, "toggle-type").as_deref().unwrap_or_default() {
    "checkmark" => ToggleType::Checkmark,
    "radio" => ToggleType::Radio,
    _ => ToggleType::None,
  };

  let toggle_state = prop_i32(&props, "toggle-state").unwrap_or(-1);

  let mut child_items = Vec::new();
  for child_value in &raw_children {
    if let Ok(owned) = child_value.try_to_owned() {
      if let Some(item) = parse_menu_item(&owned) {
        child_items.push(item);
      }
    }
  }

  Some(MenuItem {
    id,
    label: SharedString::from(label),
    enabled,
    visible,
    is_separator,
    toggle_type,
    toggle_state,
    children: child_items,
  })
}

fn strip_mnemonic(label: &str) -> String {
  let mut result = String::with_capacity(label.len());
  let mut chars = label.chars();
  while let Some(ch) = chars.next() {
    if ch == '_' {
      if let Some(next) = chars.next() {
        if next == '_' {
          result.push('_');
        } else {
          result.push(next);
        }
      }
    } else {
      result.push(ch);
    }
  }
  result
}

pub struct TrayMenu {
  items: Vec<MenuItem>,
  menu_stack: Vec<(Vec<MenuItem>, Option<usize>)>,
  selected_index: Option<usize>,
  closing: bool,
  dismiss_task: Option<Task<()>>,
  focus_handle: FocusHandle,
  proxy: DBusMenuProxy<'static>,
}

impl EventEmitter<TrayMenuEvent> for TrayMenu {}

impl TrayMenu {
  pub fn new(
    items: Vec<MenuItem>,
    proxy: DBusMenuProxy<'static>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Self {
    cx.bind_keys([
      KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
      KeyBinding::new("down", SelectNext, Some(CONTEXT)),
      KeyBinding::new("j", SelectNext, Some(CONTEXT)),
      KeyBinding::new("up", SelectPrev, Some(CONTEXT)),
      KeyBinding::new("k", SelectPrev, Some(CONTEXT)),
      KeyBinding::new("enter", Activate, Some(CONTEXT)),
      KeyBinding::new("space", Activate, Some(CONTEXT)),
      KeyBinding::new("right", OpenSubmenu, Some(CONTEXT)),
      KeyBinding::new("l", OpenSubmenu, Some(CONTEXT)),
      KeyBinding::new("left", CloseSubmenu, Some(CONTEXT)),
      KeyBinding::new("h", CloseSubmenu, Some(CONTEXT)),
    ]);

    let focus_handle = cx.focus_handle();
    window.focus(&focus_handle, cx);

    let initial_index = first_selectable_index(&items, None, Direction::Forward);

    Self {
      items,
      menu_stack: Vec::new(),
      selected_index: initial_index,
      closing: false,
      dismiss_task: None,
      focus_handle,
      proxy,
    }
  }

  fn dismiss(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }

    self.closing = true;
    cx.notify();

    self.dismiss_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;

      this
        .update(cx, |_this, cx| {
          cx.emit(TrayMenuEvent::Dismissed);
        })
        .log_err();
    }));
  }

  fn dismiss_action(&mut self, _: &Dismiss, _window: &mut Window, cx: &mut Context<Self>) {
    if !self.menu_stack.is_empty() {
      self.close_submenu(cx);
    } else {
      self.dismiss(cx);
    }
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    self.selected_index =
      first_selectable_index(&self.items, self.selected_index, Direction::Forward);
    cx.notify();
  }

  fn select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
    self.selected_index =
      first_selectable_index(&self.items, self.selected_index, Direction::Backward);
    cx.notify();
  }

  fn activate_action(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
    self.activate_selected(window, cx);
  }

  fn selected_visible_item(&self) -> Option<MenuItem> {
    let index = self.selected_index?;
    self
      .items
      .iter()
      .filter(|item| item.visible)
      .nth(index)
      .cloned()
  }

  fn activate_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(item) = self.selected_visible_item() else {
      return;
    };

    if !item.enabled || item.is_separator {
      return;
    }

    if item.has_submenu() {
      self.open_submenu_for(&item, window, cx);
    } else {
      let id = item.id;
      let proxy = self.proxy.clone();
      cx.spawn_in(window, async move |_, cx| {
        proxy
          .event(id, "clicked", &Value::I32(0), 0)
          .await
          .log_err();
        let _ = cx.update(|window, _| window.remove_window());
      })
      .detach();
    }
  }

  fn open_submenu_action(
    &mut self,
    _: &OpenSubmenu,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(item) = self.selected_visible_item() else {
      return;
    };

    if item.has_submenu() && item.enabled {
      self.open_submenu_for(&item, window, cx);
    }
  }

  fn open_submenu_for(
    &mut self,
    item: &MenuItem,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let id = item.id;
    let children = item.children.clone();
    let proxy = self.proxy.clone();
    let parent_items = std::mem::replace(&mut self.items, Vec::new());
    let parent_selection = self.selected_index;

    self.menu_stack.push((parent_items, parent_selection));

    let initial_index = first_selectable_index(&children, None, Direction::Forward);
    self.items = children;
    self.selected_index = initial_index;
    cx.notify();

    cx.spawn_in(window, async move |this, cx| {
      let needs_update = proxy.about_to_show(id).await.unwrap_or(false);
      if needs_update {
        let layout = proxy.get_layout(id, -1, vec![]).await;
        if let Ok((_revision, raw)) = layout {
          if let Ok(new_items) = parse_layout(raw) {
            this
              .update_in(cx, |this, _window, cx| {
                this.items = new_items;
                this.selected_index =
                  first_selectable_index(&this.items, None, Direction::Forward);
                cx.notify();
              })
              .log_err();
          }
        }
      }
    })
    .detach();
  }

  fn close_submenu_action(
    &mut self,
    _: &CloseSubmenu,
    _window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    if self.menu_stack.is_empty() {
      self.dismiss(cx);
    } else {
      self.close_submenu(cx);
    }
  }

  fn close_submenu(&mut self, cx: &mut Context<Self>) {
    if let Some((parent_items, parent_selection)) = self.menu_stack.pop() {
      self.items = parent_items;
      self.selected_index = parent_selection;
      cx.notify();
    }
  }

  fn activate_item_at(&mut self, visible_index: usize, window: &mut Window, cx: &mut Context<Self>) {
    self.selected_index = Some(visible_index);
    cx.notify();
    self.activate_selected(window, cx);
  }
}

#[derive(Clone, Copy)]
enum Direction {
  Forward,
  Backward,
}

fn first_selectable_index(
  items: &[MenuItem],
  current: Option<usize>,
  direction: Direction,
) -> Option<usize> {
  let visible: Vec<(usize, &MenuItem)> = items
    .iter()
    .enumerate()
    .filter(|(_, item)| item.visible)
    .collect();

  if visible.is_empty() {
    return None;
  }

  let count = visible.len();

  let start = match (current, direction) {
    (None, Direction::Forward) => 0,
    (None, Direction::Backward) => count - 1,
    (Some(current), Direction::Forward) => {
      if current + 1 >= count {
        0
      } else {
        current + 1
      }
    }
    (Some(current), Direction::Backward) => {
      if current == 0 {
        count - 1
      } else {
        current - 1
      }
    }
  };

  for offset in 0..count {
    let index = match direction {
      Direction::Forward => (start + offset) % count,
      Direction::Backward => (start + count - offset) % count,
    };

    let (_, item) = &visible[index];
    if !item.is_separator && item.enabled {
      return Some(index);
    }
  }

  None
}

impl Focusable for TrayMenu {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for TrayMenu {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let closing = self.closing;
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);
    let mut visible_items: Vec<MenuItem> = self
      .items
      .iter()
      .filter(|i| i.visible)
      .cloned()
      .fold(Vec::new(), |mut acc, item| {
        if item.is_separator {
          if acc.last().is_some_and(|last: &MenuItem| !last.is_separator) {
            acc.push(item);
          }
        } else {
          acc.push(item);
        }
        acc
      });
    if visible_items.last().is_some_and(|i| i.is_separator) {
      visible_items.pop();
    }
    let selected_index = self.selected_index;

    // Check if any item has a toggle indicator for alignment
    let has_toggles = visible_items
      .iter()
      .any(|item| item.toggle_type != ToggleType::None);

    div()
      .id("tray-menu-overlay")
      .absolute()
      .top_0()
      .left_0()
      .size_full()
      .flex()
      .items_end()
      .justify_end()
      .pb_4()
      .pr_4()
      .key_context(CONTEXT)
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(Self::dismiss_action))
      .on_action(cx.listener(Self::select_next))
      .on_action(cx.listener(Self::select_prev))
      .on_action(cx.listener(Self::activate_action))
      .on_action(cx.listener(Self::open_submenu_action))
      .on_action(cx.listener(Self::close_submenu_action))
      .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
        this.dismiss(cx);
      }))
      .child(
        div()
          .id("tray-menu-content")
          .occlude()
          .w(px(250.))
          .max_h(px(400.))
          .overflow_y_scroll()
          .bg(rgba(0x171717F0))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .rounded_lg()
          .py_1()
          .children(visible_items.iter().enumerate().map(|(index, item)| {
            let is_selected = selected_index == Some(index);

            if item.is_separator {
              return div()
                .id(ElementId::NamedInteger("tray-sep".into(), index as u64))
                .mx_2()
                .my_1()
                .h(px(1.))
                .bg(rgba(0xFFFFFF15))
                .into_any_element();
            }

            let enabled = item.enabled;
            let has_submenu = item.has_submenu();

            div()
              .id(ElementId::NamedInteger("tray-item".into(), index as u64))
              .px_2()
              .py(px(5.))
              .mx_1()
              .rounded_md()
              .text_sm()
              .flex()
              .items_center()
              .gap_2()
              .when(is_selected && enabled, |this| this.bg(rgba(0xFFFFFF0F)))
              .when(!enabled, |this| this.text_color(rgba(0xFFFFFF44)))
              .when(enabled, |this| {
                this.cursor_pointer().on_mouse_move({
                  let index = index;
                  cx.listener(move |this, _, _, cx| {
                    if this.selected_index != Some(index) {
                      this.selected_index = Some(index);
                      cx.notify();
                    }
                  })
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                  this.activate_item_at(index, window, cx);
                }))
              })
              .when(has_toggles, |this| {
                this.child(
                  div()
                    .w(px(16.))
                    .h(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|this| match (item.toggle_type, item.toggle_state) {
                      (ToggleType::Checkmark, 1) => this.child(
                        Icon::new(IconName::Check)
                          .size(px(14.))
                          .text_color(rgba(0xFFFFFFCC)),
                      ),
                      (ToggleType::Radio, 1) => this.child(
                        Icon::new(IconName::CircleDot)
                          .size(px(14.))
                          .text_color(rgba(0xFFFFFFCC)),
                      ),
                      _ => this,
                    }),
                )
              })
              .child(div().flex_grow().min_w_0().truncate().child(item.label.clone()))
              .when(has_submenu, |this| {
                this.child(
                  Icon::new(IconName::ChevronRight)
                    .size(px(14.))
                    .text_color(rgba(0xFFFFFF66))
                    .flex_none(),
                )
              })
              .into_any_element()
          }))
          .with_animation(
            ElementId::NamedInteger("tray-menu-slide".into(), closing as u64),
            Animation::new(ANIM_ENTER_DURATION).with_easing(easing),
            move |this, delta| {
              let progress = if closing { delta } else { 1.0 - delta };
              let opacity = if closing { 1.0 - delta } else { delta };
              let scale = 0.9 + 0.1 * (1.0 - progress);
              let offset = 15.0 * progress;
              this
                .w(px(250. * scale))
                .mb(px(-offset))
                .mr(px(-offset))
                .opacity(opacity)
            },
          ),
      )
  }
}
