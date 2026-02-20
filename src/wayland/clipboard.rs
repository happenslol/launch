use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::path::PathBuf;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use rusqlite::OpenFlags;
use rustix::fs::{OFlags, fcntl_setfl};
use rustix::pipe::PipeFlags;
use tracing::{debug, error, warn};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
  zwlr_data_control_device_v1, zwlr_data_control_offer_v1, zwlr_data_control_source_v1,
};

use super::{State, WaylandEvent};

const TEXT_MIME_TYPES: &[&str] = &[
  "text/plain;charset=utf-8",
  "text/plain",
  "UTF8_STRING",
  "STRING",
  "TEXT",
];

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

    conn.execute_batch(
      r#"
      CREATE TABLE IF NOT EXISTS clipboard_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp INTEGER NOT NULL DEFAULT (unixepoch()),
        mime_types TEXT NOT NULL,
        data BLOB NOT NULL
      ) STRICT;
      "#,
    )?;

    let reader = ClipboardDbReader::new(path);

    Ok((Self(conn), reader))
  }

  fn insert(&self, mime_types: &[String], data: &[u8]) {
    let mime_types_json = mime_types_to_json(mime_types);
    let mut statement = match self
      .0
      .prepare_cached("INSERT INTO clipboard_history (mime_types, data) VALUES (?1, ?2)")
    {
      Ok(statement) => statement,
      Err(err) => {
        error!(?err, "Failed to prepare clipboard history insert");
        return;
      }
    };

    if let Err(err) = statement.execute(rusqlite::params![mime_types_json, data]) {
      error!(?err, "Failed to insert clipboard history entry");
    }
  }
}

pub struct ClipboardEntry {
  pub id: i64,
  pub timestamp: i64,
  pub _mime_types: Vec<String>,
  pub data: Vec<u8>,
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

  pub fn get_by_id(&self, id: i64) -> anyhow::Result<Option<ClipboardEntry>> {
    let conn = self.open()?;
    let mut statement = conn.prepare_cached(
      "SELECT id, timestamp, mime_types, data FROM clipboard_history WHERE id = ?1",
    )?;

    let mut rows = statement.query_map([id], |row| {
      Ok((
        row.get::<_, i64>(0)?,
        row.get::<_, i64>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, Vec<u8>>(3)?,
      ))
    })?;

    match rows.next() {
      Some(Ok((id, timestamp, mime_types_json, data))) => Ok(Some(ClipboardEntry {
        id,
        timestamp,
        _mime_types: parse_mime_types_json(&mime_types_json),
        data,
      })),
      Some(Err(err)) => Err(err.into()),
      None => Ok(None),
    }
  }

