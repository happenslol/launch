#![feature(bool_to_result)]
#![feature(vec_into_chunks)]
#![feature(result_option_map_or_default)]
#![feature(string_from_utf8_lossy_owned)]
#![feature(if_let_guard)]

mod assets;
mod audio;
mod bluetooth;
mod clipboard;
mod config;
mod confirmation;
mod db;
mod dbus;
mod icon;
mod input;
mod instance;
mod launcher;
mod llm;
mod logging;
mod markdown;
mod matcher;
mod network;
mod niri;
mod notification_osd;
mod picker;
mod polkit;
mod power;
mod scrollbar;
mod submenu;
mod tokio;
mod tray_menu;
mod util;
mod volume_osd;
mod wayland;
mod workspace_osd;
mod xdg;

use std::process;

use anyhow::Result;
use clap::Parser;
use flume::Receiver;
use fork::Fork;
use gpui::{App, Application, QuitMode, prelude::*};
use tracing::{debug, error, info};

use crate::{
  assets::{Assets, load_embedded_fonts},
  db::NotificationDbReader,
  input::state::InputState,
  instance::{Message, Response, Role},
  launcher::Launcher,
};

#[derive(Debug, Parser)]
struct Args {
  panel: Option<String>,
  #[arg(long, global = true)]
  foreground: bool,
  #[arg(long, global = true)]
  no_keyboard_capture: bool,
  #[command(subcommand)]
  command: Option<Command>,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
  /// Start the daemon without opening a window
  Daemon,
  /// Print the most recent notifications from the history
  Notifications {
    /// Number of notifications to print
    #[arg(short = 'n', long = "count", default_value_t = 10)]
    count: u32,
  },
}

fn main() -> Result<()> {
  let _guard = logging::init();
  let args = Args::try_parse()?;

  match buildid::build_id() {
    Some(id) => debug!(build_id = hex::encode(id)),
    None => debug!("no build id"),
  }

  if let Some(Command::Notifications { count }) = &args.command {
    return print_recent_notifications(*count);
  }

  let daemon_only = matches!(args.command, Some(Command::Daemon));

  if args.foreground {
    let listener = instance::force_acquire()?;
    let receiver = instance::listen(listener);
    run_app(args.panel, args.no_keyboard_capture, receiver, daemon_only);
    return Ok(());
  }

  let role = instance::acquire()?;

  match role {
    Role::Client(mut stream) => {
      let response = if daemon_only {
        info!("Daemon already running, checking version");
        instance::send_version_check(&mut stream)
      } else {
        info!("Sending open message to background process");
        instance::send_open(&mut stream, args.panel.clone())
      };
      drop(stream);

      let listener = match response {
        Ok(Response::Accepted) => {
          if daemon_only {
            info!("Same-version daemon already running");
          } else {
            info!("Background process accepted request");
          }
          return Ok(());
        }
        Ok(Response::Quitting) => {
          info!("Background process is a different version, taking over");
          instance::acquire_after_quit()?
        }
        Err(err) => {
          info!(?err, "Incompatible daemon, replacing");
          instance::force_acquire()?
        }
      };

      let panel = if daemon_only { None } else { args.panel };
      fork_and_run(listener, panel, args.no_keyboard_capture, daemon_only);
    }
    Role::Server(listener) => {
      info!("No existing instance, daemonizing");
      fork_and_run(listener, args.panel, args.no_keyboard_capture, daemon_only);
    }
  }

  Ok(())
}

fn print_recent_notifications(count: u32) -> Result<()> {
  let records = NotificationDbReader::at_default_path().recent(count)?;

  if records.is_empty() {
    println!("No notifications in history.");
    return Ok(());
  }

  for record in records {
    let when = chrono::DateTime::from_timestamp(record.timestamp, 0)
      .map(|time| {
        time
          .with_timezone(&chrono::Local)
          .format("%Y-%m-%d %H:%M:%S")
          .to_string()
      })
      .unwrap_or_else(|| record.timestamp.to_string());

    let urgency = match record.urgency {
      0 => "low",
      2 => "critical",
      _ => "normal",
    };

    println!(
      "{when}  [{urgency}]  app={:?}  icon={:?}",
      record.app_name, record.app_icon
    );
    if !record.summary.is_empty() {
      println!("    summary: {}", record.summary);
    }
    if !record.body.is_empty() {
      println!("    body:    {}", record.body);
    }
  }

  Ok(())
}

fn fork_and_run(
  listener: std::os::unix::net::UnixListener,
  panel: Option<String>,
  no_keyboard_capture: bool,
  daemon_only: bool,
) {
  if let Fork::Child = fork::fork().expect("Failed to fork") {
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
    run_app(panel, no_keyboard_capture, receiver, daemon_only);
  }
}

fn run_app(
  panel: Option<String>,
  no_keyboard_capture: bool,
  receiver: Receiver<Message>,
  daemon_only: bool,
) {
  Application::with_platform(gpui_linux::current_platform(false))
    .with_assets(Assets)
    .with_quit_mode(QuitMode::Explicit)
    .run(move |cx| {
      tokio::init(cx);
      wayland::init(cx).unwrap();
      matcher::init(cx);
      config::init(cx);
      dbus::init(cx);
      dbus::status_notifier::init(cx);
      dbus::notifications::init(cx);
      audio::init(cx);
      volume_osd::init(cx);
      xdg::init(cx);
      niri::init(cx);
      workspace_osd::init(cx);
      notification_osd::init(cx);
      polkit::init(cx);
      InputState::init(cx);
      load_embedded_fonts(cx).unwrap();

      if !daemon_only {
        open_launcher_window(cx, panel, no_keyboard_capture);
      }

      cx.spawn(async move |cx| {
        while let Ok(message) = receiver.recv_async().await {
          match message {
            Message::Open { panel } => {
              debug!(?panel, "Processing open window message");
              cx.update(|cx| {
                let has_launcher = cx
                  .windows()
                  .iter()
                  .any(|w| w.downcast::<Launcher>().is_some());
                if has_launcher {
                  debug!("Skipping open, launcher window already exists");
                } else {
                  open_launcher_window(cx, panel, no_keyboard_capture);
                }
              });
            }
          }
        }
      })
      .detach();
    });
}

fn open_launcher_window(cx: &mut App, panel: Option<String>, no_keyboard_capture: bool) {
  if let Err(err) = cx.open_window(
    Launcher::get_window_options(no_keyboard_capture),
    move |window, cx| cx.new(move |cx| Launcher::new(window, cx, panel, no_keyboard_capture)),
  ) {
    error!(?err, "Failed to launch");
  }
}
