use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;

use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
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

pub struct ClipboardState {
  pub pending_offer: Option<zwlr_data_control_offer_v1::ZwlrDataControlOfferV1>,
  pub pending_mime_types: Vec<String>,
  pub clipboard_data: HashMap<String, Vec<u8>>,
  pub selection_state: SelectionState,
}

impl ClipboardState {
  pub fn new() -> Self {
    Self {
      pending_offer: None,
      pending_mime_types: Vec::new(),
      clipboard_data: HashMap::new(),
      selection_state: SelectionState::Free,
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

    if let Some(event_tx) = &self.event_tx
      && let Err(err) = event_tx.send(WaylandEvent::ClipboardText(data))
    {
      error!(?err, "Failed to send clipboard event");
    }

    self.offer_captured_clipboard(qh);
  }

  fn offer_captured_clipboard(&mut self, qh: &QueueHandle<Self>) {
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
