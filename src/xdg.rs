use anyhow::Result;
use fork::Fork;
use freedesktop_desktop_entry::{DesktopEntry, Iter, default_paths, get_languages_from_env};
use gpui::SharedString;
use tracing::error;

use crate::launcher::{Item, ItemAction};

pub fn get_items() -> Result<Vec<Item>> {
  let locales = get_languages_from_env();
  let desktop_entries = Iter::new(default_paths())
    .entries(Some(&locales))
    .collect::<Vec<_>>();

  let items = desktop_entries
    .iter()
    .map(|entry| Item {
      name: SharedString::from(entry.name(&locales).unwrap().to_string()),
      action: ItemAction::Launch(Box::new(entry.clone())),
    })
    .collect();

  Ok(items)
}

pub fn start(entry: &DesktopEntry) -> Result<()> {
  let cmd = entry.parse_exec()?;

  // TODO: If we want to capture the child's output, we have to create pipes and set out own
  // stdin/out/err to them before forking, then set them back to our own in the parent.

  if let Fork::Child = fork::fork()? {
    let err = exec::execvp(&cmd[0], &cmd);
    error!(?err, ?cmd, "child: Failed to execvp process");
    std::process::exit(1);
  }

  Ok(())
}
