use std::collections::HashMap;
use std::sync::{Arc, atomic::AtomicBool};

use gpui::{
  AnyElement, App, Context, Entity, FocusHandle, Focusable, Image, ImageFormat, ImageSource,
  IntoElement, KeyBinding, ObjectFit, Render, SharedString, Styled, Subscription, Task, Window,
  actions, img, prelude::*, rems, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  confirmation::{ConfirmationEvent, ConfirmationPrompt, render_confirmation_overlay},
  icon::{Icon, IconName},
  launcher::RootItem,
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  util::{ResultExt, h_flex, v_flex},
  wayland::{self, ClipboardDbReader, ClipboardEntry, ContentType},
};

actions!(clipboard, [DeleteEntry]);

const TEXT_MIME_TYPES: &[&str] = &[
  "text/plain;charset=utf-8",
  "text/plain",
  "UTF8_STRING",
  "STRING",
  "TEXT",
];

pub fn get_items() -> Vec<RootItem> {
  vec![RootItem::Panel {
    id: "clipboard".into(),
    icon: IconName::Clipboard,
    name: "Clipboard history".into(),
    description: "Browse and paste from clipboard history".into(),
    terms: vec!["clipboard".into(), "paste".into(), "history".into()],
    view: Arc::new(|window, cx| cx.new(|cx| ClipboardPanel::new(window, cx)).into()),
  }]
}

#[derive(Clone)]
struct ClipboardItem {
  id: i64,
  timestamp: i64,
  content_type: ContentType,
  preview: SharedString,
  search_string: String,
}

impl ClipboardItem {
  fn from_entry(entry: ClipboardEntry) -> Self {
    let preview: SharedString = entry.preview.clone().into();
    let search_string = entry.preview.clone();

    Self {
      id: entry.id,
      timestamp: entry.timestamp,
      content_type: entry.content_type,
      preview,
      search_string,
    }
  }
}

fn icon_for_content_type(content_type: ContentType) -> IconName {
  match content_type {
    ContentType::Text => IconName::Clipboard,
    ContentType::Url => IconName::Link,
    ContentType::Code => IconName::Code,
    ContentType::File => IconName::FileText,
    ContentType::Image => IconName::Photo,
    ContentType::Other => IconName::FileUnknown,
  }
}

enum PreviewContent {
  Text(SharedString),
  Image(Arc<Image>),
}

struct PreviewState {
  item_id: i64,
  content: PreviewContent,
}

struct ClipboardPanel {
  picker: Entity<Picker<ClipboardDelegate>>,
  connection: Entity<wayland::WaylandConnection>,
  preview: Option<PreviewState>,
  confirmation_prompt: Option<Entity<ConfirmationPrompt>>,
  _preview_task: Option<Task<()>>,
  _load_task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl ClipboardPanel {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let connection = wayland::WaylandConnection::global(cx);

    let picker = cx.new(|cx| {
      let mut picker = Picker::new(ClipboardDelegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search clipboard history...", cx);
      picker
    });

    let subscriptions = vec![
      cx.subscribe_in(&picker, window, |this, _picker, event, window, cx| {
        if let PickerEvent::Picked(item) = event {
          this
            .connection
            .read(cx)
            .send_command(wayland::Command::CopyHistoryEntry { id: item.id });
          window.remove_window();
        }
      }),
      cx.subscribe_in(&connection, window, |this, _connection, _event, window, cx| {
        this._load_task = Some(Self::load_entries(&this.picker, window, cx));
      }),
      cx.observe(&picker, |_this, _picker, cx| {
        cx.notify();
      }),
    ];

    cx.bind_keys([KeyBinding::new("ctrl-d", DeleteEntry, None)]);

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let load_task = Self::load_entries(&picker, window, cx);

    Self {
      picker,
      connection,
      preview: None,
      confirmation_prompt: None,
      _preview_task: None,
      _load_task: Some(load_task),
      _subscriptions: subscriptions,
    }
  }

  fn load_entries(
    picker: &Entity<Picker<ClipboardDelegate>>,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) -> Task<()> {
    let reader = ClipboardDbReader::global(cx);
    let picker = picker.clone();

    cx.spawn_in(window, async move |_this, cx| {
      let Some(reader) = reader else {
        return;
      };

      let entries = cx.background_spawn(async move { reader.recent(50) }).await;

      let Ok(entries) = entries else {
        return;
      };

      let items: Vec<ClipboardItem> = entries.into_iter().map(ClipboardItem::from_entry).collect();

      picker
        .update_in(cx, |picker, window, cx| {
          picker.set_items(items, window, cx);
        })
        .log_err();
    })
  }

  fn load_preview(
    &mut self,
    item_id: i64,
    content_type: ContentType,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let reader = ClipboardDbReader::global(cx);

    self._preview_task = Some(cx.spawn_in(window, async move |this, cx| {
      let Some(reader) = reader else {
        return;
      };

      let mime_data: anyhow::Result<HashMap<String, Vec<u8>>> =
        cx.background_spawn(async move { reader.get_mime_data_by_id(item_id) }).await;

      let Ok(mime_data) = mime_data else {
        return;
      };

      let content = match content_type {
        ContentType::Image => {
          let image_entry = mime_data
            .iter()
            .find(|(mime, _)| mime.starts_with("image/"));

          if let Some((mime, bytes)) = image_entry {
            if let Some(format) = ImageFormat::from_mime_type(mime) {
              Some(PreviewContent::Image(Arc::new(Image::from_bytes(
                format,
                bytes.clone(),
              ))))
            } else {
              None
            }
          } else {
            None
          }
        }
        _ => {
          let text = TEXT_MIME_TYPES
            .iter()
            .find_map(|mime| mime_data.get(*mime))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("");

          Some(PreviewContent::Text(SharedString::from(text.to_string())))
        }
      };

      if let Some(content) = content {
        this
          .update_in(cx, |this, _window, cx| {
            this.preview = Some(PreviewState { item_id, content });
            cx.notify();
          })
          .log_err();
      }
    }));
  }

  fn delete_entry(&mut self, _: &DeleteEntry, window: &mut Window, cx: &mut Context<Self>) {
    let Some(item) = self.picker.read(cx).get_selected_item().cloned() else {
      return;
    };

    let prompt = cx.new(|cx| ConfirmationPrompt::new(window, cx));
    let picker = self.picker.clone();
    let connection = self.connection.clone();

    let subscription = cx.subscribe_in(
      &prompt,
      window,
      move |this, _, event: &ConfirmationEvent, window, cx| {
        if let ConfirmationEvent::Confirm = event {
          picker.update(cx, |picker, cx| {
            picker.remove_selected_item(window, cx);
          });

          if this
            .preview
            .as_ref()
            .is_some_and(|preview| preview.item_id == item.id)
          {
            this.preview = None;
            this._preview_task = None;
          }

          connection
            .read(cx)
            .send_command(wayland::Command::DeleteHistoryEntry { id: item.id });
        }

        this.confirmation_prompt = None;
        cx.focus_view(&this.picker.read(cx).search_input.clone(), window);
        cx.notify();
      },
    );

    self._subscriptions.push(subscription);
    self.confirmation_prompt = Some(prompt);
    cx.notify();
  }

  fn render_preview(&self) -> AnyElement {
    match &self.preview {
      None => gpui::div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .child(
          gpui::div()
            .text_sm()
            .text_color(rgb(0x555555))
            .child("No item selected"),
        )
        .into_any_element(),
      Some(PreviewState {
        content: PreviewContent::Text(text),
        ..
      }) => gpui::div()
        .id("preview-text")
        .size_full()
        .overflow_y_scroll()
        .p_3()
        .text_sm()
        .child(text.clone())
        .into_any_element(),
      Some(PreviewState {
        content: PreviewContent::Image(image),
        ..
      }) => gpui::div()
        .size_full()
        .child(
          img(ImageSource::Image(image.clone()))
            .object_fit(ObjectFit::Contain)
            .w_full()
            .max_h(rems(20.)),
        )
        .into_any_element(),
    }
  }
}

impl Focusable for ClipboardPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.picker.read(cx).focus_handle(cx)
  }
}

