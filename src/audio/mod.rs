mod pulse;
pub mod sections;
mod types;

use std::collections::BTreeMap;

use anyhow::{Result, anyhow};
use futures::{channel::oneshot, stream::StreamExt};
use gpui::{App, AsyncApp, Entity, EventEmitter, Global, Task, prelude::*};
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

pub struct GlobalAudioState(Entity<AudioState>);

impl Global for GlobalAudioState {}

pub enum AudioEvent {
  SinksChanged,
}

pub struct AudioState {
  pulse: PulseThread,
  sinks: BTreeMap<SinkId, SinkInfo>,
  sources: BTreeMap<SourceId, SourceInfo>,
  default_sink: Option<SinkId>,
  default_source: Option<SourceId>,
}

impl EventEmitter<AudioEvent> for AudioState {}

impl AudioState {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalAudioState>().0.clone()
  }

  fn new() -> Self {
    let pulse = PulseThread::spawn();
    let sinks = BTreeMap::new();
    let sources = BTreeMap::new();

    Self {
      default_sink: None,
      default_source: None,
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

    cx.on_app_quit({
      let this = this.clone();
      move |cx| this.update(cx, |this, cx| this.pulse.quit(cx))
    })
    .detach();
  }

  fn handle_pulse_event(this: &Entity<Self>, ev: pulse::Event, cx: &mut AsyncApp) -> Result<()> {
    use pulse::Event::*;

    match ev {
      // Sink events
      SinkFound(sink) => this.update(cx, |this, cx| {
        this.sinks.insert(sink.id, sink);
        cx.emit(AudioEvent::SinksChanged);
        cx.notify();
      })?,
      SinkInfoChanged(sink) => this.update(cx, |this, cx| {
        this.sinks.insert(sink.id, sink);
        cx.emit(AudioEvent::SinksChanged);
        cx.notify()
      })?,
      SinkVolumeChanged(sink, volume) => this.update(cx, |this, cx| {
        if let Some(sink) = this.sinks.get_mut(&sink) {
          sink.volume = volume;
          cx.emit(AudioEvent::SinksChanged);
          cx.notify();
        }
      })?,
      SinkMuteChanged(sink, muted) => this.update(cx, |this, cx| {
        if let Some(sink) = this.sinks.get_mut(&sink) {
          sink.mute = muted;
          cx.emit(AudioEvent::SinksChanged);
          cx.notify();
        }
      })?,
      SinkRemoved(sink) => this.update(cx, |this, cx| {
        this.sinks.remove(&sink);
        cx.emit(AudioEvent::SinksChanged);
        cx.notify();
      })?,
      DefaultSinkChanged(default) => this.update(cx, |this, cx| {
        this.default_sink = default;
        cx.emit(AudioEvent::SinksChanged);
        cx.notify();
      })?,

      // Source events
      SourceFound(source) => this.update(cx, |this, cx| {
        this.sources.insert(source.id, source);
        cx.notify()
      })?,
      SourceInfoChanged(source) => this.update(cx, |this, cx| {
        this.sources.insert(source.id, source);
        cx.notify()
      })?,
      SourceVolumeChanged(source, volume) => this.update(cx, |this, cx| {
        if let Some(source) = this.sources.get_mut(&source) {
          source.volume = volume;
          cx.notify();
        }
      })?,
      SourceMuteChanged(source, muted) => this.update(cx, |this, cx| {
        if let Some(source) = this.sources.get_mut(&source) {
          source.mute = muted;
          cx.notify();
        }
      })?,
      SourceRemoved(source) => this.update(cx, |this, cx| {
        this.sources.remove(&source);
        cx.notify();
      })?,
      DefaultSourceChanged(default) => this.update(cx, |this, cx| {
        this.default_source = default;
        cx.notify();
      })?,

      Exited(err) => error!(?err, "Pulse thread exited"),
    }

    Ok(())
  }

  fn async_command(
    &self,
    cx: &mut Context<Self>,
    make_cmd: impl FnOnce(oneshot::Sender<bool>) -> Command,
  ) -> Task<Result<()>> {
    let (tx, rx) = oneshot::channel::<bool>();
    self.pulse.send_command(make_cmd(tx));
    cx.spawn(async move |_, _| rx.await?.ok_or(anyhow!("Command failed")))
  }

  pub fn set_default_sink(&self, sink: SinkId, cx: &mut Context<Self>) -> Task<Result<()>> {
    self.async_command(cx, |tx| Command::SetDefaultSink(sink, tx))
  }

  pub fn set_default_source(&self, source: SourceId, cx: &mut Context<Self>) -> Task<Result<()>> {
    self.async_command(cx, |tx| Command::SetDefaultSource(source, tx))
  }
}
