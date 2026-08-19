//! The screen that asks for a password, shared by the lock screen and the
//! greeter.
//!
//! Both are the same surface: a dark backdrop, a centred column with an avatar,
//! a name, a masked field flanked by a status icon and a fingerprint indicator,
//! and whatever the last attempt had to say. What differs is where the state
//! comes from and what a success does, so this module holds no state of its own
//! - the screens hand it a snapshot and it draws.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpui::{
  AnyElement, App, Div, Entity, EntityId, FontWeight, ImageSource, IntoElement, ObjectFit, Pixels,
  Rems, Resource, SharedString, Styled, Window, div, img, prelude::*, px, rems, rgb, rgba,
};

use uzers::os::unix::UserExt as _;

use crate::config::config_dir;
use crate::icon::{Icon, IconName, Spinner};
use crate::input::input;
use crate::input::state::InputState;
use crate::util::{h_flex, v_flex};

pub const AVATAR_SIZE: Pixels = px(104.);

/// How large the fallback initial is relative to the circle around it, so a
/// small chip in a user switcher fills the same way the full-size portrait
/// does.
const AVATAR_INITIAL_RATIO: f32 = 40. / 104.;

/// The password field and the icons that flank it. The icons are sized in rems
/// so they track the text.
const FIELD_TEXT_SIZE: Pixels = px(20.);
const FIELD_ICON_SIZE: Rems = rems(1.4);

/// How far the password field fades while it takes no input.
const FIELD_DISABLED_OPACITY: f32 = 0.6;

/// The colour a turned-down attempt is reported in, as the message and as the
/// outline of the field it came from.
const ERROR_COLOR: u32 = 0xE07070;

/// The near-black the prompt is drawn on. The lock screen paints the outputs
/// that carry no prompt with it too, so the desktop is covered everywhere rather
/// than showing next to the screen that hides it.
pub const BACKDROP: u32 = 0x0D0D0D;

/// How long the field stays outlined after an attempt is turned down. Long
/// enough to be noticed, short enough to be gone by the time the next one is
/// typed.
pub const REJECTION_HIGHLIGHT: Duration = Duration::from_secs(3);

/// Who is being authenticated, and what to show for them.
#[derive(Clone)]
pub struct AuthUser {
  /// Login name, i.e. what PAM verifies against.
  pub name: SharedString,
  pub display_name: SharedString,
  /// First letter of the display name, for when there is no avatar to show.
  pub initial: SharedString,
  /// Held as an `Arc<Path>` because that is what the image source wants, and
  /// this is cloned on every repaint.
  pub avatar: Option<Arc<Path>>,
}

impl AuthUser {
  /// Builds a user from a passwd entry. The naming rules live in `greet-ipc` so
  /// the daemon, which resolves users the greeter can't see, agrees with what
  /// is drawn here.
  pub fn from_passwd(name: String, gecos: &str, avatar: Option<PathBuf>) -> Self {
    let display_name = greet_ipc::user::display_name(gecos, &name).to_owned();
    let initial = greet_ipc::user::initial(&display_name);

    Self {
      name: name.into(),
      display_name: display_name.into(),
      initial: initial.into(),
      avatar: avatar.map(Arc::from),
    }
  }
}

impl AuthUser {
  /// Builds a user from what the login daemon reported.
  ///
  /// The display name arrives already resolved, because the daemon is the only
  /// side that can read the passwd entry for accounts other than its own.
  pub fn from_ipc(user: greet_ipc::IpcUser) -> Self {
    let initial = greet_ipc::user::initial(&user.display_name);

    Self {
      name: user.name.into(),
      display_name: user.display_name.into(),
      initial: initial.into(),
      avatar: user.avatar.map(Arc::from),
    }
  }
}

/// What the fingerprint reader is up to, so the password field can show it.
///
/// The lock screen drives fprintd itself and sets this directly; the greeter is
/// told over IPC by the daemon, which is the only side with the privileges to
/// ask.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FingerprintState {
  /// There is no reader to use, or it gave up.
  Off,
  /// Looking the reader up and claiming it.
  Starting,
  /// Armed, with nothing on the sensor.
  Waiting,
  /// A finger is on the sensor and being read.
  Reading,
}

impl From<greet_ipc::FingerprintState> for FingerprintState {
  fn from(state: greet_ipc::FingerprintState) -> Self {
    match state {
      greet_ipc::FingerprintState::Off => Self::Off,
      greet_ipc::FingerprintState::Starting => Self::Starting,
      greet_ipc::FingerprintState::Waiting => Self::Waiting,
      greet_ipc::FingerprintState::Reading => Self::Reading,
    }
  }
}

/// Keeps the surfaces of one screen in step with each other.
///
/// Emitted by the state entity and applied by every surface except the one it
/// came from. Mostly for the lock screen: every one of its surfaces takes
/// typing, wherever the compositor happened to put the keyboard, while only one
/// of them shows what is being typed. The greeter has a single field and uses
/// this only to have it emptied.
pub struct PasswordMirror {
  /// The value every other surface should show.
  pub value: SharedString,
  /// The surface it was typed on, or `None` to clear every surface including
  /// whichever asked.
  pub source: Option<EntityId>,
}

