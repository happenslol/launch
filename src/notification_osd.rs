use std::{
  cell::Cell,
  collections::{HashMap, HashSet},
  path::PathBuf,
  rc::Rc,
  sync::LazyLock,
  time::{Duration, Instant},
};

use gpui::{
  Animation, AnimationExt, AnyElement, App, Bounds, ClickEvent, Context, DisplayId, Div, ElementId,
  Entity, FontWeight, Global, ImageSource, IntoElement, MouseButton, ObjectFit, Pixels, Render,
  Resource, SharedString, Size, Stateful, Styled, Subscription, Task, WeakEntity, Window,
  WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img,
  point, prelude::*, px, rems, rgb, rgba,
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

// Per-layout card heights (with a taller variant for those that grow to fit
// action buttons). The compact and media layouts are fixed at these heights. The
// message layout instead treats its height as a minimum and grows with its
// wrapping body (up to a three-line clamp). The surface opens at an upper-bound
// estimate (see [`total_height`] / [`NotificationLayout::open_height`]) and the
// measured content height trims it down — see [`NotificationsView::render`] and
// [`NotificationOsd::report_content_height`].
const COMPACT_HEIGHT: f32 = 64.0;
const COMPACT_ICON_SIZE: f32 = 36.0;

const MESSAGE_HEIGHT: f32 = 110.0;
const MESSAGE_HEIGHT_ACTIONS: f32 = 150.0;
const MESSAGE_ICON_SIZE: f32 = 44.0;
// Upper bounds for the message layout (body clamped to three lines, optionally
// with action buttons), used only to size the surface when it first opens. The
// surface opens at this overestimate and the measured height trims it back down,
// so it never has to grow while a card is visible — a grow reads as the card
// "unfolding". These must stay >= any real message height. See
// [`NotificationLayout::open_height`].
const MESSAGE_HEIGHT_MAX: f32 = 170.0;
const MESSAGE_HEIGHT_MAX_ACTIONS: f32 = 220.0;

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

/// Per-card enter/exit animation: a fade paired with a subtle horizontal slide
/// along the right edge (in from the right, back out to the right). A leaving
/// card is kept in the stack and
/// rendered with the exit animation until [`ANIM_EXIT_DURATION`] elapses, then
/// removed — see [`NotificationsView::update_content`].
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(150);
/// Extra time before a leaving card is dropped from the stack, so the exit
/// animation fully finishes (it only starts on the next render) before the slot
/// collapses, avoiding a visible pop.
const ANIM_EXIT_GRACE: Duration = Duration::from_millis(60);
const SLIDE_OFFSET: f32 = 12.0;

pub fn init(cx: &mut App) {
  let notifications = Notifications::global(cx);
  let osd = cx.new(|cx| NotificationOsd::new(notifications, cx));
  cx.set_global(GlobalNotificationOsd(osd));
}

struct GlobalNotificationOsd(#[allow(dead_code)] Entity<NotificationOsd>);

impl Global for GlobalNotificationOsd {}

/// Mirrors the live notification list onto a single layer-shell surface anchored
/// to the top-right of the first display, resizing it to fit the current stack.
///
/// The window opens at an estimated height (see [`total_height`]); the rendered
/// view then measures its real content and reports it back via
/// [`NotificationOsd::report_content_height`], which is the authority for the
/// surface's final size. This lets layouts with wrapping text (the message
/// layout's body) grow to fit instead of being clipped.
struct NotificationOsd {
  notifications: Entity<Notifications>,
  window: Option<WindowHandle<NotificationsView>>,
  height: Pixels,
  target_height: Option<Pixels>,
  sync_task: Option<Task<()>>,
  resize_task: Option<Task<()>>,
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
      target_height: None,
      sync_task: None,
      resize_task: None,
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
    self.target_height = None;
    self.resize_task = None;
    self.sync(cx);
  }

  fn sync(&mut self, cx: &mut Context<Self>) {
    let active = self.notifications.read(cx).active().to_vec();

    if let Some(handle) = self.window {
      // Update the existing surface's content in place rather than recreating
      // it when the stack changes. The view animates cards in and out and
      // reports its measured height back, which drives the resize — and, once
      // the last card has finished leaving, the close (a reported height of 0).
      // See [`Self::report_content_height`] and
      // [`NotificationsView::update_content`].
      let updated = handle
        .update(cx, |view, _window, cx| view.update_content(active.clone(), cx))
        .is_ok();

      if updated {
        return;
      }

      // The window was closed since we last opened it; reopen below.
      self.window = None;
    }

    // No surface yet: nothing to show until a notification arrives.
    if active.is_empty() {
      self.height = px(0.);
      self.target_height = None;
      self.resize_task = None;
      return;
    }

    let Some(display_id) = target_display(cx) else {
      error!("no display available to show notifications on");
      return;
    };

    // Open at an estimated height so the surface starts near its final size; the
    // view corrects it once it has measured the rendered content.
    let height = total_height(&active);
    let osd = cx.weak_entity();

    match cx.open_window(window_options(display_id, height), {
      let notifications = self.notifications.clone();
      move |_window, cx| cx.new(|_cx| NotificationsView::new(notifications, active, osd))
    }) {
      Ok(handle) => {
        self.window = Some(handle);
        self.height = height;
        self.target_height = None;
      }
      Err(error) => error!(?error, "Failed to open notifications window"),
    }
  }

  /// Resizes the layer-shell surface to fit the height the view measured for its
  /// rendered content, or closes it when the view has emptied (a reported height
  /// of 0, once the last card has animated out). Called from the view's prepaint
  /// (deferred, so never run mid-paint).
  ///
  /// Grows apply immediately and shrinks are debounced. A grow has to land before
  /// the card it makes room for becomes visible, or the card appears to unfold;
  /// the triggering card has only just been rendered (its enter animation is at
  /// ~zero opacity), so resizing now is hidden. The surface opens at an
  /// overestimate (see [`total_height`]), so growing only happens for later
  /// additions, never right after open — keeping resizes off the initial
  /// configure handshake. Shrinks just trim transparent space off the bottom, so
  /// they can wait and coalesce.
  fn report_content_height(&mut self, height: Pixels, cx: &mut Context<Self>) {
    // Sub-pixel measurement noise would otherwise cause endless tiny resizes.
    let height = height.round();

    if height > self.height {
      let Some(handle) = self.window else {
        return;
      };
      // Drop any pending shrink; this grow supersedes it.
      self.target_height = None;
      self.resize_task = None;

      let resized = handle
        .update(cx, |_view, window, _cx| {
          window.resize(Size::new(px(WINDOW_WIDTH), height));
        })
        .is_ok();

      if resized {
        self.height = height;
      } else {
        self.window = None;
      }
      return;
    }

    if height == self.height && self.target_height.is_none() {
      // The surface already matches the rendered content; nothing to do.
      return;
    }

    if self.target_height == Some(height) {
      // This change is already scheduled; don't reset the timer, or a steady
      // stream of identical reports would keep deferring it forever.
      return;
    }

    self.target_height = Some(height);
    self.resize_task = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(SYNC_DEBOUNCE).await;
      this.update(cx, |this, cx| this.apply_content_height(cx)).log_err();
    }));
  }

  fn apply_content_height(&mut self, cx: &mut Context<Self>) {
    let Some(height) = self.target_height.take() else {
      return;
    };
    let Some(handle) = self.window else {
      return;
    };

    // A height of 0 means the view has no cards left (the last one finished
    // leaving): tear the surface down rather than resizing it to nothing.
    if height <= px(0.) {
      handle
        .update(cx, |_view, window, _cx| window.remove_window())
        .log_err();
      self.window = None;
      self.height = px(0.);
      return;
    }

    let resized = handle
      .update(cx, |_view, window, _cx| {
        window.resize(Size::new(px(WINDOW_WIDTH), height));
      })
      .is_ok();

    if resized {
      self.height = height;
    } else {
      self.window = None;
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

/// Height the surface is opened at: an upper bound on each notification's
/// rendered height plus the gaps between cards. Deliberately an overestimate so
/// the measured height only ever trims it (see
/// [`NotificationOsd::report_content_height`]).
fn total_height(active: &[Notification]) -> Pixels {
  let mut total = 0.0;
  for (index, notification) in active.iter().enumerate() {
    if index > 0 {
      total += CARD_GAP;
    }
    total += pick_layout(notification).open_height(notification);
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
  /// The rendered height (fixed layouts) or minimum height (the message layout,
  /// which grows with its body). Used to size the card itself.
  fn height(self, notification: &Notification) -> f32 {
    match self {
      NotificationLayout::Compact => COMPACT_HEIGHT,
      NotificationLayout::Message if has_actions(notification) => MESSAGE_HEIGHT_ACTIONS,
      NotificationLayout::Message => MESSAGE_HEIGHT,
      NotificationLayout::Media if has_actions(notification) => MEDIA_HEIGHT_ACTIONS,
      NotificationLayout::Media => MEDIA_HEIGHT,
    }
  }

  /// An upper bound on the rendered height, used to size the surface on open. For
  /// the fixed layouts this equals [`Self::height`]; the message layout reserves
  /// room for the tallest case (a three-line body, plus actions) so a tall
  /// message does not have to grow the surface after it is already on screen.
  fn open_height(self, notification: &Notification) -> f32 {
    match self {
      NotificationLayout::Message if has_actions(notification) => MESSAGE_HEIGHT_MAX_ACTIONS,
      NotificationLayout::Message => MESSAGE_HEIGHT_MAX,
      _ => self.height(notification),
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

/// A notification as the view tracks it, including ones that have been dismissed
/// but are still animating out (`leaving`).
struct CardState {
  notification: Notification,
  leaving: bool,
}

struct NotificationsView {
  notifications: Entity<Notifications>,
  cards: Vec<CardState>,
  /// Pending removals for leaving cards, keyed by id so a re-notified card can
  /// cancel its own removal and a finished one drops its task.
  removal_tasks: HashMap<u32, Task<()>>,
  /// The last content height reported to the OSD, shared with the prepaint
  /// listener. Enter/exit animations redraw every frame but only change opacity
  /// and horizontal offset, not height, so this lets the listener skip the
  /// resize round-trip on the frames where the measured height is unchanged.
  reported_height: Rc<Cell<Pixels>>,
  osd: WeakEntity<NotificationOsd>,
}

impl NotificationsView {
  fn new(
    notifications: Entity<Notifications>,
    items: Vec<Notification>,
    osd: WeakEntity<NotificationOsd>,
  ) -> Self {
    let cards = items
      .into_iter()
      .map(|notification| CardState {
        notification,
        leaving: false,
      })
      .collect();

    Self {
      notifications,
      cards,
      removal_tasks: HashMap::new(),
      // A sentinel that no real measurement equals, so the first report fires.
      reported_height: Rc::new(Cell::new(Pixels::MIN)),
      osd,
    }
  }

  /// Reconciles the displayed cards against the live set: refreshes those still
  /// present, starts the exit animation for those that vanished (keeping them in
  /// the stack until [`ANIM_EXIT_DURATION`] passes), and inserts new arrivals at
  /// the top, mirroring `Notifications`' newest-first ordering.
  fn update_content(&mut self, active: Vec<Notification>, cx: &mut Context<Self>) {
    let active_ids: HashSet<u32> = active.iter().map(|notification| notification.id).collect();

    for notification in &active {
      if let Some(card) = self.cards.iter_mut().find(|card| card.notification.id == notification.id)
      {
        card.notification = notification.clone();
        if card.leaving {
          // The notification came back before its exit finished; cancel removal.
          card.leaving = false;
          self.removal_tasks.remove(&notification.id);
        }
      }
    }

    for card in self.cards.iter_mut() {
      if active_ids.contains(&card.notification.id) || card.leaving {
        continue;
      }
      card.leaving = true;
      let id = card.notification.id;
      self.removal_tasks.insert(
        id,
        cx.spawn(async move |this, cx| {
          cx.background_executor().timer(ANIM_EXIT_DURATION + ANIM_EXIT_GRACE).await;
          this.update(cx, |this, cx| this.remove_card(id, cx)).log_err();
        }),
      );
    }

    let known: HashSet<u32> = self.cards.iter().map(|card| card.notification.id).collect();
    for notification in active.iter().rev() {
      if !known.contains(&notification.id) {
        self.cards.insert(
          0,
          CardState {
            notification: notification.clone(),
            leaving: false,
          },
        );
      }
    }

    cx.notify();
  }

  /// Drops a leaving card once its exit animation has finished, collapsing the
  /// stack. The view re-measures on the next render and the surface follows.
  fn remove_card(&mut self, id: u32, cx: &mut Context<Self>) {
    self.cards.retain(|card| card.notification.id != id);
    self.removal_tasks.remove(&id);
    cx.notify();
  }

  fn render_card(&self, card: &CardState, cx: &mut Context<Self>) -> AnyElement {
    let notification = &card.notification;
    let element = match pick_layout(notification) {
      NotificationLayout::Compact => self.render_compact(notification, cx),
      NotificationLayout::Message => self.render_message(notification, cx),
      NotificationLayout::Media => self.render_media(notification, cx),
    };

    // Fade + slide along the right edge: in from the right on appear, back out
    // to the right on dismiss. Re-keying the animation by `leaving` restarts it
    // with the exit parameters when the card is dismissed. The slide uses
    // relative positioning so it doesn't shift the card's measured height.
    let leaving = card.leaving;
    element
      .relative()
      .with_animation(
        ElementId::NamedInteger(format!("notif-anim-{}", notification.id).into(), leaving as u64),
        Animation::new(if leaving {
          ANIM_EXIT_DURATION
        } else {
          ANIM_ENTER_DURATION
        })
        .with_easing(|delta| 1.0 - (1.0 - delta).powi(3)),
        move |this, delta| {
          if leaving {
            this.opacity(1.0 - delta).left(px(SLIDE_OFFSET * delta))
          } else {
            this.opacity(delta).left(px(SLIDE_OFFSET * (1.0 - delta)))
          }
        },
      )
      .into_any_element()
  }

  /// The shared card chrome: background, urgency border, and the whole-card
  /// click / right-click-to-dismiss handlers. Each layout sizes the card (fixed
  /// or content-driven) and fills it with its own content. `flex_none` keeps the
  /// card at its natural height in the stack so its bounds can be measured to
  /// size the surface, rather than being shrunk to fit the current window.
  fn card_base(&self, notification: &Notification, cx: &mut Context<Self>) -> Stateful<Div> {
    let id = notification.id;
    let has_default = notification.actions.iter().any(|action| action.key == "default");
    let border_color = match notification.urgency {
      Urgency::Critical => rgba(0xE0524Fcc),
      _ => rgba(0xFFFFFF15),
    };

    div()
      .id(SharedString::from(format!("notif-{id}")))
      .flex_none()
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
      .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
        this
          .notifications
          .update(cx, |notifications, cx| notifications.set_hovered(id, *hovered, cx));
      }))
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

  fn render_compact(&self, notification: &Notification, cx: &mut Context<Self>) -> Stateful<Div> {
    let buttons = self.compact_buttons(notification, cx);

    self
      .card_base(notification, cx)
      .h(px(COMPACT_HEIGHT))
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
  }

  fn render_message(&self, notification: &Notification, cx: &mut Context<Self>) -> Stateful<Div> {
    // The message card grows with its (wrapping) body. The per-layout height is
    // only a floor so short messages still read as a comfortable card.
    let min_height = NotificationLayout::Message.height(notification);
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, cx)
      .min_h(px(min_height))
      .child(
        v_flex()
          .w_full()
          .gap_3()
          .p_3()
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
                        // Grow with the body, but cap it: past ~3 lines a toast
                        // is better truncated than turned into a wall of text.
                        .line_clamp(3)
                        .child(notification.body.clone()),
                    )
                  }),
              ),
          )
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().w_full().gap_2().children(buttons))
          }),
      )
  }

  fn render_media(&self, notification: &Notification, cx: &mut Context<Self>) -> Stateful<Div> {
    let height = NotificationLayout::Media.height(notification);
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, cx)
      .h(px(height))
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
  }
}

