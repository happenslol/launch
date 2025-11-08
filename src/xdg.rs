use anyhow::Result;
use freedesktop_file_parser::DesktopEntry;
use gpui::SharedString;

use crate::{ItemAction, Item};

pub fn get_items() -> Result<Vec<Item>> {
  let desktop_entries = find_all_desktop_entries()?;
  let items = desktop_entries
    .iter()
    .map(|entry| Item {
      name: SharedString::from(entry.name.default.clone()),
      action: ItemAction::Launch(entry.clone()),
    })
    .collect();

  Ok(items)
}

fn find_all_desktop_entries() -> Result<Vec<DesktopEntry>> {
  let data_dirs = std::env::var("XDG_DATA_DIRS").ok().map(|p| {
    std::env::split_paths(&p)
      .filter(|p| p.is_absolute())
      .collect::<Vec<_>>()
  });

  let app_dirs = data_dirs.map(|dirs| {
    dirs
      .iter()
      .map(|dir| dir.join("applications"))
      .filter(|dir| dir.try_exists().unwrap_or_default())
      .collect::<Vec<_>>()
  });

  let Some(app_dirs) = app_dirs else {
    return Ok(Vec::new());
  };

  let mut applications = Vec::new();

  for dir in app_dirs {
    let walker = walkdir::WalkDir::new(&dir).into_iter();

    // TODO: Deduplicate by dektop id
    for entry in walker.filter_entry(|e| {
      e.file_type().is_dir()
        || e
          .file_name()
          .to_str()
          .is_some_and(|s| s.ends_with(".desktop"))
    }) {
      let Ok(entry) = entry else {
        continue;
      };

      if entry.file_type().is_dir() {
        continue;
      }

      let contents = std::fs::read_to_string(entry.path())?;
      let parsed = freedesktop_file_parser::parse(&contents)?;
      applications.push(parsed.entry);
    }
  }

  Ok(applications)
}
