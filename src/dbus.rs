use anyhow::Result;
use gpui::{App, AppContext, AsyncApp, Entity, Global};

pub fn init(cx: &mut App) {
  let conn = cx.new(|_| DbusConnection::new());
  cx.set_global(GlobalDbusConnection(conn.clone()));
  DbusConnection::init(conn, cx);
}

struct GlobalDbusConnection(Entity<DbusConnection>);

impl Global for GlobalDbusConnection {}

#[derive(Default)]
pub struct DbusConnection {
  system: Option<zbus::Connection>,
  session: Option<zbus::Connection>,
}

impl DbusConnection {
  fn new() -> Self {
    Self::default()
  }

  fn init(this: Entity<Self>, cx: &mut App) {
    cx.spawn(async move |cx| {
      let system = make_dbus_connection(zbus::Address::system()?, cx)
        .await
        .ok();
      let session = make_dbus_connection(zbus::Address::session()?, cx)
        .await
        .ok();

      this.update(cx, |this, _cx| {
        this.system = system;
        this.session = session;
      })
    })
    .detach_and_log_err(cx);
  }
}

pub trait DbusConnectionAppExt {
  fn system_dbus(&self) -> Option<zbus::Connection>;
  fn session_dbus(&self) -> Option<zbus::Connection>;
}

impl DbusConnectionAppExt for App {
  fn system_dbus(&self) -> Option<zbus::Connection> {
    self
      .try_global::<GlobalDbusConnection>()
      .and_then(|conn| conn.0.read(self).system.clone())
  }

  fn session_dbus(&self) -> Option<zbus::Connection> {
    self
      .try_global::<GlobalDbusConnection>()
      .and_then(|conn| conn.0.read(self).session.clone())
  }
}

async fn make_dbus_connection(addr: zbus::Address, cx: &mut AsyncApp) -> Result<zbus::Connection> {
  let conn = zbus::connection::Builder::address(addr)?
    .internal_executor(false)
    .build()
    .await?;

  cx.background_spawn({
    let conn = conn.clone();
    async move {
      loop {
        conn.executor().tick().await;
      }
    }
  })
  .detach();

  Ok(conn)
}
