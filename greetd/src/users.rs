//! Which accounts the login screen offers.
//!
//! The greeter runs as its own unprivileged user and could read `/etc/passwd`
//! itself, but the filtering rules live here so that the account list and the
//! avatars synced for it are decided in one place, by the side that can see
//! every home directory.

use std::collections::HashMap;
use std::path::PathBuf;

use greet_ipc::IpcUser;
use tracing::debug;
use uzers::os::unix::UserExt as _;

use crate::config::UsersConfig;

/// Login shells that mean the account is not for logging in.
const NOLOGIN_SHELLS: &[&str] = &["/bin/false", "/usr/bin/false", "/sbin/nologin"];

pub struct Account {
  pub name: String,
  /// Raw GECOS field; the display name is derived from it by `greet_ipc::user`
  /// so the greeter and the daemon agree.
  pub gecos: String,
  pub uid: u32,
  pub home: PathBuf,
}

/// Every account the greeter should offer, sorted by name so the list doesn't
/// reshuffle between restarts.
pub fn list(config: &UsersConfig) -> Vec<Account> {
  // SAFETY: uzers walks the process-global passwd handle, which is only sound
  // while nothing else touches it. The daemon is single-threaded by
  // construction (see main.rs), and this runs before any worker is forked.
  let users = unsafe { uzers::all_users() };

  let mut accounts: Vec<Account> = users
    .filter(|user| {
      let name = user.name().to_string_lossy();

      // An explicit exclusion always wins, so a machine can hide an account
      // that would otherwise qualify on every other ground.
      if config.exclude.iter().any(|excluded| excluded == &*name) {
        return false;
      }

      if config.include.iter().any(|included| included == &*name) {
        return true;
      }

      if user.uid() < config.minimum_uid || user.uid() > config.maximum_uid {
        return false;
      }

      let shell = user.shell().to_string_lossy();
      if NOLOGIN_SHELLS
        .iter()
        .any(|nologin| shell.ends_with(nologin))
      {
        debug!(user = %name, %shell, "Skipping an account that cannot log in");
        return false;
      }

      true
    })
    .map(|user| Account {
      name: user.name().to_string_lossy().into_owned(),
      gecos: user.gecos().to_string_lossy().into_owned(),
      uid: user.uid(),
      home: user.home_dir().to_path_buf(),
    })
    .collect();

  accounts.sort_by(|left, right| left.name.cmp(&right.name));
  accounts.dedup_by(|left, right| left.name == right.name);
  accounts
}

/// Turns the accounts into what the greeter is told about them.
///
/// The display name is derived by `greet_ipc::user` rather than here, so the
/// lock screen and the login screen spell the same person's name the same way.
pub fn to_ipc(accounts: &[Account], avatars: &HashMap<String, PathBuf>) -> Vec<IpcUser> {
  accounts
    .iter()
    .map(|account| IpcUser {
      display_name: greet_ipc::user::display_name(&account.gecos, &account.name).to_owned(),
      avatar: avatars.get(&account.name).cloned(),
      name: account.name.clone(),
    })
    .collect()
}
