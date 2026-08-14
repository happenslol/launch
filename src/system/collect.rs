//! Reads system and process metrics straight out of `/proc`.
//!
//! The sampling strategy follows btop rather than bottom: everything about a
//! process that cannot change while it lives (its name, command line and owner)
//! is read once when the pid is first seen and then cached, so a steady-state
//! tick opens exactly one file per process. On a machine with ~1200 processes
//! that keeps a full sample in the low milliseconds, which is what makes it
//! viable to run on a one second timer behind a UI.

use std::{
  collections::{HashMap, HashSet},
  fs::File,
  io::Read,
  time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow, bail};
use gpui::SharedString;
use rustix::{
  fd::OwnedFd,
  fs::{CWD, Mode, OFlags},
};

pub type Pid = i32;

/// `PF_KTHREAD` from `include/linux/sched.h`, found in field 9 of
/// `/proc/<pid>/stat`. Kernel threads are otherwise indistinguishable from
/// ordinary processes with an empty command line.
const KERNEL_THREAD_FLAG: u64 = 0x0020_0000;

/// `comm` in `/proc/<pid>/stat` is truncated to 15 characters plus a null
/// terminator. At or above that length the name is unreliable and the command
/// line is a better source.
const MAX_STAT_NAME_LEN: usize = 15;

#[derive(Debug, Clone, Copy)]
pub struct SampleOptions {
  /// Whether to walk `/proc` for processes. Skipping it leaves three small file
  /// reads, which is what the idle tier does to keep the graphs moving.
  pub include_processes: bool,
  pub show_kernel_threads: bool,
  pub normalize_cpu: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryStats {
  pub total_bytes: u64,
  pub used_bytes: u64,
  pub available_bytes: u64,
  pub cached_bytes: u64,
  pub swap_total_bytes: u64,
  pub swap_used_bytes: u64,
}

impl MemoryStats {
  pub fn used_fraction(&self) -> f32 {
    if self.total_bytes == 0 {
      return 0.0;
    }
    self.used_bytes as f32 / self.total_bytes as f32
  }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetworkStats {
  pub rx_per_sec: u64,
  pub tx_per_sec: u64,
  pub total_rx: u64,
  pub total_tx: u64,
}

#[derive(Debug, Clone)]
pub struct ProcessInfo {
  pub pid: Pid,
  /// Who spawned it, which is the only link there is between processes and so
  /// the only way to work out what a tree of them looks like.
  pub parent_pid: Pid,
  pub name: SharedString,
  /// Where the binary lives, which for anything launched through a runtime is
  /// the only thing that says which program it actually is - a dozen unrelated
  /// apps all report a name of `electron`. Empty when it cannot be read, which
  /// happens for kernel threads and for other users' processes.
  pub executable: SharedString,
  pub user: SharedString,
  /// Usage over the last interval, which is what a monitor is usually asked
  /// for, and which also jumps around from one sample to the next.
  pub cpu_percent: f32,
  /// Usage averaged over the whole life of the process. Barely moves between
  /// samples, which is what makes it a usable sort key - see
  /// `panel::sort_groups`.
  pub cpu_lifetime_percent: f32,
  pub memory_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
  pub cpu_percent: f32,
  pub core_percent: Vec<f32>,
  pub memory: MemoryStats,
  pub network: NetworkStats,
  /// Empty when [`SampleOptions::include_processes`] was not set.
  pub processes: Vec<ProcessInfo>,
}

/// Jiffies from one `cpu`/`cpuN` line of `/proc/stat`, folded down to the only
/// two numbers a usage percentage needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CpuTimes {
  idle: u64,
  busy: u64,
}

impl CpuTimes {
  fn total(&self) -> u64 {
    self.idle + self.busy
  }
}

/// Everything about a process that is worth remembering between ticks: the
/// fields that never change, plus the cpu counter the next delta is measured
/// against.
struct ProcessCache {
  name: SharedString,
  executable: SharedString,
  user: SharedString,
  start_time: u64,
  cpu_jiffies: u64,
}

#[derive(Default, Clone, Copy)]
struct NetCounters {
  received: u64,
  transmitted: u64,
}

pub struct Sampler {
  page_size: u64,
  previous_total: CpuTimes,
  previous_cores: Vec<CpuTimes>,
  previous_network: HashMap<String, NetCounters>,
  previous_sample: Option<Instant>,
  processes: HashMap<Pid, ProcessCache>,
  users: HashMap<u32, SharedString>,
  /// Reused across every file read in a tick so that a scan of a thousand
  /// processes performs no allocation at all.
  buffer: Vec<u8>,
}

impl Sampler {
  pub fn new() -> Self {
    Self {
      page_size: rustix::param::page_size() as u64,
      previous_total: CpuTimes::default(),
      previous_cores: Vec::new(),
      previous_network: HashMap::new(),
      previous_sample: None,
      processes: HashMap::new(),
      users: HashMap::new(),
      buffer: Vec::with_capacity(4096),
    }
  }

