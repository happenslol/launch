use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, Context, Entity, EventEmitter, FocusHandle, Focusable, Global, ImageSource, IntoElement,
  SharedString, Subscription, Task, Window, img, prelude::*, rems, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  icon::{Icon, IconName},
  launcher::RootItem,
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, h_flex, v_flex},
  xdg::XdgIconCache,
};

pub struct NiriState {
  windows: Vec<niri_ipc::Window>,
}

struct GlobalNiriState(Entity<NiriState>);
impl Global for GlobalNiriState {}

#[derive(Debug, Clone)]
pub enum NiriEvent {
  WindowsChanged,
}

impl EventEmitter<NiriEvent> for NiriState {}

impl NiriState {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalNiriState>().0.clone()
  }

  pub fn windows(&self) -> &[niri_ipc::Window] {
    &self.windows
  }

  pub fn find_window_by_app_id(&self, app_id: &str) -> Option<u64> {
    self.windows
      .iter()
      .find(|w| w.app_id.as_deref() == Some(app_id))
      .map(|w| w.id)
  }

  fn apply_event(&mut self, event: niri_ipc::Event) {
    match event {
      niri_ipc::Event::WindowsChanged { windows } => {
        self.windows = windows;
      }
      niri_ipc::Event::WindowOpenedOrChanged { window } => {
        if let Some(existing) = self.windows.iter_mut().find(|w| w.id == window.id) {
          *existing = window;
        } else {
          self.windows.push(window);
        }
      }
      niri_ipc::Event::WindowClosed { id } => {
        self.windows.retain(|w| w.id != id);
      }
      niri_ipc::Event::WindowFocusChanged { id } => {
        for window in &mut self.windows {
          window.is_focused = Some(window.id) == id;
        }
      }
      _ => {}
    }
  }
}

pub fn init(cx: &mut App) {
  if std::env::var_os("NIRI_SOCKET").is_none() {
    return;
  }

  let (event_tx, event_rx) = flume::unbounded::<niri_ipc::Event>();

  std::thread::spawn(move || {
    if let Err(error) = run_event_stream(event_tx) {
      tracing::error!(?error, "Niri event stream ended");
    }
  });

  let state = cx.new(|_| NiriState {
    windows: Vec::new(),
  });
  cx.set_global(GlobalNiriState(state.clone()));

  cx.spawn({
    let state = state.clone();
    async move |cx| {
      while let Ok(event) = event_rx.recv_async().await {
        state.update(cx, |state, cx| {
          state.apply_event(event);
          cx.emit(NiriEvent::WindowsChanged);
          cx.notify();
        });
      }
    }
  })
  .detach();
}

fn run_event_stream(event_tx: flume::Sender<niri_ipc::Event>) -> anyhow::Result<()> {
  let mut socket = niri_ipc::socket::Socket::connect()?;
  let reply = socket.send(niri_ipc::Request::EventStream)?;
  if let Err(message) = reply {
    anyhow::bail!("Niri rejected EventStream request: {message}");
  }

  let mut read_event = socket.read_events();
  loop {
    let event = read_event()?;
    if event_tx.send(event).is_err() {
      break;
    }
  }

  Ok(())
}

pub fn find_window_by_app_id(app_id: &str, cx: &App) -> Option<u64> {
  let state = cx.try_global::<GlobalNiriState>()?;
  state.0.read(cx).find_window_by_app_id(app_id)
}

pub fn focus_niri_window(window_id: u64, window: &mut Window, cx: &App) {
  window
    .spawn(cx, async move |cx| {
      cx.background_spawn(async move {
        let mut socket = niri_ipc::socket::Socket::connect()?;
        let action = niri_ipc::Action::FocusWindow { id: window_id };
        let reply = socket.send(niri_ipc::Request::Action(action))?;
        if let Err(message) = reply {
          anyhow::bail!("Niri error: {message}");
        }
        anyhow::Ok(())
      })
      .await
      .log_err();

      cx.update(|window, _cx| {
        window.remove_window();
      })
      .log_err();
    })
    .detach();
}

pub fn get_items(cx: &App) -> Vec<RootItem> {
  if cx.try_global::<GlobalNiriState>().is_none() {
    return vec![];
  }

  vec![RootItem::Panel {
    id: "windows".into(),
    icon: IconName::AppWindow,
    name: "Windows".into(),
    description: "Switch between open windows".into(),
    terms: vec!["windows".into(), "switch".into(), "focus".into()],
    view: Arc::new(|window, cx| cx.new(|cx| WindowsPanel::new(window, cx)).into()),
  }]
}

