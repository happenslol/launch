//! One greeter connection.
//!
//! Requests arrive on this task and are dispatched as they come; events go out
//! on a task of their own, which is the only thing that ever writes to the
//! socket, so two frames can never interleave.
//!
//! The read and write halves are handled by separate tasks rather than one
//! `select!` over both. Framed reads are not cancel-safe - losing a partially
//! read frame would desynchronise the stream for good - and `select!` drops
//! whichever future did not finish.

use std::rc::Rc;

use anyhow::Result;
use greet_ipc::codec::Error as CodecError;
use greet_ipc::codec::tokio_io::{read_frame, write_frame};
use greet_ipc::{Event, Request};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task;
use tracing::{debug, info, warn};

use crate::context::Context;

pub async fn serve(context: Rc<Context>, stream: UnixStream) {
  let (events, mut outgoing) = mpsc::unbounded_channel();

  if let Err(error) = context.attach_client(events).await {
    warn!(?error, "Refusing a greeter connection");
    return;
  }

  info!("Greeter connected");

  let (mut reader, mut writer) = stream.into_split();

  let sender = task::spawn_local(async move {
    while let Some(event) = outgoing.recv().await {
      if let Err(error) = write_frame(&mut writer, &event).await {
        debug!(?error, "Failed to write to the greeter");
        break;
      }
    }
  });

  loop {
    let request = match read_frame::<Request, _>(&mut reader).await {
      Ok(request) => request,
      Err(CodecError::Eof) => {
        info!("Greeter disconnected");
        break;
      }
      Err(error) => {
        warn!(?error, "Dropping the greeter connection");
        break;
      }
    };

    if let Err(error) = dispatch(&context, request).await {
      // Surfaced as well as logged: a greeter that has got out of step should
      // say so rather than sitting there looking ready.
      warn!(?error, "Request failed");
      context
        .notify(Event::RequestFailed {
          message: format!("{error:#}"),
        })
        .await;
    }
  }

  sender.abort();
  context.detach_client().await;
}

async fn dispatch(context: &Rc<Context>, request: Request) -> Result<()> {
  match request {
    Request::Hello { version } => {
      if version != greet_ipc::PROTOCOL_VERSION {
        warn!(
          greeter = version,
          daemon = greet_ipc::PROTOCOL_VERSION,
          "Protocol version mismatch"
        );
      }

      context.notify(context.welcome()).await;
      Ok(())
    }
    Request::Authenticate { username } => context.authenticate(username).await,
    Request::Password { value } => context.password(value).await,
    Request::Cancel => {
      context.cancel().await;
      Ok(())
    }
    Request::StartSession => context.start_session().await,
  }
}
