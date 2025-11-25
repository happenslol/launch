#![feature(bool_to_result)]

mod assets;
mod audio;
mod db;
mod dbus;
mod input;
mod launcher;
mod logging;
mod picker;
mod picker2;
mod util;
mod xdg;

use anyhow::{Context as _, Result};
use gpui::{App, Application};
use tracing::error;

use crate::{
  assets::{Assets, load_embedded_fonts},
  input::state::InputState,
  launcher::Launcher,
};

fn main() -> Result<()> {
  logging::init();

  Application::new().with_assets(Assets).run(move |cx| {
    dbus::init(cx);
    audio::init(cx);
    db::init(cx);
    InputState::init(cx);

    load_embedded_fonts(cx).unwrap();
    if let Err(err) = show_launcher(cx) {
      error!(?err, "Failed to launch");
      cx.quit();
    }
  });

  Ok(())
}

fn show_launcher(cx: &mut App) -> Result<()> {
  cx.open_window(Launcher::get_window_options(), Launcher::view)
    .context("Failed to open window")?;

  Ok(())
}
