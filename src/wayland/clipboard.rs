use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::Rc;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use rusqlite::OpenFlags;
use rustix::fs::{OFlags, fcntl_setfl};
use rustix::pipe::PipeFlags;
use tracing::{debug, error, info, warn};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{
  Connection, Dispatch, Proxy, QueueHandle, delegate_noop, event_created_child,
};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1, zwlr_data_control_manager_v1, zwlr_data_control_offer_v1,
  zwlr_data_control_source_v1,
};
use xxhash_rust::xxh3::xxh3_128;

use super::{State, WaylandEvent};

const TEXT_MIME_TYPES: &[&str] = &[
  "text/plain;charset=utf-8",
  "text/plain",
  "UTF8_STRING",
  "STRING",
  "TEXT",
];

const X11_METADATA_MIMES: &[&str] = &[
  "TARGETS",
  "TIMESTAMP",
  "MULTIPLE",
  "SAVE_TARGETS",
  "x-special/gnome-copied-files-icon",
];

const SECRET_HINT_MIME: &str = "x-kde-passwordManagerHint";
const REOFFER_HINT_MIME: &str = "x-launch";

const MAX_ENTRY_SIZE: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentType {
  Text,
  Url,
  Code,
  File,
  Image,
  Other,
}

impl ContentType {
  pub fn as_str(self) -> &'static str {
    match self {
      ContentType::Text => "text",
      ContentType::Url => "url",
      ContentType::Code => "code",
      ContentType::File => "file",
      ContentType::Image => "image",
      ContentType::Other => "other",
    }
  }

  pub fn from_str(s: &str) -> Self {
    match s {
      "text" => ContentType::Text,
      "url" => ContentType::Url,
      "code" => ContentType::Code,
      "file" => ContentType::File,
      "image" => ContentType::Image,
      _ => ContentType::Other,
    }
  }
}

pub enum SelectionState {
  Free,
  Ours(zwlr_data_control_source_v1::ZwlrDataControlSourceV1),
  Client {
    data_offer_id: wayland_client::backend::ObjectId,
  },
}

fn clipboard_db_path() -> anyhow::Result<PathBuf> {
  let local = dirs::data_local_dir()
    .ok_or_else(|| anyhow::anyhow!("No local data dir"))?
    .join("launch");
  fs::create_dir_all(&local)?;
  Ok(local.join("clipboard.db"))
}

struct ClipboardDbWriter(rusqlite::Connection);

impl ClipboardDbWriter {
  fn new() -> anyhow::Result<(Self, ClipboardDbReader)> {
    let path = clipboard_db_path()?;

    let conn = rusqlite::Connection::open_with_flags(
      &path,
      OpenFlags::SQLITE_OPEN_CREATE | OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;

    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    // Check if we need to migrate from old schema
    let needs_migration: bool = conn
      .prepare(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='clipboard_mime_data'",
      )?
      .query_row([], |row| row.get::<_, i64>(0))
      .map(|count| count == 0)?;

    if needs_migration {
      // Drop old table if it exists (it had a different schema)
      conn.execute_batch("DROP TABLE IF EXISTS clipboard_history;")?;
    }

    conn.execute_batch(
      r#"
      CREATE TABLE IF NOT EXISTS clipboard_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp INTEGER NOT NULL DEFAULT (unixepoch()),
        mime_types TEXT NOT NULL,
        content_type TEXT NOT NULL DEFAULT 'text',
        preview TEXT NOT NULL DEFAULT '',
        content_hash BLOB
      ) STRICT;

      CREATE INDEX IF NOT EXISTS idx_clipboard_content_hash
        ON clipboard_history(content_hash);

      CREATE TABLE IF NOT EXISTS clipboard_mime_data (
        history_id INTEGER NOT NULL REFERENCES clipboard_history(id) ON DELETE CASCADE,
        mime_type TEXT NOT NULL,
        data BLOB NOT NULL,
        PRIMARY KEY (history_id, mime_type)
      ) STRICT;
      "#,
    )?;

    let reader = ClipboardDbReader::new(path);

    Ok((Self(conn), reader))
  }

