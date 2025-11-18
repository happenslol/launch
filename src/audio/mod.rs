mod pulse;
pub mod sections;
mod types;

use std::collections::BTreeMap;

use anyhow::Result;
use futures::{channel::oneshot, stream::StreamExt};
use gpui::{App, AsyncApp, Entity, EventEmitter, Global, prelude::*};
use tracing::error;

use crate::audio::{
  pulse::{Command, PulseThread},
  types::{SinkId, SinkInfo, SourceId, SourceInfo},
};

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
  sinks: BTreeMap<SinkId, SinkInfo>,
  sources: BTreeMap<SourceId, SourceInfo>,
}

impl EventEmitter<AudioEvent> for AudioState {}

impl AudioState {
  fn new() -> Self {
    let pulse = PulseThread::spawn();
    let sinks = BTreeMap::new();
    let sources = BTreeMap::new();

    Self {
      pulse,
      sinks,
      sources,
    }
  }

  fn init(this: Entity<Self>, cx: &mut App) {
    let mut pulse_rx = this.read(cx).pulse.get_event_rx().into_stream();

    cx.spawn({
      let this = this.clone();
      async move |cx| {
        while let Some(ev) = pulse_rx.next().await {
          if let Err(err) = Self::handle_pulse_event(&this, ev, cx) {
            error!(?err, "Failed to handle pulse event");
          };
        }
      }
    })
    .detach();
  }

  fn handle_pulse_event(this: &Entity<Self>, ev: pulse::Event, cx: &mut AsyncApp) -> Result<()> {
    use pulse::Event;
    match ev {
      Event::SinkFound(sink) => this.update(cx, |this, cx| {
        this.sinks.insert(sink.id, sink);
        cx.notify();
      })?,
      Event::SourceFound(source) => this.update(cx, |this, cx| {
        this.sources.insert(source.id, source);
        cx.notify()
      })?,

      Event::SinkInfoChanged(sink) => this.update(cx, |this, cx| {
        this.sinks.insert(sink.id, sink);
        cx.notify()
      })?,
      Event::SourceInfoChanged(source) => this.update(cx, |this, cx| {
        this.sources.insert(source.id, source);
        cx.notify()
      })?,
      Event::SinkVolumeChanged(sink, volume) => this.update(cx, |this, cx| {
        if let Some(sink) = this.sinks.get_mut(&sink) {
          sink.volume = volume;
          cx.notify();
        }
      })?,
      Event::SourceVolumeChanged(source, volume) => this.update(cx, |this, cx| {
        if let Some(source) = this.sources.get_mut(&source) {
          source.volume = volume;
          cx.notify();
        }
      })?,
      Event::SinkMuteChanged(sink, muted) => this.update(cx, |this, cx| {
        if let Some(sink) = this.sinks.get_mut(&sink) {
          sink.mute = muted;
          cx.notify();
        }
      })?,
      Event::SourceMuteChanged(source, muted) => this.update(cx, |this, cx| {
        if let Some(source) = this.sources.get_mut(&source) {
          source.mute = muted;
          cx.notify();
        }
      })?,
      Event::SinkRemoved(sink) => this.update(cx, |this, cx| {
        this.sinks.remove(&sink);
        cx.notify();
      })?,
      Event::SourceRemoved(source) => this.update(cx, |this, cx| {
        this.sources.remove(&source);
        cx.notify();
      })?,

      Event::Exited(err) => error!(?err, "Pulse thread exited"),
    }

    Ok(())
  }

  pub async fn set_default_sink(&self, sink: SinkId) {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::SetDefaultSink(sink, tx));
    match rx.await {
      Ok(false) => error!("Failed to set default sink"),
      Err(err) => error!(?err, "Failed to set default sink"),
      _ => {}
    };
  }
}
