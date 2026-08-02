//! Copies user avatars somewhere the greeter can read them.
//!
//! The greeter runs as its own system user and cannot read anyone's home
//! directory, so it can't find `~/.face` itself. The daemon can, and publishes
//! a copy into the greeter's own home.
//!
//! That makes this the one part of the login manager that reads arbitrary files
//! as root and republishes them where another user can read them - the exact
//! shape of a file disclosure bug. A user who points `~/.face` at
//! `/etc/shadow` must not get it copied somewhere the greeter can read, so
//! every candidate is opened without following a final symlink and then checked
//! to be a regular file, owned by whoever controls the directory it came from,
//! small enough, and actually an image.

use std::collections::HashMap;
use std::io::Read as _;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use nix::fcntl::{OFlag, open};
use nix::sys::stat::{Mode, SFlag, fstat};
use nix::unistd::{Gid, Uid, chown};
use tracing::{debug, info, warn};

use crate::users::Account;

/// Subdirectory of the greeter's home the copies live in.
const AVATAR_DIR: &str = "avatars";

/// Largest avatar that will be copied. Well past any plausible portrait, and
/// the point is to bound the work rather than to be a style guide.
const MAX_AVATAR_BYTES: u64 = 4 * 1024 * 1024;

/// Image formats the greeter can decode, with the bytes that identify them.
///
/// Sniffed rather than trusted from the extension: the file name is chosen by
/// whoever owns the home directory.
#[derive(Debug, Clone, Copy)]
enum ImageFormat {
  Png,
  Jpeg,
  WebP,
}

impl ImageFormat {
  fn detect(bytes: &[u8]) -> Option<Self> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
      return Some(Self::Png);
    }

    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
      return Some(Self::Jpeg);
    }

    // RIFF container with a WEBP fourcc at offset 8.
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
      return Some(Self::WebP);
    }

    None
  }

  fn extension(self) -> &'static str {
    match self {
      Self::Png => "png",
      Self::Jpeg => "jpg",
      Self::WebP => "webp",
    }
  }
}

/// Where a candidate came from, which decides who is allowed to own it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
  /// Inside the user's home. Must be owned by that user - anything else there
  /// is either a mistake or an attempt to have us read a file they can't.
  Home,
  /// A system directory. Must be owned by root, for the same reason in reverse:
  /// a user-owned file there would mean the directory is already compromised.
  System,
}

/// Refreshes every avatar and drops the ones that no longer apply, returning
/// where each user's portrait ended up.
///
/// Failures are per-user and never fatal: a login screen without a portrait is
/// fine, one that refuses to start is not.
pub fn sync(
  accounts: &[Account],
  greeter_home: &Path,
  uid: Uid,
  gid: Gid,
) -> Result<HashMap<String, PathBuf>> {
  let directory = greeter_home.join(AVATAR_DIR);

  std::fs::create_dir_all(&directory)
    .with_context(|| format!("creating {}", directory.display()))?;

  chown(&directory, Some(uid), Some(gid))
    .with_context(|| format!("handing {} to the greeter", directory.display()))?;

  let mut published = HashMap::new();

  for account in accounts {
    match publish(account, &directory) {
      Ok(Some(path)) => {
        published.insert(account.name.clone(), path);
      }
      Ok(None) => debug!(user = account.name, "No avatar found"),
      Err(error) => warn!(user = account.name, ?error, "Could not publish an avatar"),
    }
  }

  if let Err(error) = prune(&directory, &published) {
    warn!(?error, "Could not remove stale avatars");
  }

  Ok(published)
}