  fn delete(&self, id: i64) {
    if let Err(err) = self.0.execute(
      "DELETE FROM clipboard_history WHERE id = ?1",
      rusqlite::params![id],
    ) {
      error!(?err, id, "Failed to delete clipboard history entry");
    }
  }

  fn insert(
    &self,
    mime_types: &[String],
    mime_data: &HashMap<String, Vec<u8>>,
    content_type: ContentType,
    preview: &str,
  ) {
    let mime_types_json = mime_types_to_json(mime_types);
    let content_hash = compute_content_hash(mime_data);

    let tx = match self.0.unchecked_transaction() {
      Ok(tx) => tx,
      Err(err) => {
        error!(?err, "Failed to begin clipboard insert transaction");
        return;
      }
    };

    // Remove any existing entry with the same content hash
    if let Err(err) = tx.execute(
      "DELETE FROM clipboard_history WHERE content_hash = ?1",
      rusqlite::params![content_hash],
    ) {
      error!(?err, "Failed to delete duplicate clipboard entry");
      return;
    }

    let history_id: i64 = match tx.query_row(
      "INSERT INTO clipboard_history (mime_types, content_type, preview, content_hash) VALUES (?1, ?2, ?3, ?4) RETURNING id",
      rusqlite::params![mime_types_json, content_type.as_str(), preview, content_hash],
      |row| row.get(0),
    ) {
      Ok(id) => id,
      Err(err) => {
        error!(?err, "Failed to insert clipboard history entry");
        return;
      }
    };

    let mut statement = match tx.prepare_cached(
      "INSERT INTO clipboard_mime_data (history_id, mime_type, data) VALUES (?1, ?2, ?3)",
    ) {
      Ok(s) => s,
      Err(err) => {
        error!(?err, "Failed to prepare mime data insert");
        return;
      }
    };

    for (mime_type, data) in mime_data {
      if let Err(err) = statement.execute(rusqlite::params![history_id, mime_type, data]) {
        error!(?err, mime_type, "Failed to insert mime data");
        return;
      }
    }

    drop(statement);
    if let Err(err) = tx.commit() {
      error!(?err, "Failed to commit clipboard insert transaction");
    }
  }
}

pub struct ClipboardEntry {
  pub id: i64,
  pub timestamp: i64,
  pub _mime_types: Vec<String>,
  pub content_type: ContentType,
  pub preview: String,
}

#[derive(Debug, Clone)]
pub struct ClipboardDbReader(PathBuf);

impl ClipboardDbReader {
  fn new(path: PathBuf) -> Self {
    Self(path)
  }

