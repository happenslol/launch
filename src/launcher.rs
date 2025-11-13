use std::{
  ops::Range,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use async_lock::Mutex;
use freedesktop_file_parser::DesktopEntry;
use gpui::{
  AnyView, App, Bounds, Entity, FocusHandle, Focusable, KeyBinding, ScrollStrategy, SharedString,
  Size, Subscription, Task, UniformListScrollHandle, Window, WindowBounds, WindowKind,
  WindowOptions, actions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rgb, rgba, uniform_list, white,
};
use nucleo_matcher::{
  Config, Matcher, Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  audio::{self, AudioStateAppExt},
  text_input::{TextInput, TextInputEvent},
  util::v_flex,
  xdg,
};

actions!(root, [Quit, SelectNext, SelectPrev]);

type SectionView = dyn Fn(&mut Window, &mut App) -> AnyView + Send + Sync;

pub enum ItemAction {
  Launch(Box<DesktopEntry>),
  Section(Box<SectionView>),
}

pub struct Item {
  pub name: SharedString,
  pub action: ItemAction,
}

pub struct Launcher {
  focus_handle: FocusHandle,
  search_input: Entity<TextInput>,
  current_query: String,
  selected_item: Option<usize>,
  scroll_handle: UniformListScrollHandle,
  active_section: Option<AnyView>,

  items: Arc<Vec<Item>>,
  matches: Option<Vec<(usize, u32)>>,
  matcher: Arc<Mutex<Matcher>>,

  subscriptions: Vec<Subscription>,

  // Picker (maybe pull this out?)
  // The id currently displayed search
  search_id: Option<usize>,
  // Sequence used to generate new search ids
  next_search_id: usize,
  // Condition var to signal old search tasks to stop
  cancel_flag: Arc<AtomicBool>,
  // In-progress search task (drop to cancel)
  search_task: Option<Task<()>>,
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
        keyboard_interactivity: KeyboardInteractivity::OnDemand,
      }),
      ..Default::default()
    }
  }

  pub fn view(window: &mut Window, cx: &mut App) -> Entity<Self> {
    cx.new(|cx| Self::new(window, cx))
  }

  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("escape", Quit, None),
      KeyBinding::new("down", SelectNext, None),
      KeyBinding::new("up", SelectPrev, None),
    ]);

    let focus_handle = cx.focus_handle();
    let search_input = cx.new(|cx| TextInput::new(window, cx));
    let matcher = Matcher::new(Config::DEFAULT);

    let mut items = vec![];

    items.extend(xdg::get_items().unwrap());
    // items.extend(audio::get_items().unwrap());

    let mut this = Self {
      search_input: search_input.clone(),
      selected_item: None,
      focus_handle,
      subscriptions: Vec::new(),
      items: Arc::new(items),
      matches: None,
      matcher: Arc::new(Mutex::new(matcher)),
      current_query: String::new(),
      scroll_handle: UniformListScrollHandle::new(),
      active_section: None,

      search_id: None,
      next_search_id: 0,
      cancel_flag: Arc::new(AtomicBool::new(false)),
      search_task: None,
    };

    this
      .subscriptions
      .extend([cx.subscribe_in(&search_input, window, {
        let search_input = search_input.clone();
        move |this, _, ev: &TextInputEvent, window, cx| match *ev {
          TextInputEvent::Submit => this.launch(window, cx),
          TextInputEvent::Change => {
            // See if something actually changed
            let new_value = &search_input.read(cx).content.trim();
            if &this.current_query == new_value {
              return;
            }

            // Update our search results
            this.current_query = new_value.to_string();
            this.update_matches(window, cx);
          }
          _ => {}
        }
      })]);

    this
  }

  // Uses current_query to kick off a search task
  fn update_matches(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    // Cancel ongoing search
    self.cancel_flag.store(true, Ordering::Release);
    self.cancel_flag = Arc::new(AtomicBool::new(false));
    self.search_task = None;

    let current_search_id = self.next_search_id;
    self.next_search_id += 1;

    // Show all items
    if self.current_query.is_empty() {
      self.search_id = Some(current_search_id);
      self.matches = None;
      cx.notify();
      return;
    }

    let items = self.items.clone();
    let cancel_flag = self.cancel_flag.clone();
    let query = self.current_query.clone();
    let matcher = self.matcher.clone();

    self.search_task = Some(cx.spawn_in(window, async move |this, cx| {
      let matches = cx
        .background_spawn(async move {
          if cancel_flag.load(Ordering::Acquire) {
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

      let _ = this.update(cx, |this, cx| {
        // Check if the search id is still valid right before we synchronously update the matches
        if this.search_id.is_some_and(|id| id > current_search_id) {
          return;
        }

        // Clamp the selected item, the item count might have changed
        this.selected_item = if !matches.is_empty() {
          Some(
            this
              .selected_item
              .map(|i| i.min(matches.len() - 1))
              .unwrap_or(0),
          )
        } else {
          None
        };

        this.search_id = Some(current_search_id);
        this.matches = Some(matches);

        cx.notify();
      });
    }));
  }

  fn select_next(&mut self, _: &SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self
      .matches
      .as_ref()
      .map_or(self.items.len(), |matches| matches.len());
    if item_count == 0 {
      return;
    }

    // Wrap around to first item
    match self.selected_item {
      Some(i) => self.selected_item = Some((i + 1) % item_count),
      None => self.selected_item = Some(0),
    }

    self
      .scroll_handle
      .scroll_to_item(self.selected_item.unwrap(), ScrollStrategy::Bottom);

    cx.notify();
  }

  fn select_prev(&mut self, _: &SelectPrev, _window: &mut Window, cx: &mut Context<Self>) {
    let item_count = self
      .matches
      .as_ref()
      .map_or(self.items.len(), |matches| matches.len());
    if item_count == 0 {
      return;
    }

    match self.selected_item {
      Some(0) | None => self.selected_item = Some(item_count - 1),
      Some(i) => self.selected_item = Some(i - 1),
    }

    self
      .scroll_handle
      .scroll_to_item(self.selected_item.unwrap(), ScrollStrategy::Top);

    cx.notify();
  }

  fn quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
    if self.active_section.take().is_some() {
      cx.focus_view(&self.search_input, window);
      cx.notify();
      return;
    }

    cx.quit();
  }

  fn launch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let audio = cx.audio();

    // let Some(item) = self.selected_item.map(|i| {
    //   self
    //     .matches
    //     .as_ref()
    //     .map_or_else(|| &self.items[i], |matches| &self.items[matches[i].0])
    // }) else {
    //   return;
    // };
    //
    // match &item.action {
    //   ItemAction::Launch(entry) => xdg::start(entry),
    //   ItemAction::Section(make_section) => {
    //     self.active_section = Some(make_section(window, cx));
    //     cx.notify();
    //   }
    // }
  }
}