/// Copies one user's avatar, returning where it landed.
fn publish(account: &Account, directory: &Path) -> Result<Option<PathBuf>> {
  let owner = Uid::from_raw(account.uid);

  for (candidate, origin) in candidates(account) {
    let bytes = match read_image(&candidate, origin, owner) {
      Ok(Some(bytes)) => bytes,
      Ok(None) => continue,
      Err(error) => {
        // Worth saying out loud: this is where a rejected symlink or a
        // wrong-owner file shows up.
        warn!(path = %candidate.display(), ?error, "Rejected an avatar");
        continue;
      }
    };

    let Some(format) = ImageFormat::detect(&bytes) else {
      warn!(path = %candidate.display(), "Rejected an avatar that is not an image");
      continue;
    };

    let destination = directory.join(format!("{}.{}", account.name, format.extension()));
    write_atomically(&destination, &bytes)?;

    info!(user = account.name, path = %destination.display(), "Published an avatar");
    return Ok(Some(destination));
  }

  Ok(None)
}

/// Where a user's avatar might be, in the order the first match wins. Mirrors
/// what the lock screen looks through for the logged-in user.
fn candidates(account: &Account) -> Vec<(PathBuf, Origin)> {
  let mut candidates = Vec::new();

  let config = account.home.join(".config").join("launch");
  candidates.extend(
    greet_ipc::user::CONFIG_AVATAR_NAMES
      .iter()
      .map(|name| (config.join(name), Origin::Home)),
  );

  candidates.extend(
    greet_ipc::user::HOME_AVATAR_NAMES
      .iter()
      .map(|name| (account.home.join(name), Origin::Home)),
  );

  candidates.push((
    Path::new(greet_ipc::user::ACCOUNTS_SERVICE_ICON_DIR).join(&account.name),
    Origin::System,
  ));

  candidates
}

/// Opens a candidate and reads it, if it passes every check.
///
/// `Ok(None)` means there was nothing there; an error means there was something
/// and it was refused.
fn read_image(path: &Path, origin: Origin, owner: Uid) -> Result<Option<Vec<u8>>> {
  // O_NOFOLLOW refuses a symlink as the final component, which is the case that
  // matters: it is the one an attacker controls the name of. An intermediate
  // symlink can still redirect the lookup, which is what the ownership check
  // below is for.
  let file = match open(
    path,
    OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
    Mode::empty(),
  ) {
    Ok(file) => file,
    Err(nix::errno::Errno::ENOENT) => return Ok(None),
    Err(nix::errno::Errno::ELOOP) => bail!("is a symlink"),
    Err(error) => return Err(error).context("opening the file"),
  };

  let stat = fstat(file.as_fd()).context("inspecting the file")?;

  // O_NOFOLLOW doesn't stop a fifo or a device node, and reading either can
  // block forever or have side effects.
  if SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT != SFlag::S_IFREG {
    bail!("is not a regular file");
  }

  let expected = match origin {
    Origin::Home => owner,
    Origin::System => Uid::from_raw(0),
  };

  // The check that makes an intermediate symlink harmless: whatever the lookup
  // landed on has to belong to whoever controls the directory it was found in.
  if stat.st_uid != expected.as_raw() {
    bail!(
      "is owned by uid {} rather than {}",
      stat.st_uid,
      expected.as_raw()
    );
  }

  if stat.st_size < 0 || stat.st_size as u64 > MAX_AVATAR_BYTES {
    bail!(
      "is {} bytes, over the {MAX_AVATAR_BYTES} byte limit",
      stat.st_size
    );
  }

  // Bounded by the size seen above plus a byte, so a file growing between the
  // stat and the read can't make this unbounded.
  let mut bytes = Vec::with_capacity(stat.st_size as usize);
  std::fs::File::from(file)
    .take(MAX_AVATAR_BYTES + 1)
    .read_to_end(&mut bytes)
    .context("reading the file")?;

  if bytes.len() as u64 > MAX_AVATAR_BYTES {
    bail!("grew past the {MAX_AVATAR_BYTES} byte limit while being read");
  }

  Ok(Some(bytes))
}

