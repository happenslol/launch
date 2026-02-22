use std::ops::Range;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{
  AnyElement, App, Context, Div, Entity, FocusHandle, Focusable, FontStyle, FontWeight,
  HighlightStyle, KeyBinding, Render, ScrollHandle, SharedString, StrikethroughStyle, StyledText,
  Subscription, Task, Window, actions, div, prelude::*, px, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::providers::openrouter;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use tracing::{error, info};

use crate::{
  confirmation::{ConfirmationEvent, ConfirmationPrompt, render_confirmation_overlay},
  db::DB,
  icon::IconName,
  input::{self, input, state::InputState},
  launcher::RootItem,
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  scrollbar::Scrollbar,
  tokio::TokioExt,
  util::{ResultExt, v_flex},
};

const MODEL: &str = "google/gemini-2.5-flash";
const CONTEXT: &str = "llm";
const CONVERSATION_PICKER_CONTEXT: &str = "llm_conversation_picker";

actions!(
  llm,
  [OpenConversationPicker, CloseConversationPicker, DeleteConversation]
);

pub fn get_items() -> Vec<RootItem> {
  vec![RootItem::Panel {
    id: "llm".into(),
    icon: IconName::MessageChatbot,
    name: "LLM Chat".into(),
    description: "Talk to an LLM".into(),
    terms: vec!["llm".into(), "chat".into(), "ai".into()],
    view: Arc::new(|window, cx| cx.new(|cx| LlmPanel::new(window, cx)).into()),
  }]
}

#[derive(Clone)]
struct ConversationEntry {
  conversation_id: i64,
  title: SharedString,
  search_string: String,
}

struct ConversationPickerDelegate;

impl PickerDelegate for ConversationPickerDelegate {
  type ListItem = ConversationEntry;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    div()
      .w_full()
      .px_2()
      .py_2()
      .rounded_md()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(item.title.clone())
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
      let matches = cx
        .background_spawn(async move {
          let mut matcher = matchers.get().await.ok()?;
          let needle = Pattern::parse(&query, CaseMatching::Smart, Normalization::Smart);
          let mut matches = Vec::new();
          let mut buf = Vec::new();

          for (index, entry) in items.iter().enumerate() {
            if let Some(score) =
              needle.score(Utf32Str::new(&entry.search_string, &mut buf), &mut matcher)
            {
              matches.push((index, score));
            }
          }
          Some(matches)
        })
        .await;

      picker
        .update(cx, |picker, cx| {
          picker.complete_search(cx, search_id, matches);
        })
        .log_err();
    })
  }
}

struct ChatMessage {
  role: SharedString,
  content: SharedString,
}

