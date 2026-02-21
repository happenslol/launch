use std::{
  cmp::Reverse,
  collections::HashMap,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use chrono::{DateTime, Local, TimeZone};

use freedesktop_desktop_entry::DesktopEntry;
use gpui::{
  AnyView, App, Bounds, Entity, FocusHandle, Focusable, ImageSource, KeyBinding, SharedString,
  Size, Subscription, Task, Window, WindowBounds, WindowKind, WindowOptions, actions, div, img,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rems, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use tracing::error;

use crate::{
  audio, bluetooth, clipboard,
  db::DB,
  icon::{Icon, IconName},
  matcher::MatcherPool,
  network,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, h_flex, v_flex},
  xdg::{self, XdgIconCache},
};

struct FendInterrupt(Arc<AtomicBool>);

impl fend_core::Interrupt for FendInterrupt {
  fn should_interrupt(&self) -> bool {
    self.0.load(Ordering::Acquire)
  }
}

actions!(launcher, [Quit, GoBack]);
const CONTEXT: &str = "launcher";

pub struct Launcher {
  focus_handle: FocusHandle,
  picker: Entity<Picker<RootDelegate>>,
  active_panel: Option<AnyView>,
  timestamp_result: Option<SharedString>,
  fend_result: Option<SharedString>,
  fend_cancel_flag: Arc<AtomicBool>,
  fend_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl Launcher {
  pub fn get_window_options(no_keyboard_capture: bool) -> WindowOptions {
    let keyboard_interactivity = if no_keyboard_capture {
      KeyboardInteractivity::OnDemand
    } else {
      KeyboardInteractivity::Exclusive
    };

    WindowOptions {
      titlebar: None,
      app_id: Some("launch".to_string()),
      window_bounds: Some(WindowBounds::Windowed(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(800.), px(450.)),
      })),
      window_background: gpui::WindowBackgroundAppearance::Transparent,
      kind: WindowKind::LayerShell(LayerShellOptions {
        namespace: "launch".to_string(),
        layer: Layer::Overlay,
        anchor: Anchor::all(),
        exclusive_zone: None,
        exclusive_edge: None,
        margin: None,
        keyboard_interactivity,
      }),
      ..Default::default()
    }
  }

  pub fn new(window: &mut Window, cx: &mut Context<Self>, panel: Option<String>) -> Self {
    cx.bind_keys([
      KeyBinding::new("escape", Quit, Some(CONTEXT)),
      KeyBinding::new("shift-escape", GoBack, Some(CONTEXT)),
    ]);

    let launches = Arc::new(DB.get_launches());
    let icon_cache = XdgIconCache::global(cx);

    let mut items = vec![];

    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let (desktop_items, _) = xdg::get_items(&locales).unwrap();

    icon_cache.update(cx, |cache, cx| {
      cache.refresh(locales.clone(), cx);
    });

    items.extend(desktop_items);
    items.extend(audio::panels::get_items());
    items.extend(network::get_items());
    items.extend(bluetooth::get_items());
    items.extend(clipboard::get_items());

    let items = Arc::new(items);

    let picker = cx.new(|cx| {
      Picker::new(
        RootDelegate::new(launches, locales, icon_cache),
        items.clone(),
        window,
        cx,
      )
    });

    let active_panel = panel.as_ref().and_then(|panel| {
      items
        .iter()
        .find(|item| matches!(item, RootItem::Panel { id, .. } if id == panel))
        .map_or_else(
          || {
            error!("Could not find panel with id {panel}");
            None
          },
          |item| match &item {
            RootItem::Panel { view, .. } => Some(view(window, cx)),
            _ => None,
          },
        )
    });

    // Focus the appropriate view: panel if one was created via CLI, otherwise the picker
    if active_panel.is_none() {
      cx.focus_view(&picker.read(cx).search_input.clone(), window);
    }

    let subscriptions = vec![cx.subscribe_in(
      &picker,
      window,
      move |this, _, ev: &PickerEvent<RootDelegate>, window, cx| match ev {
        PickerEvent::Picked(item) => this.launch(item.clone(), window, cx),
        PickerEvent::QueryChanged(query) => {
          this.update_inline_results(query.clone(), window, cx);
        }
      },
    )];

    Self {
      focus_handle: cx.focus_handle(),
      picker,
      active_panel,
      timestamp_result: None,
      fend_result: None,
      fend_cancel_flag: Arc::new(AtomicBool::new(false)),
      fend_task: None,
      _subscriptions: subscriptions,
    }
  }

  fn try_parse_timestamp(query: &str) -> Option<SharedString> {
    let trimmed = query.trim();
    let len = trimmed.len();
    let timestamp: i64 = trimmed.parse().ok()?;

    // 10 digits = seconds, 13 = milliseconds, 19 = nanoseconds
    let datetime: DateTime<Local> = match len {
      10 => Local.timestamp_opt(timestamp, 0).single()?,
      13 => Local.timestamp_millis_opt(timestamp).single()?,
      19 => {
        let seconds = timestamp / 1_000_000_000;
        let nanos = (timestamp % 1_000_000_000) as u32;
        Local.timestamp_opt(seconds, nanos).single()?
      }
      _ => return None,
    };

    Some(SharedString::from(
      datetime.format("%A, %Y-%m-%d %H:%M:%S %Z").to_string(),
    ))
  }

  fn update_inline_results(
    &mut self,
    query: String,
    _window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.fend_cancel_flag.store(true, Ordering::Release);
    self.fend_cancel_flag = Arc::new(AtomicBool::new(false));
    self.timestamp_result = None;
    self.fend_result = None;
    self.fend_task = None;
    cx.notify();

    self.timestamp_result = Self::try_parse_timestamp(&query);

    if query.is_empty() {
      return;
    }

    let cancel_flag = self.fend_cancel_flag.clone();
    self.fend_task = Some(cx.spawn({
      let cancel_flag = cancel_flag.clone();
      async move |this, cx| {
        let result = cx
          .background_spawn(async move {
            let interrupt = FendInterrupt(cancel_flag);
            let mut context = fend_core::Context::new();
            let expr =
              fend_core::parse_with_interrupt(&query, &mut context, &interrupt).ok()?;
            if !expr.contains_computation() {
              return None;
            }
            let result =
              fend_core::evaluate_expr_with_interrupt(expr, &mut context, &interrupt).ok()?;
            if result.output_is_empty() {
              return None;
            }
            Some(SharedString::from(result.get_main_result().to_string()))
          })
          .await;

        if let Some(result) = result {
          this
            .update(cx, |this, cx| {
              this.fend_result = Some(result);
              cx.notify();
            })
            .log_err();
        }
      }
    }));
  }

  fn quit(&mut self, _: &Quit, window: &mut Window, _cx: &mut Context<Self>) {
    window.remove_window();
  }

  fn go_back(&mut self, _: &GoBack, window: &mut Window, cx: &mut Context<Self>) {
    if self.active_panel.take().is_some() {
      cx.notify();
      // Defer focusing the root picker so it happens after the panel is removed from the render tree
      let picker = self.picker.clone();
      window.defer(cx, move |window, cx| {
        window.focus(&picker.read(cx).search_input.focus_handle(cx), cx);
      });
    }
  }

  fn launch(&mut self, item: RootItem, window: &mut Window, cx: &mut Context<Self>) {
    DB.record_launch(&item.id());

    match &item {
      RootItem::App { entry, .. } => match xdg::start(entry) {
        Ok(_) => window.remove_window(),
        Err(err) => error!(?err, "Failed to start process"),
      },
      RootItem::Panel { view, .. } => {
        let panel = view(window, cx);
        self.active_panel = Some(panel);
        let search_input = self.picker.read(cx).search_input.clone();
        window.defer(cx, move |window, cx| {
          search_input.update(cx, |input, cx| {
            input.clean(window, cx);
          });
        });
        cx.notify();
      }
    }
  }
}

