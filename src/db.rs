use std::{collections::HashMap, fs, sync::Arc};

use async_lock::Mutex;
use gpui::{App, Global};
use rusqlite::OpenFlags;

pub fn init(cx: &mut App) {
  cx.set_global(Db::new());
}

impl Global for Db {}

#[derive(Debug, Clone)]
pub struct Db(Arc<Mutex<rusqlite::Connection>>);

impl Db {
  pub fn global(cx: &App) -> Self {
    cx.global::<Db>().clone()
  }

  fn new() -> Self {
    let local = dirs::data_local_dir()
      .expect("No local data dir")
      .join("launch");

    fs::create_dir_all(&local).expect("Failed to create ensure local data dir");

    let connection = rusqlite::Connection::open_with_flags(
      local.join("launch.db"),
      OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .expect("Failed to open database");

    connection
      .execute(
        r#"CREATE TABLE IF NOT EXISTS launches (
          id TEXT PRIMARY KEY,
          count INTEGER NOT NULL DEFAULT 1
        );"#,
        [],
      )
      .expect("Failed to create database");

    Self(Arc::new(Mutex::new(connection)))
  }

  pub async fn record_launch(&self, id: &str) {
    let conn = self.0.lock().await;
    let mut query = conn
      .prepare_cached(
        r#"INSERT INTO launches (id) VALUES (?1)
          ON CONFLICT (id) DO UPDATE SET count = count+1;"#,
      )
      .expect("Failed to prepare query");

    query.execute([id]).expect("Failed to execute query");
  }

  pub async fn get_launches(&self) -> HashMap<String, u32> {
    let conn = self.0.lock().await;
    let mut query = conn
      .prepare_cached("SELECT id, count FROM launches")
      .expect("Failed to prepare query");

    let mut result = HashMap::new();
    let rows = query
      .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
      .expect("Failed to execute query");

    for row in rows {
      let (id, count) = row.expect("Failed to get row");
      result.insert(id, count);
    }

    result
  }
}
