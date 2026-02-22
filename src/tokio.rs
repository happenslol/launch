use gpui::{App, Global};
use tokio::runtime::Runtime;

struct GlobalTokioRuntime(Runtime);

impl Global for GlobalTokioRuntime {}

pub fn init(cx: &mut App) {
  let rt = tokio::runtime::Builder::new_multi_thread()
    .thread_name("launch-tokio")
    .enable_all()
    .build()
    .unwrap();

  cx.set_global(GlobalTokioRuntime(rt));
}

pub trait TokioExt {
  fn spawn_tokio<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
  where
    F: Future + Send + 'static,
    F::Output: Send + 'static;
}

impl TokioExt for App {
  fn spawn_tokio<F>(&self, fut: F) -> tokio::task::JoinHandle<F::Output>
  where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
  {
    self.global::<GlobalTokioRuntime>().0.spawn(fut)
  }
}
