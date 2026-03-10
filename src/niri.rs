use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  App, Context, Entity, FocusHandle, Focusable, ImageSource, IntoElement, SharedString,
  Subscription, Task, Window, img, prelude::*, rems, rgb, rgba,
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

pub fn get_items() -> Vec<RootItem> {
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

struct WindowsPanel {
  picker: Entity<Picker<WindowsDelegate>>,
  _load_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl WindowsPanel {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let icon_cache = XdgIconCache::global(cx);
    let picker = cx.new(|cx| {
      let delegate = WindowsDelegate { icon_cache };
      let mut picker = Picker::new(delegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search windows...", cx);
      picker
    });

    let subscriptions = vec![cx.subscribe_in(
      &picker,
      window,
      |_this, _picker, event, window, cx| {
        if let PickerEvent::Picked(item) = event {
          let window_id = item.id;
          cx.spawn_in(window, async move |_this, cx| {
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
      },
    )];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let load_task = Self::load_windows(&picker, window, cx);

    Self {
      picker,
      _load_task: Some(load_task),
      _subscriptions: subscriptions,
    }
  }

  fn load_windows(
    picker: &Entity<Picker<WindowsDelegate>>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    let picker = picker.clone();

    cx.spawn_in(window, async move |_this, cx| {
      let windows = cx
        .background_spawn(async move {
          let mut socket = niri_ipc::socket::Socket::connect()?;
          match socket.send(niri_ipc::Request::Windows)? {
            Ok(niri_ipc::Response::Windows(windows)) => Ok(windows),
            Ok(other) => anyhow::bail!("Unexpected response: {other:?}"),
            Err(message) => anyhow::bail!("Niri error: {message}"),
          }
        })
        .await;

      let windows: Vec<niri_ipc::Window> = match windows {
        Ok(windows) => windows,
        Err(error) => {
          tracing::error!(?error, "Failed to list niri windows");
          return;
        }
      };

      let items: Vec<WindowItem> = windows
        .into_iter()
        .map(|window| {
          let title: SharedString = window
            .title
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "(untitled)".to_string())
            .into();
          let app_id: SharedString = window.app_id.unwrap_or_default().into();
          let search_string = format!("{title} {app_id}");

          WindowItem {
            id: window.id,
            title,
            app_id,
            is_focused: window.is_focused,
            search_string,
          }
        })
        .collect();

      picker
        .update_in(cx, |picker, window, cx| {
          picker.set_items(items, window, cx);
        })
        .log_err();
    })
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
        this.child(img(ImageSource::Resource(icon.clone())).size(icon_size))
      })
      .when_none(&icon, |this| {
        this.child(Icon::new(IconName::AppWindow).size(icon_size))
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
