pub mod panels;
pub mod pulse;
pub mod types;

use anyhow::{Result, anyhow};
use flume::Receiver;
use futures::channel::oneshot;
use gpui::{App, Entity, Global, Task, prelude::*};

use crate::audio::{
  pulse::{Command, PulseThread, SetMute, SetVolume},
  types::{
    SinkEvent, SinkId, SinkInfo, SinkInputEvent, SinkInputId, SinkInputInfo, SinkInputListEvent,
    SinkListEvent, SourceEvent, SourceId, SourceInfo, SourceListEvent,
  },
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
            this.update(cx, |this, cx| {
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
            this.update(cx, |this, cx| {
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
  pub fn list_sinks(&self, cx: &App) -> Task<Vec<SinkInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::ListSinks(tx));
    cx.background_executor()
      .spawn(async move { rx.await.unwrap_or_default() })
  }

  pub fn list_sources(&self, cx: &App) -> Task<Vec<SourceInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::ListSources(tx));
    cx.background_executor()
      .spawn(async move { rx.await.unwrap_or_default() })
  }

  #[allow(dead_code)]
  pub fn get_sink_info(&self, id: SinkId, cx: &App) -> Task<Option<SinkInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetSinkInfo(id, tx));
    cx.background_executor()
      .spawn(async move { rx.await.ok().flatten() })
  }

  #[allow(dead_code)]
  pub fn get_source_info(&self, id: SourceId, cx: &App) -> Task<Option<SourceInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetSourceInfo(id, tx));
    cx.background_executor()
      .spawn(async move { rx.await.ok().flatten() })
  }

  #[allow(dead_code)]
  pub fn get_default_sink(&self, cx: &App) -> Task<Option<SinkId>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetDefaultSink(tx));
    cx.background_executor()
      .spawn(async move { rx.await.ok().flatten() })
  }

  #[allow(dead_code)]
  pub fn get_default_source(&self, cx: &App) -> Task<Option<SourceId>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::GetDefaultSource(tx));
    cx.background_executor()
      .spawn(async move { rx.await.ok().flatten() })
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
    self
      .pulse
      .send_command(Command::SetSourceVolume(source, set))
  }

  pub fn set_source_mute(&self, source: SourceId, set: SetMute) {
    self.pulse.send_command(Command::SetSourceMute(source, set))
  }

  // Sink input query methods
  pub fn list_sink_inputs(&self, cx: &App) -> Task<Vec<SinkInputInfo>> {
    let (tx, rx) = oneshot::channel();
    self.pulse.send_command(Command::ListSinkInputs(tx));
    cx.background_executor()
      .spawn(async move { rx.await.unwrap_or_default() })
  }

  // Sink input subscription methods
  pub fn subscribe_sink_input(&self, id: SinkInputId) -> Receiver<SinkInputEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSinkInput(id, tx));
    rx
  }

  pub fn subscribe_sink_input_list(&self) -> Receiver<SinkInputListEvent> {
    let (tx, rx) = flume::unbounded();
    self.pulse.send_command(Command::SubscribeSinkInputList(tx));
    rx
  }

  // Sink input action methods
  pub fn set_sink_input_volume(&self, id: SinkInputId, set: SetVolume) {
    self
      .pulse
      .send_command(Command::SetSinkInputVolume(id, set))
  }

  pub fn set_sink_input_mute(&self, id: SinkInputId, set: SetMute) {
    self.pulse.send_command(Command::SetSinkInputMute(id, set))
  }

  pub fn move_sink_input(&self, input_id: SinkInputId, sink_id: SinkId) {
    self
      .pulse
      .send_command(Command::MoveSinkInput(input_id, sink_id))
  }
}