/// One surface's worth of state, as plain data.
pub struct AuthPrompt<'a> {
  pub user: &'a AuthUser,
  pub password: &'a Entity<InputState>,
  /// Something is in flight: the field is frozen and its icon spins.
  pub busy: bool,
  pub fingerprint: FingerprintState,
  /// An attempt was just turned down, which the field is outlined for.
  pub rejected: bool,
  /// Whatever PAM had to say during an attempt.
  pub hint: Option<SharedString>,
  pub error: Option<SharedString>,
  /// A spinner and a line of text under the field, for a screen waiting on
  /// something other than the password. The lock screen has nothing to wait on
  /// and passes `None`.
  pub status: Option<SharedString>,
  /// Drawn at the bottom, for what one screen has and the other doesn't: the
  /// greeter's user switcher.
  pub below: Option<AnyElement>,
}

/// The backdrop and the centred column.
///
/// Returns a [`Div`] rather than an opaque element so the caller can hang its
/// own click handler and clock off it:
///
/// ```ignore
/// render_auth_screen(prompt)
///   .on_mouse_down(MouseButton::Left, cx.listener(..))
///   .when(self.clock, |this| this.child(clock))
/// ```
pub fn render_auth_screen(prompt: AuthPrompt<'_>) -> Div {
  let AuthPrompt {
    user,
    password,
    busy,
    fingerprint,
    rejected,
    hint,
    error,
    status,
    below,
  } = prompt;

  let display_name = user.display_name.clone();
  let username = user.name.clone();

  div()
    .size_full()
    .relative()
    .flex()
    .flex_col()
    .items_center()
    .justify_center()
    .bg(rgb(BACKDROP))
    .text_color(rgb(0xFFFFFF))
    .child(
      v_flex()
        .items_center()
        .gap_4()
        .w(px(360.))
        .child(render_avatar(
          user.avatar.as_ref(),
          user.initial.clone(),
          AVATAR_SIZE,
        ))
        .child(
          v_flex()
            .items_center()
            .gap_1()
            .child(
              div()
                .text_size(px(22.))
                .font_weight(FontWeight::BOLD)
                .child(display_name.clone()),
            )
            .when(display_name != username, |this| {
              this.child(div().text_sm().text_color(rgba(0xFFFFFFAA)).child(username))
            }),
        )
        // The field sits a little further from the name than the rest of the
        // column is spaced, so the two don't read as one block.
        .child(render_password(password, busy, fingerprint, rejected).mt_2())
        .when_some(status, |this, status| {
          this.child(hint_row(
            Spinner::new()
              .size(rems(0.95))
              .color(rgba(0xFFFFFFAA).into())
              .into_any_element(),
            status,
          ))
        })
        .when_some(hint, |this, hint| {
          this.child(hint_row(
            Icon::new(IconName::Asterisk)
              .size(rems(0.95))
              .text_color(rgba(0xFFFFFF88))
              .into_any_element(),
            hint,
          ))
        })
        .when_some(error, |this, error| {
          this.child(div().text_sm().text_color(rgb(ERROR_COLOR)).child(error))
        })
        .when_some(below, |this, below| this.child(below)),
    )
}

/// The password field stays mounted while an attempt is in flight, so it keeps
/// keyboard focus and holds on to what was typed; only the leading icon turns
/// into a spinner.
///
/// Typing is off while either check is mid-flight, since a rejected attempt
/// empties the field and would take anything typed since with it.
fn render_password(
  password: &Entity<InputState>,
  busy: bool,
  fingerprint: FingerprintState,
  rejected: bool,
) -> Div {
  let leading = if busy {
    Spinner::new()
      .size(FIELD_ICON_SIZE)
      .color(rgb(0x888888).into())
      .into_any_element()
  } else {
    Icon::new(IconName::Lock)
      .size(FIELD_ICON_SIZE)
      .text_color(rgba(0xFFFFFF66))
      .into_any_element()
  };

  let disabled = field_disabled(busy, fingerprint);

  h_flex()
    .w_full()
    .gap_3()
    .px_4()
    .py_3()
    .text_size(FIELD_TEXT_SIZE)
    .rounded_lg()
    .bg(rgba(0xFFFFFF0F))
    .border_1()
    .map(|this| match rejected {
      true => this.border_color(rgb(ERROR_COLOR)),
      false => this.border_color(rgba(0xFFFFFF1F)),
    })
    .when(disabled, |this| this.opacity(FIELD_DISABLED_OPACITY))
    .child(leading)
    .child(input(password).flex_grow().disabled(disabled))
    .when_some(render_fingerprint(fingerprint), |this, indicator| {
      this.child(indicator)
    })
}

