#![feature(bool_to_result)]
#![feature(vec_into_chunks)]

mod assets;
mod audio;
mod db;
mod input;
mod launcher;
mod logging;
mod network;
mod picker;
mod util;
mod xdg;

use anyhow::Result;
use clap::Parser;
use gpui::{Application, prelude::*};
use tracing::error;

use crate::{
  assets::{Assets, load_embedded_fonts},
  input::state::InputState,
  launcher::Launcher,
};

#[derive(Debug, Parser)]
struct Args {
  panel: Option<String>,
}

fn main() -> Result<()> {
  logging::init();
  let args = Args::try_parse()?;

  Application::new().with_assets(Assets).run(move |cx| {
    audio::init(cx);
    InputState::init(cx);

    load_embedded_fonts(cx).unwrap();
    if let Err(err) = cx.open_window(Launcher::get_window_options(), move |window, cx| {
      cx.new(move |cx| Launcher::new(window, cx, args.panel))
    }) {
      error!(?err, "Failed to launch");
      cx.quit();
    }
  });

  Ok(())
}
