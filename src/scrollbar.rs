use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use gpui::{
  App, Bounds, ContentMask, Edges, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior,
  InspectorElementId, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels,
  Point, Position, ScrollHandle, Style, Window, fill, point, prelude::*, px, relative, rgba, size,
};

const SCROLLBAR_WIDTH: Pixels = px(8.0);
const THUMB_WIDTH: Pixels = px(4.0);
const THUMB_INSET: Pixels = px(2.0);
const THUMB_RADIUS: Pixels = px(2.0);
const THUMB_MIN_HEIGHT: Pixels = px(24.0);
const THUMB_COLOR: u32 = 0xFFFFFF33;
const THUMB_HOVER_COLOR: u32 = 0xFFFFFF55;
const THUMB_DRAG_COLOR: u32 = 0xFFFFFF77;

#[derive(Debug, Clone)]
struct ScrollbarState(Rc<Cell<ScrollbarStateInner>>);

#[derive(Debug, Clone, Copy)]
struct ScrollbarStateInner {
  dragging: bool,
  drag_start_y: Pixels,
  drag_start_offset: Pixels,
  hovered_on_thumb: bool,
  last_scroll_offset: Point<Pixels>,
  last_scroll_time: Option<Instant>,
}

impl Default for ScrollbarState {
  fn default() -> Self {
    Self(Rc::new(Cell::new(ScrollbarStateInner {
      dragging: false,
      drag_start_y: px(0.0),
      drag_start_offset: px(0.0),
      hovered_on_thumb: false,
      last_scroll_offset: point(px(0.0), px(0.0)),
      last_scroll_time: None,
    })))
  }
}

pub struct Scrollbar {
  id: ElementId,
  scroll_handle: ScrollHandle,
}

impl Scrollbar {
  #[track_caller]
  pub fn new(scroll_handle: &ScrollHandle) -> Self {
    let caller = std::panic::Location::caller();
    Self {
      id: ElementId::CodeLocation(*caller),
      scroll_handle: scroll_handle.clone(),
    }
  }
}

impl IntoElement for Scrollbar {
  type Element = Self;
  fn into_element(self) -> Self::Element {
    self
  }
}

pub struct ScrollbarPrepaintState {
  hitbox: Hitbox,
  state: ScrollbarState,
  thumb_bounds: Option<Bounds<Pixels>>,
  track_bounds: Bounds<Pixels>,
  content_height: Pixels,
  viewport_height: Pixels,
  thumb_height: Pixels,
}

impl Element for Scrollbar {
  type RequestLayoutState = ();
  type PrepaintState = ScrollbarPrepaintState;

  fn id(&self) -> Option<ElementId> {
    Some(self.id.clone())
  }

  fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
    None
  }

  fn request_layout(
    &mut self,
    _: Option<&GlobalElementId>,
    _: Option<&InspectorElementId>,
    window: &mut Window,
    cx: &mut App,
  ) -> (LayoutId, ()) {
    let style = Style {
      position: Position::Absolute,
      inset: Edges::default(),
      size: size(relative(1.), relative(1.)).map(Into::into),
      ..Style::default()
    };
    (window.request_layout(style, None, cx), ())
  }

  fn prepaint(
    &mut self,
    _: Option<&GlobalElementId>,
    _: Option<&InspectorElementId>,
    bounds: Bounds<Pixels>,
    _: &mut (),
    window: &mut Window,
    cx: &mut App,
  ) -> ScrollbarPrepaintState {
    let hitbox = window.with_content_mask(Some(ContentMask { bounds }), |window| {
      window.insert_hitbox(bounds, HitboxBehavior::Normal)
    });

    let state = window
      .use_state(cx, |_, _| ScrollbarState::default())
      .read(cx)
      .clone();

    let viewport_height = bounds.size.height;
    let max_offset = self.scroll_handle.max_offset();
    let content_height = viewport_height + max_offset.y;

    let has_overflow = content_height > viewport_height && viewport_height > px(0.0);

    let track_bounds = Bounds {
      origin: point(
        bounds.origin.x + bounds.size.width - SCROLLBAR_WIDTH,
        bounds.origin.y,
      ),
      size: size(SCROLLBAR_WIDTH, viewport_height),
    };

    let thumb_bounds = if has_overflow {
      let thumb_ratio = viewport_height / content_height;
      let thumb_height = (viewport_height * thumb_ratio).max(THUMB_MIN_HEIGHT);
      let scrollable = content_height - viewport_height;
      let scroll_progress = if scrollable > px(0.0) {
        (-self.scroll_handle.offset().y / scrollable).clamp(0.0, 1.0)
      } else {
        0.0
      };
      let track_space = viewport_height - thumb_height;
      let thumb_top = track_space * scroll_progress;

      Some(Bounds {
        origin: point(
          track_bounds.origin.x + THUMB_INSET,
          track_bounds.origin.y + thumb_top,
        ),
        size: size(THUMB_WIDTH, thumb_height),
      })
    } else {
      None
    };

    let thumb_height = thumb_bounds.map(|b| b.size.height).unwrap_or(px(0.0));

    ScrollbarPrepaintState {
      hitbox,
      state,
      thumb_bounds,
      track_bounds,
      content_height,
      viewport_height,
      thumb_height,
    }
  }

  fn paint(
    &mut self,
    _: Option<&GlobalElementId>,
    _: Option<&InspectorElementId>,
    _bounds: Bounds<Pixels>,
    _: &mut (),
    prepaint: &mut ScrollbarPrepaintState,
    window: &mut Window,
    _cx: &mut App,
  ) {
    let Some(thumb_bounds) = prepaint.thumb_bounds else {
      return;
    };

    let state = &prepaint.state;
    let view_id = window.current_view();
    let track_bounds = prepaint.track_bounds;
    let content_height = prepaint.content_height;
    let viewport_height = prepaint.viewport_height;
    let thumb_height = prepaint.thumb_height;

    if self.scroll_handle.offset() != state.0.get().last_scroll_offset {
      let mut inner = state.0.get();
      inner.last_scroll_offset = self.scroll_handle.offset();
      inner.last_scroll_time = Some(Instant::now());
      state.0.set(inner);
    }

    let inner = state.0.get();
    let thumb_color = if inner.dragging {
      rgba(THUMB_DRAG_COLOR)
    } else if inner.hovered_on_thumb {
      rgba(THUMB_HOVER_COLOR)
    } else {
      rgba(THUMB_COLOR)
    };

    window.with_content_mask(
      Some(ContentMask {
        bounds: prepaint.hitbox.bounds,
      }),
      |window| {
        window.paint_layer(prepaint.hitbox.bounds, |window| {
          window.paint_quad(fill(thumb_bounds, thumb_color).corner_radii(THUMB_RADIUS));
        });

        window.on_mouse_event({
          let state = state.0.clone();
          let scroll_handle = self.scroll_handle.clone();

          move |event: &MouseDownEvent, phase, _, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
              return;
            }
            if !track_bounds.contains(&event.position) {
              return;
            }

            cx.stop_propagation();

            if thumb_bounds.contains(&event.position) {
              let mut inner = state.get();
              inner.dragging = true;
              inner.drag_start_y = event.position.y;
              inner.drag_start_offset = scroll_handle.offset().y;
              state.set(inner);
            } else {
              let scrollable = content_height - viewport_height;
              let track_space = viewport_height - thumb_height;
              if track_space > px(0.0) {
                let click_in_track = event.position.y - track_bounds.origin.y - thumb_height / 2.0;
                let ratio = (click_in_track / track_space).clamp(0.0, 1.0);
                let new_y = -(scrollable * ratio);
                scroll_handle.set_offset(point(scroll_handle.offset().x, new_y));
              }
            }

            cx.notify(view_id);
          }
        });

        window.on_mouse_event({
          let state = state.0.clone();
          let scroll_handle = self.scroll_handle.clone();

          move |event: &MouseMoveEvent, _, _, cx| {
            let mut inner = state.get();
            let was_hovered = inner.hovered_on_thumb;
            inner.hovered_on_thumb = thumb_bounds.contains(&event.position);

            if inner.dragging && event.dragging() {
              cx.stop_propagation();

              let delta_y = event.position.y - inner.drag_start_y;
              let scrollable = content_height - viewport_height;
              let track_space = viewport_height - thumb_height;
              if track_space > px(0.0) {
                let offset_delta = delta_y / track_space * scrollable;
                let new_y = (inner.drag_start_offset - offset_delta).clamp(-scrollable, px(0.0));
                scroll_handle.set_offset(point(scroll_handle.offset().x, new_y));
              }

              state.set(inner);
              cx.notify(view_id);
            } else if inner.hovered_on_thumb != was_hovered {
              state.set(inner);
              cx.notify(view_id);
            }
          }
        });

        window.on_mouse_event({
          let state = state.0.clone();

          move |_event: &MouseUpEvent, phase, _, cx| {
            if phase.bubble() && state.get().dragging {
              let mut inner = state.get();
              inner.dragging = false;
              state.set(inner);
              cx.notify(view_id);
            }
          }
        });
      },
    );
  }
}
