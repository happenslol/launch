mod api;

use anyhow::Result;
use futures::{Stream, StreamExt as _, stream};

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
}

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
