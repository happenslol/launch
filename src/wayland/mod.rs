//! Even though we already have a wayland connection through gpui, we can't reasonably implement all
//! wayland protocols we require there. So, this module provides a separate global wayland
//! connection that allows us to use any protocols we want.

use std::thread;

use anyhow::Result;
use calloop::{
  EventLoop, LoopSignal,
  channel::{Channel, Event},
};
use calloop_wayland_source::WaylandSource;
use gpui::{App, Entity, EventEmitter, Global, prelude::*};
use tracing::{debug, error, info};
use wayland_client::{
  Connection, Dispatch, QueueHandle, delegate_noop, event_created_child,
  protocol::{wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
  zwlr_data_control_source_v1,
};

#[derive(Debug)]
pub enum Command {}

#[derive(Debug, Clone)]
pub enum WaylandEvent {}

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

#[derive(Default)]
struct State {
  seat: Option<wl_seat::WlSeat>,
  data_device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
  data_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
  event_tx: Option<flume::Sender<WaylandEvent>>,
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
  let mut state = State {
    event_tx: Some(event_tx),
    ..Default::default()
  };

  event_queue.roundtrip(&mut state)?;

  // Create data control device for the seat
  if let (Some(data_manager), Some(seat)) = (&state.data_manager, &state.seat) {
    let device = data_manager.get_data_device(seat, &qh, ());
    state.data_device = Some(device);
    debug!("Created zwlr_data_control_device_v1");
  }

  let mut event_loop = EventLoop::<State>::try_new()?;
  let handle = event_loop.handle();

  handle
    .insert_source(cmd_rx, |cmd, _, state| state.handle_command(cmd))
    .unwrap();

  WaylandSource::new(conn, event_queue)
    .insert(handle)
    .unwrap();

  init_tx.send(event_loop.get_signal())?;
  event_loop.run(None, &mut state, |_| {})?;

  Ok(())
}

impl State {
  fn handle_command(&mut self, cmd: Event<Command>) {
    let _cmd = match cmd {
      Event::Msg(cmd) => cmd,
      Event::Closed => {
        error!("Command channel closed");
        return;
      }
    };

    // handle command
  }
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

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
  fn event(
    _state: &mut Self,
    _device: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    event: zwlr_data_control_device_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &wayland_client::QueueHandle<Self>,
  ) {
    info!("Received zwlr_data_control_device_v1 event: {:?}", event);
    match event {
      zwlr_data_control_device_v1::Event::DataOffer { id } => {}
      zwlr_data_control_device_v1::Event::Selection { id } => {}
      zwlr_data_control_device_v1::Event::Finished => {}
      zwlr_data_control_device_v1::Event::PrimarySelection { id } => {}
      _ => {}
    }
  }

  event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
    zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
  ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
  fn event(
    _state: &mut Self,
    _offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    event: zwlr_data_control_offer_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &wayland_client::QueueHandle<Self>,
  ) {
    if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
      info!("Received mime type: {}", mime_type);
    }
  }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
  fn event(
    _state: &mut Self,
    _source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    event: zwlr_data_control_source_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &wayland_client::QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {}
      zwlr_data_control_source_v1::Event::Cancelled => {}
      _ => {}
    }
  }
}