impl Focusable for Launcher {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for Launcher {
  fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .track_focus(&self.focus_handle)
      .on_action(cx.listener(Self::quit))
      .on_action(cx.listener(Self::select_next))
      .on_action(cx.listener(Self::select_prev))
      .size_full()
      .bg(rgba(0x00000015))
      .when_some(self.active_section.as_ref(), |this, section| {
        this.child(section.clone())
      })
      .when_none(&self.active_section, |this| {
        this.child(self.search_input.clone()).child(
          uniform_list(
            "results",
            self
              .matches
              .as_ref()
              .map_or_else(|| self.items.len(), |matches| matches.len()),
            cx.processor(move |this, range: Range<usize>, _window, _cx| {
              range
                .map(|i| {
                  let is_selected = this
                    .selected_item
                    .is_some_and(|selected_idx| selected_idx == i);

                  let item_index = this.matches.as_ref().map_or(i, |matches| matches[i].0);
                  let item = &this.items[item_index];

                  div()
                    .id(i)
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .bg(white())
                    .when(is_selected, |div| div.bg(rgb(0xDDDDDD)))
                    .when(!is_selected, |div| div.hover(|div| div.bg(rgb(0xAAAAAA))))
                    .child(item.name.clone())
                })
                .collect()
            }),
          )
          .track_scroll(self.scroll_handle.clone())
          .h_full(),
        )
      })
  }
}

fn get_matches(matcher: &mut Matcher, items: &[Item], query: &str) -> Vec<(usize, u32)> {
  let mut matches = Vec::new();

  let needle = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
  let mut buf = Vec::new();
  for (i, item) in items.iter().enumerate() {
    if let Some(score) = needle.score(Utf32Str::new(item.name.as_ref(), &mut buf), matcher) {
      matches.push((i, score));
    };
  }

  matches
}
