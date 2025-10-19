mod assets;
mod logging;
mod text_input;
mod util;

use gpui::{
  App, Application, Bounds, Entity, FocusHandle, Focusable, InputEvent, KeyBinding, Size,
  Subscription, Window, WindowBounds, WindowKind, WindowOptions, actions, div,
  layer_shell::{Anchor, KeyboardInteractivity, Layer, LayerShellOptions},
  point,
  prelude::*,
  px, rgb,
};
use tracing::error;

use crate::{
  assets::{Assets, load_embedded_fonts},
  text_input::{TextInput, TextInputEvent},
};

fn main() {
  logging::init();

  Application::new().with_assets(Assets).run(move |cx| {
    Launcher::init(cx);
    TextInput::init(cx);

    load_embedded_fonts(cx).unwrap();
    show_launcher(cx);
  });
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
      layer: Layer::Top,
      anchor: Anchor::all(),
      exclusive_zone: None,
      exclusive_edge: None,
      margin: None,
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
  _subscriptions: Vec<Subscription>,
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

    let _subscriptions = vec![cx.subscribe_in(&search_input, window, {
      let search_input = search_input.clone();
      move |_, _, ev: &TextInputEvent, _window, cx| {
        let value = search_input.read(cx).content.clone();
        println!("TextInputEvent: {:?}, value: {:?}", ev, value);
      }
    })];

    Self {
      search_input,
      focus_handle,
      _subscriptions,
    }
  }
}

impl Focusable for Launcher {
  fn focus_handle(&self, _: &App) -> FocusHandle {
    self.focus_handle.clone()
  }
}

impl Render for Launcher {
  fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
    div()
      .size_full()
      .bg(rgb(0x000000))
      .rounded_lg()
      .child(self.search_input.clone())
  }
}
