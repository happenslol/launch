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
mod lock;
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
mod status;
mod submenu;
mod tokio;
mod tray_menu;
mod util;
mod volume_osd;
mod wayland;
mod workspace_osd;
mod xdg;

use std::{mem, process};

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
  /// Lock the screen
  Lock,
}

/// What the app does once it is up.
enum Startup {
  /// Open the launcher, on a specific panel if one was named.
  Launcher { panel: Option<String> },
  /// Only run the background services.
  Daemon,
  /// Lock the screen right away.
  Lock,
}

fn main() -> Result<()> {
  let (guard, reload) = logging::init();
  let args = Args::try_parse()?;

  match buildid::build_id() {
    Some(id) => debug!(build_id = hex::encode(id)),
    None => debug!("no build id"),
  }

  if let Some(Command::Notifications { count }) = &args.command {
    return print_recent_notifications(*count);
  }

  let startup = match args.command {
    Some(Command::Daemon) => Startup::Daemon,
    Some(Command::Lock) => Startup::Lock,
    // Handled above; it never reaches the app.
    Some(Command::Notifications { .. }) => return Ok(()),
    None => Startup::Launcher { panel: args.panel },
  };

  if args.foreground {
    let listener = instance::force_acquire()?;
    let receiver = instance::listen(listener);
    run_app(startup, args.no_keyboard_capture, receiver);
    return Ok(());
  }

  let role = instance::acquire()?;

  match role {
    Role::Client(mut stream) => {
      let response = match &startup {
        Startup::Daemon => {
          info!("Daemon already running, checking version");
          instance::send_version_check(&mut stream)
        }
        Startup::Lock => {
          info!("Sending lock message to background process");
          instance::send_lock(&mut stream)
        }
        Startup::Launcher { panel } => {
          info!("Sending open message to background process");
          instance::send_open(&mut stream, panel.clone())
        }
      };
      drop(stream);

      let listener = match response {
        Ok(Response::Accepted) => {
          if matches!(startup, Startup::Daemon) {
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

      fork_and_run(listener, startup, args.no_keyboard_capture, guard, reload);
    }
    Role::Server(listener) => {
      info!("No existing instance, daemonizing");
      fork_and_run(listener, startup, args.no_keyboard_capture, guard, reload);
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
  startup: Startup,
  no_keyboard_capture: bool,
  guard: logging::Guard,
  reload: logging::Reload,
) {
  if let Fork::Child = fork::fork().expect("Failed to fork") {
    // The log writers' worker threads stayed behind in the parent. Its guard
    // would wait on threads that don't exist here, so drop it on the floor and
    // start writers of our own.
    mem::forget(guard);
    let _guard = logging::reinit_after_fork(&reload);

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
    run_app(startup, no_keyboard_capture, receiver);
  }
}

fn run_app(startup: Startup, no_keyboard_capture: bool, receiver: Receiver<Message>) {
  Application::with_platform(gpui_linux::current_platform(false))
    .with_assets(Assets)
    .with_quit_mode(QuitMode::Explicit)
    .run(move |cx| {
      load_embedded_fonts(cx).unwrap();

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
      status::init(cx);
      polkit::init(cx);
      lock::init(cx);
      InputState::init(cx);

      match startup {
        Startup::Launcher { panel } => open_launcher_window(cx, panel, no_keyboard_capture),
        Startup::Lock => lock::lock(cx),
        Startup::Daemon => {}
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
                if lock::is_locked() {
                  // The compositor would hide it, and it would still be there
                  // after unlocking.
                  debug!("Skipping open, session is locked");
                } else if has_launcher {
                  debug!("Skipping open, launcher window already exists");
                } else {
                  open_launcher_window(cx, panel, no_keyboard_capture);
                }
              });
            }
            Message::Lock => {
              debug!("Processing lock message");
              cx.update(lock::lock);
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
