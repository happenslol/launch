use std::{
  cell::RefCell,
  collections::HashMap,
  fs::File,
  io::{Read, Write},
  os::fd::{AsRawFd, OwnedFd},
  rc::Rc,
  thread::{self, JoinHandle},
};

use flume::{Receiver, Sender};
use futures::channel::oneshot;
use gpui::{App, AppContext, SharedString, Task};
use pulse::{
  callbacks::ListResult,
  context::{
    Context, FlagSet as ContextFlagSet,
    introspect::{self, Introspector, ServerInfo},
    subscribe::{Facility, InterestMaskSet, Operation},
  },
  def::Retval,
  mainloop::{
    api::Mainloop as _,
    events::io::FlagSet as EventFlagSet,
    standard::{IterateResult, Mainloop},
  },
};
use rustix::pipe::PipeFlags;
use thiserror::Error;
use tracing::{debug, warn};

use super::types::{ChannelVolumes, SinkId, SinkInfo, SourceId, SourceInfo};

#[derive(Debug)]
pub enum Event {
  // Sink events
  SinkFound(SinkInfo),
  SinkInfoChanged(SinkInfo),
  SinkVolumeChanged(SinkId, ChannelVolumes),
  SinkMuteChanged(SinkId, bool),
  SinkRemoved(SinkId),
  DefaultSinkChanged(Option<SinkId>),

  // Source events
  SourceFound(SourceInfo),
  SourceInfoChanged(SourceInfo),
  SourceVolumeChanged(SourceId, ChannelVolumes),
  SourceMuteChanged(SourceId, bool),
  SourceRemoved(SourceId),
  DefaultSourceChanged(Option<SourceId>),

  Exited(PulseError),
}

#[derive(Debug, Clone, Copy)]
pub enum SetVolume {
  Absolute(u32),
  AbsolutePercent(u32),
  Relative(i32),
  RelativePercent(i32),
}

#[derive(Debug, Clone, Copy)]
pub enum SetMute {
  Mute,
  Unmute,
  Toggle,
}

#[derive(Debug)]
pub enum Command {
  SetSinkVolume(SinkId, SetVolume),
  SetSourceVolume(SourceId, SetVolume),
  SetSinkMute(SinkId, SetMute),
  SetSourceMute(SourceId, SetMute),
  SetDefaultSink(SinkId, oneshot::Sender<bool>),
  SetDefaultSource(SourceId, oneshot::Sender<bool>),
  Quit(oneshot::Sender<()>),
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
  event_rx: Receiver<Event>,

  // Pipe to notify the pulse main loop when we have sent new commands
  notify_fd: File,
  command_tx: Sender<Command>,
}

impl PulseThread {
  pub fn spawn() -> Self {
    let (reader, writer) =
      rustix::pipe::pipe_with(PipeFlags::CLOEXEC).expect("failed to create pipe");

    // Are these limits reasonable? How fast do we get events when changing volume?
    let (event_tx, event_rx) = flume::bounded(20);
    let (command_tx, command_rx) = flume::bounded(20);

    let handle = thread::spawn(move || {
      if let Err(err) = thread_main(reader, event_tx.clone(), command_rx) {
        let _ = event_tx.send(Event::Exited(err));
      }
    });

    let notify_fd = File::from(writer);

    Self {
      handle: Some(handle),
      event_rx,
      notify_fd,
      command_tx,
    }
  }

  pub fn get_event_rx(&self) -> Receiver<Event> {
    self.event_rx.clone()
  }

  pub fn send_command(&self, cmd: Command) {
    let _ = self.command_tx.send(cmd);
    let _ = self.notify_fd.try_clone().unwrap().write(&[1]);
  }

  pub fn quit(&mut self, cx: &mut App) -> Task<()> {
    let (tx, rx) = oneshot::channel::<()>();
    self.send_command(Command::Quit(tx));
    let handle = self.handle.take().expect("Pulse thread already quit");

    cx.background_spawn(async move {
      // TODO: Timeout?
      let _ = rx.await;
      let _ = handle.join();
    })
  }
}