  fn open(&self) -> anyhow::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_with_flags(&self.0, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn)
  }

  pub fn get_mime_data_by_id(&self, id: i64) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let conn = self.open()?;
    let mut statement = conn
      .prepare_cached("SELECT mime_type, data FROM clipboard_mime_data WHERE history_id = ?1")?;

    let rows = statement.query_map([id], |row| {
      Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;

    let mut data = HashMap::new();
    for row in rows {
      let (mime_type, blob) = row?;
      data.insert(mime_type, blob);
    }

    Ok(data)
  }

  pub fn recent(&self, limit: u32) -> anyhow::Result<Vec<ClipboardEntry>> {
    let conn = self.open()?;
    let mut statement = conn.prepare_cached(
      "SELECT id, timestamp, mime_types, content_type, preview FROM clipboard_history ORDER BY id DESC LIMIT ?1",
    )?;

    let rows = statement.query_map([limit], |row| {
      Ok((
        row.get::<_, i64>(0)?,
        row.get::<_, i64>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, String>(4)?,
      ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
      let (id, timestamp, mime_types_json, content_type_str, preview) = row?;
      entries.push(ClipboardEntry {
        id,
        timestamp,
        _mime_types: parse_mime_types_json(&mime_types_json),
        content_type: ContentType::from_str(&content_type_str),
        preview,
      });
    }

    Ok(entries)
  }
}

fn mime_types_to_json(mime_types: &[String]) -> String {
  serde_json::to_string(mime_types).expect("serialize mime types")
}

fn parse_mime_types_json(json: &str) -> Vec<String> {
  serde_json::from_str(json).unwrap_or_default()
}

fn compute_content_hash(mime_data: &HashMap<String, Vec<u8>>) -> Vec<u8> {
  let mut sorted_keys: Vec<&String> = mime_data.keys().collect();
  sorted_keys.sort();

  let mut combined = Vec::new();
  for key in sorted_keys {
    if let Some(data) = mime_data.get(key) {
      combined.extend_from_slice(key.as_bytes());
      combined.extend_from_slice(&(data.len() as u64).to_le_bytes());
      combined.extend_from_slice(data);
    }
  }

  xxh3_128(&combined).to_le_bytes().to_vec()
}

pub struct ClipboardState {
  pub pending_offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
  pub pending_mime_types: Vec<String>,
  pub clipboard_data: HashMap<String, Vec<u8>>,
  pub selection_state: SelectionState,
  db_writer: Option<ClipboardDbWriter>,
  pub db_reader: Option<ClipboardDbReader>,
}

impl ClipboardState {
  pub fn new() -> Self {
    let (db_writer, db_reader) = match ClipboardDbWriter::new() {
      Ok((writer, reader)) => (Some(writer), Some(reader)),
      Err(err) => {
        error!(?err, "Failed to open clipboard history database");
        (None, None)
      }
    };

    Self {
      pending_offer: None,
      pending_mime_types: Vec::new(),
      clipboard_data: HashMap::new(),
      selection_state: SelectionState::Free,
      db_writer,
      db_reader,
    }
  }
}

fn is_text_mime(mime: &str) -> bool {
  TEXT_MIME_TYPES.contains(&mime)
}

fn is_x11_metadata(mime: &str) -> bool {
  X11_METADATA_MIMES.contains(&mime)
}

/// Select which mime types to actually read from the offer.
/// Groups text aliases so we only read one representative, picks the best image format,
/// and reads all other content mimes directly.
fn select_target_mimes(offered: &[String]) -> Vec<String> {
  let mut targets = Vec::new();

  // Pick one text representative
  let have_text = TEXT_MIME_TYPES
    .iter()
    .find(|preferred| offered.iter().any(|m| m.as_str() == **preferred));
  if let Some(text_mime) = have_text {
    targets.push((*text_mime).to_string());
  }

  // Pick best image format
  let image_preference = [
    "image/png",
    "image/jpeg",
    "image/bmp",
    "image/gif",
    "image/webp",
  ];
  let have_image = image_preference
    .iter()
    .find(|preferred| offered.iter().any(|m| m.as_str() == **preferred));
  if let Some(image_mime) = have_image {
    targets.push((*image_mime).to_string());
  }

  // Add remaining non-text, non-metadata, non-already-picked mimes
  for mime in offered {
    if is_text_mime(mime) || is_x11_metadata(mime) {
      continue;
    }
    if mime.starts_with("image/") && have_image.is_some() {
      continue;
    }
    if !targets.contains(mime) {
      targets.push(mime.clone());
    }
  }

  targets
}

/// Expand text aliases: if we read one text mime, copy its data to all offered text aliases.
fn expand_text_aliases(mime_data: &mut HashMap<String, Vec<u8>>, offered_mime_types: &[String]) {
  let text_data = TEXT_MIME_TYPES
    .iter()
    .find_map(|t| mime_data.get(*t).cloned());

  if let Some(data) = text_data {
    for offered in offered_mime_types {
      if is_text_mime(offered) && !mime_data.contains_key(offered.as_str()) {
        mime_data.insert(offered.clone(), data.clone());
      }
    }
  }
}

fn detect_content_type(
  offered_mime_types: &[String],
  mime_data: &HashMap<String, Vec<u8>>,
) -> ContentType {
  if offered_mime_types.iter().any(|m| m.starts_with("image/")) {
    return ContentType::Image;
  }

  let has_file_mime = offered_mime_types.iter().any(|m| {
    m == "text/uri-list"
      || m == "x-special/gnome-copied-files"
      || m.contains("nautilus")
      || m.contains("dolphin")
  });
  if has_file_mime {
    return ContentType::File;
  }

  // Check text content
  let text_data = TEXT_MIME_TYPES.iter().find_map(|t| mime_data.get(*t));

  if let Some(data) = text_data
    && let Ok(text) = std::str::from_utf8(data)
  {
    let trimmed = text.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
      return ContentType::Url;
    }
    let code_indicators = [
      "fn ",
      "impl ",
      "class ",
      "function ",
      "def ",
      "import ",
      "pub fn ",
      "struct ",
      "#include",
      "const ",
      "let ",
      "var ",
      "package ",
      "module ",
    ];
    if code_indicators.iter().any(|kw| trimmed.contains(kw)) {
      return ContentType::Code;
    }
    return ContentType::Text;
  }

  ContentType::Other
}

fn compute_preview(content_type: ContentType, mime_data: &HashMap<String, Vec<u8>>) -> String {
  match content_type {
    ContentType::Image => {
      let size = mime_data.values().map(|d| d.len()).max().unwrap_or(0);
      format!("Image ({}) KiB", size / 1024)
    }
    ContentType::File => {
      if let Some(data) = mime_data.get("text/uri-list")
        && let Ok(text) = std::str::from_utf8(data)
        && let Some(first_line) = text.lines().next()
      {
        return first_line.to_string();
      }

      "File".to_string()
    }
    ContentType::Text | ContentType::Url | ContentType::Code => {
      let text_data = TEXT_MIME_TYPES.iter().find_map(|t| mime_data.get(*t));
      if let Some(data) = text_data
        && let Ok(text) = std::str::from_utf8(data)
      {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
          return trimmed
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(200)
            .collect();
        }
      }
      String::new()
    }
    ContentType::Other => {
      let best_mime = mime_data.keys().next().unwrap_or(&String::new()).clone();
      let size = mime_data.values().map(|d| d.len()).max().unwrap_or(0);
      format!("{best_mime} ({} KiB)", size / 1024)
    }
  }
}

struct MimeReadState {
  buffers: HashMap<String, Vec<u8>>,
  completed: HashSet<String>,
  total_count: usize,
  original_offer_id: wayland_client::backend::ObjectId,
  offered_mime_types: Vec<String>,
  total_bytes: usize,
}

impl State {
  pub fn offer_text(&mut self, text: String) {
    let data = text.into_bytes();
    let mut mime_data = HashMap::new();
    for mime in TEXT_MIME_TYPES {
      mime_data.insert((*mime).to_string(), data.clone());
    }

    self.clipboard.clipboard_data = mime_data;

    let Some(qh) = &self.qh else {
      error!("No QueueHandle available");
      return;
    };
    let qh = qh.clone();
    self.offer(&qh);
  }

  pub fn delete_history_entry(&mut self, id: i64) {
    if let Some(writer) = &self.clipboard.db_writer {
      writer.delete(id);
    }
  }

  pub fn copy_history_entry(&mut self, id: i64) {
    let Some(reader) = &self.clipboard.db_reader else {
      error!("No clipboard database reader available");
      return;
    };

    let mime_data = match reader.get_mime_data_by_id(id) {
      Ok(data) if data.is_empty() => {
        error!(id, "No mime data found for clipboard history entry");
        return;
      }
      Ok(data) => data,
      Err(err) => {
        error!(?err, id, "Failed to read clipboard history entry mime data");
        return;
      }
    };

    self.clipboard.clipboard_data = mime_data;

    let Some(qh) = &self.qh else {
      error!("No QueueHandle available");
      return;
    };
    let qh = qh.clone();
    self.offer(&qh);
  }

  fn on_all_mimes_read_complete(
    &mut self,
    mut mime_data: HashMap<String, Vec<u8>>,
    original_offer_id: wayland_client::backend::ObjectId,
    offered_mime_types: Vec<String>,
    qh: &QueueHandle<Self>,
  ) {
    let is_current = matches!(
      &self.clipboard.selection_state,
      SelectionState::Client { data_offer_id } if *data_offer_id == original_offer_id
    );
    if !is_current {
      debug!("Clipboard read completed but selection has changed, discarding");
      return;
    }

    expand_text_aliases(&mut mime_data, &offered_mime_types);

    let content_type = detect_content_type(&offered_mime_types, &mime_data);
    let preview = compute_preview(content_type, &mime_data);

    debug!(
      content_type = ?content_type,
      mime_count = mime_data.len(),
      preview = &preview,
      "Clipboard data captured"
    );

    if let Some(writer) = &self.clipboard.db_writer {
      writer.insert(&offered_mime_types, &mime_data, content_type, &preview);
    }

    self.clipboard.clipboard_data = mime_data;

    if let Some(event_tx) = &self.event_tx
      && let Err(err) = event_tx.send(WaylandEvent::ClipboardText)
    {
      error!(?err, "Failed to send clipboard event");
    }

    self.offer(qh);
  }

  fn offer(&mut self, qh: &QueueHandle<Self>) {
    if let SelectionState::Ours(old_source) = &self.clipboard.selection_state {
      old_source.destroy();
    }

    let (Some(data_manager), Some(data_device)) = (&self.data_manager, &self.data_device) else {
      return;
    };

    let source = data_manager.create_data_source(qh, ());
    for mime_type in self.clipboard.clipboard_data.keys() {
      source.offer(mime_type.clone());
    }
    source.offer(REOFFER_HINT_MIME.to_string());

    // Set state to Ours before set_selection so the echoed Selection event is recognized
    self.clipboard.selection_state = SelectionState::Ours(source);
    let source_ref = match &self.clipboard.selection_state {
      SelectionState::Ours(source) => source,
      _ => unreachable!(),
    };
    data_device.set_selection(Some(source_ref));
  }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for State {
  fn event(
    state: &mut Self,
    _device: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    event: zwlr_data_control_device_v1::Event,
    _data: &(),
    _conn: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_device_v1::Event::DataOffer { id } => {
        if let Some(old_offer) = state.clipboard.pending_offer.take() {
          old_offer.destroy();
        }
        state.clipboard.pending_mime_types.clear();
        state.clipboard.pending_offer = Some(id);
      }
      zwlr_data_control_device_v1::Event::Selection { id } => {
        // If we own the selection, this is the compositor echoing our set_selection
        if matches!(&state.clipboard.selection_state, SelectionState::Ours(_)) {
          if let Some(offer) = id {
            offer.destroy();
          }
          state.clipboard.pending_offer = None;
          state.clipboard.pending_mime_types.clear();
          return;
        }

        let Some(offer) = id else {
          // Selection cleared
          state.clipboard.selection_state = SelectionState::Free;
          state.clipboard.pending_offer = None;
          state.clipboard.pending_mime_types.clear();
          return;
        };

        let offer_id = offer.id();
        state.clipboard.selection_state = SelectionState::Client {
          data_offer_id: offer_id.clone(),
        };

        let mime_types = std::mem::take(&mut state.clipboard.pending_mime_types);
        state.clipboard.pending_offer = None;

        if mime_types
          .iter()
          .any(|m| m == SECRET_HINT_MIME || m == REOFFER_HINT_MIME)
        {
          debug!("Clipboard offer contains secret or reoffer hint, ignoring");
          offer.destroy();
          return;
        }

        let target_mimes = select_target_mimes(&mime_types);
        if target_mimes.is_empty() {
          debug!("No supported mime types in offer");
          offer.destroy();
          return;
        }

        let Some(loop_handle) = &state.loop_handle else {
          error!("No loop handle available");
          offer.destroy();
          return;
        };

        let read_state = Rc::new(RefCell::new(MimeReadState {
          buffers: HashMap::new(),
          completed: HashSet::new(),
          total_count: target_mimes.len(),
          original_offer_id: offer_id,
          offered_mime_types: mime_types,
          total_bytes: 0,
        }));

        for target_mime in &target_mimes {
          let (read_fd, write_fd) = match rustix::pipe::pipe_with(PipeFlags::CLOEXEC) {
            Ok(fds) => fds,
            Err(err) => {
              error!(?err, mime_type = target_mime, "Failed to create pipe");
              continue;
            }
          };

          offer.receive(target_mime.clone(), write_fd.as_fd());
          drop(write_fd);

          let generic_source = Generic::new(fs::File::from(read_fd), Interest::READ, Mode::Level);

          let mime_key = target_mime.clone();
          let closure_read_state = Rc::clone(&read_state);
          let qh = qh.clone();

          if let Err(err) = loop_handle.insert_source(generic_source, move |_, file, state| {
            let read_state = &closure_read_state;
            // SAFETY: safe as long as we don't close the underlying file
            let file: &mut fs::File = unsafe { file.get_mut() };
            let mut reader = BufReader::new(file);
            match reader.fill_buf() {
              Ok([]) => {
                let all_done = {
                  let mut rs = read_state.borrow_mut();
                  rs.completed.insert(mime_key.clone());
                  rs.completed.len() == rs.total_count
                };

                if all_done {
                  let rs = read_state.borrow();
                  let mime_data = rs.buffers.clone();
                  let original_offer_id = rs.original_offer_id.clone();
                  let offered_mime_types = rs.offered_mime_types.clone();
                  drop(rs);

                  state.on_all_mimes_read_complete(
                    mime_data,
                    original_offer_id,
                    offered_mime_types,
                    &qh,
                  );
                }

                Ok(PostAction::Remove)
              }
              Ok(buf) => {
                let mut rs = read_state.borrow_mut();
                rs.total_bytes += buf.len();
                if rs.total_bytes > MAX_ENTRY_SIZE {
                  warn!(
                    total_bytes = rs.total_bytes,
                    "Clipboard entry exceeds size limit, truncating"
                  );
                  rs.completed.insert(mime_key.clone());
                  let all_done = rs.completed.len() == rs.total_count;
                  drop(rs);

                  if all_done {
                    let rs = read_state.borrow();
                    let mime_data = rs.buffers.clone();
                    let original_offer_id = rs.original_offer_id.clone();
                    let offered_mime_types = rs.offered_mime_types.clone();
                    drop(rs);

                    state.on_all_mimes_read_complete(
                      mime_data,
                      original_offer_id,
                      offered_mime_types,
                      &qh,
                    );
                  }
                  return Ok(PostAction::Remove);
                }

                let entry = rs.buffers.entry(mime_key.clone()).or_default();
                entry.extend_from_slice(buf);
                let len = buf.len();
                reader.consume(len);
                Ok(PostAction::Continue)
              }
              Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(PostAction::Continue),
              Err(err) => {
                error!(?err, mime_type = mime_key, "Error reading clipboard pipe");

                let all_done = {
                  let mut rs = read_state.borrow_mut();
                  rs.completed.insert(mime_key.clone());
                  rs.completed.len() == rs.total_count
                };

                if all_done {
                  let rs = read_state.borrow();
                  let mime_data = rs.buffers.clone();
                  let original_offer_id = rs.original_offer_id.clone();
                  let offered_mime_types = rs.offered_mime_types.clone();
                  drop(rs);

                  state.on_all_mimes_read_complete(
                    mime_data,
                    original_offer_id,
                    offered_mime_types,
                    &qh,
                  );
                }

                Ok(PostAction::Remove)
              }
            }
          }) {
            error!(?err, "Failed to insert pipe read source");
            let mut rs = read_state.borrow_mut();
            rs.completed.insert(target_mime.clone());
          }
        }

        offer.destroy();
      }
      zwlr_data_control_device_v1::Event::Finished => {
        state.data_device = None;
      }
      _ => {}
    }
  }

  event_created_child!(State, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
    zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
  ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for State {
  fn event(
    state: &mut Self,
    offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    event: zwlr_data_control_offer_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event else {
      return;
    };

    let Some(pending) = state.clipboard.pending_offer.as_ref() else {
      return;
    };

    if pending.id() == offer.id() {
      state.clipboard.pending_mime_types.push(mime_type);
    }
  }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for State {
  fn event(
    state: &mut Self,
    source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    event: zwlr_data_control_source_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
        let is_ours = matches!(
          &state.clipboard.selection_state,
          SelectionState::Ours(our_source) if our_source.id() == source.id()
        );
        if !is_ours {
          return;
        }

        let data = if mime_type == REOFFER_HINT_MIME {
          &[] as &[u8]
        } else {
          let Some(data) = state.clipboard.clipboard_data.get(&mime_type) else {
            warn!(?mime_type, "Requested mime type not in clipboard data");
            return;
          };
          data.as_slice()
        };

        if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
          error!(?err, "Failed to clear O_NONBLOCK on send fd");
          return;
        }

        let mut file = fs::File::from(fd);
        if let Err(err) = file.write_all(data) {
          error!(?err, "Failed to write clipboard data to fd");
        }
      }
      zwlr_data_control_source_v1::Event::Cancelled => {
        let is_ours = matches!(
          &state.clipboard.selection_state,
          SelectionState::Ours(our_source) if our_source.id() == source.id()
        );

        if is_ours {
          state.clipboard.selection_state = SelectionState::Free;
        }
        source.destroy();
      }
      _ => {}
    }
  }
}

pub(crate) fn build_text_mime_data(text: &str) -> HashMap<String, Vec<u8>> {
  let data = text.as_bytes().to_vec();
  let mut mime_data = HashMap::new();
  for mime in TEXT_MIME_TYPES {
    mime_data.insert((*mime).to_string(), data.clone());
  }
  mime_data
}

struct StandaloneState {
  seat: Option<wl_seat::WlSeat>,
  data_device: Option<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1>,
  data_manager: Option<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
  clipboard_data: HashMap<String, Vec<u8>>,
  selection_state: SelectionState,
}

pub fn offer_and_wait(mime_data: HashMap<String, Vec<u8>>) -> anyhow::Result<()> {
  let conn = Connection::connect_to_env()?;
  let display = conn.display();
  let mut event_queue = conn.new_event_queue();
  let qh = event_queue.handle();
  let _registry = display.get_registry(&qh, ());

  let mut state = StandaloneState {
    seat: None,
    data_device: None,
    data_manager: None,
    clipboard_data: mime_data,
    selection_state: SelectionState::Free,
  };

  event_queue.roundtrip(&mut state)?;

  if let (Some(data_manager), Some(seat)) = (&state.data_manager, &state.seat) {
    let device = data_manager.get_data_device(seat, &qh, ());
    state.data_device = Some(device);
  } else {
    anyhow::bail!("Failed to bind wl_seat or data control manager");
  }

  // Create the data source and offer all mime types
  let source = state
    .data_manager
    .as_ref()
    .unwrap()
    .create_data_source(&qh, ());
  for mime_type in state.clipboard_data.keys() {
    source.offer(mime_type.clone());
  }

  state.selection_state = SelectionState::Ours(source);
  let source_ref = match &state.selection_state {
    SelectionState::Ours(source) => source,
    _ => unreachable!(),
  };
  state
    .data_device
    .as_ref()
    .unwrap()
    .set_selection(Some(source_ref));

  info!(
    "Forked clipboard offer: serving {} mime types",
    state.clipboard_data.len()
  );

  loop {
    event_queue.blocking_dispatch(&mut state)?;
    if matches!(state.selection_state, SelectionState::Free) {
      info!("Forked clipboard offer: selection taken by another app, exiting");
      break;
    }
  }

  Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for StandaloneState {
  fn event(
    state: &mut Self,
    registry: &wl_registry::WlRegistry,
    event: wl_registry::Event,
    _data: &(),
    _conn: &Connection,
    qh: &QueueHandle<Self>,
  ) {
    let wl_registry::Event::Global {
      name,
      interface,
      version,
    } = event
    else {
      return;
    };

    match &interface[..] {
      "wl_seat" => {
        if state.seat.is_none() {
          state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, 1, qh, ()));
        }
      }
      "zwlr_data_control_manager_v1" => {
        state.data_manager = Some(
          registry.bind::<zwlr_data_control_manager_v1::ZwlrDataControlManagerV1, _, _>(
            name,
            version,
            qh,
            (),
          ),
        );
      }
      _ => {}
    }
  }
}

