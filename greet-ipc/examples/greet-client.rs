//! A scriptable greeter, for driving `launch-greetd` without a compositor.
//!
//! The real login screen needs a Wayland session to draw into, which a headless
//! VM has no use for. This speaks the same protocol from a shell script instead,
//! so the daemon half - the forking, the PAM stacks, the VT handling - can be
//! tested on its own.
//!
//! Steps are given as arguments and run in order:
//!
//! ```text
//!   greet-client hello auth:ada password:secret start
//! ```
//!
//! Every frame received is printed to stdout as `event <json>`, and every step
//! waits for the event that answers it. Exits non-zero on a timeout or an
//! unexpected outcome, so a test script can simply check the status.

use std::time::Duration;

use async_net::unix::UnixStream;
use futures::{AsyncReadExt as _, FutureExt as _, select};
use greet_ipc::codec::futures_io::{read_frame, write_frame};
use greet_ipc::{AuthSource, Event, Request, Secret};

/// Long enough for a PAM stack on a slow VM, short enough that a wedged daemon
/// fails the test rather than hanging it.
const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `settle` waits for the next frame before deciding the daemon has
/// finished talking.
const QUIET_PERIOD: Duration = Duration::from_secs(2);

/// What a step is waiting for.
#[derive(Debug, PartialEq)]
enum Await {
  Welcome,
  Prompt,
  /// Either outcome is a valid answer; which one it was goes to stdout.
  Verdict,
  SessionStarted,
  /// Collect frames until the daemon goes quiet.
  ///
  /// Needed because the interesting events do not all answer a request: the
  /// second worker fails on its own schedule, and a step that stopped at the
  /// password prompt would exit before that ever arrived.
  Quiet,
  Nothing,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let steps: Vec<String> = std::env::args().skip(1).collect();
  if steps.is_empty() {
    return Err("usage: greet-client <step>...".into());
  }

  let path = std::env::var_os(greet_ipc::SOCKET_ENV_VAR)
    .map(std::path::PathBuf::from)
    .or_else(find_socket)
    .ok_or("no greeter socket; is launch-greetd running?")?;

  futures::executor::block_on(async move {
    let stream = UnixStream::connect(&path).await?;
    let (mut reader, mut writer) = stream.split();

    for step in steps {
      let (request, expect) = parse(&step)?;
      println!("step {step}");

      if let Some(request) = request {
        write_frame(&mut writer, &request).await?;
      }

      if expect == Await::Nothing {
        continue;
      }

      // Frames that are not the answer are still printed: an `Info` on the way to
      // a `Prompt` is exactly the sort of thing a test wants to assert on.
      loop {
        let waited = match expect {
          Await::Quiet => QUIET_PERIOD,
          _ => STEP_TIMEOUT,
        };

        let event = match timeout(read_frame::<Event, _>(&mut reader), waited).await {
          Some(event) => event?,
          None if expect == Await::Quiet => break,
          None => return Err(format!("timed out waiting for {expect:?}").into()),
        };

        println!("event {}", serde_json::to_string(&event)?);

        if settles(&event, &expect) {
          break;
        }
      }
    }

    Ok(())
  })
}

/// Whether this frame is the one the step was waiting for.
fn settles(event: &Event, expect: &Await) -> bool {
  match (event, expect) {
    (Event::Welcome { .. }, Await::Welcome) => true,
    (
      Event::Prompt {
        source: AuthSource::Password,
        ..
      },
      Await::Prompt,
    ) => true,
    (Event::Authenticated { .. }, Await::Verdict) => true,
    // Only the password path settles a verdict: the fingerprint worker going
    // quiet is expected on a machine with no reader, and would otherwise end the
    // wait before the password has been judged.
    (
      Event::Failed {
        source: AuthSource::Password,
        ..
      },
      Await::Verdict,
    ) => true,
    (Event::SessionStarted, Await::SessionStarted) => true,
    // Terminal regardless of what was expected; waiting further would only turn a
    // clear failure into a timeout.
    (Event::SessionFailed { .. } | Event::RequestFailed { .. }, _) => true,
    _ => false,
  }
}

fn parse(step: &str) -> Result<(Option<Request>, Await), Box<dyn std::error::Error>> {
  let (name, argument) = match step.split_once(':') {
    Some((name, argument)) => (name, Some(argument)),
    None => (step, None),
  };

  let missing = || -> Box<dyn std::error::Error> { format!("{name} needs an argument").into() };

  Ok(match name {
    "hello" => (
      Some(Request::Hello {
        version: greet_ipc::PROTOCOL_VERSION,
      }),
      Await::Welcome,
    ),
    "auth" => (
      Some(Request::Authenticate {
        username: argument.ok_or_else(missing)?.to_owned(),
      }),
      Await::Prompt,
    ),
    "password" => (
      Some(Request::Password {
        value: Secret::new(argument.ok_or_else(missing)?.to_owned())?,
      }),
      Await::Verdict,
    ),
    "start" => (Some(Request::StartSession), Await::SessionStarted),
    "settle" => (None, Await::Quiet),
    "cancel" => (Some(Request::Cancel), Await::Nothing),
    // Waits without sending, for asserting on something the daemon pushes of its
    // own accord.
    "expect-prompt" => (None, Await::Prompt),
    other => return Err(format!("unknown step {other:?}").into()),
  })
}

async fn timeout<T>(future: impl std::future::Future<Output = T>, after: Duration) -> Option<T> {
  select! {
    value = future.fuse() => Some(value),
    _ = async_io::Timer::after(after).fuse() => None,
  }
}

/// The socket carries the daemon's pid, so a test that did not inherit the
/// environment can still find it.
fn find_socket() -> Option<std::path::PathBuf> {
  let entries = std::fs::read_dir("/run").ok()?;

  entries
    .filter_map(Result::ok)
    .map(|entry| entry.path())
    .find(|path| {
      path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("launch-greetd-") && name.ends_with(".sock"))
    })
}
