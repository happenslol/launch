use std::{
  collections::HashMap,
  fs,
  path::PathBuf,
  sync::{Arc, LazyLock, Mutex},
};

use gpui::{App, Global, Resource};
use rusqlite::OpenFlags;
use tracing::error;

#[derive(Debug, Clone)]
pub struct Db(Arc<Mutex<rusqlite::Connection>>);

// At some point we might want to switch to the model zed uses, which is basically on-demand
// read-only connections and a background thread that holds a single write connection and works
// through a queue of closures.
pub static DB: LazyLock<Db> = LazyLock::new(Db::new);

/// Path of the main application database. Used both by the shared writer
/// connection and by on-demand read-only readers (see [`NotificationDbReader`]).
pub fn launch_db_path() -> PathBuf {
  let local = dirs::data_local_dir()
    .expect("No local data dir")
    .join("launch");

  fs::create_dir_all(&local).expect("Failed to create ensure local data dir");
  local.join("launch.db")
}

impl Db {
  fn new() -> Self {
    let conn = rusqlite::Connection::open_with_flags(
      launch_db_path(),
      OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .expect("Failed to open database");

    conn
      .execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS launches (
          id TEXT PRIMARY KEY,
          count INTEGER NOT NULL DEFAULT 1,
          last_launch INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;

        CREATE TABLE IF NOT EXISTS app_icon_path_cache (
          appid TEXT PRIMARY KEY,
          icon_path TEXT
        ) STRICT;

        CREATE TABLE IF NOT EXISTS llm_conversations (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          conversation_id INTEGER NOT NULL,
          role TEXT NOT NULL,
          content TEXT NOT NULL,
          timestamp INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;

        CREATE TABLE IF NOT EXISTS notifications (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          timestamp INTEGER NOT NULL DEFAULT (unixepoch()),
          app_name TEXT NOT NULL DEFAULT '',
          summary TEXT NOT NULL DEFAULT '',
          body TEXT NOT NULL DEFAULT '',
          app_icon TEXT NOT NULL DEFAULT '',
          urgency INTEGER NOT NULL DEFAULT 1
        ) STRICT;
        "#,
      )
      .unwrap();

    Self(Arc::new(Mutex::new(conn)))
  }

  pub fn lock(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
    self.0.lock().unwrap()
  }

  pub fn record_launch(&self, id: &str) {
    let conn = self.0.lock().unwrap();
    let mut query = conn
      .prepare_cached(
        r#"
        INSERT INTO launches (id) VALUES (?1)
        ON CONFLICT (id) DO UPDATE SET count = count+1
        "#,
      )
      .expect("prepare query");

    query.execute([id]).expect("record launch");
  }

  pub fn get_launches(&self) -> HashMap<String, (u32, u64)> {
    let conn = self.0.lock().unwrap();
    let mut query = conn
      .prepare_cached("SELECT id, count, last_launch FROM launches")
      .expect("Failed to prepare query");

    let mut result = HashMap::new();
    let rows = query
      .query_map([], |row| Ok((row.get(0)?, (row.get(1)?, row.get(2)?))))
      .expect("Failed to execute query");

    for row in rows {
      let (id, count) = row.expect("get row");
      result.insert(id, count);
    }

    result
  }

  pub fn store_desktop_entry_icon_paths(&self, paths: &HashMap<String, PathBuf>) {
    let conn = self.0.lock().unwrap();
    let mut query = conn
      .prepare_cached(
        r#"
        INSERT INTO app_icon_path_cache (appid, icon_path) VALUES (?1, ?2)
        ON CONFLICT (appid) DO UPDATE SET icon_path = ?2
        "#,
      )
      .expect("prepare query");

    for (appid, icon_path) in paths {
      query
        .execute([appid, &icon_path.to_string_lossy().to_string()])
        .expect("store desktop entry icon paths");
    }
  }

  pub fn get_desktop_entry_icon_paths(&self) -> HashMap<String, Resource> {
    let conn = self.0.lock().unwrap();
    let mut query = conn
      .prepare_cached(r#"SELECT appid, icon_path FROM app_icon_path_cache"#)
      .expect("prepare query");

    let mut result = HashMap::new();
    let rows = query
      .query_map([], |row| {
        Ok((
          row.get(0)?,
          Resource::Path(PathBuf::from(row.get::<_, String>(1)?).into()),
        ))
      })
      .expect("execute query");

    for row in rows {
      let (appid, icon_path) = row.expect("get row");
      result.insert(appid, icon_path);
    }

    result
  }

  /// Appends a received notification to the persistent history. Failures are
  /// logged rather than propagated so a malformed notification can never take
  /// down the daemon.
  pub fn record_notification(
    &self,
    app_name: &str,
    summary: &str,
    body: &str,
    app_icon: &str,
    urgency: u8,
  ) {
    let conn = self.0.lock().unwrap();
    let result = conn
      .prepare_cached(
        r#"
        INSERT INTO notifications (app_name, summary, body, app_icon, urgency)
        VALUES (?1, ?2, ?3, ?4, ?5)
        "#,
      )
      .and_then(|mut query| {
        query.execute(rusqlite::params![
          app_name, summary, body, app_icon, urgency
        ])
      });

    if let Err(err) = result {
      error!(?err, "Failed to record notification");
    }
  }
}

/// A single persisted notification, as read back from the history.
#[allow(dead_code)]
pub struct NotificationRecord {
  pub id: i64,
  pub timestamp: i64,
  pub app_name: String,
  pub summary: String,
  pub body: String,
  pub app_icon: String,
  pub urgency: u8,
}

/// On-demand read-only access to the notification history. Cloneable and cheap
/// (it only holds the database path), so it can be moved into background tasks
/// that open their own short-lived connection, mirroring `ClipboardDbReader`.
#[derive(Debug, Clone)]
pub struct NotificationDbReader(PathBuf);

struct GlobalNotificationDbReader(NotificationDbReader);

impl Global for GlobalNotificationDbReader {}

impl NotificationDbReader {
  pub fn install(cx: &mut App) {
    cx.set_global(GlobalNotificationDbReader(Self(launch_db_path())));
  }

  /// A reader bound to the default database path, for use outside the app (e.g.
  /// the CLI) where no [`App`] context is available.
  pub fn at_default_path() -> Self {
    Self(launch_db_path())
  }

  #[allow(dead_code)]
  pub fn global(cx: &App) -> Option<NotificationDbReader> {
    cx.try_global::<GlobalNotificationDbReader>()
      .map(|global| global.0.clone())
  }

  fn open(&self) -> anyhow::Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
      &self.0,
      OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
  }

  #[allow(dead_code)]
  pub fn recent(&self, limit: u32) -> anyhow::Result<Vec<NotificationRecord>> {
    let conn = self.open()?;
    let mut statement = conn.prepare_cached(
      "SELECT id, timestamp, app_name, summary, body, app_icon, urgency \
       FROM notifications ORDER BY id DESC LIMIT ?1",
    )?;

    let rows = statement.query_map([limit], |row| {
      Ok(NotificationRecord {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        app_name: row.get(2)?,
        summary: row.get(3)?,
        body: row.get(4)?,
        app_icon: row.get(5)?,
        urgency: row.get(6)?,
      })
    })?;

    let mut entries = Vec::new();
    for row in rows {
      entries.push(row?);
    }

    Ok(entries)
  }
}
