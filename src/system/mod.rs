//! The system monitor: cpu, memory and network usage, plus a process list.
//!
//! Sampling happens in one app-wide entity rather than in the panel, for two
//! reasons. The graphs are worth something the moment the panel opens only if
//! there is already history behind them, and the per-process cpu numbers need a
//! previous sample to subtract from, which a panel created a frame ago does not
//! have.
//!
//! To keep that from costing anything while nobody is looking, sampling runs in
//! two tiers. Idle, it reads three small files for the system totals. Active
//! (meaning the panel is open) it also walks `/proc` for every process, which is
//! the part worth avoiding when it isn't on screen.

mod collect;
mod panel;

use std::{collections::VecDeque, sync::Arc, time::Duration};

use gpui::{App, AsyncApp, Context, Entity, Global, Subscription, Task, WeakEntity, prelude::*};
use tracing::warn;

pub use collect::{Pid, ProcessInfo, Snapshot};

use crate::{
  config::{ConfigState, SystemConfig},
  icon::IconName,
  launcher::RootItem,
  system::collect::{SampleOptions, Sampler},
};

/// How long after the panel opens the first usable sample is taken. The sample
/// before it exists only to give the cpu deltas something to subtract from, so
/// this is how long the list shows zeroes for - short enough not to read as a
/// stutter, long enough that the deltas mean something.
const PRIME_DELAY: Duration = Duration::from_millis(250);

pub fn get_items() -> Vec<RootItem> {
  vec![RootItem::Panel {
    id: "system".into(),
    icon: IconName::Cpu,
    name: "System".into(),
    description: "Monitor processes and resource usage".into(),
    terms: vec![
      "process".into(),
      "monitor".into(),
      "cpu".into(),
      "ram".into(),
      "memory".into(),
      "kill".into(),
      "top".into(),
      "htop".into(),
    ],
    view: Arc::new(|window, cx| cx.new(|cx| panel::SystemPanel::new(window, cx)).into()),
  }]
}

struct GlobalSystemMonitor(Entity<SystemMonitor>);

impl Global for GlobalSystemMonitor {}

pub fn init(cx: &mut App) {
  let monitor = cx.new(SystemMonitor::new);
  cx.set_global(GlobalSystemMonitor(monitor));
}

/// A fixed length window of the most recent values, oldest first.
#[derive(Debug, Default)]
pub struct History {
  samples: VecDeque<f32>,
  capacity: usize,
}

impl History {
  fn new(capacity: usize) -> Self {
    Self {
      samples: VecDeque::with_capacity(capacity),
      capacity: capacity.max(1),
    }
  }

  fn push(&mut self, value: f32) {
    while self.samples.len() >= self.capacity {
      self.samples.pop_front();
    }
    self.samples.push_back(value);
  }

  fn set_capacity(&mut self, capacity: usize) {
    self.capacity = capacity.max(1);
    while self.samples.len() > self.capacity {
      self.samples.pop_front();
    }
  }

  pub fn iter(&self) -> impl ExactSizeIterator<Item = f32> + '_ {
    self.samples.iter().copied()
  }

  pub fn capacity(&self) -> usize {
    self.capacity
  }

  pub fn max(&self) -> f32 {
    self.samples.iter().copied().fold(0.0, f32::max)
  }
}

/// Emitted once a sample that includes processes has landed. The header redraws
/// off `cx.notify()` on its own; this exists for the process list, which has
/// filtering and sorting to redo and would rather not do it from `render`.
pub struct SnapshotUpdated;

pub struct SystemMonitor {
  config: SystemConfig,
  /// Taken out of the entity while a sample runs on a background thread, which
  /// is how the per-pid cache it owns crosses threads without a lock.
  sampler: Option<Sampler>,
  active: bool,
  /// Reset when the tier changes. The first sample of a tier has nothing to
  /// subtract from and reports zeroes, so this is what makes the one after it
  /// follow quickly instead of a whole interval later.
  samples_this_tier: u32,
  /// Shared rather than owned so the panel can take a snapshot off to a
  /// background thread to group and sort without copying a thousand processes.
  latest: Option<Arc<Snapshot>>,
  /// Cuts the sampling loop's current sleep short. The loop is never restarted,
  /// because dropping it mid-sample would take the sampler with it and leave
  /// nothing to sample with.
  wake: flume::Sender<()>,
  pub cpu: History,
  pub memory: History,
  pub received: History,
  pub transmitted: History,
  _tick: Task<()>,
  _config: Subscription,
}

