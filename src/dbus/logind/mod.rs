mod api;

use anyhow::Result;

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
