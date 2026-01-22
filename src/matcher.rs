use std::sync::Arc;

use anyhow::Result;
use deadpool::managed::{self, Manager, RecycleError};
use gpui::{App, Global};
use nucleo_matcher::{Config, Matcher};

pub struct MatcherPool;

pub fn init(cx: &mut App) {
  cx.set_global(MatcherPoolGlobal(Arc::new(
    deadpool::managed::Pool::builder(MatcherPool)
      .max_size(5)
      .build()
      .expect("Failed to create matcher pool"),
  )));
}

impl Manager for MatcherPool {
  type Type = Matcher;
  type Error = ();

  async fn create(&self) -> Result<Matcher, ()> {
    let matcher = Matcher::new(Config::DEFAULT);
    Ok(matcher)
  }

  async fn recycle(
    &self,
    _: &mut Matcher,
    _: &deadpool::managed::Metrics,
  ) -> Result<(), RecycleError<()>> {
    Ok(())
  }
}

pub struct MatcherPoolGlobal(Arc<deadpool::managed::Pool<MatcherPool>>);

impl Global for MatcherPoolGlobal {}

impl MatcherPool {
  pub fn global(cx: &App) -> Arc<managed::Pool<MatcherPool>> {
    cx.global::<MatcherPoolGlobal>().0.clone()
  }
}