  pub fn sample(&mut self, options: SampleOptions) -> Result<Snapshot> {
    let now = Instant::now();
    let elapsed = self
      .previous_sample
      .map(|previous| now.saturating_duration_since(previous));
    self.previous_sample = Some(now);

    let (total, cores) = self.read_cpu_times()?;

    let total_delta = total.total().saturating_sub(self.previous_total.total());
    let idle_delta = total.idle.saturating_sub(self.previous_total.idle);
    let cpu_percent = usage_percent(total_delta, idle_delta);

    let core_percent = cores
      .iter()
      .enumerate()
      .map(|(index, core)| {
        let previous = self.previous_cores.get(index).copied().unwrap_or_default();
        usage_percent(
          core.total().saturating_sub(previous.total()),
          core.idle.saturating_sub(previous.idle),
        )
      })
      .collect();

    self.previous_total = total;
    self.previous_cores = cores;

    let memory = self.read_memory()?;
    let network = self.read_network(elapsed)?;

    // Every core's jiffies since boot are summed into the aggregate line, so
    // dividing by the core count gives the uptime in the same clock ticks that
    // a process reports its start time in - no need to read /proc/uptime.
    let core_count = self.previous_cores.len().max(1) as u64;
    let uptime_ticks = self.previous_total.total() / core_count;

    let processes = if options.include_processes {
      self.read_processes(total_delta, uptime_ticks, &options)?
    } else {
      Vec::new()
    };

    Ok(Snapshot {
      cpu_percent,
      core_percent,
      memory,
      network,
      processes,
    })
  }

  fn read_cpu_times(&mut self) -> Result<(CpuTimes, Vec<CpuTimes>)> {
    read_file_into("/proc/stat", &mut self.buffer)?;
    let contents = std::str::from_utf8(&self.buffer).context("/proc/stat is not valid utf-8")?;
    parse_cpu_times(contents)
  }

  fn read_memory(&mut self) -> Result<MemoryStats> {
    read_file_into("/proc/meminfo", &mut self.buffer)?;
    let contents = std::str::from_utf8(&self.buffer).context("/proc/meminfo is not valid utf-8")?;
    Ok(parse_meminfo(contents))
  }

  fn read_network(&mut self, elapsed: Option<Duration>) -> Result<NetworkStats> {
    read_file_into("/proc/net/dev", &mut self.buffer)?;
    let contents = std::str::from_utf8(&self.buffer).context("/proc/net/dev is not valid utf-8")?;

    let mut total_rx = 0;
    let mut total_tx = 0;
    let mut rx_delta = 0;
    let mut tx_delta = 0;

    for (interface, counters) in parse_net_dev(contents) {
      total_rx += counters.received;
      total_tx += counters.transmitted;

      // An interface seen for the first time has no baseline to subtract, so it
      // contributes nothing to this tick's rate rather than its lifetime total.
      if let Some(previous) = self.previous_network.get(interface) {
        rx_delta += counters.received.saturating_sub(previous.received);
        tx_delta += counters.transmitted.saturating_sub(previous.transmitted);
      }

      self.previous_network.insert(interface.to_owned(), counters);
    }

    let seconds = elapsed.map(|elapsed| elapsed.as_secs_f64()).unwrap_or(0.0);
    let (rx_per_sec, tx_per_sec) = if seconds > 0.0 {
      (
        (rx_delta as f64 / seconds) as u64,
        (tx_delta as f64 / seconds) as u64,
      )
    } else {
      (0, 0)
    };

    Ok(NetworkStats {
      rx_per_sec,
      tx_per_sec,
      total_rx,
      total_tx,
    })
  }

