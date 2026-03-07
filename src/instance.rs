use std::{
  io::ErrorKind,
  os::{
    linux::net::SocketAddrExt,
    unix::net::{SocketAddr, UnixListener, UnixStream},
  },
  process, thread,
  time::Duration,
};

use anyhow::Result;
use flume::Receiver;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Request {
  build_id: Option<Vec<u8>>,
  panel: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub enum Response {
  Accepted,
  Quitting,
}

pub enum Message {
  Open { panel: Option<String> },
}

pub enum Role {
  Server(UnixListener),
  Client(UnixStream),
}

fn current_build_id() -> Option<Vec<u8>> {
  buildid::build_id().map(|id| id.to_vec())
}

fn socket_address() -> Result<SocketAddr> {
  Ok(SocketAddr::from_abstract_name("launch")?)
}

pub fn acquire() -> Result<Role> {
  let address = socket_address()?;

  match UnixListener::bind_addr(&address) {
    Ok(listener) => Ok(Role::Server(listener)),
    Err(err) if err.kind() == ErrorKind::AddrInUse => {
      let stream = UnixStream::connect_addr(&address)?;
      Ok(Role::Client(stream))
    }
    Err(err) => Err(err.into()),
  }
}

pub fn acquire_after_quit() -> Result<UnixListener> {
  let address = socket_address()?;

  for _ in 0..100 {
    match UnixListener::bind_addr(&address) {
      Ok(listener) => return Ok(listener),
      Err(err) if err.kind() == ErrorKind::AddrInUse => {
        thread::sleep(Duration::from_millis(10));
      }
      Err(err) => return Err(err.into()),
    }
  }

  anyhow::bail!("Timed out waiting for previous instance to release socket")
}

pub fn send_open(stream: &mut UnixStream, panel: Option<String>) -> Result<Response> {
  let request = Request {
    build_id: current_build_id(),
    panel,
  };
  rmp_serde::encode::write(stream, &request)?;
  let response: Response = rmp_serde::from_read(&*stream)?;
  Ok(response)
}

pub fn listen(listener: UnixListener) -> Receiver<Message> {
  let build_id = current_build_id();
  let (sender, receiver) = flume::unbounded();

  thread::spawn(move || {
    loop {
      let (mut stream, _) = match listener.accept() {
        Ok(pair) => pair,
        Err(err) => {
          tracing::error!(?err, "Failed to accept connection");
          continue;
        }
      };

      let request: Request = match rmp_serde::from_read(&stream) {
        Ok(request) => request,
        Err(err) => {
          tracing::error!(?err, "Failed to decode request");
          continue;
        }
      };

      let version_matches = match (&build_id, &request.build_id) {
        (Some(ours), Some(theirs)) => ours == theirs,
        _ => true,
      };

      if !version_matches {
        tracing::info!("New version detected, quitting to allow takeover");
        let _ = rmp_serde::encode::write(&mut stream, &Response::Quitting);
        process::exit(0);
      }

      let _ = rmp_serde::encode::write(&mut stream, &Response::Accepted);

      if sender
        .send(Message::Open {
          panel: request.panel,
        })
        .is_err()
      {
        break;
      }
    }
  });

  receiver
}
