#![feature(bool_to_result)]
#![feature(vec_into_chunks)]
#![feature(result_option_map_or_default)]
#![feature(string_from_utf8_lossy_owned)]

mod assets;
mod audio;
mod bluetooth;
mod db;
mod dbus;
mod icon;
mod input;
mod instance;
mod launcher;
mod logging;
mod matcher;
mod network;
mod picker;
mod util;
mod xdg;

use std::process;

use anyhow::Result;
use clap::Parser;
use fork::Fork;
use gpui::{App, Application, QuitMode, prelude::*};
use tracing::{error, info};

use flume::Receiver;

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
  #[arg(long)]
  no_daemon: bool,
  #[arg(long)]
  no_keyboard_capture: bool,
}

fn main() -> Result<()> {
  logging::init();
  let args = Args::try_parse()?;

  if args.no_daemon {
    run_app(args.panel, args.no_keyboard_capture, None);
    return Ok(());
  }

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
          eprintln!(
            "Failed to redirect stdio: {}",
            std::io::Error::last_os_error()
          );
        }

        let receiver = instance::listen(listener);
        run_app(args.panel, args.no_keyboard_capture, Some(receiver));
      }
    }
  }

  Ok(())
}

fn run_app(panel: Option<String>, no_keyboard_capture: bool, receiver: Option<Receiver<Message>>) {
  let mode = if receiver.is_some() {
    QuitMode::Explicit
  } else {
    QuitMode::LastWindowClosed
  };

  Application::new()
    .with_assets(Assets)
    .with_quit_mode(mode)
    .run(move |cx| {
      matcher::init(cx);
      dbus::init(cx);
      audio::init(cx);
      xdg::init(cx);
      InputState::init(cx);
      load_embedded_fonts(cx).unwrap();

      open_launcher_window(cx, panel, no_keyboard_capture);

      if let Some(receiver) = receiver {
        cx.spawn(async move |cx| {
          while let Ok(message) = receiver.recv_async().await {
            match message {
              Message::Open { panel } => {
                cx.update(|cx| {
                  if cx.windows().is_empty() {
                    open_launcher_window(cx, panel, no_keyboard_capture);
                  }
                })
                .log_err();
              }
            }
          }
        })
        .detach();
      }
    });
}

fn open_launcher_window(cx: &mut App, panel: Option<String>, no_keyboard_capture: bool) {
  if let Err(err) = cx.open_window(Launcher::get_window_options(no_keyboard_capture), move |window, cx| {
    cx.new(move |cx| Launcher::new(window, cx, panel))
  }) {
    error!(?err, "Failed to launch");
  }
}