impl Render for ClipboardPanel {
  fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    let selected_item = self.picker.read(cx).get_selected_item().cloned();

    if let Some(item) = &selected_item {
      let needs_load = self
        .preview
        .as_ref()
        .map_or(true, |preview| preview.item_id != item.id);

      if needs_load {
        self.load_preview(item.id, item.content_type, window, cx);
      }
    } else if self.preview.is_some() {
      self.preview = None;
      self._preview_task = None;
    }

    v_flex()
      .size_full()
      .relative()
      .on_action(cx.listener(Self::delete_entry))
      .child(picker_input(&self.picker).show_back_button(true))
      .child(
        h_flex()
          .flex_grow()
          .overflow_hidden()
          .child(
            v_flex()
              .w(rems(18.))
              .flex_shrink_0()
              .h_full()
              .overflow_hidden()
              .child(picker_results(&self.picker)),
          )
          .child(
            gpui::div()
              .flex_grow()
              .min_w_0()
              .h_full()
              .overflow_hidden()
              .border_l_1()
              .border_color(rgba(0xFFFFFF12))
              .child(self.render_preview()),
          ),
      )
      .when_some(self.confirmation_prompt.clone(), |this, prompt| {
        this.child(render_confirmation_overlay(&prompt))
      })
  }
}

fn format_timestamp(timestamp: i64) -> SharedString {
  let now = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_secs() as i64)
    .unwrap_or(0);

  let delta = now - timestamp;

  let text = if delta < 60 {
    "just now".to_string()
  } else if delta < 3600 {
    let minutes = delta / 60;
    if minutes == 1 {
      "1 min ago".to_string()
    } else {
      format!("{minutes} mins ago")
    }
  } else if delta < 86400 {
    let hours = delta / 3600;
    if hours == 1 {
      "1 hour ago".to_string()
    } else {
      format!("{hours} hours ago")
    }
  } else {
    let days = delta / 86400;
    if days == 1 {
      "1 day ago".to_string()
    } else {
      format!("{days} days ago")
    }
  };

  SharedString::from(text)
}

struct ClipboardDelegate;

impl PickerDelegate for ClipboardDelegate {
  type ListItem = ClipboardItem;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    h_flex()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .gap_3()
      .items_center()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(
        Icon::new(icon_for_content_type(item.content_type))
          .size(rems(0.85))
          .text_color(rgb(0x666666)),
      )
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
              .when(item.preview.is_empty(), |this| {
                this.text_color(rgb(0x555555)).child("(blank)")
              })
              .when(!item.preview.is_empty(), |this| {
                this.child(item.preview.clone())
              }),
          )
          .child(
            gpui::div()
              .text_sm()
              .text_color(rgb(0x666666))
              .flex_shrink_0()
              .child(format_timestamp(item.timestamp)),
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