struct LlmPanel {
  input_state: Entity<InputState>,
  conversation_id: i64,
  messages: Vec<ChatMessage>,
  streaming_text: String,
  scroll_handle: ScrollHandle,
  autoscroll: bool,
  conversation_picker: Option<Entity<Picker<ConversationPickerDelegate>>>,
  confirmation_prompt: Option<Entity<ConfirmationPrompt>>,
  _task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl LlmPanel {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-p", OpenConversationPicker, Some(CONTEXT)),
      KeyBinding::new("escape", CloseConversationPicker, Some(CONVERSATION_PICKER_CONTEXT)),
      KeyBinding::new("ctrl-d", DeleteConversation, Some(CONVERSATION_PICKER_CONTEXT)),
    ]);

    let input_state = cx.new(|cx| InputState::new(window, cx).placeholder("Ask anything..."));

    window.focus(&input_state.read(cx).focus_handle(cx), cx);

    let subscriptions = vec![cx.subscribe_in(&input_state, window, Self::on_input_event)];

    let conversation_id = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .map(|d| d.as_secs() as i64)
      .unwrap_or(0);

    Self {
      input_state,
      conversation_id,
      messages: Vec::new(),
      streaming_text: String::new(),
      scroll_handle: ScrollHandle::new(),
      autoscroll: true,
      conversation_picker: None,
      confirmation_prompt: None,
      _task: None,
      _subscriptions: subscriptions,
    }
  }

  fn on_input_event(
    &mut self,
    _input: &Entity<InputState>,
    event: &input::state::InputEvent,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let input::state::InputEvent::PressEnter { secondary: false } = event else {
      return;
    };

    let prompt_text = self.input_state.read(cx).text().to_string();
    if prompt_text.is_empty() {
      return;
    }

    info!("Sending prompt: {prompt_text}");

    self.input_state.update(cx, |state, cx| {
      state.clean(window, cx);
    });

    self.messages.push(ChatMessage {
      role: "user".into(),
      content: prompt_text.clone().into(),
    });

    LlmDb::save_turn(self.conversation_id, "user", &prompt_text);

    let (tx, rx) = flume::unbounded::<String>();

    self.streaming_text.clear();
    self.autoscroll = true;
    self.scroll_handle.scroll_to_bottom();
    cx.notify();

    let join_handle = cx.spawn_tokio(async move {
      let api_key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(key) => key,
        Err(err) => {
          error!("OPENROUTER_API_KEY not set: {err}");
          return;
        }
      };

      let client: openrouter::Client = match openrouter::Client::new(&api_key) {
        Ok(client) => client,
        Err(err) => {
          error!("Failed to create OpenRouter client: {err}");
          return;
        }
      };

      let agent = client.agent(MODEL).build();
      let mut stream = agent.stream_prompt(&prompt_text).await;

      while let Some(chunk) = stream.next().await {
        match chunk {
          Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
            StreamedAssistantContent::Text(text) => {
              if tx.send(text.text).is_err() {
                break;
              }
            }
            _ => {}
          },
          Ok(MultiTurnStreamItem::FinalResponse(_)) => {}
          Ok(_) => {}
          Err(err) => {
            error!("Stream error: {err}");
            break;
          }
        }
      }
    });

    let conversation_id = self.conversation_id;

    self._task = Some(cx.spawn(async move |this, cx| {
      while let Ok(chunk) = rx.recv_async().await {
        this.update(cx, |this, cx| {
          this.streaming_text.push_str(&chunk);
          if this.autoscroll {
            this.scroll_handle.scroll_to_bottom();
          }
          cx.notify();
        })
        .log_err();
      }

      this.update(cx, |this, cx| {
        if !this.streaming_text.is_empty() {
          let content: SharedString = this.streaming_text.clone().into();
          LlmDb::save_turn(conversation_id, "assistant", &content);
          this.messages.push(ChatMessage {
            role: "assistant".into(),
            content,
          });
          this.streaming_text.clear();
        }
        cx.notify();
      })
      .log_err();

      let _ = join_handle.await;
    }));
  }

  fn open_conversation_picker(
    &mut self,
    _: &OpenConversationPicker,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let conversations = LlmDb::list_conversations();

    let conversation_picker = cx.new(|cx| {
      let mut picker =
        Picker::new(ConversationPickerDelegate, Arc::new(conversations), window, cx);
      picker.placeholder("Search conversations...", cx);
      picker
    });

    let subscription =
      cx.subscribe_in(&conversation_picker, window, |this, _picker, event, window, cx| {
        if let PickerEvent::Picked(entry) = event {
          this.load_conversation(entry.conversation_id, window, cx);
        }
      });

    cx.focus_view(&conversation_picker.read(cx).search_input.clone(), window);
    self._subscriptions.push(subscription);
    self.conversation_picker = Some(conversation_picker);
  }

  fn close_conversation_picker(
    &mut self,
    _: &CloseConversationPicker,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    self.conversation_picker = None;
    window.focus(&self.input_state.read(cx).focus_handle(cx), cx);
    cx.notify();
  }

  fn delete_conversation(
    &mut self,
    _: &DeleteConversation,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let Some(conversation_picker) = &self.conversation_picker else {
      return;
    };

    let Some(entry) = conversation_picker.read(cx).get_selected_item().cloned() else {
      return;
    };

    let prompt = cx.new(|cx| ConfirmationPrompt::new(window, cx));
    let conversation_picker = conversation_picker.clone();

    let subscription = cx.subscribe_in(
      &prompt,
      window,
      move |this, _, event: &ConfirmationEvent, window, cx| {
        match event {
          ConfirmationEvent::Confirm => {
            conversation_picker.update(cx, |picker, cx| {
              picker.remove_selected_item(window, cx);
            });
            LlmDb::delete_conversation(entry.conversation_id);
          }
          ConfirmationEvent::Dismiss => {}
        }

        this.confirmation_prompt = None;
        if let Some(picker) = &this.conversation_picker {
          cx.focus_view(&picker.read(cx).search_input.clone(), window);
        }
        cx.notify();
      },
    );

    self._subscriptions.push(subscription);
    self.confirmation_prompt = Some(prompt);
    cx.notify();
  }

  fn load_conversation(
    &mut self,
    conversation_id: i64,
    window: &mut Window,
    cx: &mut Context<Self>,
  ) {
    let messages = LlmDb::load_conversation(conversation_id);

    self.conversation_id = conversation_id;
    self.messages = messages
      .into_iter()
      .map(|(role, content)| ChatMessage {
        role: role.into(),
        content: content.into(),
      })
      .collect();
    self.streaming_text.clear();
    self._task = None;
    self.autoscroll = true;
    self.scroll_handle.scroll_to_bottom();

    self.conversation_picker = None;
    window.focus(&self.input_state.read(cx).focus_handle(cx), cx);
    cx.notify();
  }
}

