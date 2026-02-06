#![feature(bool_to_result)]
#![feature(vec_into_chunks)]
#![feature(result_option_map_or_default)]
#![feature(string_from_utf8_lossy_owned)]

mod assets;
mod audio;
mod bluetooth;
mod db;
mod dbus;
mod input;
mod instance;
mod launcher;
mod logging;
mod network;
mod matcher;
mod picker;
mod util;
mod xdg;

use std::process;

use anyhow::Result;
use clap::Parser;
use fork::Fork;
use gpui::{App, Application, QuitMode, prelude::*};
use tracing::{error, info};

use crate::{
  assets::{Assets, load_embedded_fonts},
  input::state::InputState,
  instance::{Message, Role},
  launcher::Launcher,
  util::ResultExt,
};

#[derive(Debug, Parser)]
struct Args {
  panel: Option<String>,
}

fn main() -> Result<()> {
  logging::init();
  let args = Args::try_parse()?;

  let role = instance::acquire()?;

  match role {
    Role::Client(mut stream) => {
      info!("Sending open message to background process");
      let message = Message::Open { panel: args.panel };
      rmp_serde::encode::write(&mut stream, &message)?;
      return Ok(());
    }
    Role::Server(listener) => {
      info!("No existing instance, daemonizing");
      if let Fork::Child = fork::fork()? {
        if fork::setsid().is_err() {
          eprintln!("Failed to setsid: {}", std::io::Error::last_os_error());
          process::exit(1);
        }
        if fork::redirect_stdio().is_err() {
          eprintln!("Failed to redirect stdio: {}", std::io::Error::last_os_error());
        }

        let receiver = instance::listen(listener);

        Application::new().with_assets(Assets).with_quit_mode(QuitMode::Explicit).run(move |cx| {
          matcher::init(cx);
          dbus::init(cx);
          audio::init(cx);
          InputState::init(cx);
          load_embedded_fonts(cx).unwrap();

          open_launcher_window(cx, args.panel);

          cx.spawn(async move |cx| {
            while let Ok(message) = receiver.recv_async().await {
              match message {
                Message::Open { panel } => {
                  cx.update(|cx| {
                    if cx.windows().is_empty() {
                      open_launcher_window(cx, panel);
                    }
                  })
                  .log_err();
                }
              }
            }
          })
          .detach();
        });
      }
    }
  }

  Ok(())
}

fn open_launcher_window(cx: &mut App, panel: Option<String>) {
  if let Err(err) = cx.open_window(Launcher::get_window_options(), move |window, cx| {
    cx.new(move |cx| Launcher::new(window, cx, panel))
  }) {
    error!(?err, "Failed to launch");
  }
}
