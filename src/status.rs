//! The desktop clock.
//!
//! A layer-shell surface in the corner of the screen showing the time, the date
//! and what is left of the battery. This used to be a separate program
//! (`status`) that ran beside the daemon; it lives here now, which also lets the
//! lock screen draw the same clock over its own surfaces instead of imitating
//! this one.
//!
//! The surface is pure decoration: it takes no keyboard focus and passes clicks
//! through to whatever is underneath.

use std::time::Duration;

use chrono::{DateTime, Local, Timelike as _};
use futures::{StreamExt as _, future::Shared, stream};
use gpui::{
  App, AsyncApp, Bounds, Context, DisplayId, Div, Entity, FontWeight, Global, IntoElement, Pixels,
  Render, Size, Styled, Subscription, Task, WeakEntity, Window, WindowBackgroundAppearance,
  WindowBounds, WindowHandle, WindowKind, WindowOptions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rems, rgb,
};
use tracing::{debug, error, warn};

use crate::config::{Config, ConfigState};
use crate::dbus::GlobalDbusConnection;
use crate::dbus::upower::Battery;
use crate::util::{ResultExt, h_flex, v_flex};

/// The clock only shows hours and minutes, but is checked every second so it
/// flips over roughly when it should.
const TICK: Duration = Duration::from_secs(1);

const TIME_FORMAT: &str = "%H:%M";
const DATE_FORMAT: &str = "%a, %e %b";

/// How far the clock sits from the corner of the screen. On the overlay these
/// are the layer surface's margins; the lock screen insets it by the same
/// amounts so the clock doesn't move when the screen locks.
const MARGIN_RIGHT: Pixels = px(10.);
const MARGIN_BOTTOM: Pixels = px(5.);

/// The surface the clock is drawn on. Larger than the clock, which sits in its
/// bottom right corner, so nothing clips when the charge reaches three digits.
const WIDTH: Pixels = px(400.);
const HEIGHT: Pixels = px(140.);

pub fn init(cx: &mut App) {
  let clock = cx.new(Clock::new);
  cx.set_global(GlobalClock(clock));

  let overlay = cx.new(StatusOverlay::new);
  cx.set_global(GlobalStatusOverlay(overlay));
}

struct GlobalClock(Entity<Clock>);

impl Global for GlobalClock {}

struct GlobalStatusOverlay(#[allow(dead_code)] Entity<StatusOverlay>);

impl Global for GlobalStatusOverlay {}

/// What the clock shows, kept up to date for as long as the app runs.
///
/// One of these for the whole app rather than one per surface: the overlay and
/// every lock screen read the same values, so they can't drift apart, and the
/// timer and the battery subscription only exist once.
pub struct Clock {
  pub now: DateTime<Local>,
  /// Charge left in percent, on machines that have a battery.
  pub battery: Option<f64>,
  _tick: Task<()>,
  _battery: Task<()>,
}

impl Clock {
  fn new(cx: &mut Context<Self>) -> Self {
    let connection = GlobalDbusConnection::system(cx);

    Self {
      now: Local::now(),
      battery: None,
      _tick: cx.spawn(async move |this, cx| tick(this, cx).await),
      _battery: cx.spawn(async move |this, cx| watch_battery(connection, this, cx).await),
    }
  }

  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalClock>().0.clone()
  }
}

/// Keeps [`Clock::now`] current, repainting only when the displayed minute
/// actually changes.
async fn tick(clock: WeakEntity<Clock>, cx: &mut AsyncApp) {
  loop {
    cx.background_executor().timer(TICK).await;

    let updated = clock.update(cx, |clock, cx| {
      let now = Local::now();
      if now.minute() == clock.now.minute() && now.hour() == clock.now.hour() {
        return;
      }

      clock.now = now;
      cx.notify();
    });

    if updated.is_err() {
      break;
    }
  }
}

/// Keeps [`Clock::battery`] current. Machines without a battery never report
/// one, and the clock then shows the time alone.
async fn watch_battery(
  connection: Shared<Task<Option<zbus::Connection>>>,
  clock: WeakEntity<Clock>,
  cx: &mut AsyncApp,
) {
  let Some(connection) = connection.await else {
    warn!("System bus unavailable, the clock won't show the battery");
    return;
  };

  let battery = match Battery::find(&connection).await {
    Ok(Some(battery)) => battery,
    Ok(None) => return,
    Err(error) => {
      warn!(?error, "Failed to look up the battery");
      return;
    }
  };

  let changes = match battery.listen().await {
    Ok(changes) => changes,
    Err(error) => {
      warn!(?error, "Failed to follow the battery charge");
      return;
    }
  };

  // Subscribed before the first read, so a change that lands in between is not
  // lost.
  let initial = stream::iter(battery.percentage().await.log_err());
  let percentages = initial.chain(changes);
  futures::pin_mut!(percentages);

  while let Some(percentage) = percentages.next().await {
    let updated = clock.update(cx, |clock, cx| {
      clock.battery = Some(percentage);
      cx.notify();
    });

    if updated.is_err() {
      break;
    }
  }
}

/// Holds the overlay on the display it is configured for, following the config
/// and whatever screens are attached.
struct StatusOverlay {
  window: Option<WindowHandle<StatusView>>,
  /// The display the surface was opened on, so a change that doesn't move it
  /// leaves it alone.
  display: Option<DisplayId>,
  _subscriptions: Vec<Subscription>,
}

