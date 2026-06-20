use std::{
  path::PathBuf,
  sync::LazyLock,
  time::{Duration, Instant},
};

use gpui::{
  AnyElement, App, Bounds, ClickEvent, Context, DisplayId, Div, Entity, FontWeight, Global,
  ImageSource, IntoElement, MouseButton, ObjectFit, Pixels, Render, Resource, SharedString, Size,
  Stateful, Styled, Subscription, Task, Window, WindowBackgroundAppearance, WindowBounds,
  WindowHandle, WindowKind, WindowOptions, div, img, point, prelude::*, px, rems, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use regex::Regex;
use tracing::{error, warn};

use crate::{
  config::ConfigState,
  dbus::notifications::{
    CloseReason, Notification, NotificationAction, NotificationEvent, Notifications, Urgency,
  },
  icon::{Icon, IconName},
  util::{ResultExt, h_flex, v_flex},
};

const WINDOW_WIDTH: f32 = 400.0;
const CARD_GAP: f32 = 10.0;
const MARGIN: f32 = 12.0;

// Per-layout card heights. Layouts have fixed heights (with a taller variant
// for those that grow to fit action buttons) so the surface can be sized to the
// whole stack without measuring rendered content.
const COMPACT_HEIGHT: f32 = 64.0;
const COMPACT_ICON_SIZE: f32 = 36.0;

const MESSAGE_HEIGHT: f32 = 110.0;
const MESSAGE_HEIGHT_ACTIONS: f32 = 150.0;
const MESSAGE_ICON_SIZE: f32 = 44.0;

const MEDIA_COVER_HEIGHT: f32 = 120.0;
const MEDIA_HEIGHT: f32 = 216.0;
const MEDIA_HEIGHT_ACTIONS: f32 = 272.0;

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
  height: Pixels,
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
      height: px(0.),
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
    self.height = px(0.);
    self.sync(cx);
  }

  fn sync(&mut self, cx: &mut Context<Self>) {
    let active = self.notifications.read(cx).active().to_vec();

    if active.is_empty() {
      if let Some(handle) = self.window.take() {
        handle
          .update(cx, |_view, window, _cx| window.remove_window())
          .log_err();
      }
      self.height = px(0.);
      return;
    }

    let height = total_height(&active);

    if let Some(handle) = self.window {
      // Resize the existing surface in place rather than recreating it when the
      // stack grows or shrinks, matching the workspace OSD's approach.
      let needs_resize = self.height != height;
      let updated = handle
        .update(cx, |view, window, cx| {
          if needs_resize {
            window.resize(Size::new(px(WINDOW_WIDTH), height));
          }
          view.update_content(active.clone(), cx);
        })
        .is_ok();

      if updated {
        self.height = height;
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
        self.height = height;
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

/// Total surface height for the current stack: the sum of each notification's
/// chosen-layout height plus the gaps between cards.
fn total_height(active: &[Notification]) -> Pixels {
  let mut total = 0.0;
  for (index, notification) in active.iter().enumerate() {
    if index > 0 {
      total += CARD_GAP;
    }
    total += pick_layout(notification).height(notification);
  }
  px(total)
}

/// The visual layouts a notification can be rendered with, chosen per
/// notification by [`pick_layout`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum NotificationLayout {
  /// A single short row: icon, title over a one-line detail, inline actions.
  Compact,
  /// A messaging-style card: avatar, app name with timestamp, title, body, and
  /// optional action buttons.
  Message,
  /// A now-playing card: cover art on top, then label, title, subtitle, and
  /// optional action buttons.
  Media,
}

/// Layout used when no rule matches. Most notifications are simple, transient
/// toasts (e.g. `Claude Code`, `Ghostty`), which read best compact.
const DEFAULT_LAYOUT: NotificationLayout = NotificationLayout::Compact;

impl NotificationLayout {
  fn height(self, notification: &Notification) -> f32 {
    match self {
      NotificationLayout::Compact => COMPACT_HEIGHT,
      NotificationLayout::Message if has_actions(notification) => MESSAGE_HEIGHT_ACTIONS,
      NotificationLayout::Message => MESSAGE_HEIGHT,
      NotificationLayout::Media if has_actions(notification) => MEDIA_HEIGHT_ACTIONS,
      NotificationLayout::Media => MEDIA_HEIGHT,
    }
  }
}

/// A rule mapping a notification onto a layout. The pattern is tested against
/// both the app name and the summary; the rule applies if either matches. More
/// conditions can be added here later.
struct LayoutRule {
  pattern: Regex,
  layout: NotificationLayout,
}

impl LayoutRule {
  fn matches(&self, notification: &Notification) -> bool {
    self.pattern.is_match(&notification.app_name) || self.pattern.is_match(&notification.summary)
  }
}

static LAYOUT_RULES: LazyLock<Vec<LayoutRule>> = LazyLock::new(build_layout_rules);

fn build_layout_rules() -> Vec<LayoutRule> {
  [
    // Screenshot tools (the compositor, screenshot utilities) announce a saved
    // or captured shot — a small confirmation toast.
    (r"(?i)screenshot", NotificationLayout::Compact),
    // Chat apps get the messaging layout with sender, body, and reply actions.
    (
      r"(?i)\b(telegram|discord|slack|signal|whatsapp)\b",
      NotificationLayout::Message,
    ),
    // Manual test rules: a summary or app naming a layout selects it. Also the
    // only way to exercise the media layout until a real rule exists for it.
    (r"(?i)\bcompact\b", NotificationLayout::Compact),
    (r"(?i)\bmessage\b", NotificationLayout::Message),
    (r"(?i)\bmedia\b", NotificationLayout::Media),
  ]
  .into_iter()
  .filter_map(|(pattern, layout)| match Regex::new(pattern) {
    Ok(pattern) => Some(LayoutRule { pattern, layout }),
    Err(error) => {
      error!(?error, pattern, "invalid notification layout rule");
      None
    }
  })
  .collect()
}

fn pick_layout(notification: &Notification) -> NotificationLayout {
  LAYOUT_RULES
    .iter()
    .find(|rule| rule.matches(notification))
    .map(|rule| rule.layout)
    .unwrap_or(DEFAULT_LAYOUT)
}

fn has_actions(notification: &Notification) -> bool {
  notification.actions.iter().any(|action| action.key != "default")
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

  fn render_card(&self, notification: &Notification, cx: &mut Context<Self>) -> AnyElement {
    match pick_layout(notification) {
      NotificationLayout::Compact => self.render_compact(notification, cx),
      NotificationLayout::Message => self.render_message(notification, cx),
      NotificationLayout::Media => self.render_media(notification, cx),
    }
  }

  /// The shared card chrome: fixed size, background, urgency border, and the
  /// whole-card click / right-click-to-dismiss handlers. Each layout fills it
  /// with its own content.
  fn card_base(
    &self,
    notification: &Notification,
    height: f32,
    cx: &mut Context<Self>,
  ) -> Stateful<Div> {
    let id = notification.id;
    let has_default = notification.actions.iter().any(|action| action.key == "default");
    let border_color = match notification.urgency {
      Urgency::Critical => rgba(0xE0524Fcc),
      _ => rgba(0xFFFFFF15),
    };

    div()
      .id(SharedString::from(format!("notif-{id}")))
      .h(px(height))
      .w_full()
      .overflow_hidden()
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
          this
            .notifications
            .update(cx, |notifications, cx| notifications.dismiss(id, CloseReason::Dismissed, cx));
        }),
      )
  }

  fn action_button(
    &self,
    notification_id: u32,
    index: usize,
    action: &NotificationAction,
    variant: ButtonVariant,
    cx: &mut Context<Self>,
  ) -> Stateful<Div> {
    let key = action.key.clone();
    let element = div()
      .id(SharedString::from(format!("notif-{notification_id}-action-{index}")))
      .cursor_pointer();

    let element = match variant {
      ButtonVariant::Compact => element
        .flex_none()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .bg(rgba(0xFFFFFF12))
        .text_color(rgba(0xFFFFFFCC))
        .hover(|style| style.bg(rgba(0xFFFFFF22))),
      ButtonVariant::Secondary => element
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.))
        .rounded_lg()
        .text_sm()
        .bg(rgba(0xFFFFFF12))
        .text_color(rgba(0xFFFFFFCC))
        .hover(|style| style.bg(rgba(0xFFFFFF22))),
      ButtonVariant::Primary => element
        .flex_1()
        .flex()
        .items_center()
        .justify_center()
        .h(px(36.))
        .rounded_lg()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .bg(rgb(0xF0F0F0))
        .text_color(rgb(0x1A1A1A))
        .hover(|style| style.bg(rgb(0xFFFFFF))),
    };

    element.child(action.label.clone()).on_click(cx.listener(
      move |this, _: &ClickEvent, _window, cx| {
        let key = key.clone();
        this
          .notifications
          .update(cx, |notifications, cx| notifications.invoke_action(notification_id, key, cx));
        cx.stop_propagation();
      },
    ))
  }

  /// Small, label-width buttons for the compact layout.
  fn compact_buttons(
    &self,
    notification: &Notification,
    cx: &mut Context<Self>,
  ) -> Vec<Stateful<Div>> {
    notification
      .actions
      .iter()
      .filter(|action| action.key != "default")
      .enumerate()
      .map(|(index, action)| {
        self.action_button(notification.id, index, action, ButtonVariant::Compact, cx)
      })
      .collect()
  }

  /// Full-width buttons that split the row, with the last action emphasized as
  /// the primary affordance. Used by the message and media layouts.
  fn prominent_buttons(
    &self,
    notification: &Notification,
    cx: &mut Context<Self>,
  ) -> Vec<Stateful<Div>> {
    let actions: Vec<&NotificationAction> = notification
      .actions
      .iter()
      .filter(|action| action.key != "default")
      .collect();
    let last = actions.len().saturating_sub(1);

    actions
      .iter()
      .enumerate()
      .map(|(index, action)| {
        let variant = if index == last {
          ButtonVariant::Primary
        } else {
          ButtonVariant::Secondary
        };
        self.action_button(notification.id, index, action, variant, cx)
      })
      .collect()
  }

  fn render_compact(&self, notification: &Notification, cx: &mut Context<Self>) -> AnyElement {
    let buttons = self.compact_buttons(notification, cx);

    self
      .card_base(notification, COMPACT_HEIGHT, cx)
      .child(
        h_flex()
          .size_full()
          .items_center()
          .gap_3()
          .px_3()
          .child(render_avatar(notification, COMPACT_ICON_SIZE))
          .child(
            v_flex()
              .flex_1()
              .min_w(px(0.))
              .gap(px(1.))
              .child(
                div()
                  .text_sm()
                  .font_weight(FontWeight::BOLD)
                  .text_color(rgb(0xEEEEEE))
                  .truncate()
                  .child(notification.summary.clone()),
              )
              .when(!notification.body.is_empty(), |this| {
                this.child(
                  div()
                    .text_xs()
                    .text_color(rgba(0xFFFFFF99))
                    .truncate()
                    .child(notification.body.clone()),
                )
              }),
          )
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().flex_none().gap_2().children(buttons))
          }),
      )
      .into_any_element()
  }

  fn render_message(&self, notification: &Notification, cx: &mut Context<Self>) -> AnyElement {
    let height = NotificationLayout::Message.height(notification);
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, height, cx)
      .child(
        v_flex()
          .size_full()
          .gap_3()
          .p_3()
          .when(!buttons.is_empty(), |this| this.justify_between())
          .child(
            h_flex()
              .w_full()
              .items_start()
              .gap_3()
              .child(render_avatar(notification, MESSAGE_ICON_SIZE))
              .child(
                v_flex()
                  .flex_1()
                  .min_w(px(0.))
                  .gap(px(2.))
                  .child(
                    h_flex()
                      .w_full()
                      .items_baseline()
                      .gap_2()
                      .child(
                        div()
                          .flex_1()
                          .min_w(px(0.))
                          .text_xs()
                          .text_color(rgba(0xFFFFFF80))
                          .truncate()
                          .child(notification.app_name.clone()),
                      )
                      .child(
                        div()
                          .flex_none()
                          .text_xs()
                          .text_color(rgba(0xFFFFFF55))
                          .child(relative_time(notification.received)),
                      ),
                  )
                  .child(
                    div()
                      .text_base()
                      .font_weight(FontWeight::BOLD)
                      .text_color(rgb(0xF0F0F0))
                      .truncate()
                      .child(notification.summary.clone()),
                  )
                  .when(!notification.body.is_empty(), |this| {
                    this.child(
                      div()
                        .text_sm()
                        .text_color(rgba(0xFFFFFFAA))
                        .line_height(rems(1.3))
                        .line_clamp(2)
                        .child(notification.body.clone()),
                    )
                  }),
              ),
          )
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().w_full().gap_2().children(buttons))
          }),
      )
      .into_any_element()
  }

  fn render_media(&self, notification: &Notification, cx: &mut Context<Self>) -> AnyElement {
    let height = NotificationLayout::Media.height(notification);
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, height, cx)
      .child(
        v_flex()
          .size_full()
          .child(render_cover(notification))
          .child(
            v_flex()
              .flex_1()
              .w_full()
              .justify_between()
              .p_3()
              .child(
                v_flex()
                  .gap(px(2.))
                  .when(!notification.app_name.is_empty(), |this| {
                    this.child(
                      div()
                        .text_xs()
                        .text_color(rgba(0xFFFFFF80))
                        .truncate()
                        .child(notification.app_name.clone()),
                    )
                  })
                  .child(
                    div()
                      .text_base()
                      .font_weight(FontWeight::BOLD)
                      .text_color(rgb(0xF0F0F0))
                      .truncate()
                      .child(notification.summary.clone()),
                  )
                  .when(!notification.body.is_empty(), |this| {
                    this.child(
                      div()
                        .text_sm()
                        .text_color(rgba(0xFFFFFFAA))
                        .truncate()
                        .child(notification.body.clone()),
                    )
                  }),
              )
              .when(!buttons.is_empty(), |this| {
                this.child(h_flex().w_full().gap_2().children(buttons))
              }),
          ),
      )
      .into_any_element()
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

