mod audio;
mod control;
mod pipewire;
mod pulse;

use anyhow::Result;
use gpui::{
  AnyView, App, AppContext, Context, Entity, FocusHandle, Focusable, IntoElement, Render,
  SharedString, Styled, Subscription, Window, prelude::*,
};

use crate::{
  launcher::{Item, ItemAction},
  text_input::{TextInput, TextInputEvent},
  util::v_flex,
};

pub fn get_items() -> Result<Vec<Item>> {
  Ok(vec![Item {
    name: "sinks".into(),
    action: ItemAction::Section(Box::new(AudioSection::view)),
  }])
}

struct AudioSection {
  search_input: Entity<TextInput>,
  focus_handle: FocusHandle,
  sinks: Vec<SharedString>,
  subscriptions: Vec<Subscription>,
}

impl AudioSection {
  fn view(window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| AudioSection::new(window, cx)).into()
  }

  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    let focus_handle = cx.focus_handle();
    let search_input = cx.new(|cx| TextInput::new(window, cx));

    cx.focus_view(&search_input, window);

    let mut this = Self {
      focus_handle,
      search_input: search_input.clone(),
      sinks: Vec::new(),
      subscriptions: Vec::new(),
    };

    this
      .subscriptions
      .extend([cx.subscribe_in(&search_input, window, {
        let search_input = search_input.clone();
        move |this, _, ev: &TextInputEvent, window, cx| {}
      })]);

    cx.spawn_in(window, async move |this, cx| {
      let audio = audio::Audio::new().unwrap();
      let mut device_events = audio.device_events();

      while let Ok(ev) = device_events.recv().await {
        match ev {
          audio::DeviceEvent::SinkAdded(sink) => {
            println!("sink added: {sink:?}");
          }
          audio::DeviceEvent::SinkRemoved(sink_id) => {}
          audio::DeviceEvent::SourceAdded(source) => {}
          audio::DeviceEvent::SourceRemoved(source_id) => {}
        }
      }
    })
    .detach();

    this
  }
}

impl Focusable for AudioSection {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for AudioSection {
  fn render(&mut self, _: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
    v_flex()
      .track_focus(&self.focus_handle)
      .size_full()
      .child(self.search_input.clone())
  }
}
