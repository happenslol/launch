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
  Entity, FontStyle, FontWeight, Global, HighlightStyle, ImageSource, IntoElement, MouseButton,
  ObjectFit, Pixels, Render, Resource, SharedString, Size, Stateful, Styled, StyledText,
  Subscription, Task, UnderlineStyle, WeakEntity, Window,
  WindowBackgroundAppearance, WindowBounds, WindowHandle, WindowKind, WindowOptions, div, img,
  point, prelude::*, px, rems, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use regex::Regex;
use tracing::{error, warn};

use crate::{
  config::ConfigState,
  dbus::notifications::{
    Body, BodyBuilder, CloseReason, Emphasis, Notification, NotificationAction, NotificationEvent,
    Notifications, Urgency,
  },
  icon::{Icon, IconName},
  util::{ResultExt, h_flex, v_flex},
};

const WINDOW_WIDTH: f32 = 400.0;
const CARD_GAP: f32 = 10.0;
const MARGIN: f32 = 12.0;

// Cards size themselves to their content with even padding; none have a fixed
// height. The surface is full-height regardless, and the rendered stack reports
// the extent its cards occupy. Only the avatar/cover dimensions are fixed.
const COMPACT_ICON_SIZE: f32 = 36.0;

const MESSAGE_ICON_SIZE: f32 = 44.0;

const MEDIA_COVER_HEIGHT: f32 = 120.0;

/// The family the cards draw in. It is embedded with regular, bold, italic and
/// bold-italic faces, which is what lets body markup resolve to real ones.
const CARD_FONT: &str = "Noto Sans";

/// The most lines a wrapping body is allowed to occupy before it is clamped and
/// ellipsized. Past a few lines a toast is better truncated than turned into a
/// wall of text.
const BODY_MAX_LINES: usize = 3;

/// A hard ceiling on a card's height. Every layout already bounds its own
/// content — the text is flattened onto single lines and the wrapping body is
/// clamped to [`BODY_MAX_LINES`] — so this never clips in practice; it is a
/// backstop that keeps a card from taking over the screen if some future layout
/// or an unforeseen input escapes those limits.
const MAX_CARD_HEIGHT: f32 = 280.0;

/// A burst of notifications (e.g. several `notify-send`s at once) produces a
/// rapid sequence of change events. Coalescing them into a single `sync` avoids
/// redundant content updates while the burst is still arriving.
const SYNC_DEBOUNCE: Duration = Duration::from_millis(50);

/// Per-card enter/exit animation: a fade paired with a subtle horizontal slide
/// along the right edge (in from the right, back out to the right). A leaving
/// card is kept in the stack and rendered with the exit animation until
/// [`ANIM_EXIT_DURATION`] elapses, then removed — see
/// [`NotificationsView::update_content`].
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(150);
/// Extra time before a leaving card is dropped from the stack, so the exit
/// animation fully finishes (it only starts on the next render) before the card
/// is removed, avoiding a visible pop.
const ANIM_EXIT_GRACE: Duration = Duration::from_millis(60);
const SLIDE_OFFSET: f32 = 12.0;

pub fn init(cx: &mut App) {
  let notifications = Notifications::global(cx);
  let osd = cx.new(|cx| NotificationOsd::new(notifications, cx));
  cx.set_global(GlobalNotificationOsd(osd));
}