impl Render for NotificationsView {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let mut cards = Vec::with_capacity(self.cards.len());
    for card in &self.cards {
      cards.push(self.render_card(card, cx));
    }

    let osd = self.osd.clone();
    let reported_height = self.reported_height.clone();

    v_flex()
      .size_full()
      .gap(px(CARD_GAP))
      .children(cards)
      // Measure the laid-out card stack and report its real height so the
      // surface can be resized to fit content that grows past its estimate
      // (e.g. a message body wrapping onto several lines). An empty stack reports
      // 0, which tells the OSD to tear the surface down once the last card has
      // finished leaving.
      //
      // This runs on every prepaint, including each frame of an enter/exit
      // animation — but those only change opacity and horizontal offset, not
      // height, so we skip the work whenever the measured height is unchanged.
      // When it has changed, the report is deferred to the next effect cycle:
      // the prepaint can run synchronously inside an `NotificationOsd` update
      // (the open/resize that triggered it), and deferring avoids re-entering it.
      .on_children_prepainted(move |bounds, _window, cx| {
        let height = stack_height(&bounds).unwrap_or(Pixels::ZERO).round();
        if height == reported_height.get() {
          return;
        }
        reported_height.set(height);

        let osd = osd.clone();
        cx.defer(move |cx| {
          osd
            .update(cx, |osd, cx| osd.report_content_height(height, cx))
            .log_err();
        });
      })
  }
}

/// The total height spanned by the laid-out card bounds, measured from the top
/// of the first card to the bottom of the last. The cards are `flex_none`, so
/// these bounds reflect each card's natural content height even when the current
/// surface is too small to contain them. Returns `None` when there are no cards.
fn stack_height(bounds: &[Bounds<Pixels>]) -> Option<Pixels> {
  if bounds.is_empty() {
    return None;
  }

  let mut top = f32::MAX;
  let mut bottom = f32::MIN;
  for card in bounds {
    top = top.min(card.top().as_f32());
    bottom = bottom.max(card.bottom().as_f32());
  }

  Some(px(bottom - top))
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
