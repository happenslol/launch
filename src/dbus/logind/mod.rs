mod api;

use anyhow::{Context as _, Result};
use futures::{Stream, StreamExt as _, stream};

use crate::util::ResultExt;

pub struct Logind;

impl Logind {
  pub async fn reboot(conn: &zbus::Connection) -> Result<()> {
    let proxy = api::ManagerProxy::new(conn).await?;
    proxy.reboot(true).await?;
    Ok(())
  }

  pub async fn power_off(conn: &zbus::Connection) -> Result<()> {
    let proxy = api::ManagerProxy::new(conn).await?;
    proxy.power_off(true).await?;
    Ok(())
  }

  pub async fn suspend(conn: &zbus::Connection) -> Result<()> {
    let proxy = api::ManagerProxy::new(conn).await?;
    proxy.suspend(true).await?;
    Ok(())
  }

  /// Holds the system back from suspending until the returned lock is dropped,
  /// which is how a locker gets the screen covered before the machine goes to
  /// sleep. logind only waits so long - `InhibitDelayMaxSec`, 5 seconds by
  /// default - and then suspends regardless.
  ///
  /// The lock is one use: it is gone once sleep has happened, and taking another
  /// is what arms this for the next time.
  pub async fn inhibit_sleep(conn: &zbus::Connection, why: &str) -> Result<SleepLock> {
    let proxy = api::ManagerProxy::new(conn).await?;

    let lock = proxy
      .inhibit("sleep", "launch", why, "delay")
      .await
      .context("taking a sleep inhibitor lock")?;

    Ok(SleepLock(lock))
  }

  /// Follows the system into and out of sleep: `true` arrives before suspending,
  /// `false` after resuming.
  pub async fn listen_sleep(conn: &zbus::Connection) -> Result<impl Stream<Item = bool> + use<>> {
    let proxy = api::ManagerProxy::new(conn).await?;
    let transitions = proxy.receive_prepare_for_sleep().await?;

    Ok(
      transitions
        .filter_map(|signal| async move { signal.args().log_err().map(|args| args.start) }),
    )
  }
}

/// A held sleep inhibitor. Sleep is delayed for as long as this is alive.
pub struct SleepLock(#[allow(dead_code)] zvariant::OwnedFd);

/// A lock or unlock request logind forwarded to our session.
#[derive(Clone, Copy, Debug)]
pub enum SessionRequest {
  Lock,
  Unlock,
}

/// The login session this process runs in.
pub struct Session(api::SessionProxy<'static>);

impl Session {
  pub async fn current(conn: &zbus::Connection) -> Result<Self> {
    Ok(Self(api::SessionProxy::new(conn).await?))
  }

  /// Publishes the lock state so anything watching the session (`loginctl`, idle
  /// daemons) agrees with what is on screen.
  pub async fn set_locked_hint(&self, locked: bool) -> Result<()> {
    self.0.set_locked_hint(locked).await?;
    Ok(())
  }

  /// Lock and unlock requests aimed at this session, merged in arrival order.
  pub async fn listen_requests(&self) -> Result<impl Stream<Item = SessionRequest> + use<>> {
    let lock = self.0.receive_lock().await?.map(|_| SessionRequest::Lock);

    let unlock = self
      .0
      .receive_unlock()
      .await?
      .map(|_| SessionRequest::Unlock);

    Ok(stream::select(lock, unlock))
  }
}
