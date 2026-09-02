pub mod application;
pub mod bluez;
pub mod fprintd;
pub mod logind;
pub mod networkmanager;
pub mod notifications;
pub mod polkit;
pub mod status_notifier;
pub mod systemd;
pub mod upower;

use futures::FutureExt;
use futures::future::Shared;

use gpui::{App, AppContext, AsyncApp, Global, Task};
use tracing::error;

#[derive(Default)]
pub struct GlobalDbusConnection {
  system: Option<Shared<Task<Option<zbus::Connection>>>>,
  session: Option<Shared<Task<Option<zbus::Connection>>>>,
  _ticks: Vec<Task<()>>,
}

impl Global for GlobalDbusConnection {}

pub fn init(cx: &mut App) {
  cx.set_global(GlobalDbusConnection::default());
}

impl GlobalDbusConnection {
  pub fn system(cx: &mut App) -> Shared<Task<Option<zbus::Connection>>> {
    let this = cx.global::<Self>();
    if let Some(shared) = this.system.as_ref() {
      return shared.clone();
    }

    let task = cx
      .spawn(async move |cx| open(cx, zbus::connection::Builder::system()).await)
      .shared();

    cx.global_mut::<Self>().system = Some(task.clone());
    task
  }

  pub fn session(cx: &mut App) -> Shared<Task<Option<zbus::Connection>>> {
    let this = cx.global::<Self>();
    if let Some(shared) = this.session.as_ref() {
      return shared.clone();
    }

    let task = cx
      .spawn(async move |cx| open(cx, zbus::connection::Builder::session()).await)
      .shared();

    cx.global_mut::<Self>().session = Some(task.clone());
    task
  }
}

async fn open(
  cx: &mut AsyncApp,
  builder: Result<zbus::connection::Builder<'_>, zbus::Error>,
) -> Option<zbus::Connection> {
  let builder = match builder {
    Ok(builder) => builder,
    Err(error) => {
      error!(?error, "Failed to create dbus connection builder");
      return None;
    }
  };

  let conn = match builder.internal_executor(false).build().await {
    Ok(conn) => conn,
    Err(error) => {
      error!(?error, "Failed to open dbus connection");
      return None;
    }
  };

  let tick = cx.background_spawn({
    let conn = conn.clone();
    async move {
      loop {
        conn.executor().tick().await;
      }
    }
  });

  cx.update_global::<GlobalDbusConnection, _>(|this, _cx| {
    this._ticks.push(tick);
  });

  Some(conn)
}
