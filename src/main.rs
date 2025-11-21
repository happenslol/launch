#![feature(bool_to_result)]

mod assets;
mod audio;
mod dbus;
mod launcher;
mod logging;
mod picker;
mod text_input;
mod util;
mod xdg;

use anyhow::Result;
use gpui::{App, Application};
use tracing::error;

use crate::{
  assets::{Assets, load_embedded_fonts},
  launcher::Launcher,
  text_input::TextInput,
};

fn main() -> Result<()> {
  logging::init();

  Application::new().with_assets(Assets).run(move |cx| {
    dbus::init(cx);
    audio::init(cx);
    TextInput::init(cx);

    load_embedded_fonts(cx).unwrap();
    show_launcher(cx);
  });

  Ok(())
}

fn show_launcher(cx: &mut App) {
  if let Err(err) = cx.open_window(Launcher::get_window_options(), Launcher::view) {
    error!(?err, "Failed to open window");
    cx.quit();
  }
}
