use std::{path::PathBuf, time::Duration};

use gpui::{
  AnyElement, App, Bounds, ClickEvent, Context, DisplayId, Entity, FontWeight, Global, ImageSource,
  IntoElement, MouseButton, Pixels, Render, Resource, SharedString, Size, Styled, Subscription, Task,
  Window, WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img,
  point, prelude::*, px, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use tracing::{error, warn};

use crate::{
  config::ConfigState,
  dbus::notifications::{CloseReason, Notification, NotificationEvent, Notifications, Urgency},
  icon::{Icon, IconName},
  util::{ResultExt, h_flex, v_flex},
};

const WINDOW_WIDTH: f32 = 400.0;
const CARD_HEIGHT: f32 = 96.0;
const CARD_GAP: f32 = 10.0;
const MARGIN: f32 = 12.0;

/// A burst of notifications (e.g. several `notify-send`s at once) produces a
/// rapid sequence of changes. Resizing the layer-shell surface for each one
/// back-to-back is unreliable — consecutive resizes race the compositor's
/// configure handshake and the surface can stay stuck at its initial size.
/// Coalescing the changes into a single sync resizes the surface once, after
/// the burst has settled.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(50);

pub fn init(cx: &mut App) {
  let notifications = Notifications::global(cx);
  let osd = cx.new(|cx| NotificationOsd::new(notifications, cx));
  cx.set_global(GlobalNotificationOsd(osd));
}

struct GlobalNotificationOsd(#[allow(dead_code)] Entity<NotificationOsd>);

impl Global for GlobalNotificationOsd {}

/// Mirrors the live notification list onto a single layer-shell surface anchored
/// to the top-right of the first display, resizing it to fit the current stack.
struct NotificationOsd {
  notifications: Entity<Notifications>,
  window: Option<WindowHandle<NotificationsView>>,
  count: usize,
  sync_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl NotificationOsd {
  fn new(notifications: Entity<Notifications>, cx: &mut Context<Self>) -> Self {
    let config = ConfigState::global(cx);
    let subscriptions = vec![
      cx.subscribe(
        &notifications,
        |this, _notifications, _event: &NotificationEvent, cx| this.schedule_sync(cx),
      ),
      cx.observe(&config, |this, _config, cx| this.reload_display(cx)),
    ];

    let mut this = Self {
      notifications,
      window: None,
      count: 0,
      sync_task: None,
      _subscriptions: subscriptions,
    };

    this.sync(cx);
    this
  }

  /// Coalesces rapid notification changes into a single `sync`. Each call
  /// replaces the pending task, so a burst of changes only triggers one resize
  /// once the burst has settled. See [`SYNC_DEBOUNCE`].
  fn schedule_sync(&mut self, cx: &mut Context<Self>) {
    self.sync_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(SYNC_DEBOUNCE).await;
      this.update(cx, |this, cx| this.sync(cx)).log_err();
    }));
  }

  /// Tears down the current surface so the next `sync` reopens it on the
  /// configured display. Called when the config changes so the live-reloaded
  /// display setting takes effect immediately.
  fn reload_display(&mut self, cx: &mut Context<Self>) {
    if let Some(handle) = self.window.take() {
      handle
        .update(cx, |_view, window, _cx| window.remove_window())
        .log_err();
    }
    self.count = 0;
    self.sync(cx);
  }

  fn sync(&mut self, cx: &mut Context<Self>) {
    let active = self.notifications.read(cx).active().to_vec();
    let count = active.len();

    if count == 0 {
      if let Some(handle) = self.window.take() {
        handle
          .update(cx, |_view, window, _cx| window.remove_window())
          .log_err();
      }
      self.count = 0;
      return;
    }

    let height = window_height(count);

    if let Some(handle) = self.window {
      // Resize the existing surface in place rather than recreating it when the
      // stack grows or shrinks, matching the workspace OSD's approach.
      let needs_resize = self.count != count;
      let updated = handle
        .update(cx, |view, window, cx| {
          if needs_resize {
            window.resize(Size::new(px(WINDOW_WIDTH), height));
          }
          view.update_content(active.clone(), cx);
        })
        .is_ok();

      if updated {
        self.count = count;
        return;
      }

      // The window was closed since we last opened it; reopen below.
      self.window = None;
    }

    let Some(display_id) = target_display(cx) else {
      error!("no display available to show notifications on");
      return;
    };

    match cx.open_window(window_options(display_id, height), {
      let notifications = self.notifications.clone();
      move |_window, cx| cx.new(|_cx| NotificationsView::new(notifications, active))
    }) {
      Ok(handle) => {
        self.window = Some(handle);
        self.count = count;
      }
      Err(error) => error!(?error, "Failed to open notifications window"),
    }
  }
}

/// Resolves the display to show notifications on. Uses the configured output
/// name when set and present, otherwise falls back to the first display.
fn target_display(cx: &App) -> Option<DisplayId> {
  let configured = ConfigState::get(cx).notifications.display;
  let displays = cx.displays();

  if let Some(name) = configured {
    if let Some(display) = displays.iter().find(|display| display.name() == Some(name.as_str())) {
      return Some(display.id());
    }

    warn!(%name, "Configured notification display not found, using first display");
  }

  displays.into_iter().next().map(|display| display.id())
}

