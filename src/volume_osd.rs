use std::time::Duration;

use gpui::{
  Animation, AnimationExt, App, Bounds, Context, ElementId, Entity, Global, IntoElement, Render,
  Size, Styled, Subscription, Task, Window, WindowBackgroundAppearance, WindowBounds, WindowHandle,
  WindowKind, WindowOptions, div, point, prelude::*, px, relative, rems, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use tracing::error;

use crate::{
  audio::{
    AudioState,
    types::{SinkEvent, SinkId, Volume},
  },
  icon::{Icon, IconName},
  launcher::Launcher,
  util::{ResultExt, h_flex},
};

const DISPLAY_TIMEOUT: Duration = Duration::from_millis(1500);
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(200);

pub fn init(cx: &mut App) {
  let audio = AudioState::global(cx);
  let osd = cx.new(|cx| VolumeOsd::new(audio, cx));
  cx.set_global(GlobalVolumeOsd(osd));
}

struct GlobalVolumeOsd(#[allow(dead_code)] Entity<VolumeOsd>);

impl Global for GlobalVolumeOsd {}

/// Watches the default sink for volume changes and drives a transient on-screen
/// display showing the current volume.
struct VolumeOsd {
  audio: Entity<AudioState>,
  current_sink: Option<SinkId>,
  base_volume: Volume,
  current_percent: u32,
  muted: bool,
  window: Option<WindowHandle<VolumeOsdView>>,
  _default_observer: Subscription,
  _sink_listener: Task<()>,
}

impl VolumeOsd {
  fn new(audio: Entity<AudioState>, cx: &mut Context<Self>) -> Self {
    let observer = cx.observe(&audio, |this, audio, cx| {
      let sink = audio.read(cx).default_sink;
      this.set_sink(sink, cx);
    });

    let initial_sink = audio.read(cx).default_sink;

    let mut this = Self {
      audio,
      current_sink: None,
      base_volume: Volume(0),
      current_percent: 0,
      muted: false,
      window: None,
      _default_observer: observer,
      _sink_listener: Task::ready(()),
    };

    this.set_sink(initial_sink, cx);
    this
  }

  fn set_sink(&mut self, sink: Option<SinkId>, cx: &mut Context<Self>) {
    if sink == self.current_sink {
      return;
    }
    self.current_sink = sink;

    let Some(sink_id) = sink else {
      self._sink_listener = Task::ready(());
      return;
    };

    let audio = self.audio.read(cx);
    let info_task = audio.get_sink_info(sink_id, cx);
    let events = audio.subscribe_sink(sink_id);

    // Prime the cached volume state from the current sink info before listening
    // for changes, so the first change event renders an accurate percentage. The
    // initial info fetch itself must never pop the OSD - only real changes do.
    self._sink_listener = cx.spawn(async move |this, cx| {
      if let Some(info) = info_task.await {
        this
          .update(cx, |this, _cx| {
            this.base_volume = info.base_volume;
            this.current_percent = info.volume.as_percent(info.base_volume);
            this.muted = info.mute;
          })
          .log_err();
      }

      while let Ok(event) = events.recv_async().await {
        let stop = matches!(event, SinkEvent::Removed | SinkEvent::NoLongerDefault);

        this
          .update(cx, |this, cx| match event {
            SinkEvent::VolumeChanged(volume) => {
              this.current_percent = volume.as_percent(this.base_volume);
              this.show(cx);
            }
            SinkEvent::MuteChanged(mute) => {
              this.muted = mute;
              this.show(cx);
            }
            SinkEvent::InfoChanged(info) => {
              this.base_volume = info.base_volume;
              this.current_percent = info.volume.as_percent(info.base_volume);
              this.muted = info.mute;
            }
            SinkEvent::BecameDefault | SinkEvent::NoLongerDefault | SinkEvent::Removed => {}
          })
          .log_err();

        if stop {
          break;
        }
      }
    });
  }

  fn show(&mut self, cx: &mut Context<Self>) {
    // The volume/sinks panel already shows live volume, so the OSD is redundant
    // (and would overlap) while it is open.
    if launcher_showing_volume_panel(cx) {
      return;
    }

    let percent = self.current_percent;
    let muted = self.muted;

    if let Some(handle) = self.window {
      if handle
        .update(cx, |view, _window, cx| {
          view.update_volume(percent, muted, cx)
        })
        .is_ok()
      {
        return;
      }
      // The window was closed since we last opened it; fall through and reopen.
      self.window = None;
    }

    match cx.open_window(window_options(), move |window, cx| {
      cx.new(|cx| VolumeOsdView::new(percent, muted, window, cx))
    }) {
      Ok(handle) => self.window = Some(handle),
      Err(err) => error!(?err, "Failed to open volume OSD window"),
    }
  }
}

fn launcher_showing_volume_panel(cx: &App) -> bool {
  cx.windows().iter().any(|window| {
    window
      .downcast::<Launcher>()
      .and_then(|handle| handle.read(cx).ok().map(Launcher::is_showing_volume_panel))
      .unwrap_or(false)
  })
}

fn window_options() -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-volume-osd".to_string()),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(px(320.), px(56.)),
    })),
    window_background: WindowBackgroundAppearance::Transparent,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-volume-osd".to_string(),
      layer: Layer::Overlay,
      anchor: Anchor::BOTTOM,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: Some((px(0.), px(0.), px(96.), px(0.))),
      keyboard_interactivity: KeyboardInteractivity::None,
    }),
    ..Default::default()
  }
}

