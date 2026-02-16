//! Even though we already have a wayland connection through gpui, we can't reasonably implement all
//! wayland protocols we require there. So, this module provides a separate global wayland
//! connection that allows us to use any protocols we want.

mod clipboard;

use std::thread;

use anyhow::Result;
use calloop::{
  EventLoop, LoopSignal,
  channel::{Channel, Event},
};
use calloop_wayland_source::WaylandSource;
use gpui::{App, Entity, EventEmitter, Global, prelude::*};
use tracing::{debug, error};
use wayland_client::{
  Connection, Dispatch, QueueHandle, delegate_noop,
  protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1,
};

use clipboard::ClipboardState;

#[derive(Debug)]
pub enum Command {}

#[derive(Debug, Clone)]
pub enum WaylandEvent {
  ClipboardText(Vec<u8>),
}

struct GlobalWaylandConnection(Entity<WaylandConnection>);

impl Global for GlobalWaylandConnection {}

pub struct WaylandConnection {
  cmd_tx: calloop::channel::Sender<Command>,
  signal: LoopSignal,
}

impl EventEmitter<WaylandEvent> for WaylandConnection {}

impl WaylandConnection {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalWaylandConnection>().0.clone()
  }
}

pub struct State {
  pub seat: Option<wl_seat::WlSeat>,
  pub data_device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
  pub data_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
  pub event_tx: Option<flume::Sender<WaylandEvent>>,
  pub loop_handle: Option<calloop::LoopHandle<'static, State>>,
  pub clipboard: ClipboardState,
}

impl State {
  fn new(event_tx: flume::Sender<WaylandEvent>) -> Self {
    Self {
      seat: None,
      data_device: None,
      data_manager: None,
      event_tx: Some(event_tx),
      loop_handle: None,
      clipboard: ClipboardState::new(),
    }
  }

  fn handle_command(&mut self, cmd: Event<Command>) {
    let _cmd = match cmd {
      Event::Msg(cmd) => cmd,
      Event::Closed => return,
    };

    // handle command
  }
}

pub fn init(cx: &mut App) -> Result<()> {
  let (cmd_tx, cmd_rx) = calloop::channel::channel::<Command>();
  let (init_tx, init_rx) = flume::bounded::<LoopSignal>(1);
  let (event_tx, event_rx) = flume::unbounded::<WaylandEvent>();

  // Bit awkward since we can't move the event loop into the background thread, so we have to send
  // the signal back to the main thread after init.
  thread::spawn(move || {
    if let Err(err) = run(cmd_rx, init_tx, event_tx) {
      error!(?err, "Error in wayland connection");
    }
  });

  // If an error occurs during startup, this fails and the main thread exits as well.
  let signal = init_rx.recv()?;

  let connection = cx.new(|_| WaylandConnection { cmd_tx, signal });
  cx.set_global(GlobalWaylandConnection(connection.clone()));

  cx.on_app_quit({
    let connection = connection.clone();
    move |cx| {
      connection.read(cx).signal.stop();
      async {}
    }
  })
  .detach();

  cx.spawn({
    let connection = connection.clone();
    async move |cx| {
      while let Ok(event) = event_rx.recv_async().await {
        connection.update(cx, |_, cx| {
          cx.emit(event);
        });
      }
    }
  })
  .detach();

  Ok(())
}

fn run(
  cmd_rx: Channel<Command>,
  init_tx: flume::Sender<LoopSignal>,
  event_tx: flume::Sender<WaylandEvent>,
) -> Result<()> {
  let conn = Connection::connect_to_env()?;
  let display = conn.display();
  let mut event_queue = conn.new_event_queue();
  let qh = event_queue.handle();
  let _registry = display.get_registry(&qh, ());
  let mut state = State::new(event_tx);

  if let Some(reader) = &state.clipboard.db_reader {
    match reader.recent(10) {
      Ok(entries) => {
        for entry in &entries {
          let text = String::from_utf8_lossy(&entry.data);
          debug!(
            id = entry.id,
            timestamp = entry.timestamp,
            mime_types = ?entry.mime_types,
            text = %text,
            "Clipboard history entry"
          );
        }
        debug!(count = entries.len(), "Loaded recent clipboard history");
      }
      Err(err) => error!(?err, "Failed to load clipboard history"),
    }
  }

  event_queue.roundtrip(&mut state)?;

  // Create data control device for the seat
  if let (Some(data_manager), Some(seat)) = (&state.data_manager, &state.seat) {
    let device = data_manager.get_data_device(seat, &qh, ());
    state.data_device = Some(device);
    debug!("Created zwlr_data_control_device_v1");
  }

  let mut event_loop = EventLoop::<State>::try_new()?;
  let handle = event_loop.handle();

  state.loop_handle = Some(handle.clone());

  handle
    .insert_source(cmd_rx, |cmd, _, state| state.handle_command(cmd))
    .map_err(|err| anyhow::anyhow!("Failed to insert command source: {err}"))?;

  WaylandSource::new(conn, event_queue)
    .insert(handle)
    .map_err(|err| anyhow::anyhow!("Failed to insert wayland source: {err}"))?;

  init_tx.send(event_loop.get_signal())?;
  event_loop.run(None, &mut state, |_| {})?;

  Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
  fn event(
    state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _data: &(),
    _conn: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    else {
      return;
    };

    match &interface[..] {
      "wl_seat" => {
        if state.seat.is_some() {
          return;
        }

        let seat = registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ());
        debug!("Bound wl_seat");
        state.seat = Some(seat);
      }
      "zwlr_data_control_manager_v1" => {
        let data_manager = registry
          .bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
            name,
            version,
            qh,
            (),
          );

        debug!("Bound zwlr_data_control_manager_v1");
        state.data_manager = Some(data_manager);
      }
      _ => {}
    }
  }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
