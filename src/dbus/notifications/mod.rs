mod server;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use anyhow::Result;
use gpui::{App, AppContext, Entity, EventEmitter, Global, RenderImage, SharedString, Task};
use image::{Frame, ImageBuffer, Rgba};
use smallvec::SmallVec;
use tracing::warn;
use zvariant::{OwnedValue, Type, Value};

use crate::db::{DB, NotificationDbReader};
use crate::dbus::GlobalDbusConnection;
use crate::util::ResultExt;

use server::{NotificationServer, ServerRequest};

/// Server-chosen default lifetime for notifications that don't request one.
const DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Size requested when resolving a themed icon name. Themed icons only exist at
/// the discrete sizes an app ships, and the lookup returns the closest one
/// without exceeding the request, so we ask for a size well above the ~40px
/// display slot: that way a crisp larger icon is downscaled rather than a tiny
/// one (e.g. a 32px icon) being upscaled and looking blurry.
const ICON_LOOKUP_SIZE: u16 = 128;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
  Low,
  Normal,
  Critical,
}

impl Urgency {
  fn from_hint(value: u8) -> Self {
    match value {
      0 => Urgency::Low,
      2 => Urgency::Critical,
      _ => Urgency::Normal,
    }
  }

  pub fn as_u8(self) -> u8 {
    match self {
      Urgency::Low => 0,
      Urgency::Normal => 1,
      Urgency::Critical => 2,
    }
  }
}

/// Why a notification was removed, matching the reason codes in the
/// freedesktop notification spec emitted with `NotificationClosed`.
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum CloseReason {
  Expired = 1,
  Dismissed = 2,
  /// Closed via a `CloseNotification` method call.
  Closed = 3,
}

#[derive(Clone)]
pub struct NotificationAction {
  pub key: String,
  pub label: SharedString,
}

#[derive(Clone)]
pub struct Notification {
  pub id: u32,
  pub app_name: SharedString,
  pub summary: SharedString,
  pub body: SharedString,
  pub urgency: Urgency,
  pub expire_timeout: i32,
  pub actions: Vec<NotificationAction>,
  /// Decoded raw pixels from an `image-data` hint, ready to render.
  pub image: Option<Arc<RenderImage>>,
  /// A resolved on-disk icon path (from `image-path` or `app_icon`).
  pub icon_path: Option<SharedString>,
  /// The raw `app_icon` argument, kept for the persisted history.
  pub app_icon: SharedString,
  /// When this notification was received, for relative-time display.
  pub received: Instant,
}

impl Notification {
  #[allow(clippy::too_many_arguments)]
  fn from_dbus(
    id: u32,
    app_name: String,
    app_icon: String,
    summary: String,
    body: String,
    actions: Vec<String>,
    hints: &HashMap<String, OwnedValue>,
    expire_timeout: i32,
  ) -> Self {
    let urgency = hints
      .get("urgency")
      .and_then(|value| value.downcast_ref::<u8>().ok())
      .map(Urgency::from_hint)
      .unwrap_or(Urgency::Normal);

    let image = image_from_hints(hints);
    let icon_path = if image.is_some() {
      None
    } else {
      icon_path_from_hints(hints)
        .or_else(|| resolve_icon(&app_icon))
        .or_else(|| desktop_entry_icon(hints))
        .map(|path| SharedString::from(path.to_string_lossy().into_owned()))
    };

    let actions = actions
      .chunks_exact(2)
      .map(|pair| NotificationAction {
        key: pair[0].clone(),
        label: SharedString::from(pair[1].clone()),
      })
      .collect();

    Notification {
      id,
      app_name: SharedString::from(app_name),
      summary: SharedString::from(summary),
      body: SharedString::from(strip_body_markup(body)),
      urgency,
      expire_timeout,
      actions,
      image,
      icon_path,
      app_icon: SharedString::from(app_icon),
      received: Instant::now(),
    }
  }

  /// The effective lifetime in milliseconds, resolving the spec's sentinel
  /// values. `0` means the notification never expires on its own.
  fn effective_timeout_ms(&self) -> u64 {
    match self.expire_timeout {
      timeout if timeout < 0 => match self.urgency {
        Urgency::Critical => 0,
        _ => DEFAULT_TIMEOUT_MS,
      },
      0 => 0,
      timeout => timeout as u64,
    }
  }
}

