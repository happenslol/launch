pub mod bluez;
pub mod logind;
pub mod networkmanager;

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Error, Result};
use gpui::{App, AsyncApp, Global, Task, prelude::*};

static SYSTEM_STARTING: AtomicBool = AtomicBool::new(false);
static SESSION_STARTING: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
pub struct GlobalDbusConnection {
  system: Option<(Task<()>, zbus::Connection)>,
  session: Option<(Task<()>, zbus::Connection)>,
}

impl Global for GlobalDbusConnection {}

pub fn init(cx: &mut App) {
  cx.set_global(GlobalDbusConnection::default());
}

impl GlobalDbusConnection {
  // TODO: Wait for connection to be ready here?
  pub fn system(cx: &mut App) -> Option<zbus::Connection> {
    let this = cx.global::<Self>();
    if let Some((_, conn)) = this.system.as_ref() {
      return Some(conn.clone());
    }

    if !SYSTEM_STARTING.load(Ordering::Acquire) {
      SYSTEM_STARTING.store(true, Ordering::Release);

      cx.spawn(async move |cx| {
        let conn = open(cx, zbus::connection::Builder::system().unwrap()).await?;
        cx.update_global::<Self, _>(move |this, _cx| this.system = Some(conn));
        Ok::<(), Error>(())
      })
      .detach_and_log_err(cx);
    }

    None
  }

  #[allow(unused)]
  pub fn session(cx: &mut App) -> Option<zbus::Connection> {
    let this = cx.global::<Self>();
    if let Some((_, conn)) = this.session.as_ref() {
      return Some(conn.clone());
    }

    if !SESSION_STARTING.load(Ordering::Acquire) {
      SESSION_STARTING.store(true, Ordering::Release);

      cx.spawn(async move |cx| {
        let conn = open(cx, zbus::connection::Builder::session().unwrap()).await?;
        cx.update_global::<Self, _>(move |this, _cx| this.session = Some(conn));
        Ok::<(), Error>(())
      })
      .detach_and_log_err(cx);
    }

    None
  }
}

async fn open<'a>(
  cx: &mut AsyncApp,
  builder: zbus::connection::Builder<'a>,
) -> Result<(Task<()>, zbus::Connection)> {
  let conn = builder.internal_executor(false).build().await?;

  let tick = cx.background_spawn({
    let conn = conn.clone();
    async move {
      loop {
        conn.executor().tick().await;
      }
    }
  });

  Ok((tick, conn))
}
