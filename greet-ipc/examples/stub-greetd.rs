//! A stand-in for `launch-greetd`, for exercising the login screen without root
//! or a spare VT.
//!
//! Speaks enough of the protocol to drive every branch of the greeter's state
//! machine: two accounts, a password that is accepted only if it is
//! `"correct"`, and a session that reports itself started. Run it with a socket
//! path, point `LAUNCH_GREETD_SOCK` at the same path, and start `launch greet`.

use async_net::unix::UnixListener;
use futures::{AsyncReadExt as _, io::WriteHalf};
use greet_ipc::codec::futures_io::{read_frame, write_frame};
use greet_ipc::{AuthFailure, AuthSource, Event, FingerprintState, IpcUser, Request};

const GOOD_PASSWORD: &str = "correct";

/// Steps to play out after each `Authenticate`, from `STUB_SCRIPT`.
fn script() -> Vec<String> {
  std::env::var("STUB_SCRIPT")
    .unwrap_or_default()
    .split(',')
    .filter(|step| !step.is_empty())
    .map(str::to_owned)
    .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let path = std::env::args()
    .nth(1)
    .ok_or("usage: stub-greetd <socket>")?;
  let _ = std::fs::remove_file(&path);

  futures::executor::block_on(async {
    let listener = UnixListener::bind(&path)?;
    eprintln!("stub-greetd listening on {path}");

    loop {
      let (stream, _) = listener.accept().await?;
      eprintln!("greeter connected");

      if let Err(error) = serve(stream).await {
        eprintln!("connection ended: {error}");
      }
    }
  })
}

async fn serve(stream: async_net::unix::UnixStream) -> Result<(), Box<dyn std::error::Error>> {
  let (mut reader, mut writer) = stream.split();

  loop {
    let request: Request = read_frame(&mut reader).await?;
    eprintln!("-> {request:?}");

    match request {
      Request::Hello { .. } => {
        send(&mut writer, welcome()).await?;
        // Nothing is armed yet, but the greeter should show the indicator as
        // soon as it knows a reader exists.
        send(
          &mut writer,
          Event::Fingerprint {
            state: FingerprintState::Waiting,
          },
        )
        .await?;
      }
      Request::Authenticate { username } => {
        eprintln!("   authenticating {username}");
        send(
          &mut writer,
          Event::Prompt {
            source: AuthSource::Password,
            echo: false,
          },
        )
        .await?;

        // Everything a keyboard would normally trigger, driven from here
        // instead: the test environment is headless and the only synthetic
        // input tool available injects into the real session.
        for step in script() {
          std::thread::sleep(std::time::Duration::from_millis(700));

          // A step can be more than one frame, because some of the daemon's
          // transitions are: it never reports a fingerprint path failing without
          // also turning the indicator off, and a stub that sent one without the
          // other would be testing a state the daemon cannot produce.
          let events = match step.split_once(':') {
            Some(("info", message)) => vec![Event::Info {
              source: AuthSource::Fingerprint,
              message: message.to_owned(),
            }],
            Some(("error", message)) => vec![Event::Error {
              source: AuthSource::Password,
              message: message.to_owned(),
            }],
            Some(("sessionfail", message)) => vec![Event::SessionFailed {
              message: message.to_owned(),
            }],
            Some(("requestfail", message)) => vec![Event::RequestFailed {
              message: message.to_owned(),
            }],
            // A fingerprint that failed to match, while the password worker
            // carries on: the field must stay usable and keep its prompt.
            Some(("fpfail", message)) => vec![
              Event::Fingerprint {
                state: FingerprintState::Off,
              },
              Event::Failed {
                source: AuthSource::Fingerprint,
                failure: AuthFailure::Error {
                  message: message.to_owned(),
                },
              },
            ],
            _ => match step.as_str() {
              // The greeter should respond by asking for a whole new attempt,
              // exactly as it does when a typed password is turned down.
              "reject" => vec![Event::Failed {
                source: AuthSource::Password,
                failure: AuthFailure::Rejected,
              }],
              // The reader giving up after too many failed matches.
              "fpexhausted" => vec![
                Event::Error {
                  source: AuthSource::Fingerprint,
                  message: "Too many attempts, use your password".to_owned(),
                },
                Event::Fingerprint {
                  state: FingerprintState::Off,
                },
              ],
              // As though a finger landed on the reader: the greeter should go
              // straight to starting a session without anything being typed.
              "win" => vec![Event::Authenticated {
                via: AuthSource::Fingerprint,
              }],
              "reading" => vec![Event::Fingerprint {
                state: FingerprintState::Reading,
              }],
              other => {
                eprintln!("   unknown script step {other:?}");
                continue;
              }
            },
          };

          for event in events {
            send(&mut writer, event).await?;
          }
        }
      }
      Request::Password { value } => match value.expose() == GOOD_PASSWORD {
        true => {
          send(
            &mut writer,
            Event::Authenticated {
              via: AuthSource::Password,
            },
          )
          .await?
        }
        // No prompt follows. The daemon abandons the whole attempt, and the
        // greeter comes back with a fresh `Authenticate` - which is what produces
        // the next prompt, above.
        false => {
          send(
            &mut writer,
            Event::Failed {
              source: AuthSource::Password,
              failure: AuthFailure::Rejected,
            },
          )
          .await?
        }
      },
      Request::Cancel => {}
      Request::StartSession => send(&mut writer, Event::SessionStarted).await?,
    }
  }
}

fn welcome() -> Event {
  Event::Welcome {
    version: greet_ipc::PROTOCOL_VERSION,
    users: vec![
      IpcUser {
        name: "ada".to_owned(),
        display_name: "Ada Lovelace".to_owned(),
        avatar: None,
      },
      IpcUser {
        name: "grace".to_owned(),
        display_name: "Grace Hopper".to_owned(),
        avatar: None,
      },
    ],
    default_user: "ada".to_owned(),
    fingerprint: true,
    primary_output: None,
  }
}

async fn send(
  writer: &mut WriteHalf<async_net::unix::UnixStream>,
  event: Event,
) -> Result<(), Box<dyn std::error::Error>> {
  eprintln!("<- {event:?}");
  write_frame(writer, &event).await?;
  Ok(())
}
