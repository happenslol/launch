use std::thread::JoinHandle;

use futures::{channel::mpsc::UnboundedReceiver, select, stream::StreamExt, stream_select};
use gpui::{App, AsyncApp, Entity, Global, prelude::*};

mod pipewire;
mod pulse;
mod sections;
mod temp;
mod types;

pub fn init(cx: &mut App) {
  let (state, pipewire_rx, pulse_rx) = AudioState::new();
  let state = cx.new(|_| state);

  cx.set_global(GlobalAudioState(state.clone()));
  AudioState::init(state, cx, pipewire_rx, pulse_rx);
}

struct GlobalAudioState(Entity<AudioState>);

impl Global for GlobalAudioState {}

// TODO: Cleanup/join threads on app quit
pub struct AudioState {
  pipewire_handle: Option<JoinHandle<()>>,
  pulse_handle: Option<JoinHandle<()>>,
}

impl AudioState {
  pub fn new() -> (
    Self,
    flume::Receiver<pipewire::Event>,
    flume::Receiver<pulse::Event>,
  ) {
    let (pipewire_tx, pipewire_rx) = flume::unbounded();
    let (pulse_tx, pulse_rx) = flume::unbounded();

    let pipewire_handle = pipewire::spawn_thread(pipewire_tx);
    let pulse_handle = pulse::spawn_thread(pulse_tx);

    (
      Self {
        pipewire_handle: Some(pipewire_handle),
        pulse_handle: Some(pulse_handle),
      },
      pipewire_rx,
      pulse_rx,
    )
  }

  fn init(
    this: Entity<Self>,
    cx: &mut App,
    pipewire_rx: flume::Receiver<pipewire::Event>,
    pulse_rx: flume::Receiver<pulse::Event>,
  ) {
    // TODO: This feels like a backend spawn thing, how are streams usually handled in zed?
    cx.spawn({
      let this = this.clone();
      async move |cx| {
        enum Event {
          Pipewire(pipewire::Event),
          Pulse(pulse::Event),
        }

        let mut stream = stream_select!(
          pipewire_rx.into_stream().map(Event::Pipewire),
          pulse_rx.into_stream().map(Event::Pulse)
        );

        while let Some(ev) = stream.next().await {
          match ev {
            Event::Pipewire(ev) => Self::handle_pipewire_event(this.clone(), ev, cx),
            Event::Pulse(ev) => Self::handle_pulse_event(this.clone(), ev, cx),
          }
        }
      }
    })
    .detach();
  }

  fn handle_pipewire_event(this: Entity<Self>, ev: pipewire::Event, cx: &mut AsyncApp) {
    println!("pipewire event: {ev:?}");
  }

  fn handle_pulse_event(this: Entity<Self>, ev: pulse::Event, cx: &mut AsyncApp) {
    println!("pulse event: {ev:?}");
  }
}
