use std::{
  collections::{BTreeMap, HashMap, HashSet},
  path::{Path, PathBuf},
  process,
};

use anyhow::Result;
use fork::Fork;
use freedesktop_desktop_entry::{DesktopEntry, Iter, default_paths};
use gpui::{App, Entity, Global, Resource, Task, prelude::*};

use crate::{db::DB, launcher::RootItem, util::ResultExt};

pub struct XdgIconCache {
  cache: HashMap<String, Resource>,
  refresh_task: Option<Task<()>>,
}

struct GlobalXdgIconCache(Entity<XdgIconCache>);

impl Global for GlobalXdgIconCache {}

pub fn init(cx: &mut App) {
  let entity = cx.new(|_| XdgIconCache {
    cache: DB.get_desktop_entry_icon_paths(),
    refresh_task: None,
  });
  cx.set_global(GlobalXdgIconCache(entity));
}

impl XdgIconCache {
  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalXdgIconCache>().0.clone()
  }

  pub fn get(&self, name: &str) -> Option<&Resource> {
    self.cache.get(name)
  }

  pub fn lookup(&mut self, items: Vec<(String, Option<String>)>, cx: &mut Context<Self>) {
    let items: Vec<(String, Option<String>)> = items
      .into_iter()
      .filter(|(name, _)| !self.cache.contains_key(name))
      .collect();

    if items.is_empty() {
      return;
    }

    cx.spawn(async move |this, cx| {
      let entries = cx
        .background_spawn(async move {
          let mut entries = HashMap::new();
          for (name, theme_path) in &items {
            if let Some(path) = get_icon(name, theme_path.as_deref()) {
              entries.insert(name.clone(), Resource::Path(path.into()));
            }
          }
          entries
        })
        .await;

      if !entries.is_empty() {
        this
          .update(cx, |this, cx| {
            this.cache.extend(entries);
            cx.notify();
          })
          .log_err();
      }
    })
    .detach();
  }

  pub fn refresh(&mut self, locales: Vec<String>, cx: &mut Context<Self>) {
    if self.refresh_task.is_some() {
      return;
    }

    self.refresh_task = Some(cx.spawn(async move |this, cx| {
      let (db_entries, cache_entries) = cx
        .background_spawn(async move {
          let entries: Vec<_> = Iter::new(default_paths()).entries(Some(&locales)).collect();

          let mut db_entries = HashMap::new();
          let mut cache_entries = HashMap::new();

          for entry in &entries {
            let Some(icon_name) = entry.icon() else {
              continue;
            };

            let Some(icon_path) = get_icon(icon_name, None) else {
              continue;
            };

            db_entries
              .entry(icon_name.to_string())
              .or_insert_with(|| icon_path.clone());

            let resource = Resource::Path(icon_path.into());

            cache_entries
              .entry(icon_name.to_string())
              .or_insert_with(|| resource.clone());

            let name = entry.name(&locales).unwrap_or_default();
            cache_entries
              .entry(name.to_lowercase())
              .or_insert_with(|| resource.clone());

            cache_entries
              .entry(entry.appid.to_lowercase())
              .or_insert_with(|| resource.clone());

            if let Some(generic_name) = entry.generic_name(&locales) {
              cache_entries
                .entry(generic_name.to_lowercase())
                .or_insert_with(|| resource.clone());
            }
          }

          (db_entries, cache_entries)
        })
        .await;

      DB.store_desktop_entry_icon_paths(&db_entries);

      this
        .update(cx, |this, cx| {
          this.cache.extend(cache_entries);
          this.refresh_task = None;
          cx.notify();
        })
        .log_err();
    }));
  }
}

pub fn get_items(locales: &[String]) -> Result<(Vec<RootItem>, Vec<String>)> {
  let desktop_entries = Iter::new(default_paths())
    .entries(Some(locales))
    .collect::<Vec<_>>();

  let mut result = BTreeMap::new();
  let mut icon_names = HashSet::new();

  for entry in desktop_entries {
    if entry.no_display() || result.contains_key(&entry.appid) {
      continue;
    }

    if let Some(icon) = entry.icon() {
      icon_names.insert(icon.to_string());
    }

    let name = entry.name(locales).unwrap().to_string();
    result.insert(
      entry.appid.clone(),
      RootItem::App {
        name: name.into(),
        entry,
      },
    );
  }

  Ok((
    result.into_values().collect(),
    icon_names.into_iter().collect(),
  ))
}

pub fn get_icon(name: &str, theme_path: Option<&str>) -> Option<PathBuf> {
  // Some apps put an absolute path in icon_name directly
  let path = Path::new(name);
  if path.is_absolute() && path.is_file() {
    return Some(path.to_path_buf());
  }

  // SNI's IconThemePath should be searched before the standard XDG dirs
  if let Some(theme_path) = theme_path
    && let Some(found) = find_in_theme_path(theme_path, name)
  {
    return Some(found);
  }

  freedesktop_icons::lookup(name)
    .with_cache()
    .with_scale(1)
    .with_size(24)
    .find()
}

fn find_in_theme_path(dir: &str, name: &str) -> Option<PathBuf> {
  // Most apps drop icons directly in the theme path, no theme structure
  for ext in ["svg", "png"] {
    let path = PathBuf::from(dir).join(format!("{name}.{ext}"));
    if path.is_file() {
      return Some(path);
    }
  }

  // Otherwise scan, preferring SVG
  let mut svg_match: Option<PathBuf> = None;
  let mut png_match: Option<PathBuf> = None;
  for entry in walkdir::WalkDir::new(dir)
    .max_depth(5)
    .into_iter()
    .filter_map(|e| e.ok())
  {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
      continue;
    };
    if stem != name {
      continue;
    }
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
      continue;
    };
    match ext {
      "svg" if svg_match.is_none() => svg_match = Some(path.to_path_buf()),
      "png" if png_match.is_none() => png_match = Some(path.to_path_buf()),
      _ => {}
    }
  }
  svg_match.or(png_match)
}

pub fn open_url(url: &str) -> Result<()> {
  if let Fork::Child = fork::fork()? {
    if fork::setsid().is_err() {
      eprintln!("Failed to setsid: {}", std::io::Error::last_os_error());
      process::exit(1);
    }
    if fork::redirect_stdio().is_err() {
      eprintln!("Failed to close_fd: {}", std::io::Error::last_os_error());
    }

    let err = exec::execvp("xdg-open", &["xdg-open", url]);
    eprintln!("Failed to exec xdg-open: {}", err);
    process::exit(1);
  }

  Ok(())
}

pub fn start(entry: &DesktopEntry) -> Result<()> {
  let cmd = entry.parse_exec()?;

  // TODO: If we want to capture child's output, we have to create pipes and set out own
  // stdin/out/err to them before forking, then set them back to our own in the parent.

  if let Fork::Child = fork::fork()? {
    if fork::setsid().is_err() {
      eprintln!("Failed to setsid: {}", std::io::Error::last_os_error());
      process::exit(1);
    }
    if fork::redirect_stdio().is_err() {
      eprintln!("Failed to close_fd: {}", std::io::Error::last_os_error());
    }

    let err = exec::execvp(&cmd[0], &cmd);
    eprintln!("Failed to exec {:?}: {}", cmd, err);
    process::exit(1);
  }

  Ok(())
}