/// Visual emphasis for an action button.
#[derive(Clone, Copy)]
enum ButtonVariant {
  /// Small and label-width, low emphasis (compact layout).
  Compact,
  /// Full-width, low emphasis.
  Secondary,
  /// Full-width, light fill — the primary affordance.
  Primary,
}

/// A rounded square holding the notification's image or icon, falling back to a
/// letter derived from the app or title.
fn render_avatar(notification: &Notification, size: f32) -> AnyElement {
  // A subtle light hairline so a dark icon or image doesn't blend into the
  // equally dark card background.
  let frame = div()
    .flex_none()
    .size(px(size))
    .rounded_lg()
    .overflow_hidden()
    .border_1()
    .border_color(rgba(0xFFFFFF2E));

  if let Some(image) = &notification.image {
    return frame
      .child(img(ImageSource::Render(image.clone())).size_full().object_fit(ObjectFit::Cover))
      .into_any_element();
  }

  if let Some(path) = &notification.icon_path {
    let resource = Resource::Path(PathBuf::from(path.to_string()).into());
    return frame
      .child(img(ImageSource::Resource(resource)).size_full().object_fit(ObjectFit::Cover))
      .into_any_element();
  }

  let frame = frame.flex().items_center().justify_center().bg(rgba(0xFFFFFF14));
  let letter = avatar_letter(notification);

  if letter.is_empty() {
    frame
      .child(Icon::new(IconName::Bell).size(px(size * 0.45)).text_color(rgba(0xFFFFFF88)))
      .into_any_element()
  } else {
    frame
      .child(
        div()
          .text_size(px(size * 0.42))
          .font_weight(FontWeight::SEMIBOLD)
          .text_color(rgb(0xDDDDDD))
          .child(letter),
      )
      .into_any_element()
  }
}

