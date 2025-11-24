use std::{
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::Instant,
};

use async_lock::Mutex;
use freedesktop_desktop_entry::DesktopEntry;
use gpui::{
  AnyView, App, Bounds, Entity, KeyBinding, SharedString, Size, Subscription, Task, Window,
  WindowBounds, WindowKind, WindowOptions, actions,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rgb, rgba,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use tracing::{error, trace};

use crate::{
  audio,
  db::Db,
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{h_flex, v_flex},
  xdg,
};

actions!(launcher, [Quit]);

type PanelView = dyn Fn(&mut Window, &mut App) -> AnyView + Send + Sync;

pub struct Launcher {
  picker: Entity<Picker<RootDelegate>>,
  active_panel: Option<AnyView>,
  _subscriptions: Vec<Subscription>,
}

impl Launcher {
  pub fn get_window_options() -> WindowOptions {
    WindowOptions {
      titlebar: None,
      app_id: Some("launch".to_string()),
      window_bounds: Some(WindowBounds::Windowed(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(600.), px(240.)),
      })),
      window_background: gpui::WindowBackgroundAppearance::Transparent,
      kind: WindowKind::LayerShell(LayerShellOptions {
        namespace: "launch".to_string(),
        layer: Layer::Overlay,
        anchor: Anchor::all(),
        exclusive_zone: None,
        exclusive_edge: None,
        margin: None,
        keyboard_interactivity: KeyboardInteractivity::Exclusive,
      }),
      ..Default::default()
    }
  }

  pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
    cx.new(|cx| Self::new(window, cx))
  }

  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([KeyBinding::new("escape", Quit, None)]);

    let db = Db::global(cx);
    cx.spawn(async move |_, _| {
      let _launches = db.get_launches().await;
      // TODO: Use for scoring
    })
    .detach();

    let mut items = vec![];

    items.extend(xdg::get_items().unwrap());
    items.extend(audio::panels::get_items().unwrap());

    let picker = cx.new(|cx| Picker::new(RootDelegate::new(), items, window, cx));
    cx.focus_view(&picker, window);

    let subscriptions = vec![cx.subscribe_in(
      &picker,
      window,
      move |this, _, ev: &PickerEvent<RootDelegate>, window, cx| match ev {
        PickerEvent::Picked(item) => this.launch(item.clone(), window, cx),
        PickerEvent::QueryChanged(query) => {
          println!("Query changed: {query}");
          let mut context = fend_core::Context::new();
        },
      },
    )];

    Self {
      picker,
      active_panel: None, // Some(audio_section),
      _subscriptions: subscriptions,
    }
  }

  fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
    if self.active_panel.take().is_some() {
      cx.focus_view(&self.picker, window);
      cx.notify();
      return;
    }

    cx.quit();
  }

  fn launch(&mut self, item: Item, window: &mut Window, cx: &mut Context<Self>) {
    let db = Db::global(cx);

    cx.spawn_in(window, async move |this, cx| {
      let _ = db.record_launch(&item.id).await;

      let _ = this.update_in(cx, |this, window, cx| match &item.action {
        ItemAction::Launch(entry) => match xdg::start(entry) {
          Ok(_) => cx.quit(),
          Err(err) => error!(?err, "Failed to start process"),
        },
        ItemAction::Panel(make_panel) => {
          let panel = make_panel(window, cx);
          this.active_panel = Some(panel);
          cx.notify();
        }
      });
    })
    .detach();
  }
}

impl Render for Launcher {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .on_action(cx.listener(Self::quit))
      .size_full()
      .bg(rgba(0xFFFFFFFF))
      .when_some(self.active_panel.as_ref(), |this, panel| {
        this.child(panel.clone())
      })
      .when_none(&self.active_panel, |this| this.child(self.picker.clone()))
  }
}

fn get_matches(matcher: &mut Matcher, items: &[Item], query: &str) -> Vec<(usize, u32)> {
  let start_time = Instant::now();

  let mut matches = Vec::new();

  let needle = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
  let mut buf = Vec::new();
  for (i, item) in items.iter().enumerate() {
    let mut result_score: Option<u32> = None;
    for term in &item.terms {
      if let Some(score) = needle.score(Utf32Str::new(term, &mut buf), matcher) {
        result_score = if let Some(result_score) = result_score {
          Some(result_score.max(score))
        } else {
          Some(score)
        }
      };
    }

    if let Some(score) = result_score {
      matches.push((i, score));
    }
  }

  trace!(duration = ?start_time.elapsed(), count = items.len(), "Done matching root items");

  matches
}

struct RootDelegate {
  matcher: Arc<Mutex<Matcher>>,
}

impl RootDelegate {
  fn new() -> Self {
    let matcher = Matcher::new(Config::DEFAULT);
    let matcher = Arc::new(Mutex::new(matcher));
    Self { matcher }
  }
}

#[derive(Clone)]
pub struct Item {
  pub id: String,
  pub name: SharedString,
  // TODO: Maybe we can put these in contiguous memory?
  pub terms: Vec<String>,
  pub action: ItemAction,
}

#[derive(Clone)]
pub enum ItemAction {
  Launch(Box<DesktopEntry>),
  Panel(Arc<PanelView>),
}

impl PickerDelegate for RootDelegate {
  type ListItem = Item;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    h_flex()
      .w_full()
      .when_else(
        is_selected,
        |div| div.bg(rgb(0xDDDDDD)),
        |div| div.bg(rgb(0xFFFFFF)),
      )
      .child(item.name.clone())
  }

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    query: String,
    cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    let matcher = self.matcher.clone();

    cx.spawn_in(window, async move |picker, cx| {
      let matches = cx
        .background_spawn(async move {
          if cancel_flag.load(Ordering::Acquire) {
            trace!(search_id, "Stopping search on background spawn");
            return None;
          }

          let matches = {
            // TODO: Should find a better solution, maybe have a pool of matchers? Or a queue that
            // just takes jobs?
            let mut matcher = matcher.lock().await;
            get_matches(&mut matcher, &items, &query)
          };

          if cancel_flag.load(Ordering::Acquire) {
            return None;
          }

          Some(matches)
        })
        .await;

      let Some(matches) = matches else {
        return;
      };

      let _ = picker.update(cx, |picker, cx| {
        picker.complete_search(cx, search_id, matches)
      });
    })
  }
}
