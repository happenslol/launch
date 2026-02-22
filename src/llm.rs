use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{
  App, Context, Div, Entity, FocusHandle, Focusable, Render, ScrollHandle, SharedString,
  Subscription, Task, Window, div, prelude::*, px, rgb, rgba,
};
use rig::agent::MultiTurnStreamItem;
use rig::client::CompletionClient;
use rig::providers::openrouter;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use tracing::{error, info};

use crate::{
  db::DB,
  icon::IconName,
  input::{self, input, state::InputState},
  launcher::RootItem,
  tokio::TokioExt,
  util::{ResultExt, v_flex},
};

const MODEL: &str = "google/gemini-2.5-flash";

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
  _task: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl LlmPanel {
  fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
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
}

impl Focusable for LlmPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    self.input_state.read(cx).focus_handle(cx)
  }
}

impl Render for LlmPanel {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .key_context("llm")
      .size_full()
      .p_4()
      .gap_3()
      .child(
        div()
          .id("llm-messages")
          .flex_grow()
          .overflow_y_scroll()
          .track_scroll(&self.scroll_handle)
          .on_scroll_wheel(cx.listener(|this, _event, _window, _cx| {
            let offset = this.scroll_handle.offset();
            let max = this.scroll_handle.max_offset();
            let distance_from_bottom = max.height + offset.y;
            this.autoscroll = distance_from_bottom < px(20.0);
          }))
          .gap_2()
          .children(self.messages.iter().map(|message| {
            let is_user = message.role.as_ref() == "user";
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
              .child(
                div()
                  .text_color(rgb(0xFFFFFF))
                  .child(message.content.clone()),
              )
          }))
          .when(!self.streaming_text.is_empty(), |this: gpui::Stateful<Div>| {
            let text: SharedString = self.streaming_text.clone().into();
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
                .child(div().text_color(rgb(0xFFFFFF)).child(text)),
            )
          }),
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