  pub fn recent(&self, limit: u32) -> anyhow::Result<Vec<ClipboardEntry>> {
    let conn = self.open()?;
    let mut statement = conn.prepare_cached(
      "SELECT id, timestamp, mime_types, data FROM clipboard_history ORDER BY id DESC LIMIT ?1",
    )?;

    let rows = statement.query_map([limit], |row| {
      Ok((
        row.get::<_, i64>(0)?,
        row.get::<_, i64>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, Vec<u8>>(3)?,
      ))
    })?;

    let mut entries = Vec::new();
    for row in rows {
      let (id, timestamp, mime_types_json, data) = row?;
      entries.push(ClipboardEntry {
        id,
        timestamp,
        _mime_types: parse_mime_types_json(&mime_types_json),
        data,
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

pub struct ClipboardState {
  pub pending_offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
  pub pending_mime_types: Vec<String>,
  pub clipboard_data: HashMap<String, Vec<u8>>,
  pub selection_state: SelectionState,
  db_writer: Option<ClipboardDbWriter>,
  pub db_reader: Option<ClipboardDbReader>,
}

impl ClipboardState {
  pub fn new(clipboard_monitoring: bool) -> Self {
    let (db_writer, db_reader) = if clipboard_monitoring {
      match ClipboardDbWriter::new() {
        Ok((writer, reader)) => (Some(writer), Some(reader)),
        Err(err) => {
          error!(?err, "Failed to open clipboard history database");
          (None, None)
        }
      }
    } else {
      (None, None)
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

fn pick_mime_type(offered: &[String]) -> Option<String> {
  for preferred in TEXT_MIME_TYPES {
    if let Some(found) = offered.iter().find(|m| m.as_str() == *preferred) {
      return Some(found.clone());
    }
  }
  None
}

impl State {
  pub fn copy_history_entry(&mut self, id: i64) {
    let Some(reader) = &self.clipboard.db_reader else {
      error!("No clipboard database reader available");
      return;
    };

    let entry = match reader.get_by_id(id) {
      Ok(Some(entry)) => entry,
      Ok(None) => {
        error!(id, "Clipboard history entry not found");
        return;
      }
      Err(err) => {
        error!(?err, id, "Failed to read clipboard history entry");
        return;
      }
    };

    self.clipboard.clipboard_data.clear();
    for mime_type in TEXT_MIME_TYPES {
      self
        .clipboard
        .clipboard_data
        .insert((*mime_type).to_string(), entry.data.clone());
    }

    let Some(qh) = &self.qh else {
      error!("No QueueHandle available");
      return;
    };
    let qh = qh.clone();
    self.offer(&qh);
  }

  fn on_clipboard_read_complete(
    &mut self,
    data: Vec<u8>,
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

    self.clipboard.clipboard_data.clear();
    for mime_type in &offered_mime_types {
      if TEXT_MIME_TYPES.contains(&mime_type.as_str()) {
        self
          .clipboard
          .clipboard_data
          .insert(mime_type.clone(), data.clone());
      }
    }

    debug!(
      bytes = data.len(),
      mime_types = ?self.clipboard.clipboard_data.keys().collect::<Vec<_>>(),
      "Clipboard data captured"
    );

    if let Some(writer) = &self.clipboard.db_writer {
      writer.insert(&offered_mime_types, &data);
    }

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

        let Some(chosen_mime) = pick_mime_type(&mime_types) else {
          debug!("No supported text mime type in offer");
          offer.destroy();
          return;
        };

        // Create a pipe to read the offer data
        let (read_fd, write_fd) = match rustix::pipe::pipe_with(PipeFlags::CLOEXEC) {
          Ok(fds) => fds,
          Err(err) => {
            error!(?err, "Failed to create pipe");
            offer.destroy();
            return;
          }
        };

        offer.receive(chosen_mime, write_fd.as_fd());
        drop(write_fd);

        let generic_source = Generic::new(fs::File::from(read_fd), Interest::READ, Mode::Level);

        let Some(loop_handle) = &state.loop_handle else {
          error!("No loop handle available");
          offer.destroy();
          return;
        };

        let captured_offer_id = offer_id;
        let captured_mime_types = mime_types;
        let mut buffer = Vec::new();
        let qh = qh.clone();

        if let Err(err) = loop_handle.insert_source(generic_source, move |_, file, state| {
          // SAFETY: safe as long as we don't close the underlying file
          // TODO: Can we make this safe?
          let file: &mut fs::File = unsafe { file.get_mut() };
          let mut reader = BufReader::new(file);
          match reader.fill_buf() {
            Ok([]) => {
              state.on_clipboard_read_complete(
                std::mem::take(&mut buffer),
                captured_offer_id.clone(),
                captured_mime_types.clone(),
                &qh,
              );

              Ok(PostAction::Remove)
            }
            Ok(buf) => {
              buffer.extend_from_slice(buf);
              let len = buf.len();
              reader.consume(len);
              Ok(PostAction::Continue)
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(PostAction::Continue),
            Err(err) => {
              error!(?err, "Error reading clipboard data from pipe");
              Ok(PostAction::Remove)
            }
          }
        }) {
          error!(?err, "Failed to insert pipe read source");
          offer.destroy();
        }
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

        let Some(data) = state.clipboard.clipboard_data.get(&mime_type) else {
          warn!(?mime_type, "Requested mime type not in clipboard data");
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