  fn read_processes(
    &mut self,
    total_delta: u64,
    uptime_ticks: u64,
    options: &SampleOptions,
  ) -> Result<Vec<ProcessInfo>> {
    let core_count = self.previous_cores.len().max(1) as f32;
    let entries = std::fs::read_dir("/proc").context("listing /proc")?;

    let mut processes = Vec::with_capacity(self.processes.len().max(256));
    // A set rather than a list: this is checked once per cached pid at the end,
    // and on a machine with a thousand processes a linear scan would turn that
    // into a million comparisons.
    let mut seen = HashSet::with_capacity(self.processes.len().max(256));

    for entry in entries.flatten() {
      let file_name = entry.file_name();
      let Some(pid) = file_name.to_str().and_then(|name| name.parse::<Pid>().ok()) else {
        continue;
      };

      // A process exiting mid-scan is entirely routine, as is being denied a
      // look at one owned by another user. Either way this pid is skipped
      // rather than failing the whole sample.
      match self.read_process(
        pid,
        entry.path(),
        total_delta,
        uptime_ticks,
        core_count,
        options,
      ) {
        Ok(Some(process)) => {
          seen.insert(pid);
          processes.push(process);
        }
        Ok(None) => {
          seen.insert(pid);
        }
        Err(_) => {}
      }
    }

    self.processes.retain(|pid, _| seen.contains(pid));

    Ok(processes)
  }

  /// Reads one process. Returns `Ok(None)` for a pid that exists but is being
  /// filtered out, so the caller still counts it as seen and keeps its cache
  /// entry warm.
  fn read_process(
    &mut self,
    pid: Pid,
    path: std::path::PathBuf,
    total_delta: u64,
    uptime_ticks: u64,
    core_count: f32,
    options: &SampleOptions,
  ) -> Result<Option<ProcessInfo>> {
    let directory = rustix::fs::openat(
      CWD,
      &path,
      OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    )?;

    read_at(&directory, "stat", &mut self.buffer)?;
    let contents = std::str::from_utf8(&self.buffer).map_err(|_| anyhow!("stat is not utf-8"))?;
    let stat = ProcStat::parse(contents)?;

    if stat.is_kernel_thread && !options.show_kernel_threads {
      return Ok(None);
    }

    // A pid that has been recycled looks like the same process from the outside,
    // so the start time is what distinguishes them. Dropping the stale entry
    // forces the name and owner to be read again for the new process.
    let is_stale = self
      .processes
      .get(&pid)
      .is_some_and(|cached| cached.start_time != stat.start_time);
    if is_stale {
      self.processes.remove(&pid);
    }

    if !self.processes.contains_key(&pid) {
      // All three are fixed for the life of the process, so this is the only
      // tick that pays for them. Order matters: reading the name leaves the
      // command line in the buffer, which is the fallback for the executable.
      let name = self.read_name(&directory, &stat)?;
      let executable = self.read_executable(&directory);
      let user = self.read_user(&directory);

      self.processes.insert(
        pid,
        ProcessCache {
          name,
          executable,
          user,
          start_time: stat.start_time,
          // Seeding with the current counter means a process discovered this
          // tick reports no usage rather than everything it has ever used.
          cpu_jiffies: stat.utime + stat.stime,
        },
      );
    }

    let cached = self
      .processes
      .get_mut(&pid)
      .context("cache entry was just inserted")?;

    let cpu_jiffies = stat.utime + stat.stime;
    let process_delta = cpu_jiffies.saturating_sub(cached.cpu_jiffies);
    cached.cpu_jiffies = cpu_jiffies;

    let cpu_percent = if total_delta == 0 {
      0.0
    } else {
      let fraction = process_delta as f32 / total_delta as f32;
      // The aggregate line of /proc/stat already sums every core, so without
      // scaling back up a thread saturating one core would read 100/core_count.
      if options.normalize_cpu {
        fraction * 100.0
      } else {
        fraction * 100.0 * core_count
      }
    };

    // How much of one core this process has averaged since it started. Unlike
    // the interval figure this barely moves from one sample to the next.
    let lifetime_ticks = uptime_ticks.saturating_sub(stat.start_time).max(1);
    let cpu_lifetime_percent = cpu_jiffies as f32 / lifetime_ticks as f32 * 100.0;

    Ok(Some(ProcessInfo {
      pid,
      parent_pid: stat.parent_pid,
      name: cached.name.clone(),
      executable: cached.executable.clone(),
      user: cached.user.clone(),
      cpu_percent,
      cpu_lifetime_percent,
      memory_bytes: stat.rss_pages * self.page_size,
    }))
  }

