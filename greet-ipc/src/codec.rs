//! Framing: a little-endian `u32` length followed by JSON.
//!
//! greetd, which this protocol descends from, writes the length in native byte
//! order and then allocates a buffer of exactly that size before reading any of
//! it. Both are avoided here: the byte order is fixed so the format is a
//! property of the protocol rather than of the machine, and the length is
//! checked against [`MAX_FRAME_LEN`] before anything is allocated.
//!
//! Every buffer is [`Zeroizing`], unconditionally rather than only for the
//! frames known to carry a password. Marking individual frames would work right
//! up until someone adds one and forgets, and the cost is a memset of a few
//! hundred bytes.

use std::io;

use serde::Serialize;
use zeroize::Zeroizing;

use crate::MAX_FRAME_LEN;

/// Width of the length prefix.
pub const LEN_BYTES: usize = 4;

#[derive(Debug, thiserror::Error)]
pub enum Error {
  /// The peer announced a frame larger than [`MAX_FRAME_LEN`]. Not recoverable:
  /// the stream can't be resynchronised, so the connection has to go.
  #[error("frame of {len} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
  FrameTooLarge { len: u32 },
  /// The peer closed the connection between frames. Ordinary, not an error in
  /// itself.
  #[error("the connection was closed")]
  Eof,
  #[error("frame io failed")]
  Io(#[from] io::Error),
  #[error("frame was not valid json")]
  Serialization(#[from] serde_json::Error),
}

/// Serializes `value` into a complete frame, length prefix included, so it can
/// go out in a single write.
pub fn encode<T: Serialize>(value: &T) -> Result<Zeroizing<Vec<u8>>, Error> {
  let body = Zeroizing::new(serde_json::to_vec(value)?);

  let len = u32::try_from(body.len()).unwrap_or(u32::MAX);
  if len > MAX_FRAME_LEN {
    return Err(Error::FrameTooLarge { len });
  }

  // Sized exactly, so the extends below cannot reallocate. A reallocation would
  // leave the old buffer behind unzeroed, which is the whole thing this is
  // trying to avoid.
  let mut frame = Zeroizing::new(Vec::with_capacity(LEN_BYTES + body.len()));
  frame.extend_from_slice(&len.to_le_bytes());
  frame.extend_from_slice(&body);

  Ok(frame)
}

/// Validates an announced length before it is used to allocate.
pub fn check_len(prefix: [u8; LEN_BYTES]) -> Result<usize, Error> {
  let len = u32::from_le_bytes(prefix);
  if len > MAX_FRAME_LEN {
    return Err(Error::FrameTooLarge { len });
  }

  Ok(len as usize)
}

/// An `UnexpectedEof` while reading the length means the peer hung up between
/// frames, which is a clean close. The same error part-way through a body is a
/// truncated frame, and stays an io error.
#[cfg(any(feature = "tokio", feature = "futures-io"))]
fn length_read_error(error: io::Error) -> Error {
  if error.kind() == io::ErrorKind::UnexpectedEof {
    return Error::Eof;
  }

  Error::Io(error)
}

#[cfg(feature = "tokio")]
pub mod tokio_io {
  use super::*;
  use serde::de::DeserializeOwned;
  use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

  pub async fn read_frame<T: DeserializeOwned, R: AsyncRead + Unpin>(
    reader: &mut R,
  ) -> Result<T, Error> {
    let mut prefix = [0u8; LEN_BYTES];
    reader
      .read_exact(&mut prefix)
      .await
      .map_err(length_read_error)?;

    let mut body = Zeroizing::new(vec![0u8; check_len(prefix)?]);
    reader.read_exact(&mut body).await?;

    Ok(serde_json::from_slice(&body)?)
  }

  pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
  ) -> Result<(), Error> {
    writer.write_all(&encode(value)?).await?;
    writer.flush().await?;
    Ok(())
  }
}

#[cfg(feature = "futures-io")]
pub mod futures_io {
  use super::*;
  use futures_util::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
  use serde::de::DeserializeOwned;