/// Writes next to the destination and renames over it, so the greeter never
/// sees a half-written image.
fn write_atomically(destination: &Path, bytes: &[u8]) -> Result<()> {
  let temporary = destination.with_extension("tmp");

  std::fs::write(&temporary, bytes).with_context(|| format!("writing {}", temporary.display()))?;

  // World-readable on purpose: the greeter has to read it, and it is a
  // portrait that is about to be shown on the login screen anyway.
  std::fs::set_permissions(
    &temporary,
    std::os::unix::fs::PermissionsExt::from_mode(0o644),
  )
  .with_context(|| format!("setting the mode on {}", temporary.display()))?;

  std::fs::rename(&temporary, destination)
    .with_context(|| format!("renaming into {}", destination.display()))?;

  Ok(())
}

/// Removes copies for users that are gone, or in a format they no longer use.
/// Without this a deleted account keeps a face on the login screen.
fn prune(directory: &Path, published: &HashMap<String, PathBuf>) -> Result<()> {
  for entry in std::fs::read_dir(directory).context("listing published avatars")? {
    let path = entry.context("listing published avatars")?.path();

    if published.values().any(|kept| kept == &path) {
      continue;
    }

    debug!(path = %path.display(), "Removing a stale avatar");
    if let Err(error) = std::fs::remove_file(&path) {
      warn!(path = %path.display(), ?error, "Could not remove a stale avatar");
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

  #[test]
  fn detects_the_formats_the_greeter_can_draw() {
    assert!(matches!(ImageFormat::detect(PNG), Some(ImageFormat::Png)));
    assert!(matches!(
      ImageFormat::detect(&[0xFF, 0xD8, 0xFF, 0xE0]),
      Some(ImageFormat::Jpeg)
    ));
    assert!(matches!(
      ImageFormat::detect(b"RIFF\0\0\0\0WEBPVP8 "),
      Some(ImageFormat::WebP)
    ));
  }

  #[test]
  fn rejects_things_that_are_not_images() {
    assert!(ImageFormat::detect(b"root:x:0:0:").is_none());
    assert!(ImageFormat::detect(b"").is_none());
    assert!(ImageFormat::detect(b"RIFF\0\0\0\0AVI ").is_none());
    // A truncated PNG signature is not a PNG.
    assert!(ImageFormat::detect(&PNG[..4]).is_none());
  }

  #[test]
  fn refuses_a_symlink_as_the_final_component() {
    let directory = std::env::temp_dir().join(format!("launch-avatar-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("creates the fixture");

    let target = directory.join("secret");
    std::fs::write(&target, b"pretend this is /etc/shadow").expect("writes the target");

    let link = directory.join(".face");
    std::os::unix::fs::symlink(&target, &link).expect("creates the symlink");

    let error = read_image(&link, Origin::Home, Uid::current()).expect_err("is refused");
    assert!(
      error.to_string().contains("symlink"),
      "unexpected error: {error}"
    );

    std::fs::remove_dir_all(&directory).ok();
  }

  #[test]
  fn refuses_a_file_owned_by_someone_else() {
    let directory = std::env::temp_dir().join(format!("launch-owner-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("creates the fixture");

    let path = directory.join(".face");
    std::fs::write(&path, PNG).expect("writes the file");

    // Claiming it should belong to a different uid is the same check that
    // catches an intermediate symlink into a root-owned directory.
    let other = Uid::from_raw(Uid::current().as_raw().wrapping_add(1));
    let error = read_image(&path, Origin::Home, other).expect_err("is refused");
    assert!(
      error.to_string().contains("owned by uid"),
      "unexpected error: {error}"
    );

    // The same file passes once the expected owner matches.
    let bytes = read_image(&path, Origin::Home, Uid::current())
      .expect("is accepted")
      .expect("has contents");
    assert_eq!(bytes, PNG);

    std::fs::remove_dir_all(&directory).ok();
  }

  #[test]
  fn reports_a_missing_file_as_absent_rather_than_an_error() {
    let missing = std::env::temp_dir()
      .join("launch-definitely-not-here")
      .join(".face");
    assert!(
      read_image(&missing, Origin::Home, Uid::current())
        .expect("is not an error")
        .is_none()
    );
  }
}
