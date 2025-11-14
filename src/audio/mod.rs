mod pulse;
mod sections;
mod types;

use futures::stream::StreamExt;
use gpui::{App, AsyncApp, Entity, EventEmitter, Global, prelude::*};
use tracing::{error, info};

use crate::audio::pulse::PulseThread;

pub fn init(cx: &mut App) {
  let state = cx.new(|_| AudioState::new());
  cx.set_global(GlobalAudioState(state.clone()));
  AudioState::init(state, cx);
}

struct GlobalAudioState(Entity<AudioState>);

impl Global for GlobalAudioState {}

pub trait AudioStateAppExt {
  fn audio(&self) -> &AudioState;
}

impl AudioStateAppExt for App {
  fn audio(&self) -> &AudioState {
    self
      .try_global::<GlobalAudioState>()
      .expect("audio state not initialized")
      .0
      .read(self)
  }
}

pub enum AudioEvent {}

pub struct AudioState {
  pulse: PulseThread,
}

impl EventEmitter<AudioEvent> for AudioState {}

impl AudioState {
  fn new() -> Self {
    let pulse = PulseThread::spawn();

    Self { pulse }
  }

  fn init(this: Entity<Self>, cx: &mut App) {
    let mut pulse_rx = this.read(cx).pulse.get_event_rx().into_stream();

    cx.spawn({
      let this = this.clone();
      async move |cx| {
        while let Some(ev) = pulse_rx.next().await {
          Self::handle_pulse_event(&this, ev, cx);
        }
      }
    })
    .detach();
  }

  fn handle_pulse_event(_this: &Entity<Self>, ev: pulse::Event, _cx: &mut AsyncApp) {
    use pulse::Event;
    match ev {
      Event::SinkVolumeChanged(id, volume) => info!(?id, ?volume, "pulse: sink volume changed"),
      Event::SinkNameChanged(id, name) => info!(?id, ?name, "pulse: sink name changed"),
      Event::SinkRemoved(id) => info!(?id, "pulse: sink removed"),
      Event::SourceRemoved(id) => info!(?id, "pulse: source removed"),
      Event::Exited(err) => error!(?err, "Pulse thread exited"),
    }
  }
}
