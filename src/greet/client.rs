//! The greeter's end of the daemon socket.
//!
//! The daemon pushes as much as it answers - a fingerprint reader reports
//! progress nobody asked for - so there is no request/response pairing here.
//! Requests go out and are forgotten; every frame that comes back is delivered
//! as an event, and the state machine in [`super::Greeter`] decides whether it
//! still matters.

use async_net::unix::UnixStream;
use futures::{AsyncReadExt as _, FutureExt as _, select_biased};
use gpui::{App, AppContext as _, SharedString, Task};
use greet_ipc::codec::Error as CodecError;
use greet_ipc::codec::futures_io::{read_frame, write_frame};
use greet_ipc::{Event, Request};
use tracing::{debug, warn};

/// What the connection has to say for itself.
pub enum ClientEvent {
  /// A frame from the daemon.
  Message(Event),
  /// The socket went away, with the reason. The task has stopped, and the
  /// caller decides whether to try again.
  Disconnected(SharedString),
}

pub struct GreetClient {
  requests: flume::Sender<Request>,
  _task: Task<()>,
}

impl GreetClient {
  /// Connects to the socket named by the daemon and starts pumping frames.
  ///
  /// A failure to connect arrives as [`ClientEvent::Disconnected`] rather than
  /// an error, so the caller has one path for "never got there" and "was there
  /// and went away".
  pub fn connect(events: flume::Sender<ClientEvent>, cx: &App) -> Self {
    let (requests, outgoing) = flume::unbounded();

    let task = cx.background_spawn(async move {
      let reason = run(&outgoing, &events).await;
      debug!(%reason, "Greeter connection ended");

      if events.send(ClientEvent::Disconnected(reason)).is_err() {
        debug!("Login screen went away before the disconnect was reported");
      }
    });

    Self {
      requests,
      _task: task,
    }
  }

  /// Queues a request. Dropping it silently is correct: the only way this fails
  /// is that the connection has already gone, which the caller is about to be
  /// told about anyway.
  pub fn send(&self, request: Request) {
    if self.requests.send(request).is_err() {
      debug!("Dropping a request for a connection that has gone");
    }
  }
}

async fn run(
  outgoing: &flume::Receiver<Request>,
  events: &flume::Sender<ClientEvent>,
) -> SharedString {
  let Some(path) = std::env::var_os(greet_ipc::SOCKET_ENV_VAR) else {
    // No retry can fix this: it means we were not started by the daemon.
    return "The login screen was started outside of launch-greetd".into();
  };

  let stream = match UnixStream::connect(&path).await {
    Ok(stream) => stream,
    Err(error) => {
      warn!(?error, ?path, "Failed to reach the login service");
      return "The login service cannot be reached".into();
    }
  };

  let (mut reader, mut writer) = stream.split();

  let incoming = async {
    loop {
      match read_frame::<Event, _>(&mut reader).await {
        Ok(event) => {
          if events.send(ClientEvent::Message(event)).is_err() {
            return SharedString::from("The login screen stopped listening");
          }
        }
        Err(CodecError::Eof) => return "The login service closed the connection".into(),
        Err(error) => {
          warn!(?error, "Failed to read from the login service");
          return "The login service sent something unreadable".into();
        }
      }
    }
  };

  let sending = async {
    while let Ok(request) = outgoing.recv_async().await {
      if let Err(error) = write_frame(&mut writer, &request).await {
        warn!(?error, "Failed to write to the login service");
        return SharedString::from("The login service stopped accepting requests");
      }
    }

    "The login screen stopped sending".into()
  };

  // Whichever finishes first is the reason; the other is dropped, which is fine
  // because either one ending means the connection is over.
  select_biased! {
    reason = incoming.fuse() => reason,
    reason = sending.fuse() => reason,
  }
}