#[derive(Clone)]
struct WindowItem {
  id: u64,
  title: SharedString,
  app_id: SharedString,
  is_focused: bool,
  search_string: String,
}

fn windows_to_items(windows: &[niri_ipc::Window]) -> Vec<WindowItem> {
  windows
    .iter()
    .map(|window| {
      let title: SharedString = window
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .unwrap_or("(untitled)")
        .to_string()
        .into();
      let app_id: SharedString = window
        .app_id
        .clone()
        .unwrap_or_default()
        .into();
      let search_string = format!("{title} {app_id}");

      WindowItem {
        id: window.id,
        title,
        app_id,
        is_focused: window.is_focused,
        search_string,
      }
    })
    .collect()
}

struct WindowsPanel {
  picker: Entity<Picker<WindowsDelegate>>,
  _subscriptions: Vec<Subscription>,
}

impl WindowsPanel {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let icon_cache = XdgIconCache::global(cx);
    let niri_state = NiriState::global(cx);

    let initial_items = windows_to_items(niri_state.read(cx).windows());

    let picker = cx.new(|cx| {
      let delegate = WindowsDelegate { icon_cache };
      let mut picker = Picker::new(delegate, Arc::new(initial_items), window, cx);
      picker.placeholder("Search windows...", cx);
      picker
    });

    let mut subscriptions = vec![cx.subscribe_in(
      &picker,
      window,
      |_this, _picker, event, window, cx| {
        if let PickerEvent::Picked(item) = event {
          focus_niri_window(item.id, window, cx);
        }
      },
    )];

    subscriptions.push(cx.subscribe_in(
      &niri_state,
      window,
      {
        let picker = picker.clone();
        move |_this, niri_state, _event: &NiriEvent, window, cx| {
          let items = windows_to_items(niri_state.read(cx).windows());
          picker.update(cx, |picker, cx| {
            picker.set_items(items, window, cx);
          });
        }
      },
    ));

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    Self {
      picker,
      _subscriptions: subscriptions,
    }
  }
}

impl Focusable for WindowsPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for WindowsPanel {
  fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .size_full()
      .child(picker_input(&self.picker).show_back_button(true))
      .child(picker_results(&self.picker))
  }
}

struct WindowsDelegate {
  icon_cache: Entity<XdgIconCache>,
}

impl PickerDelegate for WindowsDelegate {
  type ListItem = WindowItem;

  fn render_list_item(
    &self,
    _window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let icon_cache = self.icon_cache.read(cx);
    let icon = icon_cache.get(&item.app_id.to_lowercase());
    let icon_size = rems(1.2);

    h_flex()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .gap_3()
      .items_center()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .when_some(icon, |this, icon| {
        this.child(img(ImageSource::Resource(icon.clone())).size(icon_size).flex_shrink_0())
      })
      .when_none(&icon, |this| {
        this.child(Icon::new(IconName::AppWindow).size(icon_size).flex_shrink_0())
      })
      .child(
        h_flex()
          .flex_grow()
          .overflow_x_hidden()
          .justify_between()
          .gap_2()
          .child(
            gpui::div()
              .text_ellipsis()
              .overflow_x_hidden()
              .when(item.is_focused, |this| this.text_color(rgb(0xCCCCCC)))
              .child(item.title.clone()),
          )
          .child(
            gpui::div()
              .text_sm()
              .text_color(rgb(0x666666))
              .flex_shrink_0()
              .child(item.app_id.clone()),
          ),
      )
  }

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    query: String,
    _cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    if query.is_empty() {
      cx.defer_in(window, move |picker, _window, cx| {
        picker.complete_search(cx, search_id, None);
      });

      return Task::ready(());
    }

    let matchers = MatcherPool::global(cx);
    cx.spawn_in(window, async move |picker, cx| {
      let mut matcher = matchers.get().await.unwrap();
      let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
      let mut matches = Vec::new();
      let mut buf = Vec::new();

      for (index, item) in items.iter().enumerate() {
        if let Some(score) =
          needle.score(Utf32Str::new(&item.search_string, &mut buf), &mut matcher)
        {
          matches.push((index, score));
        }
      }

      picker
        .update_in(cx, move |picker, _window, cx| {
          picker.complete_search(cx, search_id, Some(matches));
        })
        .log_err();
    })
  }
}
