use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use flume::Sender;
use tracing::warn;
use zvariant::OwnedValue;

use super::{CloseReason, Notification};

pub(super) const NOTIFICATIONS_BUS: &str = "org.freedesktop.Notifications";
pub(super) const NOTIFICATIONS_OBJECT: &str = "/org/freedesktop/Notifications";

/// Requests produced by inbound D-Bus method calls. They run on the connection
/// executor, so rather than touching gpui state directly they are forwarded to
/// the foreground [`super::Notifications`] entity over a channel.
pub(super) enum ServerRequest {
  /// Boxed so the variant doesn't dwarf `Close`; a `Notification` carries its
  /// decoded image and parsed body.
  Notify(Box<Notification>),
  Close(u32),
}

/// The exported `org.freedesktop.Notifications` object. Client method calls land
/// here; the daemon's own state lives in the [`super::Notifications`] entity.
pub(super) struct NotificationServer {
  requests: Sender<ServerRequest>,
  next_id: Arc<AtomicU32>,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl NotificationServer {
  #[allow(clippy::too_many_arguments)]
  async fn notify(
    &self,
    app_name: String,
    replaces_id: u32,
    app_icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: HashMap<String, OwnedValue>,
    expire_timeout: i32,
  ) -> u32 {
    let id = if replaces_id != 0 {
      replaces_id
    } else {
      self.next_id.fetch_add(1, Ordering::Relaxed)
    };

    let notification = Notification::from_dbus(
      id,
      app_name,
      app_icon,
      summary,
      body,
      actions,
      &hints,
      expire_timeout,
    );

    if let Err(error) = self
      .requests
      .send(ServerRequest::Notify(Box::new(notification)))
    {
      warn!(?error, "notification receiver dropped");
    }

    id
  }

  async fn close_notification(&self, id: u32) {
    if let Err(error) = self.requests.send(ServerRequest::Close(id)) {
      warn!(?error, "notification receiver dropped");
    }
  }

  async fn get_capabilities(&self) -> Vec<String> {
    vec![
      "actions".to_string(),
      "body".to_string(),
      // Bold, italic and underline are drawn; `<a>` keeps its text and `<img>`
      // is dropped, which is why neither `body-hyperlinks` nor `body-images`
      // is claimed alongside.
      "body-markup".to_string(),
      "icon-static".to_string(),
      "persistence".to_string(),
    ]
  }

  async fn get_server_information(&self) -> (String, String, String, String) {
    (
      "launch".to_string(),
      "launch".to_string(),
      env!("CARGO_PKG_VERSION").to_string(),
      "1.2".to_string(),
    )
  }

  #[zbus(signal)]
  async fn notification_closed(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    id: u32,
    reason: u32,
  ) -> zbus::Result<()>;

  #[zbus(signal)]
  async fn action_invoked(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    id: u32,
    action_key: &str,
  ) -> zbus::Result<()>;
}

impl NotificationServer {
  pub(super) fn new(requests: Sender<ServerRequest>, next_id: Arc<AtomicU32>) -> Self {
    Self { requests, next_id }
  }

  pub(super) async fn emit_closed(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    id: u32,
    reason: CloseReason,
  ) -> zbus::Result<()> {
    Self::notification_closed(emitter, id, reason as u32).await
  }

  pub(super) async fn emit_action_invoked(
    emitter: &zbus::object_server::SignalEmitter<'_>,
    id: u32,
    action_key: &str,
  ) -> zbus::Result<()> {
    Self::action_invoked(emitter, id, action_key).await
  }

  /// Exports the object and claims the well-known name, replacing any daemon
  /// that allows it. Fails fast if the name is already held and locked.
  pub(super) async fn attach_to(self, connection: &zbus::Connection) -> zbus::Result<()> {
    if !connection
      .object_server()
      .at(NOTIFICATIONS_OBJECT, self)
      .await?
    {
      return Err(zbus::Error::Failure(format!(
        "Object already exists at {NOTIFICATIONS_OBJECT} -- is another notification daemon running?"
      )));
    }

    let flags = [
      zbus::fdo::RequestNameFlags::ReplaceExisting,
      zbus::fdo::RequestNameFlags::DoNotQueue,
    ];
    match connection
      .request_name_with_flags(NOTIFICATIONS_BUS, flags.into_iter().collect())
      .await
    {
      Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Ok(()),
      Ok(reply) => Err(zbus::Error::Failure(format!(
        "could not become owner of {NOTIFICATIONS_BUS}: {reply:?}"
      ))),
      Err(error) => Err(error),
    }
  }
}