/// The tags the spec defines for body markup. Nothing else is treated as a tag,
/// so a body that merely happens to contain something like `<3` keeps it.
const MARKUP_TAGS: [&str; 5] = ["b", "i", "u", "a", "img"];

/// The longest entity we accept, `&#x10FFFF;` without its delimiters. Bounding
/// the search keeps a lone `&` — ordinary text, as in `AT&T` — from dragging a
/// scan across the rest of the body looking for a `;`.
const MAX_ENTITY_LEN: usize = 8;

/// Removes the markup the spec allows in a notification body and resolves the
/// XML entities that come with it.
///
/// Bodies are "XML-based, and consist of a small subset of HTML", so senders
/// escape their text: Signal sends `Kaffee &amp; Kuchen`, blueman sends
/// `<b>Hilmar&#x27;s Pixel Buds Pro</b>`. We do not advertise `body-markup`, and
/// the spec has servers that don't "filter them out" — but filtering the tags
/// without also decoding the entities is what leaves a literal `&gt;` on screen.
///
/// Summaries are left alone: markup is defined for the body only, and senders
/// treat it that way. The same Signal contact arrives as `Momo <3` in a summary
/// and `Momo &lt;3` in a body.
///
/// Tags are removed before entities are decoded, so an escaped `&lt;b&gt;` ends
/// up as literal text rather than being mistaken for markup on a second pass.
fn strip_body_markup(body: String) -> String {
  if !body.contains(['<', '&']) {
    return body;
  }

  let mut stripped = String::with_capacity(body.len());
  let mut rest = body.as_str();

  loop {
    let Some(index) = rest.find(['<', '&']) else {
      stripped.push_str(rest);
      return stripped;
    };

    stripped.push_str(&rest[..index]);
    rest = &rest[index..];

    if let Some(after) = strip_tag(rest) {
      rest = after;
    } else if let Some((character, after)) = decode_entity(rest) {
      stripped.push(character);
      rest = after;
    } else {
      // Neither, so this is a bare `<` or `&` that the body is free to contain.
      let mut characters = rest.chars();
      stripped.extend(characters.next());
      rest = characters.as_str();
    }
  }
}

/// Consumes one [`MARKUP_TAGS`] tag at the start of `text`, returning what
/// follows it.
fn strip_tag(text: &str) -> Option<&str> {
  let opened = text.strip_prefix('<')?;
  let opened = opened.strip_prefix('/').unwrap_or(opened);
  // An attribute value holding a `>` would cut the tag short here, but in XML it
  // has to arrive as `&gt;`, so there is none to find.
  let end = opened.find('>')?;
  let name = opened[..end]
    .trim_end_matches('/')
    .split([' ', '\t', '\n'])
    .next()
    .unwrap_or_default();

  MARKUP_TAGS
    .iter()
    .any(|tag| name.eq_ignore_ascii_case(tag))
    .then(|| &opened[end + 1..])
}

/// Decodes one XML entity at the start of `text`, returning the character it
/// stands for and what follows it.
fn decode_entity(text: &str) -> Option<(char, &str)> {
  let opened = text.strip_prefix('&')?;
  let end = opened.bytes().take(MAX_ENTITY_LEN).position(|byte| byte == b';')?;
  let (name, rest) = opened.split_at(end);

  let character = match name {
    "amp" => '&',
    "lt" => '<',
    "gt" => '>',
    "quot" => '"',
    "apos" => '\'',
    // Character references, which senders reach for in place of the named
    // entities XML leaves undefined — blueman's `&#x27;` for an apostrophe.
    _ => {
      let digits = name.strip_prefix('#')?;
      let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
      };
      char::from_u32(code)?
    }
  };

  Some((character, &rest[1..]))
}

/// The `image-data` hint payload, a `(iiibiiay)` struct per the spec.
#[derive(Clone, Type, Value, OwnedValue)]
struct ImageData {
  width: i32,
  height: i32,
  rowstride: i32,
  has_alpha: bool,
  bits_per_sample: i32,
  channels: i32,
  data: Vec<u8>,
}

fn image_from_hints(hints: &HashMap<String, OwnedValue>) -> Option<Arc<RenderImage>> {
  let value = ["image-data", "image_data", "icon_data"]
    .into_iter()
    .find_map(|key| hints.get(key))?;

  let data = ImageData::try_from(value.clone()).log_err()?;
  render_image_from_data(data)
}