struct LlmDb;

impl LlmDb {
  fn save_turn(conversation_id: i64, role: &str, content: &str) {
    let conn = DB.lock();
    match conn.prepare_cached(
      "INSERT INTO llm_conversations (conversation_id, role, content) VALUES (?1, ?2, ?3)",
    ) {
      Ok(mut stmt) => {
        if let Err(err) = stmt.execute(rusqlite::params![conversation_id, role, content]) {
          error!("Failed to save LLM turn: {err}");
        }
      }
      Err(err) => {
        error!("Failed to prepare LLM save query: {err}");
      }
    }
  }

  fn list_conversations() -> Vec<ConversationEntry> {
    let conn = DB.lock();
    let mut stmt = match conn.prepare_cached(
      "SELECT c.conversation_id, \
              SUBSTR(first_msg.content, 1, 100) as title, \
              MAX(c.timestamp) as latest \
       FROM llm_conversations c \
       JOIN ( \
         SELECT conversation_id, content \
         FROM llm_conversations \
         WHERE role = 'user' \
         GROUP BY conversation_id \
         HAVING id = MIN(id) \
       ) first_msg ON first_msg.conversation_id = c.conversation_id \
       GROUP BY c.conversation_id \
       ORDER BY latest DESC",
    ) {
      Ok(stmt) => stmt,
      Err(err) => {
        error!("Failed to prepare list conversations query: {err}");
        return Vec::new();
      }
    };

    let result = stmt.query_map([], |row| {
      let conversation_id: i64 = row.get(0)?;
      let title: String = row.get(1)?;
      Ok(ConversationEntry {
        conversation_id,
        search_string: title.clone(),
        title: title.into(),
      })
    });

    match result {
      Ok(rows) => rows.filter_map(|row| row.log_err()).collect(),
      Err(err) => {
        error!("Failed to list conversations: {err}");
        Vec::new()
      }
    }
  }

  fn delete_conversation(conversation_id: i64) {
    let conn = DB.lock();
    match conn.prepare_cached("DELETE FROM llm_conversations WHERE conversation_id = ?1") {
      Ok(mut stmt) => {
        if let Err(err) = stmt.execute(rusqlite::params![conversation_id]) {
          error!("Failed to delete conversation: {err}");
        }
      }
      Err(err) => {
        error!("Failed to prepare delete conversation query: {err}");
      }
    }
  }

  fn load_conversation(conversation_id: i64) -> Vec<(String, String)> {
    let conn = DB.lock();
    let mut stmt = match conn.prepare_cached(
      "SELECT role, content FROM llm_conversations \
       WHERE conversation_id = ?1 \
       ORDER BY id ASC",
    ) {
      Ok(stmt) => stmt,
      Err(err) => {
        error!("Failed to prepare load conversation query: {err}");
        return Vec::new();
      }
    };

    let result = stmt.query_map(rusqlite::params![conversation_id], |row| {
      Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });

    match result {
      Ok(rows) => rows.filter_map(|row| row.log_err()).collect(),
      Err(err) => {
        error!("Failed to load conversation: {err}");
        Vec::new()
      }
    }
  }
}

