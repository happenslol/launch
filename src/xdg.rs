use std::{
  collections::{BTreeMap, HashMap, HashSet},
  path::PathBuf,
  process,
};

use anyhow::Result;
use fork::Fork;
use freedesktop_desktop_entry::{DesktopEntry, Iter, default_paths};
use futures::{StreamExt as _, stream::FuturesUnordered};
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

  pub fn refresh(&mut self, icon_names: Vec<String>, cx: &mut Context<Self>) {
    if self.refresh_task.is_some() {
      return;
    }

    self.refresh_task = Some(cx.spawn(async move |this, cx| {
      let result = FuturesUnordered::from_iter(icon_names.chunks(10).map(|names| {
        let names = names.to_vec();
        cx.background_spawn(async move {
          names
            .iter()
            .filter_map(|name| get_icon(name).map(|icon| (name.clone(), icon)))
            .collect::<HashMap<_, _>>()
        })
      }))
      .collect::<Vec<_>>()
      .await
      .into_iter()
      .flatten()
      .collect::<HashMap<_, _>>();

      DB.store_desktop_entry_icon_paths(&result);

      let result = result
        .into_iter()
        .map(|(k, v)| (k, Resource::Path(v.into())))
        .collect::<HashMap<_, _>>();

      this
        .update(cx, |this, cx| {
          this.cache.extend(result);
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

pub fn get_icon(name: &str) -> Option<PathBuf> {
  let scale = Some(1);
  let size = Some(24);

  let mut lookup = freedesktop_icons::lookup(name).force_svg().with_cache();

  if let Some(scale) = scale {
    lookup = lookup.with_scale(scale);
  }

  if let Some(size) = size {
    lookup = lookup.with_size(size);
  }

  lookup.find()
}

pub fn get_icons_by_app_name(locales: &[String]) -> HashMap<String, Resource> {
  let entries: Vec<_> = Iter::new(default_paths()).entries(Some(locales)).collect();

  let mut result = HashMap::new();

  for entry in entries {
    let name = entry.name(locales).unwrap_or_default();
    if let Some(icon_name) = entry.icon() {
      if let Some(icon_path) = get_icon(icon_name) {
        let resource = Resource::Path(icon_path.into());

        let name_lower = name.to_lowercase();
        result.entry(name_lower).or_insert_with(|| resource.clone());

        let appid_lower = entry.appid.to_lowercase();
        result
          .entry(appid_lower)
          .or_insert_with(|| resource.clone());

        if let Some(generic_name) = entry.generic_name(locales) {
          let generic_lower = generic_name.to_lowercase();
          result
            .entry(generic_lower)
            .or_insert_with(|| resource.clone());
        }
      }
    }
  }

  result
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
