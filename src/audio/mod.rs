use std::thread::JoinHandle;

use futures::{stream::StreamExt, stream_select};
use gpui::{App, AsyncApp, Entity, Global, prelude::*};

use crate::audio::pulse::PulseThread;

mod pipewire;
mod pulse;
mod sections;
mod temp;
mod types;

pub fn init(cx: &mut App) {
  let state = cx.new(|_| AudioState::new());
  cx.set_global(GlobalAudioState(state.clone()));
  AudioState::init(state, cx);
}

struct GlobalAudioState(Entity<AudioState>);

impl Global for GlobalAudioState {}

pub struct AudioState {
  pipewire_handle: Option<JoinHandle<()>>,
  pulse_thread: PulseThread,
}

impl AudioState {
  pub fn new() -> Self {
    let (pipewire_tx, pipewire_rx) = flume::unbounded();

    let pipewire_handle = pipewire::spawn_thread(pipewire_tx);
    let pulse_thread = PulseThread::spawn();

    Self {
      pipewire_handle: Some(pipewire_handle),
      pulse_thread,
    }
  }

  fn init(this: Entity<Self>, cx: &mut App) {
    let pulse_rx = this.read(cx).pulse_thread.get_event_rx();

    let (_, pipewire_rx) = flume::unbounded();

    // TODO: This feels like a background spawn thing, how are streams usually handled in zed?
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
