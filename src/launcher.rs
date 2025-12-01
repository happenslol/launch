use std::{
  cmp::Reverse,
  collections::HashMap,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use freedesktop_desktop_entry::DesktopEntry;
use futures::{StreamExt as _, stream::FuturesUnordered};
use gpui::{
  AnyView, App, AsyncWindowContext, Bounds, Entity, FocusHandle, Focusable, ImageSource,
  KeyBinding, Resource, SharedString, Size, Subscription, Task, WeakEntity, Window, WindowBounds,
  WindowKind, WindowOptions, actions, div, img,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rgb, rgba,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use tracing::error;

use crate::{
  audio,
  db::DB,
  network,
  picker::{Picker, PickerDelegate, PickerEvent},
  util::{ResultExt, h_flex, v_flex},
  xdg::{self, get_icon},
};

actions!(launcher, [Quit]);

pub struct Launcher {
  focus_handle: FocusHandle,
  picker: Entity<Picker<RootDelegate>>,
  active_panel: Option<AnyView>,
  xdg_icon_path_cache: Entity<HashMap<String, Resource>>,
  _subscriptions: Vec<Subscription>,
}

impl Launcher {
  pub fn get_window_options() -> WindowOptions {
    WindowOptions {
      titlebar: None,
      app_id: Some("launch".to_string()),
      window_bounds: Some(WindowBounds::Windowed(Bounds {
        origin: point(px(0.), px(0.)),
        size: Size::new(px(800.), px(800.)),
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

  pub fn new(window: &mut Window, cx: &mut Context<Self>, panel: Option<String>) -> Self {
    cx.bind_keys([KeyBinding::new("escape", Quit, None)]);

    let launches = Arc::new(DB.get_launches());
    let xdg_icon_path_cache = cx.new(|_| DB.get_desktop_entry_icon_paths());

    let mut items = vec![];

    let locales = freedesktop_desktop_entry::get_languages_from_env();
    let (desktop_items, icon_names) = xdg::get_items(&locales).unwrap();

    // Find app icon paths, this is pretty slow
    cx.spawn_in(window, async move |this, cx| {
      Self::refresh_app_icons(this, cx, icon_names).await
    })
    .detach();

    items.extend(desktop_items);
    items.extend(audio::panels::get_items());
    items.extend(network::get_items());

    let items = Arc::new(items);

    let picker = cx.new(|cx| {
      Picker::new(
        RootDelegate::new(launches, locales, xdg_icon_path_cache.clone()),
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

    let subscriptions = vec![cx.subscribe_in(
      &picker,
      window,
      move |this, _, ev: &PickerEvent<RootDelegate>, window, cx| match ev {
        PickerEvent::Picked(item) => this.launch(item.clone(), window, cx),
        PickerEvent::QueryChanged(_query) => {
          // let query = query.clone();
          //
          // cx.background_spawn(async move {
          //   println!("Query changed: {query}");
          //   let mut context = fend_core::Context::new();
          //   let result = fend_core::evaluate(&query, &mut context);
          //   println!("Result: {result:#?}");
          // })
          // .detach();
        }
      },
    )];

    cx.focus_self(window);

    Self {
      focus_handle: cx.focus_handle(),
      picker,
      active_panel,
      xdg_icon_path_cache,
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

  async fn refresh_app_icons(
    this: WeakEntity<Self>,
    cx: &mut AsyncWindowContext,
    icon_names: Vec<String>,
  ) {
    // TODO: fork the icon lookups crate and make it actually async
    let result = FuturesUnordered::from_iter(icon_names.chunks(10).map(|names| {
      let names = names.to_vec();
      cx.background_spawn(async move {
        names
          .iter()
          .filter_map(|name| get_icon(name).map(|icon| (name.clone(), icon)))
          .collect::<HashMap<_, _>>()
      })
    }))
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .flatten()
    .collect::<HashMap<_, _>>();

    DB.store_desktop_entry_icon_paths(&result);

    let result = result
      .into_iter()
      .map(|(k, v)| (k, Resource::Path(v.into())))
      .collect::<HashMap<_, _>>();

    this
      .update(cx, |this, cx| {
        this.xdg_icon_path_cache.update(cx, |cache, cx| {
          cache.extend(result);
          cx.notify();
        })
      })
      .log_err();
  }

  fn launch(&mut self, item: RootItem, window: &mut Window, cx: &mut Context<Self>) {
    DB.record_launch(&item.id());

    match &item {
      RootItem::App { entry, .. } => match xdg::start(entry) {
        Ok(_) => cx.quit(),
        Err(err) => error!(?err, "Failed to start process"),
      },
      RootItem::Panel { view, .. } => {
        let panel = view(window, cx);
        self.active_panel = Some(panel);
        cx.focus_self(window);
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
      .track_focus(&self.focus_handle)
      .font_family("Noto Sans")
      .text_color(rgb(0xFFFFFF))
      .on_action(cx.listener(Self::quit))
      .rounded_lg()
      .size_full()
      .border_1()
      .p_2()
      .border_color(rgb(0x444444))
      .bg(rgba(0x212121FF))
      .when_some(self.active_panel.as_ref(), |div, panel| {
        div.child(panel.clone())
      })
      .when_none(&self.active_panel, |div| div.child(self.picker.clone()))
  }
}

struct RootDelegate {
  launches: Arc<HashMap<String, (u32, u64)>>,
  xdg_icon_path_cache: Entity<HashMap<String, Resource>>,
  xdg_locales: Vec<String>,
}

impl RootDelegate {
  fn new(
    launches: Arc<HashMap<String, (u32, u64)>>,
    xdg_locales: Vec<String>,
    xdg_icon_path_cache: Entity<HashMap<String, Resource>>,
  ) -> Self {
    Self {
      launches,
      xdg_locales,
      xdg_icon_path_cache,
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
    icon: Option<Resource>,
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
    let icon_cache = self.xdg_icon_path_cache.read(cx);

    h_flex()
      .w_full()
      .p_1()
      .when(is_selected, |this| this.bg(rgb(0x444444)))
      .justify_between()
      .child(h_flex().gap_1().map(|this| {
        match item {
          RootItem::App { entry, name } => {
            let icon = entry.icon().and_then(|icon| icon_cache.get(icon));

            this
              .when_some(icon, |this, icon| {
                this.child(img(ImageSource::Resource(icon.clone())).size_8())
              })
              .when_none(&icon, |this| this.child(div().size_8()))
              .child(name.clone())
          }
          RootItem::Panel { name, icon, .. } => this
            .when_some(icon.as_ref(), |this, icon| {
              this.child(img(ImageSource::Resource(icon.clone())).size_8())
            })
            .when_none(icon, |this| this.child(div().size_8()))
            .child(name.clone()),
        }
      }))
      .child(
        h_flex()
          .gap_1()
          .text_color(rgb(0xAAAAAA))
          .child(item.type_label()),
      )
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
    let launches = self.launches.clone();

    cx.spawn_in(window, async move |picker, cx| {
      let Some(matches) = cx
        .background_spawn(async move {
          let mut matcher = Matcher::new(Config::DEFAULT);
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

          matches.sort_by_key(|(i, score)| {
            let (count, last_launch) = launches.get(&items[*i].id()).copied().unwrap_or_default();

            // Use launch count and recency as tie breaker
            Reverse((*score, count, last_launch))
          });

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