  pub async fn read_frame<T: DeserializeOwned, R: AsyncRead + Unpin>(
    reader: &mut R,
  ) -> Result<T, Error> {
    let mut prefix = [0u8; LEN_BYTES];
    reader
      .read_exact(&mut prefix)
      .await
      .map_err(length_read_error)?;

    let mut body = Zeroizing::new(vec![0u8; check_len(prefix)?]);
    reader.read_exact(&mut body).await?;

    Ok(serde_json::from_slice(&body)?)
  }

  pub async fn write_frame<T: Serialize, W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &T,
  ) -> Result<(), Error> {
    writer.write_all(&encode(value)?).await?;
    writer.flush().await?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{AuthSource, Event, FingerprintState, Request, Secret};

  #[test]
  fn encodes_a_little_endian_length_prefix() {
    let frame = encode(&Request::Cancel).expect("encodes");
    let body_len = frame.len() - LEN_BYTES;

    let mut prefix = [0u8; LEN_BYTES];
    prefix.copy_from_slice(&frame[..LEN_BYTES]);
    assert_eq!(u32::from_le_bytes(prefix) as usize, body_len);
  }

  #[test]
  fn round_trips_every_request() {
    let requests = vec![
      Request::Hello { version: 1 },
      Request::Authenticate {
        username: "ada".to_owned(),
      },
      Request::Password {
        value: Secret::new("hunter2".to_owned()).expect("within the length bound"),
      },
      Request::Cancel,
      Request::StartSession,
    ];

    for request in requests {
      let frame = encode(&request).expect("encodes");
      let decoded: Request = serde_json::from_slice(&frame[LEN_BYTES..]).expect("decodes");
      assert_eq!(
        serde_json::to_string(&request).expect("serializes"),
        serde_json::to_string(&decoded).expect("serializes"),
      );
    }
  }

  #[test]
  fn round_trips_every_event() {
    let events = vec![
      Event::Welcome {
        version: 1,
        users: vec![],
        default_user: "ada".to_owned(),
        fingerprint: true,
        primary_output: Some("eDP-1".to_owned()),
      },
      Event::Prompt {
        source: AuthSource::Password,
        echo: false,
      },
      Event::Info {
        source: AuthSource::Fingerprint,
        message: "Swipe your finger".to_owned(),
      },
      Event::Error {
        source: AuthSource::Password,
        message: "nope".to_owned(),
      },
      Event::Fingerprint {
        state: FingerprintState::Waiting,
      },
      Event::Failed {
        source: AuthSource::Password,
        failure: crate::AuthFailure::Rejected,
        retry: true,
      },
      Event::Authenticated {
        via: AuthSource::Fingerprint,
      },
      Event::SessionStarted,
      Event::SessionFailed {
        message: "no such command".to_owned(),
      },
    ];

    for event in events {
      let frame = encode(&event).expect("encodes");
      let decoded: Event = serde_json::from_slice(&frame[LEN_BYTES..]).expect("decodes");
      assert_eq!(
        serde_json::to_string(&event).expect("serializes"),
        serde_json::to_string(&decoded).expect("serializes"),
      );
    }
  }

  #[test]
  fn rejects_an_oversized_length_before_allocating() {
    let error = check_len((MAX_FRAME_LEN + 1).to_le_bytes()).expect_err("is rejected");
    assert!(matches!(error, Error::FrameTooLarge { .. }));

    // The pathological case: a peer announcing 4 GiB. This must not allocate.
    let error = check_len(u32::MAX.to_le_bytes()).expect_err("is rejected");
    assert!(matches!(
      error,
      Error::FrameTooLarge { len } if len == u32::MAX
    ));

    assert_eq!(
      check_len(MAX_FRAME_LEN.to_le_bytes()).expect("is at the limit"),
      MAX_FRAME_LEN as usize
    );
  }
}