impl Dispatch<zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, ()> for StandaloneState {
  fn event(
    state: &mut Self,
    _device: &zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    event: zwlr_data_control_device_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_device_v1::Event::DataOffer { id } => {
        // We don't care about incoming offers in the standalone state
        id.destroy();
      }
      zwlr_data_control_device_v1::Event::Selection { id } => {
        if let Some(offer) = id {
          offer.destroy();
        }
      }
      zwlr_data_control_device_v1::Event::Finished => {
        state.data_device = None;
      }
      _ => {}
    }
  }

  event_created_child!(StandaloneState, zwlr_data_control_device_v1::ZwlrDataControlDeviceV1, [
    zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()),
  ]);
}

impl Dispatch<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1, ()> for StandaloneState {
  fn event(
    _state: &mut Self,
    _offer: &zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    _event: zwlr_data_control_offer_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
  }
}

impl Dispatch<zwlr_data_control_source_v1::ZwlrDataControlSourceV1, ()> for StandaloneState {
  fn event(
    state: &mut Self,
    source: &zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    event: zwlr_data_control_source_v1::Event,
    _data: &(),
    _conn: &Connection,
    _qh: &QueueHandle<Self>,
  ) {
    match event {
      zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
        let is_ours = matches!(
          &state.selection_state,
          SelectionState::Ours(our_source) if our_source.id() == source.id()
        );
        if !is_ours {
          return;
        }

        let Some(data) = state.clipboard_data.get(&mime_type) else {
          return;
        };

        if let Err(err) = fcntl_setfl(&fd, OFlags::empty()) {
          error!(?err, "Failed to clear O_NONBLOCK on send fd");
          return;
        }

        let mut file = fs::File::from(fd);
        if let Err(err) = file.write_all(data) {
          error!(?err, "Failed to write clipboard data to fd");
        }
      }
      zwlr_data_control_source_v1::Event::Cancelled => {
        let is_ours = matches!(
          &state.selection_state,
          SelectionState::Ours(our_source) if our_source.id() == source.id()
        );

        if is_ours {
          state.selection_state = SelectionState::Free;
        }
        source.destroy();
      }
      _ => {}
    }
  }
}

delegate_noop!(StandaloneState: ignore wl_seat::WlSeat);
delegate_noop!(StandaloneState: ignore zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);