  fn read_name(&mut self, directory: &OwnedFd, stat: &ProcStat) -> Result<SharedString> {
    read_at(directory, "cmdline", &mut self.buffer).ok();
    let cmdline = String::from_utf8_lossy(&self.buffer);

    if cmdline.is_empty() {
      // Kernel threads have no command line. Bracketing the name is how ps and
      // top mark them.
      return Ok(SharedString::from(format!("[{}]", stat.comm)));
    }

    if stat.comm.len() >= MAX_STAT_NAME_LEN {
      Ok(SharedString::from(binary_name_from_cmdline(&cmdline)))
    } else {
      Ok(SharedString::from(stat.comm.clone()))
    }
  }

  /// Resolves the binary behind a process.
  ///
  /// `exe` is the better answer, since it is the kernel's own view and survives
  /// a process rewriting its arguments, but reading it needs the same privilege
  /// as tracing the process, so for anything owned by another user it fails.
  /// `argv[0]`, already sitting in the buffer from reading the name, is readable
  /// by anyone and is usually the same path.
  fn read_executable(&mut self, directory: &OwnedFd) -> SharedString {
    if let Ok(target) = rustix::fs::readlinkat(directory, "exe", Vec::new()) {
      let path = target.to_string_lossy();
      if !path.is_empty() {
        return SharedString::from(path.into_owned());
      }
    }

    let cmdline = String::from_utf8_lossy(&self.buffer);
    SharedString::from(executable_from_cmdline(&cmdline).to_owned())
  }