impl StatusOverlay {
  fn new(cx: &mut Context<Self>) -> Self {
    let config = ConfigState::global(cx);

    let subscriptions = vec![
      cx.observe(&config, |this, _config, cx| this.sync(cx)),
      cx.on_display_changed({
        let this = cx.weak_entity();
        move |cx| {
          this.update(cx, |this, cx| this.sync(cx)).log_err();
        }
      }),
    ];

    let mut this = Self {
      window: None,
      display: None,
      _subscriptions: subscriptions,
    };

    this.sync(cx);
    this
  }

  fn sync(&mut self, cx: &mut Context<Self>) {
    let config = ConfigState::get(cx);
    let target = match config.status.enabled {
      true => target_display(&config, cx),
      false => None,
    };

    if target == self.display && self.is_open(cx) {
      return;
    }

    self.close(cx);
    self.display = target;

    let Some(display) = target else {
      return;
    };

    let clock = Clock::global(cx);

    let window = cx.open_window(window_options(display), |window, cx| {
      // The clock is decoration, so clicks belong to whatever is behind it.
      window.set_input_passthrough();
      cx.new(|cx| StatusView::new(clock, cx))
    });

    match window {
      Ok(handle) => self.window = Some(handle),
      Err(error) => error!(?error, "Failed to open the clock overlay"),
    }
  }

  /// Whether the surface we opened is still around. A display going away takes
  /// its windows with it, and the handle we hold outlives them.
  fn is_open(&self, cx: &App) -> bool {
    self
      .window
      .is_some_and(|window| cx.windows().contains(&window.into()))
  }

  fn close(&mut self, cx: &mut Context<Self>) {
    let Some(window) = self.window.take() else {
      return;
    };

    if let Err(error) = window.update(cx, |_view, window, _cx| window.remove_window()) {
      debug!(?error, "Clock overlay was already gone");
    }
  }
}

/// The display the clock belongs on: the one the status section names, else the
/// primary display, else the first one attached.
fn target_display(config: &Config, cx: &App) -> Option<DisplayId> {
  let displays = cx.displays();
  let configured = config
    .status
    .display
    .as_deref()
    .or(config.primary_display.as_deref());

  if let Some(name) = configured {
    if let Some(display) = displays.iter().find(|display| display.name() == Some(name)) {
      return Some(display.id());
    }

    warn!(display = %name, "Configured clock display not found, using the first display");
  }

  displays.into_iter().next().map(|display| display.id())
}

fn window_options(display: DisplayId) -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-status".to_string()),
    display_id: Some(display),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(WIDTH, HEIGHT),
    })),
    window_background: WindowBackgroundAppearance::Transparent,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-status".to_string(),
      layer: Layer::Top,
      anchor: Anchor::BOTTOM | Anchor::RIGHT,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: Some((px(0.), MARGIN_RIGHT, MARGIN_BOTTOM, px(0.))),
      keyboard_interactivity: KeyboardInteractivity::None,
    }),
    ..Default::default()
  }
}

struct StatusView {
  clock: Entity<Clock>,
  _subscriptions: Vec<Subscription>,
}

impl StatusView {
  fn new(clock: Entity<Clock>, cx: &mut Context<Self>) -> Self {
    let config = ConfigState::global(cx);

    let subscriptions = vec![
      cx.observe(&clock, |_this, _clock, cx| cx.notify()),
      cx.observe(&config, |_this, _config, cx| cx.notify()),
    ];

    Self {
      clock,
      _subscriptions: subscriptions,
    }
  }
}

impl Render for StatusView {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let clock = self.clock.read(cx);
    let opacity = ConfigState::get(cx).status.opacity;

    // The surface is anchored to the corner, so the clock is pushed into the
    // corner of the surface to sit exactly at the margins.
    v_flex()
      .size_full()
      .justify_end()
      .items_end()
      .child(render_clock(clock.now, clock.battery, opacity))
  }
}

/// The clock as the lock screen draws it, inset from the corner of a surface
/// that covers the whole screen rather than one anchored to it.
pub fn render_clock_in_corner(now: DateTime<Local>, battery: Option<f64>, opacity: f32) -> Div {
  render_clock(now, battery, opacity)
    .absolute()
    .bottom(MARGIN_BOTTOM)
    .right(MARGIN_RIGHT)
}

/// The charge above the date and the time, right-aligned.
fn render_clock(now: DateTime<Local>, battery: Option<f64>, opacity: f32) -> Div {
  let time = now.format(TIME_FORMAT).to_string();
  let date = now.format(DATE_FORMAT).to_string().to_uppercase();

  v_flex()
    .items_end()
    .font_family("Noto Sans")
    .text_color(rgb(0xFFFFFF))
    .opacity(opacity)
    .when_some(battery, |this, percentage| {
      this.child(
        div()
          .text_size(rems(2.))
          .line_height(rems(1.6))
          .font_weight(FontWeight::BOLD)
          .child(format!("{percentage:.0}")),
      )
    })
    .child(
      h_flex()
        .items_end()
        .gap_2()
        .child(
          div()
            .text_size(rems(1.4))
            .line_height(rems(1.95))
            .font_weight(FontWeight::BOLD)
            .child(date),
        )
        .child(
          div()
            .text_size(rems(3.5))
            .line_height(rems(3.5))
            .font_weight(FontWeight::BOLD)
            .child(time),
        ),
    )
}