fn render_image_from_data(image: ImageData) -> Option<Arc<RenderImage>> {
  if image.width <= 0 || image.height <= 0 || image.channels < 3 || image.rowstride <= 0 {
    return None;
  }

  let width = image.width as usize;
  let height = image.height as usize;
  let channels = image.channels as usize;
  let rowstride = image.rowstride as usize;
  let row_bytes = width.checked_mul(channels)?;

  if rowstride < row_bytes || image.data.len() < rowstride.checked_mul(height)? {
    return None;
  }

  // gpui frames hold BGRA bytes typed as Rgba<u8>, while the hint delivers
  // RGB(A) with red first, so each pixel is reordered and alpha is filled in
  // when the source has none.
  let mut buffer = Vec::with_capacity(width * height * 4);
  for y in 0..height {
    let row = &image.data[y * rowstride..y * rowstride + row_bytes];
    for x in 0..width {
      let pixel = &row[x * channels..x * channels + channels];
      let alpha = if channels >= 4 { pixel[3] } else { 255 };
      buffer.extend_from_slice(&[pixel[2], pixel[1], pixel[0], alpha]);
    }
  }

  let buffer = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width as u32, height as u32, buffer)?;
  Some(Arc::new(RenderImage::new(SmallVec::from_elem(
    Frame::new(buffer),
    1,
  ))))
}

fn icon_path_from_hints(hints: &HashMap<String, OwnedValue>) -> Option<PathBuf> {
  let spec = ["image-path", "image_path"]
    .into_iter()
    .find_map(|key| hints.get(key))
    .and_then(|value| value.downcast_ref::<&str>().ok())?;

  resolve_icon(spec)
}

/// Resolves an `app_icon`/`image-path` value, which may be an absolute path, a
/// `file://` URI, or a freedesktop icon name, to a concrete file on disk.
fn resolve_icon(spec: &str) -> Option<PathBuf> {
  if spec.is_empty() {
    return None;
  }

  if let Some(rest) = spec.strip_prefix("file://") {
    let decoded = urlencoding::decode(rest)
      .map(|cow| cow.into_owned())
      .unwrap_or_else(|_| rest.to_string());
    let path = PathBuf::from(decoded);
    return path.is_file().then_some(path);
  }

  if spec.starts_with('/') {
    let path = PathBuf::from(spec);
    return path.is_file().then_some(path);
  }

  freedesktop_icons::lookup(spec)
    .with_cache()
    .with_scale(1)
    .with_size(ICON_LOOKUP_SIZE)
    .find()
}

/// Falls back to the `desktop-entry` hint (an application id like
/// `com.mitchellh.ghostty`), treating it as an icon name. Many apps set only
/// this and leave `app_icon`/`image-path` empty.
fn desktop_entry_icon(hints: &HashMap<String, OwnedValue>) -> Option<PathBuf> {
  let entry = hints
    .get("desktop-entry")
    .and_then(|value| value.downcast_ref::<&str>().ok())?;

  resolve_icon(entry)
}

pub enum NotificationEvent {
  Changed,
}

struct GlobalNotifications(Entity<Notifications>);

impl Global for GlobalNotifications {}

/// A notification's time-to-live, tracked so it can be paused while hovered.
/// `task` counts down `remaining`; pausing cancels it and debits the elapsed
/// time, resuming restarts it with whatever is left.
struct Expiry {
  remaining: Duration,
  /// When the running countdown began, or `None` while paused.
  started_at: Option<Instant>,
  task: Option<Task<()>>,
}

/// The source of truth for live notifications. Owns per-notification expiry
/// timers and the connection used to emit `NotificationClosed`/`ActionInvoked`
/// back to clients. The on-screen display observes this entity.
pub struct Notifications {
  active: Vec<Notification>,
  expiry: HashMap<u32, Expiry>,
  connection: Option<zbus::Connection>,
  _host_task: Option<Task<()>>,
}

impl EventEmitter<NotificationEvent> for Notifications {}