struct VolumeOsdView {
  percent: u32,
  muted: bool,
  closing: bool,
  _timeout_task: Task<()>,
}

impl VolumeOsdView {
  fn new(percent: u32, muted: bool, _window: &mut Window, cx: &mut Context<Self>) -> Self {
    let mut this = Self {
      percent,
      muted,
      closing: false,
      _timeout_task: Task::ready(()),
    };
    this.restart_timeout(cx);
    this
  }

  fn update_volume(&mut self, percent: u32, muted: bool, cx: &mut Context<Self>) {
    self.percent = percent;
    self.muted = muted;
    self.closing = false;
    self.restart_timeout(cx);
    cx.notify();
  }

  fn restart_timeout(&mut self, cx: &mut Context<Self>) {
    self._timeout_task = cx.spawn(async move |this, cx| {
      cx.background_executor().timer(DISPLAY_TIMEOUT).await;
      this
        .update(cx, |this, cx| this.start_exit(cx))
        .log_err();
    });
  }

  fn start_exit(&mut self, cx: &mut Context<Self>) {
    if self.closing {
      return;
    }
    self.closing = true;
    cx.notify();

    self._timeout_task = cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ANIM_EXIT_DURATION).await;
      this
        .update_in(cx, |_this, window, _cx| {
          window.remove_window();
        })
        .log_err();
    });
  }
}

impl Render for VolumeOsdView {
  fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    let closing = self.closing;
    let muted = self.muted;
    let percent = self.percent;
    let fill = (percent as f32 / 100.0).clamp(0.0, 1.0);
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);

    let icon = if muted {
      IconName::VolumeOff
    } else {
      IconName::Volume
    };
    let icon_color = if muted { rgb(0x777777) } else { rgb(0xDDDDDD) };
    let (track_color, fill_color) = if muted {
      (rgba(0xFFFFFF08), rgba(0xFFFFFF20))
    } else {
      (rgba(0xFFFFFF12), rgba(0xFFFFFFCC))
    };

    div()
      .size_full()
      .flex()
      .items_center()
      .justify_center()
      .child(
        h_flex()
          .id("volume-osd")
          .w_full()
          .items_center()
          .gap_3()
          .px_4()
          .py_3()
          .rounded_xl()
          .bg(rgba(0x1D1D1DF0))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .shadow_lg()
          .child(Icon::new(icon).size(rems(1.2)).text_color(icon_color))
          .child(
            div()
              .flex_1()
              .h(px(6.))
              .rounded_full()
              .bg(track_color)
              .child(
                div()
                  .h_full()
                  .rounded_full()
                  .bg(fill_color)
                  .w(relative(fill)),
              ),
          )
          .child(
            div()
              .w(px(40.))
              .text_sm()
              .text_color(rgba(0xFFFFFFCC))
              .child(format!("{}%", percent)),
          )
          .with_animation(
            ElementId::NamedInteger("volume-osd-fade".into(), closing as u64),
            Animation::new(if closing {
              ANIM_EXIT_DURATION
            } else {
              ANIM_ENTER_DURATION
            })
            .with_easing(easing),
            move |this, delta| {
              let opacity = if closing { 1.0 - delta } else { delta };
              let offset = if closing { 6.0 * delta } else { 6.0 * (1.0 - delta) };
              this.opacity(opacity).mt(px(offset))
            },
          ),
      )
  }
}
