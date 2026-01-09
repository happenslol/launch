pub mod panels;
pub mod pulse;
pub mod types;

use anyhow::{Result, anyhow};
use flume::Receiver;
use futures::channel::oneshot;
use gpui::{App, BackgroundExecutor, Entity, Global, Task, prelude::*};

use crate::audio::{
  pulse::{Command, PulseThread, SetMute, SetVolume},
  types::{SinkEvent, SinkId, SinkInfo, SinkListEvent, SourceEvent, SourceId, SourceInfo, SourceListEvent},
};

pub fn init(cx: &mut App) {
  let state = cx.new(|_| AudioState::new());
  cx.set_global(GlobalAudioState(state.clone()));
  AudioState::init(state, cx);
}

pub struct GlobalAudioState(Entity<AudioState>);

impl Global for GlobalAudioState {}

pub struct AudioState {
  pulse: PulseThread,
  pub default_sink: Option<SinkId>,
  pub default_source: Option<SourceId>,
}

impl AudioState {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalAudioState>().0.clone()
  }

  fn new() -> Self {
    Self {
      pulse: PulseThread::spawn(),
      default_sink: None,
      default_source: None,
    }
  }

  fn init(this: Entity<Self>, cx: &mut App) {
    // Subscribe to sink list changes to keep default_sink in sync
    let sink_list_rx = this.read(cx).subscribe_sink_list();
    cx.spawn({
      let this = this.clone();
      async move |cx| {
        while let Ok(event) = sink_list_rx.recv_async().await {
          if let SinkListEvent::DefaultChanged(id) = event {
            let _ = this.update(cx, |this, cx| {
              this.default_sink = id;
              cx.notify();
            });
          }
        }
      }
    })
    .detach();

    // Subscribe to source list changes to keep default_source in sync
    let source_list_rx = this.read(cx).subscribe_source_list();
    cx.spawn({
      let this = this.clone();
      async move |cx| {
        while let Ok(event) = source_list_rx.recv_async().await {
          if let SourceListEvent::DefaultChanged(id) = event {
            let _ = this.update(cx, |this, cx| {
              this.default_source = id;
              cx.notify();
            });
          }
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

  // Query methods
  pub fn list_sinks(&self, executor: &BackgroundExecutor) -> Task<Vec<SinkInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::ListSinks(tx));
    executor.spawn(async move { rx.await.unwrap_or_default() })
  }

  pub fn list_sources(&self, executor: &BackgroundExecutor) -> Task<Vec<SourceInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::ListSources(tx));
    executor.spawn(async move { rx.await.unwrap_or_default() })
  }

  pub fn get_sink_info(&self, id: SinkId, executor: &BackgroundExecutor) -> Task<Option<SinkInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetSinkInfo(id, tx));
    executor.spawn(async move { rx.await.ok().flatten() })
  }

  pub fn get_source_info(&self, id: SourceId, executor: &BackgroundExecutor) -> Task<Option<SourceInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetSourceInfo(id, tx));
    executor.spawn(async move { rx.await.ok().flatten() })
  }

  pub fn get_default_sink(&self, executor: &BackgroundExecutor) -> Task<Option<SinkId>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetDefaultSink(tx));
    executor.spawn(async move { rx.await.ok().flatten() })
  }

  pub fn get_default_source(&self, executor: &BackgroundExecutor) -> Task<Option<SourceId>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetDefaultSource(tx));
    executor.spawn(async move { rx.await.ok().flatten() })
  }

  // Subscription methods
  pub fn subscribe_sink(&self, id: SinkId) -> Receiver<SinkEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSink(id, tx));
    rx
  }

  pub fn subscribe_source(&self, id: SourceId) -> Receiver<SourceEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSource(id, tx));
    rx
  }

  pub fn subscribe_sink_list(&self) -> Receiver<SinkListEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSinkList(tx));
    rx
  }

  pub fn subscribe_source_list(&self) -> Receiver<SourceListEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSourceList(tx));
    rx
  }

  // Action methods
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

  pub fn set_sink_mute(&self, sink: SinkId, set: SetMute) {
    self.pulse.send_command(Command::SetSinkMute(sink, set))
  }

  pub fn set_sink_volume(&self, sink: SinkId, set: SetVolume) {
    self.pulse.send_command(Command::SetSinkVolume(sink, set))
  }

  pub fn set_default_source(&self, source: SourceId, cx: &mut Context<Self>) -> Task<Result<()>> {
    self.async_command(cx, |tx| Command::SetDefaultSource(source, tx))
  }

  pub fn set_source_volume(&self, source: SourceId, set: SetVolume) {
    self.pulse.send_command(Command::SetSourceVolume(source, set))
  }

  pub fn set_source_mute(&self, source: SourceId, set: SetMute) {
    self.pulse.send_command(Command::SetSourceMute(source, set))
  }
}