impl SystemMonitor {
  fn new(cx: &mut Context<Self>) -> Self {
    let config = ConfigState::get(cx).system;
    let samples = config.history_samples;

    let config_subscription = cx.observe(&ConfigState::global(cx), |this: &mut Self, _, cx| {
      this.config = ConfigState::get(cx).system;
      let samples = this.config.history_samples;
      this.cpu.set_capacity(samples);
      this.memory.set_capacity(samples);
      this.received.set_capacity(samples);
      this.transmitted.set_capacity(samples);
      // Apply a changed interval now rather than after the current one elapses.
      this.wake();
      cx.notify();
    });

    let (wake, wakeups) = flume::unbounded();

    Self {
      config,
      sampler: Some(Sampler::new()),
      active: false,
      samples_this_tier: 0,
      latest: None,
      wake,
      cpu: History::new(samples),
      memory: History::new(samples),
      received: History::new(samples),
      transmitted: History::new(samples),
      _tick: cx.spawn(async move |this, cx| run(this, wakeups, cx).await),
      _config: config_subscription,
    }
  }

  pub fn global(cx: &App) -> Entity<Self> {
    cx.global::<GlobalSystemMonitor>().0.clone()
  }

  pub fn config(&self) -> &SystemConfig {
    &self.config
  }

  pub fn latest(&self) -> Option<Arc<Snapshot>> {
    self.latest.clone()
  }

  /// Switches to the expensive tier and restarts the timer so the panel does not
  /// have to wait out an idle interval for its first process list. The panel
  /// hands over its own handle purely so its release can switch the tier back;
  /// no strong reference to it is kept.
  pub fn activate<T: 'static>(&mut self, panel: &Entity<T>, cx: &mut Context<Self>) {
    self.active = true;
    // The idle tier never touched the per-process counters, so the first sample
    // of this tier has nothing to subtract from and only exists to seed them.
    self.samples_this_tier = 0;
    self.wake();

    cx.observe_release(panel, |this, _, _cx| {
      this.active = false;
      this.samples_this_tier = 0;
      this.wake();
    })
    .detach();
  }

  fn wake(&self) {
    // A full channel would only mean a wakeup is already pending, and a
    // disconnected one that the loop has ended; neither is worth reporting.
    self.wake.try_send(()).ok();
  }

  fn next_delay(&self) -> Duration {
    // Checked against the sample that has just been applied: after the first one
    // of a tier the numbers are all zero, so the real one should not be a whole
    // interval away.
    if self.samples_this_tier <= 1 {
      PRIME_DELAY
    } else if self.active {
      Duration::from_millis(self.config.interval_ms.max(1))
    } else {
      Duration::from_millis(self.config.idle_interval_ms.max(1))
    }
  }

  fn take_sampler(&mut self) -> Option<(Sampler, SampleOptions)> {
    let options = SampleOptions {
      include_processes: self.active,
      show_kernel_threads: self.config.show_kernel_threads,
      normalize_cpu: self.config.normalize_cpu,
    };

    self.sampler.take().map(|sampler| (sampler, options))
  }

  fn apply(&mut self, sampler: Sampler, snapshot: Snapshot, cx: &mut Context<Self>) {
    self.cpu.push(snapshot.cpu_percent);
    self.memory.push(snapshot.memory.used_fraction() * 100.0);
    self.received.push(snapshot.network.rx_per_sec as f32);
    self.transmitted.push(snapshot.network.tx_per_sec as f32);

    let has_processes = !snapshot.processes.is_empty();
    self.sampler = Some(sampler);
    self.latest = Some(Arc::new(snapshot));
    self.samples_this_tier = self.samples_this_tier.saturating_add(1);

    if has_processes {
      cx.emit(SnapshotUpdated);
    }
    cx.notify();
  }
}

impl gpui::EventEmitter<SnapshotUpdated> for SystemMonitor {}

async fn run(monitor: WeakEntity<SystemMonitor>, wakeups: flume::Receiver<()>, cx: &mut AsyncApp) {
  loop {
    let Ok(Some((mut sampler, options))) = monitor.update(cx, |monitor, _| monitor.take_sampler())
    else {
      // Either the monitor is gone or a sample is somehow still in flight; in
      // both cases this loop has nothing left to drive.
      break;
    };

    let (sampler, result) = cx
      .background_spawn(async move {
        let result = sampler.sample(options);
        (sampler, result)
      })
      .await;

    let applied = monitor.update(cx, |monitor, cx| match result {
      Ok(snapshot) => monitor.apply(sampler, snapshot, cx),
      Err(error) => {
        warn!(?error, "Failed to sample system metrics");
        monitor.sampler = Some(sampler);
        // Counted like a successful sample so a persistent failure backs off to
        // the normal interval instead of retrying every 250ms.
        monitor.samples_this_tier = monitor.samples_this_tier.saturating_add(2);
      }
    });

    if applied.is_err() {
      break;
    }

    let Ok(delay) = monitor.read_with(cx, |monitor, _| monitor.next_delay()) else {
      break;
    };

    // Whichever comes first: the interval elapsing, or something asking for a
    // sample now because the tier or the interval changed.
    let timer = cx.background_executor().timer(delay);
    futures::future::select(Box::pin(timer), Box::pin(wakeups.recv_async())).await;
  }
}
