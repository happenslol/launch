pub mod panels;

use crate::launcher::RootItem;

pub fn get_items() -> Vec<RootItem> {
  panels::get_items()
}
