use std::{
  collections::HashMap,
  fs,
  path::PathBuf,
  sync::{Arc, LazyLock, Mutex},
};

use gpui::Resource;
use rusqlite::OpenFlags;

#[derive(Debug, Clone)]
pub struct Db(Arc<Mutex<rusqlite::Connection>>);

// At some point we might want to switch to the model zed uses, which is basically on-demand
// read-only connections and a background thread that holds a single write connection and works
// through a queue of closures.
pub static DB: LazyLock<Db> = LazyLock::new(Db::new);

impl Db {
  fn new() -> Self {
    let local = dirs::data_local_dir()
      .expect("No local data dir")
      .join("launch");

    fs::create_dir_all(&local).expect("Failed to create ensure local data dir");

    let conn = rusqlite::Connection::open_with_flags(
      local.join("launch.db"),
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
}
