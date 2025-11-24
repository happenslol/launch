use std::collections::{BTreeMap, HashSet};

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

  let mut result = BTreeMap::new();
  for entry in desktop_entries {
    if entry.no_display() || result.contains_key(&entry.appid) {
      continue;
    }

    let name = entry.name(&locales).unwrap().to_string();

    let mut terms = HashSet::new();
    terms.insert(name.clone());
    terms.insert(entry.appid.clone());

    if let Some(generic_name) = entry.generic_name(&locales) {
      terms.insert(generic_name.to_string());
    }

    if let Some(categories) = entry.categories() {
      terms.extend(categories.iter().map(|cat| cat.to_string()));
    }

    if let Some(keywords) = entry.keywords(&locales) {
      terms.extend(keywords.iter().map(|kw| kw.to_string()));
    }

    result.insert(
      entry.appid.clone(),
      Item {
        name: SharedString::from(name),
        terms: terms.into_iter().collect(),
        action: ItemAction::Launch(Box::new(entry.clone())),
      },
    );
  }

  Ok(result.into_values().collect())
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