fn render_markdown(text: &str) -> Vec<AnyElement> {
  let options = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
  let parser = Parser::new_ext(text, options);

  let mut elements: Vec<AnyElement> = Vec::new();
  let mut inline_text = String::new();
  let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
  let mut style_stack: Vec<HighlightStyle> = Vec::new();
  let mut list_stack: Vec<Option<u64>> = Vec::new();
  let mut list_item_index: u64 = 0;
  let mut heading_level: Option<u8> = None;
  let mut in_code_block = false;
  let mut in_blockquote = false;

  let current_style = |stack: &[HighlightStyle]| -> HighlightStyle {
    let mut combined = HighlightStyle::default();
    for style in stack {
      if style.font_weight.is_some() {
        combined.font_weight = style.font_weight;
      }
      if style.font_style.is_some() {
        combined.font_style = style.font_style;
      }
      if style.background_color.is_some() {
        combined.background_color = style.background_color;
      }
      if style.strikethrough.is_some() {
        combined.strikethrough = style.strikethrough.clone();
      }
      if style.color.is_some() {
        combined.color = style.color;
      }
    }
    combined
  };

  let flush_block =
    |inline_text: &mut String,
     highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
     elements: &mut Vec<AnyElement>,
     heading_level: &mut Option<u8>,
     in_code_block: bool,
     in_blockquote: bool| {
      if inline_text.is_empty() {
        return;
      }

      let content: SharedString = inline_text.clone().into();
      let styled = if highlights.is_empty() {
        StyledText::new(content)
      } else {
        StyledText::new(content).with_highlights(highlights.drain(..).collect::<Vec<_>>())
      };

      let element = if let Some(level) = heading_level.take() {
        let size = match level {
          1 => px(24.0),
          2 => px(20.0),
          3 => px(18.0),
          _ => px(16.0),
        };
        div()
          .text_size(size)
          .font_weight(FontWeight::BOLD)
          .mb_2()
          .child(styled)
      } else if in_code_block {
        div()
          .bg(rgba(0xFFFFFF12))
          .rounded_md()
          .px_3()
          .py_2()
          .mb_2()
          .font_family("monospace")
          .text_sm()
          .child(styled)
      } else if in_blockquote {
        div()
          .border_l_2()
          .border_color(rgba(0xFFFFFF44))
          .pl_3()
          .mb_2()
          .text_color(rgba(0xFFFFFFAA))
          .child(styled)
      } else {
        div().mb_1().child(styled)
      };

      elements.push(element.into_any_element());
      inline_text.clear();
      highlights.clear();
    };

  for event in parser {
    match event {
      Event::Start(tag) => match tag {
        Tag::Heading { level, .. } => {
          heading_level = Some(level as u8);
        }
        Tag::CodeBlock(_) => {
          flush_block(
            &mut inline_text,
            &mut highlights,
            &mut elements,
            &mut heading_level,
            in_code_block,
            in_blockquote,
          );
          in_code_block = true;
        }
        Tag::BlockQuote(_) => {
          in_blockquote = true;
        }
        Tag::List(start) => {
          list_stack.push(start);
          list_item_index = start.unwrap_or(1);
        }
        Tag::Item => {
          flush_block(
            &mut inline_text,
            &mut highlights,
            &mut elements,
            &mut heading_level,
            in_code_block,
            in_blockquote,
          );
          let prefix = match list_stack.last() {
            Some(Some(_)) => {
              let prefix = format!("{list_item_index}. ");
              list_item_index += 1;
              prefix
            }
            _ => "• ".to_string(),
          };
          inline_text.push_str(&prefix);
        }
        Tag::Strong => {
          style_stack.push(HighlightStyle {
            font_weight: Some(FontWeight::BOLD),
            ..Default::default()
          });
        }
        Tag::Emphasis => {
          style_stack.push(HighlightStyle {
            font_style: Some(FontStyle::Italic),
            ..Default::default()
          });
        }
        Tag::Strikethrough => {
          style_stack.push(HighlightStyle {
            strikethrough: Some(StrikethroughStyle {
              thickness: px(1.0),
              color: None,
            }),
            ..Default::default()
          });
        }
        _ => {}
      },

      Event::End(tag_end) => match tag_end {
        TagEnd::Paragraph | TagEnd::Heading(_) | TagEnd::Item => {
          flush_block(
            &mut inline_text,
            &mut highlights,
            &mut elements,
            &mut heading_level,
            in_code_block,
            in_blockquote,
          );
        }
        TagEnd::CodeBlock => {
          flush_block(
            &mut inline_text,
            &mut highlights,
            &mut elements,
            &mut heading_level,
            in_code_block,
            in_blockquote,
          );
          in_code_block = false;
        }
        TagEnd::BlockQuote(_) => {
          in_blockquote = false;
        }
        TagEnd::List(_) => {
          list_stack.pop();
        }
        TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
          style_stack.pop();
        }
        _ => {}
      },

      Event::Text(text) => {
        let start = inline_text.len();
        inline_text.push_str(&text);
        let end = inline_text.len();

        let style = current_style(&style_stack);
        if style != HighlightStyle::default() {
          highlights.push((start..end, style));
        }
      }

      Event::Code(code) => {
        let start = inline_text.len();
        inline_text.push_str(&code);
        let end = inline_text.len();
        highlights.push((
          start..end,
          HighlightStyle {
            background_color: Some(rgba(0xFFFFFF18).into()),
            ..Default::default()
          },
        ));
      }

      Event::SoftBreak => {
        inline_text.push(' ');
      }

      Event::HardBreak => {
        inline_text.push('\n');
      }

      _ => {}
    }
  }

  flush_block(
    &mut inline_text,
    &mut highlights,
    &mut elements,
    &mut heading_level,
    in_code_block,
    in_blockquote,
  );

  elements
}

