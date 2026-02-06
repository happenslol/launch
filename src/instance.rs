use std::{
    io::ErrorKind,
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    thread,
};

use anyhow::Result;
use flume::Receiver;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
pub enum Message {
    Open { panel: Option<String> },
}

pub enum Role {
    Server(UnixListener),
    Client(UnixStream),
}

pub fn acquire() -> Result<Role> {
    let address = SocketAddr::from_abstract_name("launch")?;

    match UnixListener::bind_addr(&address) {
        Ok(listener) => Ok(Role::Server(listener)),
        Err(err) if err.kind() == ErrorKind::AddrInUse => {
            let stream = UnixStream::connect_addr(&address)?;
            Ok(Role::Client(stream))
        }
        Err(err) => Err(err.into()),
    }
}

pub fn listen(listener: UnixListener) -> Receiver<Message> {
    let (sender, receiver) = flume::unbounded();

    thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::error!(?err, "Failed to accept connection");
                    continue;
                }
            };

            match rmp_serde::from_read(&stream) {
                Ok(message) => {
                    if sender.send(message).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    tracing::error!(?err, "Failed to decode message");
                }
            }
        }
    });

    receiver
}