  fn read_user(&mut self, directory: &OwnedFd) -> SharedString {
    let Ok(metadata) = rustix::fs::fstat(directory) else {
      return SharedString::default();
    };

    let uid = metadata.st_uid;
    if let Some(name) = self.users.get(&uid) {
      return name.clone();
    }

    let name = uzers::get_user_by_uid(uid)
      .map(|user| SharedString::from(user.name().to_string_lossy().into_owned()))
      .unwrap_or_else(|| SharedString::from(uid.to_string()));

    self.users.insert(uid, name.clone());
    name
  }
}

impl Default for Sampler {
  fn default() -> Self {
    Self::new()
  }
}

/// The subset of `/proc/<pid>/stat` this monitor uses. See
/// <https://man7.org/linux/man-pages/man5/proc_pid_stat.5.html>.
struct ProcStat {
  comm: String,
  parent_pid: Pid,
  utime: u64,
  stime: u64,
  /// Only ever compared against a previous reading, to notice that a pid has
  /// been recycled, so the clock ticks it is measured in never need converting.
  start_time: u64,
  rss_pages: u64,
  is_kernel_thread: bool,
}

impl ProcStat {
  fn parse(line: &str) -> Result<Self> {
    let line = line.trim_end();

    // comm is wrapped in parentheses and is neither escaped nor length limited,
    // so it can itself contain parentheses and spaces - `((sd-pam))` and
    // `kworker/u16:2-events` both occur. Splitting on the last `") "` is the
    // only reliable way to find where the fixed-width fields begin.
    let start = line
      .find('(')
      .context("process stat line has no opening paren")?;
    let (comm, rest) = line[start + 1..]
      .rsplit_once(") ")
      .context("process stat line has no closing paren")?;

    let fields = rest.split(' ').collect::<Vec<_>>();

    // Field numbers below are the 1-based ones from the manpage; `state` is
    // field 3 and lands at index 0 here.
    let field = |number: usize| -> Result<&str> {
      fields
        .get(number - 3)
        .copied()
        .with_context(|| format!("process stat line is missing field {number}"))
    };

    let parent_pid = field(4)?.parse()?;
    let flags = field(9)?.parse::<u64>()?;
    let utime = field(14)?.parse()?;
    let stime = field(15)?.parse()?;
    let start_time = field(22)?.parse()?;
    let rss_pages = field(24)?.parse()?;

    Ok(Self {
      comm: comm.to_owned(),
      parent_pid,
      utime,
      stime,
      start_time,
      rss_pages,
      is_kernel_thread: flags & KERNEL_THREAD_FLAG != 0,
    })
  }
}

/// Sends a signal to a process, turning the handful of errno values that
/// actually happen here into something worth putting in front of a user.
pub fn signal_process(pid: Pid, signal: rustix::process::Signal) -> Result<()> {
  let pid = rustix::process::Pid::from_raw(pid).context("process id is out of range")?;

  match rustix::process::kill_process(pid, signal) {
    Ok(()) => Ok(()),
    Err(rustix::io::Errno::SRCH) => bail!("process has already exited"),
    Err(rustix::io::Errno::PERM) => bail!("not permitted to signal this process"),
    Err(error) => bail!("failed to signal process: {error}"),
  }
}

fn usage_percent(total_delta: u64, idle_delta: u64) -> f32 {
  if total_delta == 0 {
    return 0.0;
  }

  let busy = total_delta.saturating_sub(idle_delta);
  (busy as f32 / total_delta as f32 * 100.0).clamp(0.0, 100.0)
}

fn parse_cpu_times(contents: &str) -> Result<(CpuTimes, Vec<CpuTimes>)> {
  let mut total = None;
  let mut cores = Vec::new();

  for line in contents.lines() {
    let Some(rest) = line.strip_prefix("cpu") else {
      // The cpu lines come first, so anything else means they are done.
      break;
    };

    let Some((label, values)) = rest.split_once(' ') else {
      continue;
    };

    let times = parse_cpu_line(values);
    if label.is_empty() {
      total = Some(times);
    } else {
      cores.push(times);
    }
  }

  let total = total.context("/proc/stat has no aggregate cpu line")?;
  Ok((total, cores))
}

/// Splits one `/proc/stat` cpu line into idle and busy jiffies.
///
/// `guest` and `guest_nice` are deliberately left out: the kernel already counts
/// them inside `user` and `nice`, so adding them would double count.
fn parse_cpu_line(values: &str) -> CpuTimes {
  let mut fields = values
    .split_whitespace()
    .map(|value| value.parse::<u64>().unwrap_or(0));

  let mut next = || fields.next().unwrap_or(0);

  let user = next();
  let nice = next();
  let system = next();
  let idle = next();
  let iowait = next();
  let irq = next();
  let softirq = next();
  let steal = next();

  CpuTimes {
    idle: idle + iowait,
    busy: user + nice + system + irq + softirq + steal,
  }
}

fn parse_meminfo(contents: &str) -> MemoryStats {
  let mut total = 0;
  let mut free = 0;
  let mut available = None;
  let mut cached = 0;
  let mut swap_total = 0;
  let mut swap_free = 0;

  for line in contents.lines() {
    let Some((label, value)) = line.split_once(':') else {
      continue;
    };

    // Every value of interest is reported in kibibytes.
    let kilobytes = value
      .split_whitespace()
      .next()
      .and_then(|value| value.parse::<u64>().ok())
      .unwrap_or(0);
    let bytes = kilobytes * 1024;

    match label {
      "MemTotal" => total = bytes,
      "MemFree" => free = bytes,
      "MemAvailable" => available = Some(bytes),
      "Cached" => cached = bytes,
      "SwapTotal" => swap_total = bytes,
      "SwapFree" => {
        swap_free = bytes;
        // SwapFree is the last field of interest, and meminfo has some fifty
        // more lines after it.
        break;
      }
      _ => {}
    }
  }

  // Kernels old enough to lack MemAvailable need it approximated, since page
  // cache is reclaimable and counting it as used would be misleading.
  let available = available.unwrap_or(free + cached);

  MemoryStats {
    total_bytes: total,
    used_bytes: total.saturating_sub(available),
    available_bytes: available,
    cached_bytes: cached,
    swap_total_bytes: swap_total,
    swap_used_bytes: swap_total.saturating_sub(swap_free),
  }
}

/// Yields the byte counters of every interface except loopback, whose traffic
/// never leaves the machine and would dwarf everything else.
fn parse_net_dev(contents: &str) -> impl Iterator<Item = (&str, NetCounters)> {
  contents.lines().filter_map(|line| {
    let (interface, values) = line.split_once(':')?;
    let interface = interface.trim();
    if interface.is_empty() || interface == "lo" {
      return None;
    }

    let mut fields = values.split_whitespace();
    let received = fields.next()?.parse().ok()?;
    // Receive has eight columns before transmit starts.
    let transmitted = fields.nth(7)?.parse().ok()?;

    Some((
      interface,
      NetCounters {
        received,
        transmitted,
      },
    ))
  })
}

/// Recovers the binary's path from a command line, for when `exe` cannot be
/// read.
///
/// Normally `argv[0]` is simply the first null-separated entry. Some programs
/// rewrite the whole argument block to set what `ps` shows - Firefox labels its
/// content processes that way - which leaves a run of text with no nulls in it
/// at all, so a leading absolute path is cut at the first space to get the path
/// back out. A relative or bare command name is discarded: it says nothing the
/// process name has not already said.
fn executable_from_cmdline(cmdline: &str) -> &str {
  let argv0 = cmdline.split('\0').next().unwrap_or_default();

  if !argv0.starts_with('/') {
    return "";
  }

  match argv0.split_once(' ') {
    Some((path, _)) => path,
    None => argv0,
  }
}

/// Recovers a usable process name from a command line, for the processes whose
/// `comm` was truncated by the kernel. Arguments are separated by nulls, so the
/// first one is everything up to the first null.
fn binary_name_from_cmdline(cmdline: &str) -> String {
  let executable = cmdline.split('\0').next().unwrap_or(cmdline);

  // Interpreters and some toolkits append a colon and a role, as in
  // `/usr/bin/foo: worker`, which is not part of the binary name.
  let executable = executable.split(':').next().unwrap_or(executable);
  let executable = executable.rsplit('/').next().unwrap_or(executable);

  // A command line passed as one string rather than an argv still needs the
  // arguments trimmed off.
  executable
    .split_once(" -")
    .map(|(name, _)| name)
    .unwrap_or(executable)
    .trim()
    .to_owned()
}

fn read_file_into(path: &str, buffer: &mut Vec<u8>) -> Result<()> {
  buffer.clear();
  File::open(path)
    .with_context(|| format!("opening {path}"))?
    .read_to_end(buffer)
    .with_context(|| format!("reading {path}"))?;
  Ok(())
}

fn read_at(directory: &OwnedFd, name: &str, buffer: &mut Vec<u8>) -> Result<()> {
  buffer.clear();
  let file = rustix::fs::openat(
    directory,
    name,
    OFlags::RDONLY | OFlags::CLOEXEC,
    Mode::empty(),
  )?;
  File::from(file).read_to_end(buffer)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn stat_line(comm: &str) -> String {
    // Field numbers match the manpage: state, ppid, then padding out to rss.
    format!("1 ({comm}) S 0 1 1 0 -1 4194560 448335 0 0 0 1529 761 0 0 20 0 7 0 13 20582400 2810 0")
  }

  #[test]
  fn parses_a_real_stat_line() {
    let stat = ProcStat::parse(&stat_line("systemd")).unwrap();
    assert_eq!(stat.comm, "systemd");
    assert_eq!(stat.parent_pid, 0);
    assert_eq!(stat.utime, 1529);
    assert_eq!(stat.stime, 761);
    assert_eq!(stat.start_time, 13);
    assert_eq!(stat.rss_pages, 2810);
    assert!(!stat.is_kernel_thread);
  }

  #[test]
  fn parses_awkward_process_names() {
    assert_eq!(
      ProcStat::parse(&stat_line("(sd-pam)")).unwrap().comm,
      "(sd-pam)"
    );
    assert_eq!(
      ProcStat::parse(&stat_line("a test")).unwrap().comm,
      "a test"
    );
    assert_eq!(
      ProcStat::parse(&stat_line("kworker/u16:2-events_unbound"))
        .unwrap()
        .comm,
      "kworker/u16:2-events_unbound"
    );

    let long = "a".repeat(64);
    assert_eq!(ProcStat::parse(&stat_line(&long)).unwrap().comm, long);
  }

  #[test]
  fn detects_kernel_threads() {
    let kernel = "2 (kthreadd) S 0 0 0 0 -1 2129984 0 0 0 0 0 2 0 0 20 0 1 0 13 0 0 0";
    assert!(ProcStat::parse(kernel).unwrap().is_kernel_thread);
  }

  #[test]
  fn rejects_malformed_stat_lines() {
    assert!(ProcStat::parse("1 (blah").is_err(), "missing closing paren");
    assert!(ProcStat::parse("1 (blah)").is_err(), "no fields after comm");
    assert!(
      ProcStat::parse("1 )(").is_err(),
      "parens the wrong way round"
    );
  }

  #[test]
  fn parses_cpu_times() {
    let contents =
      "cpu  100 0 100 100 20 30 40 50 0 0\ncpu0 50 0 50 50 10 15 20 25 0 0\nintr 1 2 3\n";
    let (total, cores) = parse_cpu_times(contents).unwrap();

    assert_eq!(total.idle, 120);
    assert_eq!(total.busy, 320);
    assert_eq!(cores.len(), 1);
    assert_eq!(cores[0].idle, 60);
    assert_eq!(cores[0].busy, 160);
  }

  #[test]
  fn cpu_line_tolerates_missing_columns() {
    // Very old kernels report fewer columns than current ones.
    assert_eq!(
      parse_cpu_line("100 0 100 100"),
      CpuTimes {
        idle: 100,
        busy: 200
      }
    );
  }

  #[test]
  fn computes_usage_percent() {
    assert_eq!(usage_percent(0, 0), 0.0);
    assert_eq!(usage_percent(100, 100), 0.0);
    assert_eq!(usage_percent(100, 25), 75.0);
  }

  #[test]
  fn parses_meminfo() {
    let contents = "MemTotal:       63362548 kB\nMemFree:         3226080 kB\nMemAvailable:   15461092 kB\nBuffers:              24 kB\nCached:          5384112 kB\nSwapTotal:       8388604 kB\nSwapFree:        8000000 kB\nDirty:                12 kB\n";
    let memory = parse_meminfo(contents);

    assert_eq!(memory.total_bytes, 63362548 * 1024);
    assert_eq!(memory.available_bytes, 15461092 * 1024);
    assert_eq!(memory.used_bytes, (63362548 - 15461092) * 1024);
    assert_eq!(memory.cached_bytes, 5384112 * 1024);
    assert_eq!(memory.swap_used_bytes, (8388604 - 8000000) * 1024);
  }

  #[test]
  fn meminfo_without_available_falls_back_to_free_plus_cache() {
    let contents =
      "MemTotal:  1000 kB\nMemFree:  200 kB\nCached:  300 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n";
    let memory = parse_meminfo(contents);

    assert_eq!(memory.available_bytes, 500 * 1024);
    assert_eq!(memory.used_bytes, 500 * 1024);
  }

  #[test]
  fn parses_net_dev_and_skips_loopback() {
    let contents = "Inter-|   Receive                                                |  Transmit\n face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n    lo: 2606389445 10947885    0    0    0     0          0         0 2606389445 10947885    0    0    0     0       0          0\n  eth0: 100 1 0 0 0 0 0 0 200 2 0 0 0 0 0 0\n";
    let interfaces = parse_net_dev(contents).collect::<Vec<_>>();

    assert_eq!(interfaces.len(), 1);
    assert_eq!(interfaces[0].0, "eth0");
    assert_eq!(interfaces[0].1.received, 100);
    assert_eq!(interfaces[0].1.transmitted, 200);
  }

  #[test]
  fn derives_names_from_command_lines() {
    assert_eq!(binary_name_from_cmdline("/usr/bin/btm"), "btm");
    assert_eq!(
      binary_name_from_cmdline("/usr/bin/btm\0--asdf\0--asdf/gkj"),
      "btm"
    );
    assert_eq!(binary_name_from_cmdline("/usr/bin/btm:"), "btm");
    assert_eq!(binary_name_from_cmdline("/usr/bin/b tm\0--test"), "b tm");
    assert_eq!(
      binary_name_from_cmdline("firefox -contentproc -isForBrowser -prefsHandle 0"),
      "firefox"
    );
    assert_eq!(binary_name_from_cmdline("こんにちは\0"), "こんにちは");
  }

  #[test]
  fn recovers_a_path_from_a_rewritten_command_line() {
    assert_eq!(
      executable_from_cmdline("/usr/bin/foo\0--flag\0value"),
      "/usr/bin/foo"
    );
    // Firefox rewrites the whole block to label its content processes, leaving
    // no nulls to split on.
    assert_eq!(
      executable_from_cmdline("/nix/store/abc-firefox/lib/firefox/browser 32 tab"),
      "/nix/store/abc-firefox/lib/firefox/browser"
    );
    assert_eq!(executable_from_cmdline("firefox\0--flag"), "");
    assert_eq!(executable_from_cmdline(""), "");
  }

  /// The nested-namespace harness cannot read `exe` at all, so this is the check
  /// that the preferred path actually works when nothing is in the way.
  #[test]
  fn resolves_its_own_binary_through_exe() {
    let mut sampler = Sampler::new();
    let directory = rustix::fs::openat(
      CWD,
      "/proc/self",
      OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
      Mode::empty(),
    )
    .expect("opening our own /proc entry");

    // The buffer is empty, so anything but a working `exe` read yields nothing.
    let executable = sampler.read_executable(&directory);

    assert!(
      executable.starts_with('/'),
      "expected an absolute path, got {executable:?}"
    );
    assert!(
      std::path::Path::new(executable.as_ref()).exists(),
      "expected a path that exists, got {executable:?}"
    );
  }

  /// Per-process cpu is a delta between two samples, so it can only be checked
  /// by taking two with known work in between.
  #[test]
  fn measures_cpu_used_between_samples() {
    let mut sampler = Sampler::new();
    let options = SampleOptions {
      include_processes: true,
      show_kernel_threads: false,
      normalize_cpu: false,
    };

    sampler.sample(options).expect("priming sample");

    let deadline = Instant::now() + Duration::from_millis(400);
    let mut spin = 0u64;
    while Instant::now() < deadline {
      spin = spin.wrapping_add(1);
    }
    assert!(spin > 0);

    let snapshot = sampler.sample(options).expect("second sample");
    let us = std::process::id() as Pid;
    let ourselves = snapshot
      .processes
      .iter()
      .find(|process| process.pid == us)
      .expect("the test process should be in its own process list");

    assert!(
      ourselves.cpu_percent > 10.0,
      "a process that spun for most of the interval should not read {:.1}%",
      ourselves.cpu_percent
    );
  }

  /// The parsers are all fed fixtures, so this is the one check that the real
  /// files on this machine still look the way they are assumed to.
  #[test]
  fn samples_the_running_system() {
    let mut sampler = Sampler::new();
    let options = SampleOptions {
      include_processes: true,
      show_kernel_threads: false,
      normalize_cpu: false,
    };

    sampler.sample(options).expect("priming sample");
    let snapshot = sampler.sample(options).expect("second sample");

    assert!(snapshot.memory.total_bytes > 0);
    assert!(!snapshot.core_percent.is_empty());
    assert!(
      snapshot.processes.iter().any(|process| process.pid == 1),
      "pid 1 should always be present"
    );
    assert!(
      snapshot
        .processes
        .iter()
        .all(|process| !process.name.is_empty()),
      "every process should resolve to a name"
    );
  }
}
