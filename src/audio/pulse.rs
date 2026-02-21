use std::{
  cell::RefCell,
  collections::HashMap,
  fs::File,
  io::{Read, Write},
  os::fd::{AsRawFd, OwnedFd},
  rc::Rc,
  thread::{self, JoinHandle},
  time::Duration,
};

use flume::{Receiver, Sender};
use futures::channel::oneshot::{self, Canceled};
use gpui::{App, AppContext, FutureExt, SharedString, Task, Timeout};
use pulse::{
  callbacks::ListResult,
  context::{
    Context, FlagSet as ContextFlagSet,
    introspect::{self, Introspector, ServerInfo},
    subscribe::{Facility, InterestMaskSet, Operation},
  },
  def::{self, Retval},
  mainloop::{
    api::Mainloop as _,
    events::io::FlagSet as EventFlagSet,
    standard::{IterateResult, Mainloop},
  },
};
use rustix::pipe::PipeFlags;
use thiserror::Error;
use tracing::{debug, trace, warn};

use super::types::{
  SinkEvent, SinkId, SinkInfo, SinkInputEvent, SinkInputId, SinkInputInfo, SinkInputListEvent,
  SinkListEvent, SourceEvent, SourceId, SourceInfo, SourceListEvent,
};

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum SetVolume {
  Absolute(u32),
  AbsolutePercent(u32),
  Relative(i32),
  RelativePercent(i32),
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum SetMute {
  Mute,
  Unmute,
  Toggle,
}

#[allow(dead_code)]
pub enum Command {
  SetSinkVolume(SinkId, SetVolume),
  SetSourceVolume(SourceId, SetVolume),
  SetSinkMute(SinkId, SetMute),
  SetSourceMute(SourceId, SetMute),
  SetDefaultSink(SinkId, oneshot::Sender<bool>),
  SetDefaultSource(SourceId, oneshot::Sender<bool>),
  Quit(oneshot::Sender<()>),

  // Query commands (return cached state)
  ListSinks(oneshot::Sender<Vec<SinkInfo>>),
  ListSources(oneshot::Sender<Vec<SourceInfo>>),
  GetSinkInfo(SinkId, oneshot::Sender<Option<SinkInfo>>),
  GetSourceInfo(SourceId, oneshot::Sender<Option<SourceInfo>>),
  GetDefaultSink(oneshot::Sender<Option<SinkId>>),
  GetDefaultSource(oneshot::Sender<Option<SourceId>>),

  // Subscription commands
  SubscribeSink(SinkId, Sender<SinkEvent>),
  SubscribeSource(SourceId, Sender<SourceEvent>),
  SubscribeSinkList(Sender<SinkListEvent>),
  SubscribeSourceList(Sender<SourceListEvent>),

  // Sink input commands
  ListSinkInputs(oneshot::Sender<Vec<SinkInputInfo>>),
  SubscribeSinkInput(SinkInputId, Sender<SinkInputEvent>),
  SubscribeSinkInputList(Sender<SinkInputListEvent>),
  SetSinkInputVolume(SinkInputId, SetVolume),
  SetSinkInputMute(SinkInputId, SetMute),
  MoveSinkInput(SinkInputId, SinkId),
}

#[derive(Debug, Clone, Error)]
pub enum PulseError {
  #[error("Failed to create PA main loop")]
  MainLoopCreate,
  #[error("Failed to create PA context")]
  ContextCreate,
  #[error("Main loop quit before context became ready")]
  QuitBeforeReady,
  #[error("PA error: {0}")]
  PAErr(#[from] pulse::error::PAErr),
}

pub struct PulseThread {
  handle: Option<JoinHandle<()>>,

  // Pipe to notify the pulse main loop when we have sent new commands
  notify_fd: File,
  command_tx: Sender<Command>,
}

impl PulseThread {
  pub fn spawn() -> Self {
    let (reader, writer) =
      rustix::pipe::pipe_with(PipeFlags::CLOEXEC).expect("failed to create pipe");

    let (command_tx, command_rx) = flume::bounded(20);

    let handle = thread::spawn(move || {
      if let Err(err) = thread_main(reader, command_rx) {
        tracing::error!(?err, "Pulse thread exited with error");
      }
    });

    let notify_fd = File::from(writer);

    Self {
      handle: Some(handle),
      notify_fd,
      command_tx,
    }
  }

  pub fn send_command(&self, cmd: Command) {
    let _ = self.command_tx.send(cmd);
    let _ = self.notify_fd.try_clone().unwrap().write(&[1]);
  }

  pub fn quit(&mut self, cx: &mut App) -> Task<()> {
    let (tx, rx) = oneshot::channel::<()>();
    self.send_command(Command::Quit(tx));
    let handle = self.handle.take().expect("Pulse thread already quit");

    let shutdown = rx.with_timeout(Duration::from_millis(50), cx.background_executor());
    cx.background_spawn(async move {
      match shutdown.await {
        Ok(Ok(())) => {}
        Ok(Err(Canceled)) => warn!("Pulse thread shutdown was canceled"),
        Err(Timeout) => warn!("Pulse thread shutdown timed out"),
      }

      if let Err(err) = handle.join() {
        warn!(?err, "Failed to join pulse thread");
      }
    })
  }
}

fn thread_main(notify_fd: OwnedFd, command_rx: Receiver<Command>) -> Result<(), PulseError> {
  let state = Rc::new(RefCell::new(PulseState::new()));
  let mut main_loop = Mainloop::new().ok_or(PulseError::MainLoopCreate)?;
  let mut context = Context::new(&main_loop, "launch").ok_or(PulseError::ContextCreate)?;

  context.connect(None, ContextFlagSet::NOFAIL, None)?;

  // Wait for context to become ready
  loop {
    match main_loop.iterate(true) {
      IterateResult::Success(_) => {}
      IterateResult::Err(err) => return Err(err.into()),
      IterateResult::Quit(_) => return Err(PulseError::QuitBeforeReady),
    }

    if context.get_state() == pulse::context::State::Ready {
      break;
    }
  }

  context.subscribe(
    InterestMaskSet::SERVER
      | InterestMaskSet::SINK
      | InterestMaskSet::SINK_INPUT
      | InterestMaskSet::SOURCE
      | InterestMaskSet::CARD,
    |_| {},
  );

  let introspector = context.introspect();

  // Get the initial state by requesting all sinks and sources once
  introspector.get_sink_info_list({
    let state = state.clone();
    move |result| PulseState::handle_sink_info(&state, result)
  });

  introspector.get_source_info_list({
    let state = state.clone();
    move |result| PulseState::handle_source_info(&state, result)
  });

  introspector.get_sink_input_info_list({
    let state = state.clone();
    move |result| PulseState::handle_sink_input_info(&state, result)
  });

  // Make sure to request server info after sinks and sources so we can match default sink and
  // source up
  introspector.get_server_info({
    let state = state.clone();
    move |result| PulseState::handle_server_info(&state, result)
  });

  context.set_subscribe_callback(Some(Box::new({
    let state = state.clone();
    move |facility, operation, index| {
      handle_event(&state, &introspector, facility, operation, index);
    }
  })));

  // We have to hold this handle to keep the event source alive
  let mut notify_fd = File::from(notify_fd);
  let _io_handle = main_loop.new_io_event(notify_fd.as_raw_fd(), EventFlagSet::INPUT, {
    let state = state.clone();
    let mut main_loop = Mainloop {
      _inner: Rc::clone(&main_loop._inner),
    };

    Box::new(move |_, _, flags| {
      // TODO: The mask doesn't seem to be respected here, we get all events and not just input.
      // Is that a bug in libpulse-binding?
      if !flags.contains(EventFlagSet::INPUT) {
        return;
      }

      // It's possible that multiple notifies happened before we read, so just drain a few more
      // to be safe.
      let mut buf = [0; 16];
      let _ = notify_fd.read(&mut buf);

      while let Ok(cmd) = command_rx.try_recv() {
        handle_command(&state, &mut main_loop, &mut context, cmd);
      }
    })
  });

  if let Err((err, _)) = main_loop.run() {
    return Err(err.into());
  }

  if let Some(tx) = state.borrow_mut().shutdown_tx.take() {
    let _ = tx.send(());
  }

  Ok(())
}

fn handle_command(
  state: &Rc<RefCell<PulseState>>,
  main_loop: &mut Mainloop,
  context: &mut Context,
  command: Command,
) {
  match command {
    Command::Quit(tx) => {
      state.borrow_mut().shutdown_tx = Some(tx);
      main_loop.quit(Retval(0));
    }
    Command::SetSinkVolume(id, set) => {
      let (mut current, base) = {
        let state = state.borrow();
        let Some(sink) = state.sinks.get(&id.0) else {
          warn!(?id, "No such sink");
          return;
        };

        (sink.volume.clone(), sink.base_volume)
      };

      match set {
        SetVolume::Absolute(v) => current.set_percent(base, v),
        SetVolume::AbsolutePercent(p) => current.set_percent(base, p),
        SetVolume::Relative(v) if v >= 0 => current.add_percent(base, v as u32),
        SetVolume::Relative(v) => current.sub_percent(base, v.unsigned_abs()),
        SetVolume::RelativePercent(p) if p >= 0 => current.add_percent(base, p as u32),
        SetVolume::RelativePercent(p) => current.sub_percent(base, p.unsigned_abs()),
      }

      context
        .introspect()
        .set_sink_volume_by_index(id.0, &current.into(), None);
    }
    Command::SetSinkMute(id, set) => match set {
      SetMute::Mute => {
        context
          .introspect()
          .set_sink_mute_by_index(id.0, true, None);
      }
      SetMute::Unmute => {
        context
          .introspect()
          .set_sink_mute_by_index(id.0, false, None);
      }
      SetMute::Toggle => {
        let Some(muted) = state.borrow().sinks.get(&id.0).map(|sink| sink.mute) else {
          warn!(?id, "No such sink");
          return;
        };

        context
          .introspect()
          .set_sink_mute_by_index(id.0, !muted, None);
      }
    },
    Command::SetDefaultSink(id, res) => {
      let state = state.borrow();
      let Some(sink) = state.sinks.get(&id.0) else {
        warn!(?id, "No such sink");
        return;
      };

      let Some(name) = sink.name.as_ref() else {
        warn!(?id, "Sink has no name");
        return;
      };

      let mut res = Some(res);
      context.set_default_sink(name, move |success| {
        if let Some(res) = res.take() {
          let _ = res.send(success);
        }
      });
    }
    Command::SetSourceVolume(id, set) => {
      let (mut current, base) = {
        let state = state.borrow();
        let Some(source) = state.sources.get(&id.0) else {
          warn!(?id, "No such source");
          return;
        };

        (source.volume.clone(), source.base_volume)
      };

      match set {
        SetVolume::Absolute(v) => current.set_percent(base, v),
        SetVolume::AbsolutePercent(p) => current.set_percent(base, p),
        SetVolume::Relative(v) if v >= 0 => current.add_percent(base, v as u32),
        SetVolume::Relative(v) => current.sub_percent(base, v.unsigned_abs()),
        SetVolume::RelativePercent(p) if p >= 0 => current.add_percent(base, p as u32),
        SetVolume::RelativePercent(p) => current.sub_percent(base, p.unsigned_abs()),
      }

      context
        .introspect()
        .set_source_volume_by_index(id.0, &current.into(), None);
    }
    Command::SetSourceMute(id, set) => match set {
      SetMute::Mute => {
        context
          .introspect()
          .set_source_mute_by_index(id.0, true, None);
      }
      SetMute::Unmute => {
        context
          .introspect()
          .set_source_mute_by_index(id.0, false, None);
      }
      SetMute::Toggle => {
        let Some(muted) = state.borrow().sources.get(&id.0).map(|source| source.mute) else {
          warn!(?id, "No such source");
          return;
        };

        context
          .introspect()
          .set_source_mute_by_index(id.0, !muted, None);
      }
    },
    Command::SetDefaultSource(id, res) => {
      let state = state.borrow();
      let Some(source) = state.sources.get(&id.0) else {
        warn!(?id, "No such source");
        return;
      };

      let Some(name) = source.name.as_ref() else {
        warn!(?id, "source has no name");
        return;
      };

      let mut res = Some(res);
      context.set_default_source(name, move |success| {
        if let Some(res) = res.take() {
          let _ = res.send(success);
        }
      });
    }

    // Query commands
    Command::ListSinks(tx) => {
      let sinks = state.borrow().sinks.values().cloned().collect();
      let _ = tx.send(sinks);
    }
    Command::ListSources(tx) => {
      let sources = state.borrow().sources.values().cloned().collect();
      let _ = tx.send(sources);
    }
    Command::GetSinkInfo(id, tx) => {
      let info = state.borrow().sinks.get(&id.0).cloned();
      let _ = tx.send(info);
    }
    Command::GetSourceInfo(id, tx) => {
      let info = state.borrow().sources.get(&id.0).cloned();
      let _ = tx.send(info);
    }
    Command::GetDefaultSink(tx) => {
      let _ = tx.send(state.borrow().default_sink_id);
    }
    Command::GetDefaultSource(tx) => {
      let _ = tx.send(state.borrow().default_source_id);
    }

    // Subscription commands
    Command::SubscribeSink(id, sender) => {
      state
        .borrow_mut()
        .sink_subscribers
        .entry(id)
        .or_default()
        .push(sender);
    }
    Command::SubscribeSource(id, sender) => {
      state
        .borrow_mut()
        .source_subscribers
        .entry(id)
        .or_default()
        .push(sender);
    }
    Command::SubscribeSinkList(sender) => {
      state.borrow_mut().sink_list_subscribers.push(sender);
    }
    Command::SubscribeSourceList(sender) => {
      state.borrow_mut().source_list_subscribers.push(sender);
    }

    // Sink input commands
    Command::ListSinkInputs(tx) => {
      let sink_inputs = state.borrow().sink_inputs.values().cloned().collect();
      let _ = tx.send(sink_inputs);
    }
    Command::SubscribeSinkInput(id, sender) => {
      state
        .borrow_mut()
        .sink_input_subscribers
        .entry(id)
        .or_default()
        .push(sender);
    }
    Command::SubscribeSinkInputList(sender) => {
      state.borrow_mut().sink_input_list_subscribers.push(sender);
    }
    Command::SetSinkInputVolume(id, set) => {
      let mut current = {
        let state = state.borrow();
        let Some(sink_input) = state.sink_inputs.get(&id.0) else {
          warn!(?id, "No such sink input");
          return;
        };
        sink_input.volume.clone()
      };

      // Use NORMAL volume as base since sink inputs don't have base_volume
      let base = super::types::Volume(pulse::volume::Volume::NORMAL.0);

      match set {
        SetVolume::Absolute(v) => current.set_percent(base, v),
        SetVolume::AbsolutePercent(p) => current.set_percent(base, p),
        SetVolume::Relative(v) if v >= 0 => current.add_percent(base, v as u32),
        SetVolume::Relative(v) => current.sub_percent(base, v.unsigned_abs()),
        SetVolume::RelativePercent(p) if p >= 0 => current.add_percent(base, p as u32),
        SetVolume::RelativePercent(p) => current.sub_percent(base, p.unsigned_abs()),
      }

      context
        .introspect()
        .set_sink_input_volume(id.0, &current.into(), None);
    }
    Command::SetSinkInputMute(id, set) => match set {
      SetMute::Mute => {
        context.introspect().set_sink_input_mute(id.0, true, None);
      }
      SetMute::Unmute => {
        context.introspect().set_sink_input_mute(id.0, false, None);
      }
      SetMute::Toggle => {
        let Some(muted) = state.borrow().sink_inputs.get(&id.0).map(|i| i.mute) else {
          warn!(?id, "No such sink input");
          return;
        };
        context.introspect().set_sink_input_mute(id.0, !muted, None);
      }
    },
    Command::MoveSinkInput(input_id, sink_id) => {
      context
        .introspect()
        .move_sink_input_by_index(input_id.0, sink_id.0, None);
    }
  }
}

fn handle_event(
  state: &Rc<RefCell<PulseState>>,
  introspector: &Introspector,
  facility: Option<Facility>,
  operation: Option<Operation>,
  index: u32,
) {
  let (Some(facility), Some(operation)) = (facility, operation) else {
    debug!(
      ?facility,
      ?operation,
      index,
      "Skipping event without facility or operation"
    );
    return;
  };

  match operation {
    Operation::New | Operation::Changed => match facility {
      Facility::Server => {
        introspector.get_server_info({
          let state = state.clone();
          move |item| PulseState::handle_server_info(&state, item)
        });
      }
      Facility::Sink => {
        introspector.get_sink_info_by_index(index, {
          let state = state.clone();
          move |item| PulseState::handle_sink_info(&state, item)
        });
      }
      Facility::Source => {
        introspector.get_source_info_by_index(index, {
          let state = state.clone();
          move |item| PulseState::handle_source_info(&state, item)
        });
      }
      Facility::SinkInput => {
        introspector.get_sink_input_info(index, {
          let state = state.clone();
          move |item| PulseState::handle_sink_input_info(&state, item)
        });
      }
      _ => trace!(?facility, ?operation, index, "Skipping event"),
    },
    Operation::Removed => match facility {
      Facility::Sink => {
        let sink_id = SinkId(index);
        let mut state = state.borrow_mut();
        if state.sinks.remove(&index).is_some() {
          // Notify individual sink subscribers that sink was removed
          if let Some(subscribers) = state.sink_subscribers.remove(&sink_id) {
            for tx in subscribers {
              let _ = tx.send(SinkEvent::Removed);
            }
          }

          // Notify list subscribers
          state.notify_sink_list_subscribers(SinkListEvent::Removed(sink_id));
        }
      }
      Facility::Source => {
        let source_id = SourceId(index);
        let mut state = state.borrow_mut();
        if state.sources.remove(&index).is_some() {
          // Notify individual source subscribers that source was removed
          if let Some(subscribers) = state.source_subscribers.remove(&source_id) {
            for tx in subscribers {
              let _ = tx.send(SourceEvent::Removed);
            }
          }

          // Notify list subscribers
          state.notify_source_list_subscribers(SourceListEvent::Removed(source_id));
        }
      }
      Facility::SinkInput => {
        let sink_input_id = SinkInputId(index);
        let mut state = state.borrow_mut();
        if state.sink_inputs.remove(&index).is_some() {
          if let Some(subscribers) = state.sink_input_subscribers.remove(&sink_input_id) {
            for tx in subscribers {
              let _ = tx.send(SinkInputEvent::Removed);
            }
          }

          state.notify_sink_input_list_subscribers(SinkInputListEvent::Removed(sink_input_id));
        }
      }
      _ => trace!(?facility, index, "Skipping remove event"),
    },
  }
}

struct PulseState {
  shutdown_tx: Option<oneshot::Sender<()>>,
  sinks: HashMap<u32, SinkInfo>,
  sources: HashMap<u32, SourceInfo>,
  sink_inputs: HashMap<u32, SinkInputInfo>,
  default_sink_id: Option<SinkId>,
  default_source_id: Option<SourceId>,

  // Subscriber tracking
  sink_subscribers: HashMap<SinkId, Vec<Sender<SinkEvent>>>,
  source_subscribers: HashMap<SourceId, Vec<Sender<SourceEvent>>>,
  sink_input_subscribers: HashMap<SinkInputId, Vec<Sender<SinkInputEvent>>>,
  sink_list_subscribers: Vec<Sender<SinkListEvent>>,
  source_list_subscribers: Vec<Sender<SourceListEvent>>,
  sink_input_list_subscribers: Vec<Sender<SinkInputListEvent>>,
}

impl PulseState {
  fn new() -> Self {
    Self {
      shutdown_tx: None,
      sinks: Default::default(),
      sources: Default::default(),
      sink_inputs: Default::default(),
      default_sink_id: None,
      default_source_id: None,
      sink_subscribers: Default::default(),
      source_subscribers: Default::default(),
      sink_input_subscribers: Default::default(),
      sink_list_subscribers: Default::default(),
      source_list_subscribers: Default::default(),
      sink_input_list_subscribers: Default::default(),
    }
  }

  fn notify_sink_subscribers(&mut self, id: SinkId, event: SinkEvent) {
    if let Some(subscribers) = self.sink_subscribers.get_mut(&id) {
      subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
  }

  fn notify_source_subscribers(&mut self, id: SourceId, event: SourceEvent) {
    if let Some(subscribers) = self.source_subscribers.get_mut(&id) {
      subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
  }

  fn notify_sink_list_subscribers(&mut self, event: SinkListEvent) {
    self
      .sink_list_subscribers
      .retain(|tx| tx.send(event.clone()).is_ok());
  }

  fn notify_source_list_subscribers(&mut self, event: SourceListEvent) {
    self
      .source_list_subscribers
      .retain(|tx| tx.send(event.clone()).is_ok());
  }

  fn notify_sink_input_subscribers(&mut self, id: SinkInputId, event: SinkInputEvent) {
    if let Some(subscribers) = self.sink_input_subscribers.get_mut(&id) {
      subscribers.retain(|tx| tx.send(event.clone()).is_ok());
    }
  }

  fn notify_sink_input_list_subscribers(&mut self, event: SinkInputListEvent) {
    self
      .sink_input_list_subscribers
      .retain(|tx| tx.send(event.clone()).is_ok());
  }

  fn handle_server_info(this: &Rc<RefCell<Self>>, info: &ServerInfo) {
    let mut this = this.borrow_mut();

    let default_sink_id = info
      .default_sink_name
      .as_ref()
      .and_then(|default_sink_name| {
        this
          .sinks
          .values()
          .find(|sink| {
            sink
              .name
              .as_ref()
              .is_some_and(|name| name.as_str() == default_sink_name)
          })
          .map(|sink| sink.id)
      });

    if default_sink_id != this.default_sink_id {
      let old_default = this.default_sink_id;
      this.default_sink_id = default_sink_id;

      // Notify old default sink subscribers
      if let Some(old_id) = old_default {
        this.notify_sink_subscribers(old_id, SinkEvent::NoLongerDefault);
      }

      // Notify new default sink subscribers
      if let Some(new_id) = default_sink_id {
        this.notify_sink_subscribers(new_id, SinkEvent::BecameDefault);
      }

      // Notify list subscribers
      this.notify_sink_list_subscribers(SinkListEvent::DefaultChanged(default_sink_id));
    }

    let default_source_id = info
      .default_source_name
      .as_ref()
      .and_then(|default_source_name| {
        this
          .sources
          .values()
          .find(|source| {
            source
              .name
              .as_ref()
              .is_some_and(|name| name.as_str() == default_source_name)
          })
          .map(|source| source.id)
      });

    if default_source_id != this.default_source_id {
      let old_default = this.default_source_id;
      this.default_source_id = default_source_id;

      // Notify old default source subscribers
      if let Some(old_id) = old_default {
        this.notify_source_subscribers(old_id, SourceEvent::NoLongerDefault);
      }

      // Notify new default source subscribers
      if let Some(new_id) = default_source_id {
        this.notify_source_subscribers(new_id, SourceEvent::BecameDefault);
      }

      // Notify list subscribers
      this.notify_source_list_subscribers(SourceListEvent::DefaultChanged(default_source_id));
    }
  }

  fn handle_sink_info(this: &Rc<RefCell<Self>>, info: ListResult<&introspect::SinkInfo>) {
    let ListResult::Item(info) = info else { return };

    let mut this = this.borrow_mut();
    let sink_id = SinkId(info.index);

    // Collect events to send after releasing the sink borrow
    let mut events_to_send: Vec<SinkEvent> = Vec::new();
    let mut list_event: Option<SinkListEvent> = None;

    let is_hardware = info.flags.contains(def::SinkFlagSet::HARDWARE);

    // Skip virtual (non-hardware) sinks entirely
    if !is_hardware {
      return;
    }

    let form_factor = info
      .proplist
      .get_str("device.icon_name")
      .map(|s| SharedString::from(s.to_string()));

    let port_available = info.active_port.as_ref().and_then(|port| {
      match port.available {
        def::PortAvailable::Unknown => None,
        def::PortAvailable::No => Some(false),
        def::PortAvailable::Yes => Some(true),
      }
    });

    if let Some(sink) = this.sinks.get_mut(&info.index) {
      if sink.volume != info.volume {
        sink.volume = info.volume.into();
        sink.base_volume = info.base_volume.into();
        events_to_send.push(SinkEvent::VolumeChanged(sink.volume.clone()));
      }

      if sink.mute != info.mute {
        sink.mute = info.mute;
        events_to_send.push(SinkEvent::MuteChanged(info.mute));
      }

      if sink.name.as_ref().map(|s| s.as_str()) != info.name.as_deref()
        || sink.description.as_ref().map(|s| s.as_str()) != info.description.as_deref()
        || sink.form_factor != form_factor
        || sink.port_available != port_available
      {
        sink.name = info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        sink.description = info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        sink.form_factor = form_factor;
        sink.port_available = port_available;
        events_to_send.push(SinkEvent::InfoChanged(sink.clone()));
      }
    } else {
      let managed = SinkInfo {
        id: sink_id,
        name: info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        description: info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        form_factor,
        volume: info.volume.into(),
        base_volume: info.base_volume.into(),
        mute: info.mute,
        port_available,
      };

      this.sinks.insert(info.index, managed.clone());
      list_event = Some(SinkListEvent::Added(Box::new(managed)));
    }

    // Now send events after releasing the sink borrow
    for event in events_to_send {
      this.notify_sink_subscribers(sink_id, event);
    }
    if let Some(event) = list_event {
      this.notify_sink_list_subscribers(event);
    }
  }

  fn handle_source_info(this: &Rc<RefCell<Self>>, info: ListResult<&introspect::SourceInfo>) {
    let ListResult::Item(info) = info else { return };

    let is_hardware = info.flags.contains(def::SourceFlagSet::HARDWARE);

    // Skip virtual (non-hardware) sources entirely
    if !is_hardware {
      return;
    }

    let mut this = this.borrow_mut();
    let source_id = SourceId(info.index);

    let icon_name = info
      .proplist
      .get_str("device.icon_name")
      .map(|s| SharedString::from(s.to_string()));

    let device_class = info
      .proplist
      .get_str("device.class")
      .map(|s| SharedString::from(s.to_string()));

    let port_available = info.active_port.as_ref().and_then(|port| match port.available {
      def::PortAvailable::Unknown => None,
      def::PortAvailable::No => Some(false),
      def::PortAvailable::Yes => Some(true),
    });

    // Collect events to send after releasing the source borrow
    let mut events_to_send: Vec<SourceEvent> = Vec::new();
    let mut list_event: Option<SourceListEvent> = None;

    if let Some(source) = this.sources.get_mut(&info.index) {
      if source.volume != info.volume {
        source.volume = info.volume.into();
        source.base_volume = info.base_volume.into();
        events_to_send.push(SourceEvent::VolumeChanged(source.volume.clone()));
      }

      if source.mute != info.mute {
        source.mute = info.mute;
        events_to_send.push(SourceEvent::MuteChanged(info.mute));
      }

      if source.name.as_ref().map(|s| s.as_str()) != info.name.as_deref()
        || source.description.as_ref().map(|s| s.as_str()) != info.description.as_deref()
        || source.icon_name != icon_name
        || source.device_class != device_class
        || source.port_available != port_available
      {
        source.name = info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        source.description = info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        source.icon_name = icon_name;
        source.device_class = device_class;
        source.port_available = port_available;
        events_to_send.push(SourceEvent::InfoChanged(source.clone()));
      }
    } else {
      let managed = SourceInfo {
        id: source_id,
        name: info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        description: info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        icon_name,
        device_class,
        volume: info.volume.into(),
        base_volume: info.base_volume.into(),
        mute: info.mute,
        port_available,
      };

      this.sources.insert(info.index, managed.clone());
      list_event = Some(SourceListEvent::Added(managed));
    }

    // Now send events after releasing the source borrow
    for event in events_to_send {
      this.notify_source_subscribers(source_id, event);
    }
    if let Some(event) = list_event {
      this.notify_source_list_subscribers(event);
    }
  }

  fn handle_sink_input_info(
    this: &Rc<RefCell<Self>>,
    info: ListResult<&introspect::SinkInputInfo>,
  ) {
    let ListResult::Item(info) = info else { return };

    let mut this = this.borrow_mut();
    let sink_input_id = SinkInputId(info.index);
    let sink_id = SinkId(info.sink);

    // Extract application name from proplist
    let application_name = info
      .proplist
      .get_str("application.name")
      .map(|s| SharedString::from(s.to_string()));

    // Extract icon name from proplist, fall back to application name
    let icon_name = info
      .proplist
      .get_str("application.icon")
      .or_else(|| info.proplist.get_str("application.name"))
      .map(|s| SharedString::from(s.to_string()));

    let mut events_to_send: Vec<SinkInputEvent> = Vec::new();
    let mut list_event: Option<SinkInputListEvent> = None;

    if let Some(sink_input) = this.sink_inputs.get_mut(&info.index) {
      // Check for sink change (moved to different output)
      if sink_input.sink_id != sink_id {
        sink_input.sink_id = sink_id;
        events_to_send.push(SinkInputEvent::SinkChanged(sink_id));
      }

      if sink_input.volume != info.volume {
        sink_input.volume = info.volume.into();
        events_to_send.push(SinkInputEvent::VolumeChanged(sink_input.volume.clone()));
      }

      if sink_input.mute != info.mute {
        sink_input.mute = info.mute;
        events_to_send.push(SinkInputEvent::MuteChanged(info.mute));
      }

      if sink_input.name.as_ref().map(|s| s.as_str()) != info.name.as_deref()
        || sink_input.application_name != application_name
        || sink_input.icon_name != icon_name
      {
        sink_input.name = info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        sink_input.application_name = application_name.clone();
        sink_input.icon_name = icon_name.clone();
        events_to_send.push(SinkInputEvent::InfoChanged(sink_input.clone()));
      }
    } else {
      let managed = SinkInputInfo {
        id: sink_input_id,
        name: info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        sink_id,
        volume: info.volume.into(),
        mute: info.mute,
        application_name,
        icon_name,
      };

      this.sink_inputs.insert(info.index, managed.clone());
      list_event = Some(SinkInputListEvent::Added(managed));
    }

    for event in events_to_send {
      this.notify_sink_input_subscribers(sink_input_id, event);
    }
    if let Some(event) = list_event {
      this.notify_sink_input_list_subscribers(event);
    }
  }
}
