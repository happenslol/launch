use std::{collections::HashMap, time::Duration};

use gpui::{
  Animation, AnimationExt, App, Bounds, Context, DisplayId, ElementId, Entity, Global, IntoElement,
  Pixels, Render, Size, Styled, Subscription, Task, Window, WindowBackgroundAppearance,
  WindowBounds, WindowHandle, WindowKind, WindowOptions, div, point, prelude::*, px, rgb, rgba,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
};
use tracing::error;

use crate::{
  niri::{NiriEvent, NiriState},
  util::{ResultExt, h_flex},
};

const DISPLAY_TIMEOUT: Duration = Duration::from_millis(1500);
const ANIM_ENTER_DURATION: Duration = Duration::from_millis(150);
const ANIM_EXIT_DURATION: Duration = Duration::from_millis(200);

const WINDOW_HEIGHT: f32 = 72.0;
const DOT_SIZE: f32 = 14.0;
const DOT_GAP: f32 = 10.0;
const SLIDE_OFFSET: f32 = 10.0;

pub fn init(cx: &mut App) {
  let Some(niri) = NiriState::try_global(cx) else {
    return;
  };

  let osd = cx.new(|cx| WorkspaceOsd::new(niri, cx));
  cx.set_global(GlobalWorkspaceOsd(osd));
}

