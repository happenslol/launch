//! Naming and avatar conventions shared by the daemon and the greeter.
//!
//! The daemon resolves these as root and the greeter renders the result, so a
//! disagreement between the two shows up as a login screen that quietly draws
//! the wrong thing. Keeping them here means there is only one definition to
//! disagree with.

/// Avatar filenames under a user's `~/.config/launch`, in the order the first
/// match wins. A user with several only ever sees one of them.
pub const CONFIG_AVATAR_NAMES: &[&str] = &["profile.png", "profile.jpg", "profile.webp"];

/// Avatar filenames directly under a user's home, searched after the config
/// directory. These are the long-standing conventions other login screens read.
pub const HOME_AVATAR_NAMES: &[&str] = &[".face", ".face.icon"];

/// The directory AccountsService keeps its copies in, searched last.
pub const ACCOUNTS_SERVICE_ICON_DIR: &str = "/var/lib/AccountsService/icons";

/// The name to show for a user.
///
/// The GECOS field is comma-separated, and only the first part is the full
/// name; the rest is office numbers and phone numbers nobody wants on a login
/// screen. An empty or absent full name falls back to the login name, which is
/// always there.
pub fn display_name<'a>(gecos: &'a str, login: &'a str) -> &'a str {
  let full_name = gecos.split(',').next().unwrap_or("").trim();
  if full_name.is_empty() {
    login
  } else {
    full_name
  }
}

/// The letter drawn in place of a missing avatar.
///
/// Takes the first grapheme-ish character rather than the first byte so a name
/// starting with a multi-byte character doesn't produce mojibake, and
/// uppercases it because the source is whatever the user typed into GECOS.
pub fn initial(display_name: &str) -> String {
  display_name
    .chars()
    .next()
    .map(|character| character.to_uppercase().to_string())
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn display_name_prefers_the_full_name() {
    assert_eq!(display_name("Ada Lovelace,,,", "ada"), "Ada Lovelace");
  }

  #[test]
  fn display_name_falls_back_to_the_login() {
    assert_eq!(display_name("", "ada"), "ada");
    assert_eq!(display_name(",,,", "ada"), "ada");
    assert_eq!(display_name("   ,x", "ada"), "ada");
  }

  #[test]
  fn initial_uppercases_and_handles_multibyte() {
    assert_eq!(initial("ada"), "A");
    assert_eq!(initial("Ölbaum"), "Ö");
    assert_eq!(initial(""), "");
  }
}