impl Notifications {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalNotifications>().0.clone()
  }

  pub fn active(&self) -> &[Notification] {
    &self.active
  }

  fn push(&mut self, notification: Notification, cx: &mut gpui::Context<Self>) {
    DB.record_notification(
      &notification.app_name,
      &notification.summary,
      &notification.body,
      &notification.app_icon,
      notification.urgency.as_u8(),
    );

    let id = notification.id;
    let timeout = notification.effective_timeout_ms();

    if let Some(existing) = self.active.iter_mut().find(|entry| entry.id == id) {
      *existing = notification;
    } else {
      self.active.insert(0, notification);
    }

    self.expiry.remove(&id);
    if timeout > 0 {
      self.arm_expiry(id, timeout, cx);
    }

    cx.emit(NotificationEvent::Changed);
    cx.notify();
  }

  pub fn dismiss(&mut self, id: u32, reason: CloseReason, cx: &mut gpui::Context<Self>) {
    let Some(position) = self.active.iter().position(|entry| entry.id == id) else {
      return;
    };

    self.active.remove(position);
    self.expiry.remove(&id);
    self.emit_closed(id, reason, cx);

    cx.emit(NotificationEvent::Changed);
    cx.notify();
  }

  pub fn invoke_action(&mut self, id: u32, key: String, cx: &mut gpui::Context<Self>) {
    if !self.active.iter().any(|entry| entry.id == id) {
      return;
    }

    self.emit_action(id, key, cx);
    self.dismiss(id, CloseReason::Dismissed, cx);
  }

  /// Pauses a notification's expiry while the pointer is over it, resuming with
  /// the remaining time once it leaves.
  pub fn set_hovered(&mut self, id: u32, hovered: bool, cx: &mut gpui::Context<Self>) {
    if hovered {
      self.pause_expiry(id);
    } else {
      self.resume_expiry(id, cx);
    }
  }

  fn arm_expiry(&mut self, id: u32, timeout_ms: u64, cx: &mut gpui::Context<Self>) {
    self.expiry.insert(
      id,
      Expiry {
        remaining: Duration::from_millis(timeout_ms),
        started_at: None,
        task: None,
      },
    );
    self.start_expiry(id, cx);
  }

  /// (Re)starts the countdown for `id` from its remaining time, replacing any
  /// running timer. Dismisses immediately if no time is left.
  fn start_expiry(&mut self, id: u32, cx: &mut gpui::Context<Self>) {
    let Some(remaining) = self.expiry.get(&id).map(|expiry| expiry.remaining) else {
      return;
    };

    if remaining.is_zero() {
      self.dismiss(id, CloseReason::Expired, cx);
      return;
    }

    let task = cx.spawn(async move |this, cx| {
      cx.background_executor().timer(remaining).await;
      this
        .update(cx, |this, cx| this.dismiss(id, CloseReason::Expired, cx))
        .log_err();
    });

    if let Some(expiry) = self.expiry.get_mut(&id) {
      expiry.started_at = Some(Instant::now());
      expiry.task = Some(task);
    }
  }

  /// Pauses the countdown for `id`, debiting the time elapsed since it started
  /// so resuming continues where it left off. No-op if already paused or absent.
  fn pause_expiry(&mut self, id: u32) {
    let Some(expiry) = self.expiry.get_mut(&id) else {
      return;
    };

    if let Some(started_at) = expiry.started_at.take() {
      expiry.remaining = expiry.remaining.saturating_sub(started_at.elapsed());
      expiry.task = None;
    }
  }

  /// Resumes a paused countdown for `id` with whatever time remained.
  fn resume_expiry(&mut self, id: u32, cx: &mut gpui::Context<Self>) {
    let is_paused = self.expiry.get(&id).is_some_and(|expiry| expiry.task.is_none());
    if is_paused {
      self.start_expiry(id, cx);
    }
  }

  fn emit_closed(&self, id: u32, reason: CloseReason, cx: &mut gpui::Context<Self>) {
    let Some(connection) = self.connection.clone() else {
      return;
    };

    cx.background_spawn(async move {
      if let Ok(interface) = connection
        .object_server()
        .interface::<_, NotificationServer>(server::NOTIFICATIONS_OBJECT)
        .await
      {
        NotificationServer::emit_closed(interface.signal_emitter(), id, reason)
          .await
          .log_err();
      }
    })
    .detach();
  }

  fn emit_action(&self, id: u32, key: String, cx: &mut gpui::Context<Self>) {
    let Some(connection) = self.connection.clone() else {
      return;
    };

    cx.background_spawn(async move {
      if let Ok(interface) = connection
        .object_server()
        .interface::<_, NotificationServer>(server::NOTIFICATIONS_OBJECT)
        .await
      {
        NotificationServer::emit_action_invoked(interface.signal_emitter(), id, &key)
          .await
          .log_err();
      }
    })
    .detach();
  }
}