struct GlobalNotificationOsd(#[allow(dead_code)] Entity<NotificationOsd>);

impl Global for GlobalNotificationOsd {}

/// Mirrors the live notification list onto a single layer-shell surface that
/// spans the full height of the display, anchored to the top-right.
///
/// The surface is never resized: it is full-height from the moment it opens, so
/// notifications size themselves and the stack reflows without the compositor
/// ever scaling a stale frame. Instead the rendered view reports the extent of
/// its cards via [`NotificationOsd::report_region`], which sets the surface's
/// input region to just that area so clicks pass through the transparent
/// remainder. An empty report closes the surface.
struct NotificationOsd {
  notifications: Entity<Notifications>,
  window: Option<WindowHandle<NotificationsView>>,
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
      sync_task: None,
      _subscriptions: subscriptions,
    };

    this.sync(cx);
    this
  }

  /// Coalesces rapid notification changes into a single `sync`. Each call
  /// replaces the pending task, so a burst of changes only triggers one content
  /// update once the burst has settled. See [`SYNC_DEBOUNCE`].
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
    self.sync(cx);
  }

  fn sync(&mut self, cx: &mut Context<Self>) {
    let active = self.notifications.read(cx).active().to_vec();

    if let Some(handle) = self.window {
      // Update the existing surface's content in place. The view animates cards
      // in and out within the fixed-size surface and reports its card extent
      // back — see [`Self::report_region`] and
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
      return;
    }

    let Some((display_id, display_height)) = target_display(cx) else {
      error!("no display available to show notifications on");
      return;
    };

    let osd = cx.weak_entity();

    match cx.open_window(window_options(display_id, display_height), {
      let notifications = self.notifications.clone();
      move |_window, cx| cx.new(|_cx| NotificationsView::new(notifications, active, osd))
    }) {
      Ok(handle) => self.window = Some(handle),
      Err(error) => error!(?error, "Failed to open notifications window"),
    }
  }

  /// Sets the surface's input region to the area its cards occupy (top-left of
  /// the surface down to `height`), so clicks land on the cards while the
  /// transparent remainder passes through. `None` means the stack is empty — the
  /// last card has finished leaving — so the surface is torn down. Called from
  /// the view's prepaint, deferred so it never runs mid-paint.
  fn report_region(&mut self, height: Option<Pixels>, cx: &mut Context<Self>) {
    let Some(handle) = self.window else {
      return;
    };

    match height {
      None => {
        handle
          .update(cx, |_view, window, _cx| window.remove_window())
          .log_err();
        self.window = None;
      }
      Some(height) => {
        let region = Bounds {
          origin: point(px(0.), px(0.)),
          size: Size::new(px(WINDOW_WIDTH), height.max(px(0.))),
        };
        // Drop a stale handle if the surface is gone, so the next `sync` reopens.
        if handle
          .update(cx, |_view, window, _cx| window.set_input_region(region))
          .is_err()
        {
          self.window = None;
        }
      }
    }
  }
}

