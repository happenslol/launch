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
    action: ItemAction::Section(Arc::new(AudioSinksSection::view)),
  }])
}