impl Focusable for Launcher {
  fn focus_handle(&self, _cx: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for Launcher {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .track_focus(&self.focus_handle)
      .font_family("Noto Sans")
      .text_color(rgb(0xFFFFFF))
      .on_action(cx.listener(Self::quit))
      .on_action(cx.listener(Self::go_back))
      .rounded_xl()
      .size_full()
      .border_1()
      .border_color(rgba(0xFFFFFF15))
      .bg(rgba(0x171717F0))
      .overflow_hidden()
      .when_some(self.active_panel.as_ref(), |div, panel| {
        div.child(panel.clone())
      })
      .when_none(&self.active_panel, |this| {
        this
          .child(picker_input(&self.picker).text_size(px(18.)))
          .when_some(
            self
              .timestamp_result
              .clone()
              .or_else(|| self.fend_result.clone()),
            |this, result| {
              this.child(
                h_flex()
                  .px_3()
                  .py_2()
                  .border_b_1()
                  .border_color(rgba(0xFFFFFF12))
                  .gap_2()
                  .text_color(rgb(0x888888))
                  .child("=")
                  .child(div().text_color(rgb(0xFFFFFF)).child(result)),
              )
            },
          )
          .child(picker_results(&self.picker))
      })
  }
}

struct RootDelegate {
  launches: Arc<HashMap<String, (u32, u64)>>,
  icon_cache: Entity<XdgIconCache>,
  xdg_locales: Vec<String>,
}

impl RootDelegate {
  fn new(
    launches: Arc<HashMap<String, (u32, u64)>>,
    xdg_locales: Vec<String>,
    icon_cache: Entity<XdgIconCache>,
  ) -> Self {
    Self {
      launches,
      xdg_locales,
      icon_cache,
    }
  }
}

type PanelView = dyn Fn(&mut Window, &mut App) -> AnyView + Send + Sync;

// TODO: Maybe we can put terms in contiguous memory?
#[derive(Clone)]
pub enum RootItem {
  App {
    name: SharedString,
    entry: DesktopEntry,
  },
  Panel {
    id: String,
    icon: IconName,
    name: SharedString,
    terms: Vec<String>,
    view: Arc<PanelView>,
  },
}

impl RootItem {
  pub fn id(&self) -> String {
    match self {
      RootItem::App { entry, .. } => format!("app:{}", entry.appid),
      RootItem::Panel { id, .. } => format!("panel:{}", id),
    }
  }