/// Resolves the display to show notifications on, returning its id and height.
/// Uses the output the notification section names, else the primary display, and
/// falls back to the first display when neither is set or attached.
fn target_display(cx: &App) -> Option<(DisplayId, Pixels)> {
  let config = ConfigState::get(cx);
  let configured = config.notifications.display.or(config.primary_display);
  let displays = cx.displays();

  if let Some(name) = configured {
    if let Some(display) = displays.iter().find(|display| display.name() == Some(name.as_str())) {
      return Some((display.id(), display.bounds().size.height));
    }

    warn!(%name, "Configured notification display not found, using first display");
  }

  displays
    .into_iter()
    .next()
    .map(|display| (display.id(), display.bounds().size.height))
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
/// but are still playing their exit animation.
struct CardState {
  notification: Notification,
  /// Resolved once when the card is created or refreshed, rather than re-running
  /// the regex layout rules on every render frame.
  layout: NotificationLayout,
  /// The notification's display text, flattened alongside `layout` so the render
  /// path — which also runs for every frame of the enter/exit animation — never
  /// re-walks the strings.
  text: CardText,
  /// True once dismissed: the card stays in the stack, playing its fade/slide-out
  /// animation, until [`NotificationsView::remove_card`] drops it.
  leaving: bool,
}

impl CardState {
  fn new(notification: Notification) -> Self {
    Self {
      layout: pick_layout(&notification),
      text: CardText::new(&notification),
      notification,
      leaving: false,
    }
  }
}

/// A notification's text with every hard line break removed. See [`single_line`]
/// for why the cards never render the raw strings.
struct CardText {
  app_name: SharedString,
  summary: SharedString,
  body: Body,
}

impl CardText {
  fn new(notification: &Notification) -> Self {
    Self {
      app_name: single_line(&notification.app_name),
      summary: single_line(&notification.summary),
      body: single_line_body(&notification.body),
    }
  }
}

/// Flattens text onto one line, collapsing every run of whitespace into a single
/// space.
///
/// Notification summaries and bodies routinely arrive with hard line breaks in
/// them. GPUI shapes text by splitting it on `\n` first and only then applies
/// `truncate` and `line_clamp`, so each break yields a rendered line that neither
/// limit bounds — a twelve-line body renders twelve lines tall no matter what the
/// clamp says. Flattening up front is what makes those limits hold.
fn single_line(text: &SharedString) -> SharedString {
  // Cloning a `SharedString` is a refcount bump, so leave already-flat text —
  // the common case — alone rather than rebuilding it.
  if is_flat(text) {
    return text.clone();
  }

  SharedString::from(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// [`single_line`] for a body, carrying its emphasis across the collapse.
///
/// Spans are byte ranges into the text, so rather than remapping them the body
/// is rebuilt from the characters that survive, each carrying the emphasis it
/// had at its old offset. Collapsed whitespace takes the emphasis of the run it
/// stood for, which keeps an underline unbroken across the spaces inside it.
fn single_line_body(body: &Body) -> Body {
  if is_flat(&body.text) {
    return body.clone();
  }

  let mut builder = BodyBuilder::default();
  let mut spans = body.spans.iter().peekable();
  let mut pending_space: Option<Emphasis> = None;
  let mut buffer = [0u8; 4];

  for (offset, character) in body.text.char_indices() {
    while spans.next_if(|span| span.range.end <= offset).is_some() {}
    let emphasis = spans
      .peek()
      .filter(|span| span.range.start <= offset)
      .map_or_default(|span| span.emphasis);

    if character.is_whitespace() {
      // Held back rather than emitted, so a run collapses to one space and a
      // trailing run — like a leading one — produces nothing at all.
      if pending_space.is_none() && !builder.is_empty() {
        pending_space = Some(emphasis);
      }
      continue;
    }

    if let Some(emphasis) = pending_space.take() {
      builder.push(" ", emphasis);
    }
    builder.push(character.encode_utf8(&mut buffer), emphasis);
  }

  builder.build()
}

/// Whether text is already one line with no repeated or edge spaces, and so
/// survives [`single_line`] unchanged.
fn is_flat(text: &str) -> bool {
  !text.contains(|character: char| character.is_whitespace() && character != ' ')
    && !text.contains("  ")
    && !text.starts_with(' ')
    && !text.ends_with(' ')
}

/// The body as an element, styled where its markup asks for it. Plain bodies —
/// nearly all of them — skip [`StyledText`] and the run bookkeeping it brings.
fn render_body(body: &Body) -> AnyElement {
  if body.spans.is_empty() {
    return body.text.clone().into_any_element();
  }

  StyledText::new(body.text.clone())
    .with_highlights(
      body
        .spans
        .iter()
        .map(|span| (span.range.clone(), highlight(span.emphasis))),
    )
    .into_any_element()
}

/// Emphasis as a delta on whatever text style the card puts the body in, so the
/// size and colour each layout picks are left alone.
fn highlight(emphasis: Emphasis) -> HighlightStyle {
  HighlightStyle {
    font_weight: emphasis.bold.then_some(FontWeight::BOLD),
    font_style: emphasis.italic.then_some(FontStyle::Italic),
    underline: emphasis.underline.then(|| UnderlineStyle {
      thickness: px(1.),
      ..Default::default()
    }),
    ..Default::default()
  }
}

struct NotificationsView {
  notifications: Entity<Notifications>,
  cards: Vec<CardState>,
  /// Pending removals for leaving cards, keyed by id so a re-notified card can
  /// cancel its own removal and a finished one drops its task.
  removal_tasks: HashMap<u32, Task<()>>,
  /// The last input-region extent reported to the OSD, shared with the prepaint
  /// listener so it can skip the update on frames where the extent is unchanged.
  /// `None` means the stack was reported empty.
  reported_region: Rc<Cell<Option<Pixels>>>,
  osd: WeakEntity<NotificationOsd>,
}

impl NotificationsView {
  fn new(
    notifications: Entity<Notifications>,
    items: Vec<Notification>,
    osd: WeakEntity<NotificationOsd>,
  ) -> Self {
    let cards = items.into_iter().map(CardState::new).collect();

    Self {
      notifications,
      cards,
      removal_tasks: HashMap::new(),
      // A sentinel that no real report equals, so the first report fires.
      reported_region: Rc::new(Cell::new(Some(Pixels::MIN))),
      osd,
    }
  }

  /// Reconciles the displayed cards against the live set: refreshes those still
  /// present, starts the exit animation for those that vanished (keeping them in
  /// the stack until the animation finishes), and inserts new arrivals at the
  /// top, mirroring `Notifications`' newest-first ordering.
  fn update_content(&mut self, active: Vec<Notification>, cx: &mut Context<Self>) {
    let active_ids: HashSet<u32> = active.iter().map(|notification| notification.id).collect();

    for notification in &active {
      if let Some(card) = self.cards.iter_mut().find(|card| card.notification.id == notification.id)
      {
        card.layout = pick_layout(notification);
        card.text = CardText::new(notification);
        card.notification = notification.clone();
        if card.leaving {
          // The notification came back before its exit finished; cancel removal.
          card.leaving = false;
          self.removal_tasks.remove(&notification.id);
        }
      }
    }

    let mut leaving = Vec::new();
    for card in self.cards.iter_mut() {
      if active_ids.contains(&card.notification.id) || card.leaving {
        continue;
      }
      card.leaving = true;
      leaving.push(card.notification.id);
    }
    for id in leaving {
      self.arm_removal(id, cx);
    }

    let known: HashSet<u32> = self.cards.iter().map(|card| card.notification.id).collect();
    for notification in active.iter().rev() {
      if !known.contains(&notification.id) {
        self.cards.insert(0, CardState::new(notification.clone()));
      }
    }

    cx.notify();
  }

  /// Drops a leaving card once its exit animation has finished, collapsing the
  /// stack.
  fn arm_removal(&mut self, id: u32, cx: &mut Context<Self>) {
    self.removal_tasks.insert(
      id,
      cx.spawn(async move |this, cx| {
        cx.background_executor().timer(ANIM_EXIT_DURATION + ANIM_EXIT_GRACE).await;
        this.update(cx, |this, cx| this.remove_card(id, cx)).log_err();
      }),
    );
  }

  fn remove_card(&mut self, id: u32, cx: &mut Context<Self>) {
    self.cards.retain(|card| card.notification.id != id);
    self.removal_tasks.remove(&id);
    cx.notify();
  }

  fn render_card(&self, card: &CardState, cx: &mut Context<Self>) -> AnyElement {
    let notification = &card.notification;
    let element = match card.layout {
      NotificationLayout::Compact => self.render_compact(card, cx),
      NotificationLayout::Message => self.render_message(card, cx),
      NotificationLayout::Media => self.render_media(card, cx),
    };

    // Fade + slide along the right edge: in from the right on appear, back out
    // to the right on dismiss. Re-keying the animation by `leaving` restarts it
    // with the exit parameters when the card is dismissed. The slide uses
    // relative positioning so it doesn't shift the card's measured height.
    let id = notification.id;
    let leaving = card.leaving;
    element
      .relative()
      .with_animation(
        ElementId::NamedInteger(format!("notif-anim-{id}").into(), leaving as u64),
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
  /// click / right-click-to-dismiss handlers. Each layout fills the card with its
  /// own content, which sizes the card via even padding. `flex_none` keeps the
  /// card at its natural content height in the stack so its bounds can be
  /// measured to size the surface, rather than being shrunk to fit the window.
  fn card_base(&self, notification: &Notification, cx: &mut Context<Self>) -> Stateful<Div> {
    let id = notification.id;
    let has_default = notification.actions.iter().any(|action| action.key == "default");
    let border_color = match notification.urgency {
      Urgency::Critical => rgba(0xE0524FCC),
      _ => rgba(0xFFFFFF15),
    };

    div()
      .id(SharedString::from(format!("notif-{id}")))
      // Pinned rather than left to the default `.SystemUIFont`, which resolves
      // to a single face: a run asking for bold or italic gets the regular one
      // back, so body markup renders with no visible emphasis at all.
      .font_family(CARD_FONT)
      .flex_none()
      .w_full()
      .max_h(px(MAX_CARD_HEIGHT))
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

  fn render_compact(&self, card: &CardState, cx: &mut Context<Self>) -> Stateful<Div> {
    let notification = &card.notification;
    let buttons = self.compact_buttons(notification, cx);

    self
      .card_base(notification, cx)
      .child(
        h_flex()
          .w_full()
          .items_center()
          .gap_3()
          .p_3()
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
                  .child(card.text.summary.clone()),
              )
              .when(!card.text.body.is_empty(), |this| {
                this.child(
                  div()
                    .text_xs()
                    .text_color(rgba(0xFFFFFF99))
                    .truncate()
                    .child(render_body(&card.text.body)),
                )
              }),
          )
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().flex_none().gap_2().children(buttons))
          }),
      )
  }

  fn render_message(&self, card: &CardState, cx: &mut Context<Self>) -> Stateful<Div> {
    // The message card sizes to its (wrapping) body — up to the clamp below —
    // with even padding all around.
    let notification = &card.notification;
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, cx)
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
                          .child(card.text.app_name.clone()),
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
                      .child(card.text.summary.clone()),
                  )
                  .when(!card.text.body.is_empty(), |this| {
                    this.child(
                      div()
                        .text_sm()
                        .text_color(rgba(0xFFFFFFAA))
                        .line_height(rems(1.3))
                        // Grow with the body, but cap it at [`BODY_MAX_LINES`].
                        // `text_ellipsis` is what ends the clamped text in an
                        // ellipsis: on its own `line_clamp` just stops wrapping,
                        // leaving the remainder to run off the last line and be
                        // cut mid-word by the card's `overflow_hidden`.
                        .line_clamp(BODY_MAX_LINES)
                        .text_ellipsis()
                        .child(render_body(&card.text.body)),
                    )
                  }),
              ),
          )
          .when(!buttons.is_empty(), |this| {
            this.child(h_flex().w_full().gap_2().children(buttons))
          }),
      )
  }

  fn render_media(&self, card: &CardState, cx: &mut Context<Self>) -> Stateful<Div> {
    let notification = &card.notification;
    let buttons = self.prominent_buttons(notification, cx);

    self
      .card_base(notification, cx)
      .child(
        v_flex()
          .w_full()
          .child(render_cover(notification))
          .child(
            v_flex()
              .w_full()
              .gap_3()
              .p_3()
              .child(
                v_flex()
                  .gap(px(2.))
                  .when(!card.text.app_name.is_empty(), |this| {
                    this.child(
                      div()
                        .text_xs()
                        .text_color(rgba(0xFFFFFF80))
                        .truncate()
                        .child(card.text.app_name.clone()),
                    )
                  })
                  .child(
                    div()
                      .text_base()
                      .font_weight(FontWeight::BOLD)
                      .text_color(rgb(0xF0F0F0))
                      .truncate()
                      .child(card.text.summary.clone()),
                  )
                  .when(!card.text.body.is_empty(), |this| {
                    this.child(
                      div()
                        .text_sm()
                        .text_color(rgba(0xFFFFFFAA))
                        .truncate()
                        .child(render_body(&card.text.body)),
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
    let reported_region = self.reported_region.clone();

    v_flex()
      .size_full()
      .gap(px(CARD_GAP))
      .children(cards)
      // The surface is a fixed full-height overlay, so this never resizes it; it
      // reports the extent the cards occupy, which the OSD turns into the
      // surface's input region so clicks pass through the empty space. `None`
      // means the stack is empty, so the surface is torn down.
      //
      // Cards keep a constant height through their fade/slide animations, so the
      // extent only changes when one is added or removed — the gate below means
      // animation frames in between do no work and never re-commit the region.
      // The report is deferred to the next effect cycle: this prepaint can run
      // synchronously inside an `NotificationOsd` update (the one that triggered
      // it), and deferring avoids re-entering it.
      .on_children_prepainted(move |bounds, _window, cx| {
        let region = stack_height(&bounds).map(|height| height.round());
        if region == reported_region.get() {
          return;
        }
        reported_region.set(region);

        let osd = osd.clone();
        cx.defer(move |cx| {
          osd
            .update(cx, |osd, cx| osd.report_region(region, cx))
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

#[cfg(test)]
mod tests {
  use super::{Body, BodyBuilder, Emphasis, single_line_body};

  const NONE: Emphasis = Emphasis { bold: false, italic: false, underline: false };
  const BOLD: Emphasis = Emphasis { bold: true, italic: false, underline: false };
  const UNDERLINE: Emphasis = Emphasis { bold: false, italic: false, underline: true };

  fn body(chunks: &[(&str, Emphasis)]) -> Body {
    let mut builder = BodyBuilder::default();
    for (chunk, emphasis) in chunks {
      builder.push(chunk, *emphasis);
    }
    builder.build()
  }

  /// Each emphasized stretch as `(text, emphasis)`, in order.
  #[track_caller]
  fn spans(body: &Body) -> Vec<(String, Emphasis)> {
    body
      .spans
      .iter()
      .map(|span| (body.text[span.range.clone()].to_owned(), span.emphasis))
      .collect()
  }

  #[test]
  fn flattens_a_body_and_carries_its_emphasis() {
    let flattened = single_line_body(&body(&[("line one\nline ", NONE), ("two", BOLD)]));
    assert_eq!(flattened.text, "line one line two");
    assert_eq!(spans(&flattened), [("two".to_owned(), BOLD)]);
  }

  /// Whitespace inside a span collapses along with the rest but keeps its
  /// emphasis, so an underline isn't broken at every space it spans.
  #[test]
  fn keeps_spans_contiguous_across_collapsed_whitespace() {
    let flattened = single_line_body(&body(&[("a\n\n  b", UNDERLINE)]));
    assert_eq!(flattened.text, "a b");
    assert_eq!(spans(&flattened), [("a b".to_owned(), UNDERLINE)]);
  }

  #[test]
  fn drops_edge_whitespace_and_the_spans_over_it() {
    let flattened = single_line_body(&body(&[("  ", NONE), ("bold", BOLD), (" \n ", BOLD)]));
    assert_eq!(flattened.text, "bold");
    assert_eq!(spans(&flattened), [("bold".to_owned(), BOLD)]);
  }

  #[test]
  fn leaves_flat_bodies_alone() {
    let flattened = single_line_body(&body(&[("already flat ", NONE), ("here", BOLD)]));
    assert_eq!(flattened.text, "already flat here");
    assert_eq!(spans(&flattened), [("here".to_owned(), BOLD)]);
  }
}