/// Whether the field takes typing, which the copies out of sight follow too so
/// that none of them can slip something in past the one on show.
fn field_disabled(busy: bool, fingerprint: FingerprintState) -> bool {
  busy || fingerprint == FingerprintState::Reading
}

/// The same field with nothing drawn: clipped away to no height in the corner of
/// whatever it is put on.
///
/// For the lock screen's other outputs. Which lock surface holds the keyboard is
/// the compositor's to decide - niri gives it to the output under the pointer -
/// so every one of them has to be able to take a password, while only one shows
/// it. Clipped rather than left out or hidden: an element outside the tree gets
/// no keys, and `visibility` stops the paint that registers the listeners, so
/// either way there would be nothing to type into.
pub fn render_offscreen_password(
  password: &Entity<InputState>,
  busy: bool,
  fingerprint: FingerprintState,
) -> Div {
  div()
    .absolute()
    .top_0()
    .left_0()
    .w_full()
    .h_0()
    .overflow_hidden()
    .child(input(password).disabled(field_disabled(busy, fingerprint)))
}

/// The circular portrait, or the initial when there is no picture. `size` is a
/// parameter so a user switcher can draw the same thing small.
pub fn render_avatar(
  avatar: Option<&Arc<Path>>,
  initial: SharedString,
  size: Pixels,
) -> AnyElement {
  let frame = div()
    .size(size)
    .flex_none()
    .rounded_full()
    .overflow_hidden()
    .bg(rgba(0xFFFFFF14))
    .flex()
    .items_center()
    .justify_center();

  match avatar {
    Some(path) => frame
      .child(
        // The frame's `overflow_hidden` doesn't round what the image paints;
        // the radius has to be on the image itself.
        img(ImageSource::Resource(Resource::Path(path.clone())))
          .size_full()
          .rounded_full()
          .object_fit(ObjectFit::Cover),
      )
      .into_any_element(),
    None => frame
      .child(
        div()
          .text_size(size * AVATAR_INITIAL_RATIO)
          .text_color(rgba(0xFFFFFFCC))
          .child(initial),
      )
      .into_any_element(),
  }
}

/// Shows a reader that is armed, spinning while a finger is actually on the
/// sensor so an idle screen doesn't animate for hours. A reader that is still
/// starting up, or that isn't there at all, shows nothing rather than offering
/// a way in that may never work.
pub fn render_fingerprint(state: FingerprintState) -> Option<AnyElement> {
  match state {
    FingerprintState::Off | FingerprintState::Starting => None,
    FingerprintState::Waiting => Some(
      Icon::new(IconName::Fingerprint)
        .size(FIELD_ICON_SIZE)
        .text_color(rgba(0xFFFFFF66))
        .into_any_element(),
    ),
    FingerprintState::Reading => Some(
      Spinner::new()
        .size(FIELD_ICON_SIZE)
        .color(rgba(0xFFFFFFAA).into())
        .into_any_element(),
    ),
  }
}

fn hint_row(leading: AnyElement, hint: SharedString) -> impl IntoElement {
  h_flex()
    .w_full()
    .gap_2()
    .items_center()
    .text_sm()
    .text_color(rgba(0xFFFFFFAA))
    .child(leading)
    .child(div().flex_1().child(hint))
}

/// Applies a mirrored password to one surface's field, skipping the surface it
/// came from and skipping a value that is already there - which is what stops
/// the mirroring bouncing back and forth, since setting the value emits another
/// change of its own.
pub fn apply_password_mirror(
  event: &PasswordMirror,
  password: &Entity<InputState>,
  surface: EntityId,
  window: &mut Window,
  cx: &mut App,
) {
  if event.source == Some(surface) {
    return;
  }

  if password.read(cx).value() == event.value {
    return;
  }

  password.update(cx, |password, cx| {
    password.set_value(event.value.clone(), window, cx);
  });
}

/// The user running this process, for the screens that only ever authenticate
/// that one.
pub fn current_user() -> Option<AuthUser> {
  let user = uzers::get_user_by_uid(uzers::get_current_uid())?;
  let name = user.name().to_str()?.to_owned();
  let gecos = user.gecos().to_str().unwrap_or_default().to_owned();
  let avatar = find_avatar(&name);

  Some(AuthUser::from_passwd(name, &gecos, avatar))
}

/// Looks for a user picture: one dropped into the config directory first, then
/// the places desktops agree on.
pub fn find_avatar(username: &str) -> Option<PathBuf> {
  let mut candidates = Vec::new();

  if let Some(directory) = config_dir() {
    candidates.extend(
      greet_ipc::user::CONFIG_AVATAR_NAMES
        .iter()
        .map(|name| directory.join(name)),
    );
  }

  if let Some(home) = dirs::home_dir() {
    candidates.extend(
      greet_ipc::user::HOME_AVATAR_NAMES
        .iter()
        .map(|name| home.join(name)),
    );
  }

  candidates.push(PathBuf::from(greet_ipc::user::ACCOUNTS_SERVICE_ICON_DIR).join(username));

  candidates.into_iter().find(|path| path.is_file())
}