pub fn init(cx: &mut App) {
  NotificationDbReader::install(cx);

  let entity = cx.new(|_cx| Notifications {
    active: Vec::new(),
    expiry: HashMap::new(),
    connection: None,
    _host_task: None,
  });

  cx.set_global(GlobalNotifications(entity.clone()));

  let host_task = cx.spawn({
    let entity = entity.clone();
    async move |cx| {
      if let Err(error) = run_server(entity, cx).await {
        warn!(?error, "notification server exited");
      }
    }
  });

  entity.update(cx, |this, _cx| {
    this._host_task = Some(host_task);
  });
}

async fn run_server(entity: Entity<Notifications>, cx: &mut gpui::AsyncApp) -> Result<()> {
  let connection_task = cx.update(|cx| GlobalDbusConnection::session(cx));
  let connection = connection_task
    .await
    .ok_or_else(|| anyhow::anyhow!("failed to get session bus connection"))?;

  let (sender, receiver) = flume::unbounded::<ServerRequest>();
  let next_id = Arc::new(AtomicU32::new(1));
  let server = NotificationServer::new(sender, next_id);
  server.attach_to(&connection).await?;

  entity.update(cx, |this, _cx| {
    this.connection = Some(connection.clone());
  });

  while let Ok(request) = receiver.recv_async().await {
    match request {
      ServerRequest::Notify(notification) => {
        entity.update(cx, |this, cx| this.push(notification, cx));
      }
      ServerRequest::Close(id) => {
        entity.update(cx, |this, cx| this.dismiss(id, CloseReason::Closed, cx));
      }
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::strip_body_markup;

  #[track_caller]
  fn strip(body: &str) -> String {
    strip_body_markup(body.to_owned())
  }

  #[test]
  fn decodes_entities() {
    assert_eq!(strip("Kaffee &amp; Kuchen"), "Kaffee & Kuchen");
    // Signal wraps names in bidi isolates, which have to survive untouched.
    assert_eq!(
      strip("\u{2068}Momo &lt;3\u{2069} reacted"),
      "\u{2068}Momo <3\u{2069} reacted"
    );
    assert_eq!(strip("a &gt; b &quot;c&quot; &apos;d&apos;"), "a > b \"c\" 'd'");
    assert_eq!(strip("Hilmar&#x27;s Buds"), "Hilmar's Buds");
    assert_eq!(strip("Hilmar&#39;s Buds"), "Hilmar's Buds");
  }

  #[test]
  fn strips_the_specs_tags() {
    assert_eq!(strip("<b>Pixel Buds Pro</b> (24:29)"), "Pixel Buds Pro (24:29)");
    assert_eq!(strip("<i>a</i><u>b</u>"), "ab");
    assert_eq!(strip("see <a href=\"https://x.test\">this</a>"), "see this");
    assert_eq!(strip("<img src=\"a.png\" alt=\"a\"/>done"), "done");
    assert_eq!(strip("<B>upper</B>"), "upper");
  }

  /// Only the spec's tags are markup, so text that merely looks tag-ish stays.
  #[test]
  fn leaves_other_angle_brackets_alone() {
    assert_eq!(strip("i <3 you"), "i <3 you");
    assert_eq!(strip("<3> still text"), "<3> still text");
    assert_eq!(strip("use <div> for that"), "use <div> for that");
    assert_eq!(strip("a < b and b > c"), "a < b and b > c");
  }

  /// A bare `&` is ordinary text, and so is anything that isn't a known entity.
  #[test]
  fn leaves_other_ampersands_alone() {
    assert_eq!(strip("AT&T"), "AT&T");
    assert_eq!(strip("&nosuch; &#zz; &"), "&nosuch; &#zz; &");
    assert_eq!(strip("Q&A; later"), "Q&A; later");
  }

  /// Tags go before entities are decoded, so escaped markup survives as text
  /// instead of being stripped on a second look.
  #[test]
  fn does_not_decode_into_markup() {
    assert_eq!(strip("&lt;b&gt;not bold&lt;/b&gt;"), "<b>not bold</b>");
    assert_eq!(strip("&amp;amp;"), "&amp;");
  }

  #[test]
  fn leaves_plain_bodies_untouched() {
    assert_eq!(strip("Gruppenfotos starten um 16h"), "Gruppenfotos starten um 16h");
    assert_eq!(strip(""), "");
    assert_eq!(strip("line one\nline two"), "line one\nline two");
  }
}