fn thread_main(
  notify_fd: OwnedFd,
  event_tx: Sender<Event>,
  command_rx: Receiver<Command>,
) -> Result<(), PulseError> {
  let state = Rc::new(RefCell::new(PulseState::new(event_tx.clone())));
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

  // Make sure to request server info after sinks and sources so we can match default sink and
  // source up
  introspector.get_server_info({
    let state = state.clone();
    move |result| PulseState::handle_server_info(&state, result)
  });

  context.set_subscribe_callback(Some(Box::new({
    let state = state.clone();
    move |facility, operation, index| {
      handle_event(&state, &event_tx, &introspector, facility, operation, index);
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
    Command::SetSourceVolume(id, set) => {}
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
  }
}

fn handle_event(
  state: &Rc<RefCell<PulseState>>,
  event_tx: &Sender<Event>,
  introspector: &Introspector,
  facility: Option<Facility>,
  operation: Option<Operation>,
  index: u32,
) {
  let (Some(facility), Some(operation)) = (facility, operation) else {
    debug!(?facility, ?operation, index, "Skipping event");
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
      _ => debug!(?facility, ?operation, index, "Skipping event"),
    },
    Operation::Removed => match facility {
      Facility::Sink if state.borrow_mut().sinks.remove(&index).is_some() => {
        let _ = event_tx.send(Event::SinkRemoved(SinkId(index)));
      }
      Facility::Source if state.borrow_mut().sources.remove(&index).is_some() => {
        let _ = event_tx.send(Event::SourceRemoved(SourceId(index)));
      }
      _ => debug!(?facility, index, "Skipping remove event"),
    },
  }
}

#[derive(Debug)]
struct PulseState {
  shutdown_tx: Option<oneshot::Sender<()>>,
  event_tx: Sender<Event>,
  sinks: HashMap<u32, SinkInfo>,
  sources: HashMap<u32, SourceInfo>,
  default_sink_id: Option<SinkId>,
  default_source_id: Option<SourceId>,
}

impl PulseState {
  fn new(event_tx: Sender<Event>) -> Self {
    Self {
      shutdown_tx: None,
      event_tx,
      sinks: Default::default(),
      sources: Default::default(),
      default_sink_id: None,
      default_source_id: None,
    }
  }

  fn handle_server_info(this: &Rc<RefCell<Self>>, info: &ServerInfo) {
    let mut this = this.borrow_mut();
    let event_tx = this.event_tx.clone();

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
      this.default_sink_id = default_sink_id;
      let _ = event_tx.send(Event::DefaultSinkChanged(default_sink_id));
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
      this.default_source_id = default_source_id;
      let _ = event_tx.send(Event::DefaultSourceChanged(default_source_id));
    }
  }

  fn handle_sink_info(this: &Rc<RefCell<Self>>, info: ListResult<&introspect::SinkInfo>) {
    let ListResult::Item(info) = info else { return };

    let mut this = this.borrow_mut();
    let event_tx = this.event_tx.clone();

    if let Some(sink) = this.sinks.get_mut(&info.index) {
      if sink.volume != info.volume {
        sink.volume = info.volume.into();
        sink.base_volume = info.base_volume.into();
        let _ = event_tx.send(Event::SinkVolumeChanged(
          SinkId(info.index),
          sink.volume.clone(),
        ));
      }

      if sink.mute != info.mute {
        sink.mute = info.mute;
        let _ = event_tx.send(Event::SinkMuteChanged(SinkId(info.index), info.mute));
      }

      if sink.name.as_ref().map(|s| s.as_str()) != info.name.as_deref()
        || sink.description.as_ref().map(|s| s.as_str()) != info.description.as_deref()
      {
        sink.name = info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        sink.description = info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        let _ = event_tx.send(Event::SinkInfoChanged(sink.clone()));
      }
    } else {
      let managed = SinkInfo {
        id: SinkId(info.index),
        name: info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        description: info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        volume: info.volume.into(),
        base_volume: info.base_volume.into(),
        mute: info.mute,
      };

      this.sinks.insert(info.index, managed.clone());
      let _ = event_tx.send(Event::SinkFound(managed));
    }
  }

  fn handle_source_info(this: &Rc<RefCell<Self>>, info: ListResult<&introspect::SourceInfo>) {
    let ListResult::Item(info) = info else { return };

    let mut this = this.borrow_mut();
    let event_tx = this.event_tx.clone();

    if let Some(source) = this.sources.get_mut(&info.index) {
      if source.volume != info.volume {
        source.volume = info.volume.into();
        source.base_volume = info.base_volume.into();
        let _ = event_tx.send(Event::SourceVolumeChanged(
          SourceId(info.index),
          source.volume.clone(),
        ));
      }

      if source.mute != info.mute {
        source.mute = info.mute;
        let _ = event_tx.send(Event::SourceMuteChanged(SourceId(info.index), info.mute));
      }

      if source.name.as_ref().map(|s| s.as_str()) != info.name.as_deref()
        || source.description.as_ref().map(|s| s.as_str()) != info.description.as_deref()
      {
        source.name = info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        source.description = info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string()));
        let _ = event_tx.send(Event::SourceInfoChanged(source.clone()));
      }
    } else {
      let managed = SourceInfo {
        id: SourceId(info.index),
        name: info
          .name
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        description: info
          .description
          .as_ref()
          .map(|s| SharedString::from(s.to_string())),
        volume: info.volume.into(),
        base_volume: info.base_volume.into(),
        mute: info.mute,
      };

      this.sources.insert(info.index, managed.clone());
      let _ = event_tx.send(Event::SourceFound(managed));
    }
  }
}