fn window_height(count: usize) -> Pixels {
  let count = count.max(1) as f32;
  px(count * CARD_HEIGHT + (count - 1.0) * CARD_GAP)
}

fn window_options(display_id: DisplayId, height: Pixels) -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-notifications".to_string()),
    display_id: Some(display_id),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(px(WINDOW_WIDTH), height),
    })),
    window_background: WindowBackgroundAppearance::Transparent,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-notifications".to_string(),
      layer: Layer::Overlay,
      anchor: Anchor::TOP | Anchor::RIGHT,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: Some((px(MARGIN), px(MARGIN), px(0.), px(0.))),
      keyboard_interactivity: KeyboardInteractivity::None,
    }),
    ..Default::default()
  }
}

struct NotificationsView {
  notifications: Entity<Notifications>,
  items: Vec<Notification>,
}

impl NotificationsView {
  fn new(notifications: Entity<Notifications>, items: Vec<Notification>) -> Self {
    Self {
      notifications,
      items,
    }
  }

  fn update_content(&mut self, items: Vec<Notification>, cx: &mut Context<Self>) {
    self.items = items;
    cx.notify();
  }

  fn render_card(&self, notification: &Notification, cx: &mut Context<Self>) -> impl IntoElement + use<> {
    let id = notification.id;
    let has_default = notification.actions.iter().any(|action| action.key == "default");
    let border_color = match notification.urgency {
      Urgency::Critical => rgba(0xE0524Fcc),
      _ => rgba(0xFFFFFF15),
    };

    let buttons = notification
      .actions
      .iter()
      .filter(|action| action.key != "default")
      .enumerate()
      .map(|(index, action)| {
        let key = action.key.clone();
        div()
          .id(SharedString::from(format!("notif-{id}-action-{index}")))
          .px_2()
          .py_1()
          .rounded_md()
          .bg(rgba(0xFFFFFF12))
          .text_xs()
          .text_color(rgba(0xFFFFFFCC))
          .cursor_pointer()
          .hover(|style| style.bg(rgba(0xFFFFFF22)))
          .child(action.label.clone())
          .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
            let key = key.clone();
            this
              .notifications
              .update(cx, |notifications, cx| notifications.invoke_action(id, key, cx));
            cx.stop_propagation();
          }))
      })
      .collect::<Vec<_>>();

    h_flex()
      .id(SharedString::from(format!("notif-{id}")))
      .h(px(CARD_HEIGHT))
      .w_full()
      .items_start()
      .gap_3()
      .p_3()
      .rounded_xl()
      .bg(rgba(0x1D1D1DF0))
      .border_1()
      .border_color(border_color)
      .shadow_lg()
      .cursor_pointer()
      .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
        this.notifications.update(cx, |notifications, cx| {
          if has_default {
            notifications.invoke_action(id, "default".to_string(), cx);
          } else {
            notifications.dismiss(id, CloseReason::Dismissed, cx);
          }
        });
      }))
      .on_mouse_down(
        MouseButton::Right,
        cx.listener(move |this, _, _window, cx| {
          this.notifications.update(cx, |notifications, cx| {
            notifications.dismiss(id, CloseReason::Dismissed, cx)
          });
        }),
      )
      .child(render_icon(notification))
      .child(
        v_flex()
          .flex_1()
          .min_w(px(0.))
          .h_full()
          .overflow_hidden()
          .gap_1()
          .when(!notification.app_name.is_empty(), |this| {
            this.child(
              div()
                .text_xs()
                .text_color(rgba(0xFFFFFF99))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(notification.app_name.clone()),
            )
          })
          .child(
            div()
              .text_sm()
              .font_weight(FontWeight::SEMIBOLD)
              .text_color(rgb(0xEEEEEE))
              .overflow_hidden()
              .whitespace_nowrap()
              .text_ellipsis()
              .child(notification.summary.clone()),
          )
          .when(!notification.body.is_empty(), |this| {
            this.child(
              div()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .text_xs()
                .text_color(rgba(0xFFFFFFAA))
                .child(notification.body.clone()),
            )
          })
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().gap_2().flex_none().children(buttons))
          }),
      )
  }
}

impl Render for NotificationsView {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let mut cards = Vec::with_capacity(self.items.len());
    for notification in &self.items {
      cards.push(self.render_card(notification, cx));
    }

    v_flex().size_full().gap(px(CARD_GAP)).children(cards)
  }
}

fn render_icon(notification: &Notification) -> AnyElement {
  let container = div().flex_none().size(px(40.)).rounded_md().overflow_hidden();

  if let Some(image) = &notification.image {
    return container
      .child(img(ImageSource::Render(image.clone())).size_full())
      .into_any_element();
  }

  if let Some(path) = &notification.icon_path {
    let resource = Resource::Path(PathBuf::from(path.to_string()).into());
    return container
      .child(img(ImageSource::Resource(resource)).size_full())
      .into_any_element();
  }

  container
    .flex()
    .items_center()
    .justify_center()
    .child(Icon::new(IconName::Bell).size(px(22.)).text_color(rgba(0xFFFFFF88)))
    .into_any_element()
}
