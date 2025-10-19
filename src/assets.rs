use std::borrow::Cow;

use anyhow::{Context, Result};
use gpui::{App, AssetSource, SharedString};
use include_fs::{IncludeFs, include_fs};

static ASSETS: IncludeFs = include_fs!("assets");

pub struct Assets;

impl AssetSource for Assets {
  fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
    ASSETS
      .get(path)
      .with_context(|| format!("Failed to load asset '{path}'"))
      .map(|bytes| Some(Cow::from(bytes)))
  }

  fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
    Ok(
      ASSETS
        .list_paths()
        .iter()
        .filter(|p| p.trim_start_matches("assets/").starts_with(path))
        .map(|p| SharedString::new_static(p))
        .collect(),
    )
  }
}

pub fn load_embedded_fonts(cx: &App) -> Result<()> {
  let font_paths = cx.asset_source().list("fonts")?;
  let mut embedded_fonts = Vec::new();
  for font_path in font_paths {
    if !font_path.ends_with(".ttf") {
      continue;
    }

    let font_bytes = cx.asset_source().load(&font_path)?.unwrap();
    embedded_fonts.push(font_bytes);
  }

  cx.text_system().add_fonts(embedded_fonts)
}
