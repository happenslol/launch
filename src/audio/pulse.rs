use std::{
  cell::RefCell,
  fs::File,
  io::{Read, Write},
  os::fd::{AsRawFd, OwnedFd},
  rc::Rc,
  thread::{self, JoinHandle},
};

use flume::{Receiver, Sender};
use pulse::{
  callbacks::ListResult, context::{
    Context, FlagSet as ContextFlagSet,
    introspect::Introspector,
    subscribe::{Facility, InterestMaskSet, Operation},
  }, def::Retval, mainloop::{
    api::Mainloop as _,
    events::io::FlagSet as EventFlagSet,
    standard::{IterateResult, Mainloop},
  }, volume::ChannelVolumes
};
use rustix::pipe::PipeFlags;
use thiserror::Error;
use tracing::debug;

#[derive(Debug)]
pub enum Event {
  Exited(PulseError),
}

#[derive(Debug)]
pub enum Command {
  SetVolume(u32, u16),
  Quit,
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
  handle: JoinHandle<()>,
  event_rx: Receiver<Event>,

  // We use a pipe to notify the pulse main loop when we have sent new commands
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
      handle,
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
    let _ = self.notify_fd.try_clone().unwrap().write(&[1u8]);
  }
}

fn thread_main(
  notify_fd: OwnedFd,
  event_tx: Sender<Event>,
  command_rx: Receiver<Command>,
) -> Result<(), PulseError> {
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
      | InterestMaskSet::SOURCE
      | InterestMaskSet::CARD,
    |_| {},
  );

  let introspector = context.introspect();

  introspector.get_sink_info_list(|result| {
    let ListResult::Item(item) = result else {
      return;
    };

    // TODO: Emit event
  });

  introspector.get_source_info_list(|result| {
    let ListResult::Item(item) = result else {
      return;
    };

    // TODO: Emit event
  });

  context.set_subscribe_callback(Some(Box::new(move |facility, operation, index| {
    handle_event(&event_tx, &introspector, facility, operation, index);
  })));

  // We have to hold this handle to keep the event source alive
  let mut notify_fd = File::from(notify_fd);
  let _io_handle = main_loop.new_io_event(notify_fd.as_raw_fd(), EventFlagSet::INPUT, {
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
        handle_command(&mut main_loop, &mut context, cmd);
      }
    })
  });

  if let Err((err, _)) = main_loop.run() {
    return Err(err.into());
  }

  Ok(())
}

fn handle_command(main_loop: &mut Mainloop, context: &mut Context, command: Command) {
  match command {
    Command::Quit => main_loop.quit(Retval(0)),
    Command::SetVolume(index, _volume) => {
      let _ = context.introspect().set_sink_volume_by_index(
        index,
        &ChannelVolumes::default(),
        Some(Box::new(|_| {})),
      );
    }
  }
}

fn handle_event(
  event_tx: &Sender<Event>,
  introspector: &Introspector,
  facility: Option<Facility>,
  operation: Option<Operation>,
  index: u32,
) {
  let (Some(facility), Some(operation)) = (facility, operation) else {
    debug!(?facility, ?operation, index, "handle_event: invalid event");
    return;
  };

  match facility {
    Facility::Sink => {}
    Facility::Source => {}
    _ => {}
  }

  // println!("handle_event: {:?} {:?} {}", facility, operation, index);
}
