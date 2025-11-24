mod sinks;

use std::sync::Arc;

use anyhow::Result;

use crate::{
  audio::sections::sinks::AudioSinksSection,
  launcher::{Item, ItemAction},
};

pub fn get_items() -> Result<Vec<Item>> {
  Ok(vec![Item {
    name: "sinks".into(),
    terms: vec!["sinks".into(), "audio".into(), "volume".into()],
    action: ItemAction::Section(Arc::new(AudioSinksSection::view)),
  }])
}