  pub fn type_label(&self) -> &'static str {
    match self {
      RootItem::App { .. } => "app",
      RootItem::Panel { .. } => "panel",
    }
  }
}

impl PickerDelegate for RootDelegate {
  type ListItem = RootItem;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let icon_cache = self.icon_cache.read(cx);

    let icon_size = rems(1.125);

    h_flex()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .justify_between()
      .child(h_flex().gap_3().items_center().map(|this| {
        match item {
          RootItem::App { entry, name } => {
            let icon = entry.icon().and_then(|icon| icon_cache.get(icon));

            this
              .when_some(icon, |this, icon| {
                this.child(img(ImageSource::Resource(icon.clone())).size(icon_size))
              })
              .when_none(&icon, |this| {
                this.child(Icon::new(IconName::AppWindow).size(icon_size))
              })
              .child(name.clone())
          }
          RootItem::Panel { name, icon, .. } => this
            .child(Icon::new(*icon).size(icon_size))
            .child(name.clone()),
        }
      }))
      .child(
        h_flex()
          .gap_1()
          .text_color(rgb(0x888888))
          .text_sm()
          .child(item.type_label()),
      )
  }

  fn sort_items(&self, _cx: &App, items: &[Self::ListItem], matches: &mut [(usize, u32)]) {
    matches.sort_by_key(|(i, score)| {
      let (count, last_launch) = self
        .launches
        .get(&items[*i].id())
        .copied()
        .unwrap_or_default();

      // Sort by score (descending), then count (descending), then last_launch (descending)
      Reverse((*score, count, last_launch))
    });
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
    if query.is_empty() {
      cx.defer_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, None);
      });

      return Task::ready(());
    }

    let locales = self.xdg_locales.clone();
    let matchers = MatcherPool::global(cx);

    cx.spawn_in(window, async move |picker, cx| {
      let Some(matches) = cx
        .background_spawn(async move {
          let mut matcher = matchers.get().await.unwrap();
          let mut matches = Vec::new();
          let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
          let mut buf = Vec::new();

          if cancel_flag.load(Ordering::Acquire) {
            return None;
          }

          for (i, item) in items.iter().enumerate() {
            let mut max_score: Option<u32> = None;
            let mut do_match = |term: &str| {
              let term = Utf32Str::new(term, &mut buf);
              let Some(score) = needle.score(term, &mut matcher) else {
                return;
              };

              max_score = Some(max_score.map_or(score, |max_score| max_score.max(score)));
            };

            match item {
              RootItem::App { entry, .. } => {
                do_match(&entry.appid);
                if let Some(name) = entry.name(&locales) {
                  do_match(&name);
                }

                if let Some(generic_name) = entry.generic_name(&locales) {
                  do_match(&generic_name);
                }

                if let Some(categories) = entry.categories() {
                  for category in categories {
                    do_match(category);
                  }
                }
              }
              RootItem::Panel { name, terms, .. } => {
                do_match(name);
                for term in terms {
                  do_match(term);
                }
              }
            }

            if let Some(score) = max_score {
              matches.push((i, score));
            }
          }

          if cancel_flag.load(Ordering::Acquire) {
            return None;
          }

          Some(matches)
        })
        .await
      else {
        return;
      };

      picker
        .update(cx, |picker, cx| {
          picker.complete_search(cx, search_id, Some(matches));
        })
        .log_err();
    })
  }
}
