mod assets;
mod logging;
mod text_input;
mod util;

use std::cmp::Reverse;

use anyhow::Result;
use freedesktop_file_parser::DesktopEntry;
use gpui::{
  App, Application, Bounds, Entity, FocusHandle, Focusable, KeyBinding, Size, Subscription, Window,
  WindowBounds, WindowKind, WindowOptions, actions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rgb, rgba, white,
};
use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Pattern};
use tracing::error;

use crate::{
  assets::{Assets, load_embedded_fonts},
  text_input::{TextInput, TextInputEvent},
  util::v_flex,
};

fn main() -> Result<()> {
  logging::init();

  Application::new().with_assets(Assets).run(move |cx| {
    Launcher::init(cx);
    TextInput::init(cx);

    load_embedded_fonts(cx).unwrap();
    show_launcher(cx);
  });

  Ok(())
}

fn show_launcher(cx: &mut App) {
  let options = WindowOptions {
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
      anchor: Anchor::TOP | Anchor::RIGHT,
      exclusive_zone: None,
      exclusive_edge: None,
      margin: Some((px(100.), px(100.), px(0.), px(0.))),
      keyboard_interactivity: KeyboardInteractivity::OnDemand,
    }),
    ..Default::default()
  };

  if let Err(err) = cx.open_window(options, Launcher::view) {
    error!(?err, "Failed to open window");
    cx.quit();
  }
}

actions!(root, [Quit]);

struct Launcher {
  focus_handle: FocusHandle,
  search_input: Entity<TextInput>,
  query: String,
  entries: Vec<DesktopEntry>,
  matches: Vec<(usize, u32)>,
  matcher: Matcher,
  subscriptions: Vec<Subscription>,
}

impl Launcher {
  pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.bind_keys([KeyBinding::new("escape", Quit, None)]);
  }

  pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
    cx.new(|cx| Self::new(window, cx))
  }

  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle();
    let search_input = cx.new(|cx| TextInput::new(window, cx));
    let entries = find_all_desktop_entries().unwrap();
    let matcher = Matcher::new(Config::DEFAULT);

    let mut this = Self {
      search_input: search_input.clone(),
      focus_handle,
      entries,
      matches: Vec::new(),
      matcher,
      query: String::new(),
      subscriptions: Vec::new(),
    };

    this
      .subscriptions
      .extend([cx.subscribe_in(&search_input, window, {
        let search_input = search_input.clone();
        move |this, _, ev: &TextInputEvent, _window, cx| {
          if *ev != TextInputEvent::Change {
            return;
          }

          let value = &search_input.read(cx).content;
          if this.query == *value {
            return;
          }

          // We're gonna re-match, so we clear in any case
          this.matches.clear();
          this.query = value.to_string();

          if this.query.is_empty() {
            return;
          }

          let pattern = Pattern::new(
            value,
            nucleo_matcher::pattern::CaseMatching::Smart,
            nucleo_matcher::pattern::Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
          );

          let mut buf = Vec::new();

          for (i, entry) in this.entries.iter().enumerate() {
            if let Some(score) = pattern.score(
              Utf32Str::new(&entry.name.default, &mut buf),
              &mut this.matcher,
            ) {
              this.matches.push((i, score));
            }
          }

          this
            .matches
            .sort_unstable_by_key(|(_, score)| Reverse(*score));

          cx.notify();
        }
      })]);

    this
  }
}

impl Focusable for Launcher {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for Launcher {
  fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
    let children: Vec<String> = if self.matches.is_empty() {
      if self.query.is_empty() {
        self
          .entries
          .iter()
          .map(|entry| entry.name.default.clone())
          .collect()
      } else {
        Vec::new()
      }
    } else {
      self
        .matches
        .iter()
        .map(|(i, _)| self.entries[*i].name.default.clone())
        .collect()
    };

    v_flex()
      .size_full()
      .bg(rgba(0x00000015))
      .child(self.search_input.clone())
      .child(
        v_flex()
          .bg(rgb(0x333333))
          .id("results")
          .overflow_y_scroll()
          .max_h_full()
          .children(
            children
              .into_iter()
              .map(|entry| div().bg(rgb(0x333333)).text_color(white()).child(entry)),
          ),
      )
  }
}

fn find_all_desktop_entries() -> Result<Vec<DesktopEntry>> {
  let data_dirs = std::env::var("XDG_DATA_DIRS").ok().map(|p| {
    std::env::split_paths(&p)
      .filter(|p| p.is_absolute())
      .collect::<Vec<_>>()
  });

  let app_dirs = data_dirs.map(|dirs| {
    dirs
      .iter()
      .map(|dir| dir.join("applications"))
      .filter(|dir| dir.try_exists().unwrap_or_default())
      .collect::<Vec<_>>()
  });

  let Some(app_dirs) = app_dirs else {
    return Ok(Vec::new());
  };

  let mut applications = Vec::new();

  for dir in app_dirs {
    let walker = walkdir::WalkDir::new(&dir).into_iter();

    // TODO: Deduplicate by dektop id
    for entry in walker.filter_entry(|e| {
      e.file_type().is_dir()
        || e
          .file_name()
          .to_str()
          .is_some_and(|s| s.ends_with(".desktop"))
    }) {
      let Ok(entry) = entry else {
        continue;
      };

      if entry.file_type().is_dir() {
        continue;
      }

      let contents = std::fs::read_to_string(entry.path())?;
      let parsed = freedesktop_file_parser::parse(&contents)?;
      applications.push(parsed.entry);
    }
  }

  Ok(applications)
}
