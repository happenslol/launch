use std::{
  cell::RefCell,
  fs::File,
  os::fd::{AsRawFd, OwnedFd},
  rc::Rc,
  thread::{self, JoinHandle},
};

use flume::{Receiver, Sender};
use pulse::{
  context::{
    Context, FlagSet as ContextFlagSet,
    subscribe::{Facility, Operation},
  },
  mainloop::{
    api::{Mainloop as _, MainloopInner},
    events::io::{FlagSet as EventFlagSet, IoEvent, IoEventRef},
    standard::{Mainloop, MainloopInternal},
  },
};
use rustix::pipe::PipeFlags;
use thiserror::Error;

#[derive(Debug)]
pub enum Event {
  Exited(PulseError),
}

#[derive(Debug)]
pub enum Command {
  Quit,
}

#[derive(Debug, Clone, Error)]
pub enum PulseError {
  #[error("Failed to create PA main loop")]
  MainLoopCreate,
  #[error("Failed to create PA context")]
  ContextCreate,
  #[error("PA error: {0}")]
  PAErr(#[from] pulse::error::PAErr),
}

pub struct PulseThread {
  handle: JoinHandle<()>,
  notify_fd: File,
  event_rx: Receiver<Event>,
}

impl PulseThread {
  pub fn spawn() -> Self {
    let (reader, writer) =
      rustix::pipe::pipe_with(PipeFlags::CLOEXEC).expect("failed to create pipe");

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
    }
  }

  pub fn get_event_rx(&self) -> Receiver<Event> {
    self.event_rx.clone()
  }
}

fn thread_main(
  notify_fd: OwnedFd,
  event_tx: Sender<Event>,
  command_rx: Receiver<Command>,
) -> Result<(), PulseError> {
  let mut main_loop = Mainloop::new().ok_or(PulseError::MainLoopCreate)?;
  let mut context = Context::new(&main_loop, "launch").ok_or(PulseError::ContextCreate)?;
  let state = PulseState::new(event_tx);

  *state.notify_io.borrow_mut() =
    main_loop.new_io_event(notify_fd.as_raw_fd(), EventFlagSet::INPUT, {
      let state = state.clone();
      let notify_fd = File::from(notify_fd);
      let command_rx = command_rx.clone();

      Box::new(
        move |ev, fd, _| {
          while let Ok(cmd) = command_rx.try_recv() {}
        },
      )
    });

  context.set_subscribe_callback({
    let state = state.clone();
    Some(Box::new(move |facility, operation, index| {
      state.on_subscribe(facility, operation, index)
    }))
  });

  context.set_state_callback({
    let state = state.clone();
    Some(Box::new(move || state.on_state()))
  });

  context.connect(None, ContextFlagSet::NOFAIL, None)?;
  if let Err((err, _)) = main_loop.run() {
    return Err(err.into());
  }

  Ok(())
}

struct PulseState {
  event_tx: Sender<Event>,
  notify_io: RefCell<Option<IoEvent<MainloopInner<MainloopInternal>>>>,
}

impl PulseState {
  fn new(event_tx: Sender<Event>) -> Rc<Self> {
    Rc::new(Self {
      event_tx,
      notify_io: RefCell::new(None),
    })
  }

  fn on_subscribe(
    self: &Rc<Self>,
    facility: Option<Facility>,
    operation: Option<Operation>,
    index: u32,
  ) {
  }

  fn on_state(self: &Rc<Self>) {}

  fn on_command(self: &Rc<Self>, ev: IoEventRef<MainloopInner<MainloopInternal>>) {}
}