fn avatar_letter(notification: &Notification) -> SharedString {
  notification
    .app_name
    .chars()
    .next()
    .or_else(|| notification.summary.chars().next())
    .map(|first| SharedString::from(first.to_uppercase().to_string()))
    .unwrap_or_default()
}

/// The full-width cover art for the media layout, falling back to a tinted
/// placeholder when the notification carries no image.
fn render_cover(notification: &Notification) -> AnyElement {
  let frame = div().flex_none().w_full().h(px(MEDIA_COVER_HEIGHT)).overflow_hidden();

  if let Some(image) = &notification.image {
    return frame
      .child(img(ImageSource::Render(image.clone())).size_full().object_fit(ObjectFit::Cover))
      .into_any_element();
  }

  if let Some(path) = &notification.icon_path {
    let resource = Resource::Path(PathBuf::from(path.to_string()).into());
    return frame
      .child(img(ImageSource::Resource(resource)).size_full().object_fit(ObjectFit::Cover))
      .into_any_element();
  }

  frame
    .flex()
    .items_center()
    .justify_center()
    .bg(rgba(0xFFFFFF0A))
    .child(Icon::new(IconName::Photo).size(px(28.)).text_color(rgba(0xFFFFFF55)))
    .into_any_element()
}

/// A short relative time like `now`, `5m`, `2h`, `1d`.
fn relative_time(received: Instant) -> SharedString {
  let seconds = received.elapsed().as_secs();
  if seconds < 60 {
    SharedString::from("now")
  } else if seconds < 3600 {
    SharedString::from(format!("{}m", seconds / 60))
  } else if seconds < 86_400 {
    SharedString::from(format!("{}h", seconds / 3600))
  } else {
    SharedString::from(format!("{}d", seconds / 86_400))
  }
}