impl Focusable for LlmPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    if let Some(conversation_picker) = &self.conversation_picker {
      conversation_picker.read(cx).focus_handle(cx)
    } else {
      self.input_state.read(cx).focus_handle(cx)
    }
  }
}

impl Render for LlmPanel {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .on_action(cx.listener(Self::open_conversation_picker))
      .on_action(cx.listener(Self::close_conversation_picker))
      .on_action(cx.listener(Self::delete_conversation))
      .child(if let Some(conversation_picker) = &self.conversation_picker {
        v_flex()
          .key_context(CONVERSATION_PICKER_CONTEXT)
          .size_full()
          .relative()
          .child(picker_input(conversation_picker))
          .child(picker_results(conversation_picker))
          .when_some(self.confirmation_prompt.clone(), |this, prompt| {
            this.child(render_confirmation_overlay(&prompt))
          })
          .into_any_element()
      } else {
        self.render_chat(cx).into_any_element()
      })
  }
}

impl LlmPanel {
  fn render_chat(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .size_full()
      .p_4()
      .gap_3()
      .child(
        div()
          .relative()
          .flex_grow()
          .overflow_hidden()
          .child(
            div()
              .id("llm-messages")
              .size_full()
              .overflow_y_scroll()
              .track_scroll(&self.scroll_handle)
              .on_scroll_wheel(cx.listener(|this, _event, _window, _cx| {
                let offset = this.scroll_handle.offset();
                let max = this.scroll_handle.max_offset();
                let distance_from_bottom = max.height + offset.y;
                this.autoscroll = distance_from_bottom < px(20.0);
              }))
              .gap_2()
              .pr(px(10.0))
              .children(self.messages.iter().map(|message| {
                let is_user = message.role.as_ref() == "user";
                let content_element = div().text_color(rgb(0xFFFFFF));
                let content_element = if is_user {
                  content_element.child(message.content.clone())
                } else {
                  content_element.children(render_markdown(&message.content))
                };
                div()
                  .flex()
                  .flex_col()
                  .gap_1()
                  .child(
                    div()
                      .text_xs()
                      .text_color(rgba(0xFFFFFF88))
                      .child(if is_user { "You" } else { "Assistant" }),
                  )
                  .child(content_element)
              }))
              .when(!self.streaming_text.is_empty(), |this: gpui::Stateful<Div>| {
                let markdown_elements = render_markdown(&self.streaming_text);
                this.child(
                  div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                      div()
                        .text_xs()
                        .text_color(rgba(0xFFFFFF88))
                        .child("Assistant"),
                    )
                    .child(
                      div()
                        .text_color(rgb(0xFFFFFF))
                        .children(markdown_elements),
                    ),
                )
              }),
          )
          .child(
            div()
              .absolute()
              .top_0()
              .left_0()
              .right_0()
              .bottom_0()
              .child(Scrollbar::new(&self.scroll_handle)),
          ),
      )
      .child(
        input(&self.input_state)
          .w_full()
          .flex_shrink_0()
          .text_color(rgb(0xFFFFFF))
          .bg(rgba(0xFFFFFF0F))
          .rounded_md()
          .px_3()
          .py_2(),
      )
  }
}