struct GlobalWorkspaceOsd(#[allow(dead_code)] Entity<WorkspaceOsd>);

impl Global for GlobalWorkspaceOsd {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DotState {
  Inactive,
  Active,
  Urgent,
}

struct OutputWindow {
  handle: WindowHandle<WorkspaceOsdView>,
  count: usize,
}

/// Watches niri for workspace switches and urgency changes, driving a transient
/// on-screen display per monitor that shows the workspaces on that monitor as a
/// row of dots.
struct WorkspaceOsd {
  niri: Entity<NiriState>,
  windows: HashMap<String, OutputWindow>,
  _subscription: Subscription,
}

impl WorkspaceOsd {
  fn new(niri: Entity<NiriState>, cx: &mut Context<Self>) -> Self {
    let subscription = cx.subscribe(&niri, |this, niri, event, cx| match event {
      NiriEvent::WorkspaceActivated { id } => {
        if let Some(output) = niri.read(cx).workspace_output(*id).map(str::to_owned) {
          this.refresh_output(&output, true, cx);
        }
      }
      NiriEvent::WorkspaceUrgencyChanged { id, urgent } => {
        if let Some(output) = niri.read(cx).workspace_output(*id).map(str::to_owned) {
          this.refresh_output(&output, *urgent, cx);
        }
      }
      NiriEvent::WorkspacesChanged => {
        // Keep any currently visible OSD accurate when workspaces are added or
        // removed, but don't pop one open just because the set changed.
        let outputs = this.windows.keys().cloned().collect::<Vec<_>>();
        for output in outputs {
          this.refresh_output(&output, false, cx);
        }
      }
      NiriEvent::WindowsChanged => {}
    });

    Self {
      niri,
      windows: HashMap::new(),
      _subscription: subscription,
    }
  }

  /// Updates (or opens) the OSD for the given output. `pop` controls whether a
  /// hidden OSD should be opened; an urgent workspace always forces it open and
  /// keeps it visible regardless of `pop`.
  fn refresh_output(&mut self, output: &str, pop: bool, cx: &mut Context<Self>) {
    let dots = dots_for_output(self.niri.read(cx).workspaces(), output);

    if dots.is_empty() {
      if let Some(window) = self.windows.remove(output) {
        window
          .handle
          .update(cx, |_view, window, _cx| window.remove_window())
          .log_err();
      }
      return;
    }

    let persistent = dots.contains(&DotState::Urgent);
    let count = dots.len();

    if let Some(window) = self.windows.get(output) {
      // A changed workspace count needs a differently sized window; resize the
      // existing layer surface in place rather than recreating it.
      let needs_resize = window.count != count;
      let updated = window
        .handle
        .update(cx, |view, window, cx| {
          if needs_resize {
            window.resize(Size::new(window_width(count), px(WINDOW_HEIGHT)));
          }
          view.update_content(dots.clone(), persistent, cx);
        })
        .is_ok();

      if updated {
        if let Some(window) = self.windows.get_mut(output) {
          window.count = count;
        }
        return;
      }

      // The window was closed since we last opened it; drop the stale handle and
      // fall through to reopen below.
      self.windows.remove(output);
    }

    if !pop && !persistent {
      return;
    }

    let Some(display_id) = display_id_for_output(output, cx) else {
      return;
    };

    match cx.open_window(window_options(display_id, count), move |_window, cx| {
      cx.new(|cx| WorkspaceOsdView::new(dots, persistent, cx))
    }) {
      Ok(handle) => {
        self
          .windows
          .insert(output.to_string(), OutputWindow { handle, count });
      }
      Err(error) => error!(?error, "Failed to open workspace OSD window"),
    }
  }
}

fn dots_for_output(workspaces: &[niri_ipc::Workspace], output: &str) -> Vec<DotState> {
  let mut on_output = workspaces
    .iter()
    .filter(|workspace| workspace.output.as_deref() == Some(output))
    .collect::<Vec<_>>();
  on_output.sort_by_key(|workspace| workspace.idx);

  on_output
    .iter()
    .map(|workspace| {
      if workspace.is_urgent {
        DotState::Urgent
      } else if workspace.is_active {
        DotState::Active
      } else {
        DotState::Inactive
      }
    })
    .collect()
}

fn display_id_for_output(output: &str, cx: &App) -> Option<DisplayId> {
  cx.displays()
    .into_iter()
    .find(|display| display.name() == Some(output))
    .map(|display| display.id())
}

fn window_options(display_id: DisplayId, count: usize) -> WindowOptions {
  WindowOptions {
    titlebar: None,
    app_id: Some("launch-workspace-osd".to_string()),
    display_id: Some(display_id),
    window_bounds: Some(WindowBounds::Windowed(Bounds {
      origin: point(px(0.), px(0.)),
      size: Size::new(window_width(count), px(WINDOW_HEIGHT)),
    })),
    window_background: WindowBackgroundAppearance::Transparent,
    kind: WindowKind::LayerShell(LayerShellOptions {
      namespace: "launch-workspace-osd".to_string(),
      layer: Layer::Overlay,
      anchor: Anchor::TOP,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: Some((px(2.), px(0.), px(0.), px(0.))),
      keyboard_interactivity: KeyboardInteractivity::None,
    }),
    ..Default::default()
  }
}

// The pill auto-sizes to its dots and is centered within the fixed-size layer
// surface, so the window only needs to be comfortably wider than the pill to
// avoid clipping it.
fn window_width(count: usize) -> Pixels {
  px(60.0 + count as f32 * (DOT_SIZE + DOT_GAP + 4.0))
}

struct WorkspaceOsdView {
  dots: Vec<DotState>,
  persistent: bool,
  closing: bool,
  _timeout_task: Task<()>,
}

impl WorkspaceOsdView {
  fn new(dots: Vec<DotState>, persistent: bool, cx: &mut Context<Self>) -> Self {
    let mut this = Self {
      dots,
      persistent,
      closing: false,
      _timeout_task: Task::ready(()),
    };
    this.arm_timeout(cx);
    this
  }

  fn update_content(&mut self, dots: Vec<DotState>, persistent: bool, cx: &mut Context<Self>) {
    self.dots = dots;
    self.persistent = persistent;
    self.closing = false;
    self.arm_timeout(cx);
    cx.notify();
  }

  fn arm_timeout(&mut self, cx: &mut Context<Self>) {
    // An urgent workspace keeps the OSD visible until the urgency clears.
    if self.persistent {
      self._timeout_task = Task::ready(());
      return;
    }

    self._timeout_task = cx.spawn(async move |this, cx| {
      cx.background_executor().timer(DISPLAY_TIMEOUT).await;
      this.update(cx, |this, cx| this.start_exit(cx)).log_err();
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

impl Render for WorkspaceOsdView {
  fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    let closing = self.closing;
    let easing = |delta: f32| 1.0 - (1.0 - delta).powi(3);

    let dots = self.dots.iter().map(|dot| {
      let color = match dot {
        DotState::Inactive => rgb(0x555555),
        DotState::Active => rgb(0x4F9CF0),
        DotState::Urgent => rgb(0xE0524F),
      };
      div().size(px(DOT_SIZE)).rounded_full().bg(color)
    });

    div()
      .size_full()
      .flex()
      .items_center()
      .justify_center()
      .child(
        h_flex()
          .id("workspace-osd")
          .items_center()
          .gap(px(DOT_GAP))
          .px(px(14.))
          .py(px(10.))
          .rounded_full()
          .bg(rgba(0x1D1D1DF0))
          .border_1()
          .border_color(rgba(0xFFFFFF15))
          .shadow_lg()
          .children(dots)
          .with_animation(
            ElementId::NamedInteger("workspace-osd-fade".into(), closing as u64),
            Animation::new(if closing {
              ANIM_EXIT_DURATION
            } else {
              ANIM_ENTER_DURATION
            })
            .with_easing(easing),
            move |this, delta| {
              // Slide in downward from above on enter, and back up on exit.
              let opacity = if closing { 1.0 - delta } else { delta };
              let offset = if closing {
                -SLIDE_OFFSET * delta
              } else {
                -SLIDE_OFFSET * (1.0 - delta)
              };
              this.opacity(opacity).mt(px(offset))
            },
          ),
      )
  }
}
