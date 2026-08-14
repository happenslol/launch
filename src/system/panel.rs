//! The system panel: usage graphs over a searchable process list.
//!
//! Filtering, grouping and sorting all happen here rather than in the picker
//! delegate. The list is a flattened tree (a group row followed by its children)
//! and reordering the picker's match list would pull those apart, so the panel
//! hands the picker a list that is already in its final order and the delegate
//! passes it through untouched.

use std::{
  collections::{HashMap, HashSet, VecDeque},
  sync::{Arc, atomic::AtomicBool},
  time::Duration,
};

use gpui::{
  App, Background, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, IntoElement, KeyBinding,
  Path, Pixels, Render, RenderOnce, SharedString, StyleRefinement, Styled, Subscription, Task,
  Window, actions, canvas, div, point, prelude::*, px, relative, rems, rgb, rgba,
};
use nucleo_matcher::{
  Utf32Str,
  pattern::{CaseMatching, Normalization, Pattern},
};

use crate::{
  config::{ConfigState, SortKey},
  confirmation::{ConfirmationEvent, ConfirmationPrompt, render_confirmation_overlay},
  icon::{Icon, IconName},
  matcher::MatcherPool,
  picker::{Picker, PickerDelegate, PickerEvent, picker_input, picker_results},
  system::{
    History, Pid, ProcessInfo, SnapshotUpdated, SystemMonitor,
    collect::{self, Snapshot},
  },
  util::{ResultExt, StyledExt, h_flex, v_flex},
};

const CONTEXT: &str = "system";

/// How long a failed kill stays on screen. Long enough to read, short enough
/// that it doesn't linger over a list that keeps moving underneath it.
const ERROR_TIMEOUT: Duration = Duration::from_secs(5);

actions!(system, [KillProcess, CycleSort, ReverseSort, ToggleTree]);

/// Column widths, shared by the header and the rows so the two line up. The name
/// column takes whatever is left.
const EXPANDER_WIDTH: Pixels = px(16.);
const COUNT_WIDTH: Pixels = px(40.);
const CPU_WIDTH: Pixels = px(60.);
const MEMORY_WIDTH: Pixels = px(76.);
const PID_WIDTH: Pixels = px(60.);
const USER_WIDTH: Pixels = px(88.);

const SPARKLINE_HEIGHT: Pixels = px(22.);
const CORE_BAR_WIDTH: Pixels = px(3.);

/// Identifies a group of processes.
///
/// The path is part of it because the name on its own is not enough to tell
/// programs apart: everything built on a runtime reports the runtime's name, so
/// grouping on that alone would file every Electron app under one `electron`
/// heading, and the path shown on that row would be true of only some of them.
#[derive(Clone, PartialEq, Eq, Hash)]
struct GroupKey {
  name: SharedString,
  executable: SharedString,
}

/// Which shape the list is in.
///
/// The two answer different questions and neither subsumes the other: grouping
/// says how much of the machine a program is using in total, the tree says what
/// spawned what. Processes sharing a name are frequently unrelated - this
/// machine has nineteen `node` processes under thirteen different parents - so
/// collapsing them is an aggregate, not a hierarchy.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
  Grouped,
  Tree,
}

/// What a row stands for. Doubles as the identity that the selection and the
/// set of unfolded rows are tracked by, in both modes.
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
  Group(GroupKey),
  Process(Pid),
}

#[derive(Clone)]
pub struct ProcessRow {
  key: RowKey,
  name: SharedString,
  /// Kept raw; it is shortened for display when the row is rendered, which
  /// happens only for the handful of rows actually on screen.
  executable: SharedString,
  pid: Option<Pid>,
  /// What a kill on this row signals: the whole group in grouped mode, and just
  /// the process itself in tree mode, where reaching the descendants is what the
  /// tree options in the prompt are for.
  targets: Arc<[Pid]>,
  /// How many processes this row stands for, when that is more than itself.
  count: Option<usize>,
  expanded: bool,
  /// How deep in the tree, which is only ever more than zero in tree mode and in
  /// an unfolded group.
  depth: usize,
  user: SharedString,
  cpu_percent: f32,
  memory_bytes: u64,
}

impl ProcessRow {
  fn is_expandable(&self) -> bool {
    self.count.is_some()
  }

  fn describe(&self) -> String {
    match (self.pid, self.count) {
      (Some(pid), _) => format!("{} (pid {pid})", self.name),
      (None, Some(count)) => format!("{} ({count} processes)", self.name),
      (None, None) => self.name.to_string(),
    }
  }
}

pub struct SystemPanel {
  picker: Entity<Picker<ProcessDelegate>>,
  monitor: Entity<SystemMonitor>,
  query: String,
  sort: SortKey,
  reversed: bool,
  mode: ViewMode,
  expanded: HashSet<RowKey>,
  error: Option<SharedString>,
  confirmation_prompt: Option<Entity<ConfirmationPrompt>>,
  _rebuild: Option<Task<()>>,
  _error_timeout: Option<Task<()>>,
  _subscriptions: Vec<Subscription>,
}

impl SystemPanel {
  pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
    cx.bind_keys([
      KeyBinding::new("ctrl-x", KillProcess, Some(CONTEXT)),
      KeyBinding::new("ctrl-s", CycleSort, Some(CONTEXT)),
      KeyBinding::new("alt-s", ReverseSort, Some(CONTEXT)),
      KeyBinding::new("ctrl-t", ToggleTree, Some(CONTEXT)),
    ]);

    let sort = ConfigState::get(cx).system.default_sort;

    let picker = cx.new(|cx| {
      let mut picker = Picker::new(ProcessDelegate, Arc::new(vec![]), window, cx);
      picker.placeholder("Search processes...", cx);
      picker
    });

    let monitor = SystemMonitor::global(cx);
    monitor.update(cx, |monitor, cx| {
      monitor.activate(&cx.entity(), cx);
    });

    let subscriptions = vec![
      cx.subscribe_in(
        &picker,
        window,
        |this, _picker, event, window, cx| match event {
          PickerEvent::Picked(row) => this.activate_row(row.clone(), window, cx),
          PickerEvent::QueryChanged(query) => {
            this.query = query.clone();
            this.rebuild(window, cx);
          }
          PickerEvent::SecondaryPicked(_) => {}
        },
      ),
      cx.subscribe_in(
        &monitor,
        window,
        |this, _monitor, _event: &SnapshotUpdated, window, cx| {
          this.rebuild(window, cx);
        },
      ),
    ];

    cx.focus_view(&picker.read(cx).search_input.clone(), window);

    let mut panel = Self {
      picker,
      monitor,
      query: String::new(),
      sort,
      reversed: false,
      mode: ViewMode::Grouped,
      expanded: HashSet::new(),
      error: None,
      confirmation_prompt: None,
      _rebuild: None,
      _error_timeout: None,
      _subscriptions: subscriptions,
    };

    // The idle tier may already have a snapshot to show, in which case there is
    // no need to sit empty until the first sample of the active tier lands.
    panel.rebuild(window, cx);

    panel
  }

  /// Rebuilds the visible rows from the newest snapshot.
  ///
  /// Grouping, matching and sorting all run on a background thread; only the
  /// finished row list comes back to the foreground.
  fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
    let Some(snapshot) = self.monitor.read(cx).latest() else {
      return;
    };

    if snapshot.processes.is_empty() {
      return;
    }

    let grouping = self.monitor.read(cx).config().group_processes;
    let matchers = MatcherPool::global(cx);
    let query = self.query.clone();
    let expanded = self.expanded.clone();
    let sort = self.sort;
    let reversed = self.reversed;
    let mode = self.mode;

    self._rebuild = Some(cx.spawn_in(window, async move |this, cx| {
      let rows = cx
        .background_spawn(async move {
          let mut matcher = matchers.get().await.ok();
          build_rows(
            &snapshot,
            &query,
            sort,
            reversed,
            grouping,
            mode,
            &expanded,
            matcher.as_deref_mut(),
          )
        })
        .await;

      this
        .update_in(cx, |this, window, cx| this.apply_rows(rows, window, cx))
        .log_err();
    }));
  }

  /// Hands a freshly built list to the picker, keeping the selection on whatever
  /// it was on before rather than on whatever has ended up in the same position.
  fn apply_rows(&mut self, rows: Vec<ProcessRow>, window: &mut Window, cx: &mut Context<Self>) {
    let selected = self
      .picker
      .read(cx)
      .get_selected_item()
      .map(|row| row.key.clone());

    let index = selected.and_then(|key| rows.iter().position(|row| row.key == key));

    self.picker.update(cx, |picker, cx| {
      picker.set_items(rows, window, cx);
      if let Some(index) = index {
        picker.set_selected_index(index, cx);
      }
    });

    cx.notify();
  }

  /// Enter unfolds a row that stands for more than itself, and offers to signal
  /// one that does not.
  fn activate_row(&mut self, row: ProcessRow, window: &mut Window, cx: &mut Context<Self>) {
    if !row.is_expandable() {
      self.prompt_kill(row, window, cx);
      return;
    }

    if !self.expanded.remove(&row.key) {
      self.expanded.insert(row.key.clone());
    }

    self.rebuild(window, cx);
  }

  fn kill_selected(&mut self, _: &KillProcess, window: &mut Window, cx: &mut Context<Self>) {
    let Some(row) = self.picker.read(cx).get_selected_item().cloned() else {
      return;
    };

    self.prompt_kill(row, window, cx);
  }

  fn prompt_kill(&mut self, row: ProcessRow, window: &mut Window, cx: &mut Context<Self>) {
    let targets = row.targets.to_vec();
    if targets.is_empty() {
      return;
    }

    // Worked out from the snapshot already on screen rather than by rereading
    // `/proc`, so that what gets signalled is exactly what the count in the
    // prompt promised. The cost is that a process spawned since the last sample
    // is missed, which a fresh walk would catch but could not have named.
    let tree = self
      .monitor
      .read(cx)
      .latest()
      .map(|snapshot| process_tree(&snapshot.processes, &targets))
      .unwrap_or_else(|| targets.clone());

    let descendants = tree.len().saturating_sub(targets.len());

    let mut choices = vec!["Cancel", "Terminate", "Kill"];
    if descendants > 0 {
      choices.push("Terminate tree");
      choices.push("Kill tree");
    }

    let message = match descendants {
      0 => format!("Signal {}?", row.describe()),
      1 => format!("Signal {}? It has 1 descendant.", row.describe()),
      count => format!("Signal {}? It has {count} descendants.", row.describe()),
    };

    let prompt = cx.new(|cx| ConfirmationPrompt::with_choices(message, choices, window, cx));

    let subscription = cx.subscribe_in(
      &prompt,
      window,
      move |this, _prompt, event: &ConfirmationEvent, window, cx| match event {
        ConfirmationEvent::Closing => {
          cx.focus_view(&this.picker.read(cx).search_input.clone(), window);
        }
        ConfirmationEvent::Confirm(choice) => {
          // Cancel, Terminate, Kill, then the two tree variants when there are
          // descendants to apply them to. The even ones are the uncatchable
          // signal, the tree ones reach past the selection.
          let signal = if choice % 2 == 0 {
            rustix::process::Signal::KILL
          } else {
            rustix::process::Signal::TERM
          };
          let targets = if *choice > 2 { &tree } else { &targets };

          this.signal(targets, signal, cx);
          this.confirmation_prompt = None;
          cx.notify();
        }
        ConfirmationEvent::Dismiss => {
          this.confirmation_prompt = None;
          cx.notify();
        }
      },
    );

    self._subscriptions.push(subscription);
    self.confirmation_prompt = Some(prompt);
    cx.notify();
  }

  fn signal(&mut self, targets: &[Pid], signal: rustix::process::Signal, cx: &mut Context<Self>) {
    // Signalling a group is several calls, any of which can fail on its own -
    // a child may have exited between the sample and the keystroke. Only the
    // first failure is worth putting on screen.
    let failure = targets
      .iter()
      .filter_map(|pid| collect::signal_process(*pid, signal).err())
      .next();

    match failure {
      Some(error) => self.show_error(SharedString::from(error.to_string()), cx),
      None => self.error = None,
    }
  }

  fn show_error(&mut self, message: SharedString, cx: &mut Context<Self>) {
    self.error = Some(message);
    self._error_timeout = Some(cx.spawn(async move |this, cx| {
      cx.background_executor().timer(ERROR_TIMEOUT).await;
      this
        .update(cx, |this, cx| {
          this.error = None;
          cx.notify();
        })
        .log_err();
    }));
    cx.notify();
  }

  fn cycle_sort(&mut self, _: &CycleSort, window: &mut Window, cx: &mut Context<Self>) {
    self.sort = match self.sort {
      SortKey::Cpu => SortKey::Memory,
      SortKey::Memory => SortKey::Name,
      SortKey::Name => SortKey::Pid,
      SortKey::Pid => SortKey::Cpu,
    };
    self.reversed = false;
    self.rebuild(window, cx);
  }

  fn reverse_sort(&mut self, _: &ReverseSort, window: &mut Window, cx: &mut Context<Self>) {
    self.reversed = !self.reversed;
    self.rebuild(window, cx);
  }

  fn toggle_tree(&mut self, _: &ToggleTree, window: &mut Window, cx: &mut Context<Self>) {
    self.mode = match self.mode {
      ViewMode::Grouped => ViewMode::Tree,
      ViewMode::Tree => ViewMode::Grouped,
    };

    // The two modes key their unfolded rows differently, and a row unfolded in
    // one has no counterpart in the other.
    self.expanded.clear();
    self.rebuild(window, cx);
  }

  fn sort_by(&mut self, sort: SortKey, window: &mut Window, cx: &mut Context<Self>) {
    // Clicking the column already sorted by flips the direction, which is what
    // every other table does.
    if self.sort == sort {
      self.reversed = !self.reversed;
    } else {
      self.sort = sort;
      self.reversed = false;
    }

    self.rebuild(window, cx);
  }

  fn render_stats(&self, cx: &App) -> impl IntoElement {
    let monitor = self.monitor.read(cx);
    let snapshot = monitor.latest();

    let cpu = snapshot
      .as_ref()
      .map_or(0.0, |snapshot| snapshot.cpu_percent);
    let memory = snapshot.map(|snapshot| snapshot.memory).unwrap_or_default();
    let network = self
      .monitor
      .read(cx)
      .latest()
      .map(|snapshot| snapshot.network)
      .unwrap_or_default();

    let cores = monitor
      .latest()
      .map(|snapshot| snapshot.core_percent.clone())
      .unwrap_or_default();

    // Both directions share a scale, so a busy download doesn't make a trickle
    // of uploads look like a matching spike.
    let network_max = monitor.received.max().max(monitor.transmitted.max());

    v_flex()
      .px_4()
      .py_2()
      .gap_1()
      .border_b_1()
      .border_color(rgba(0xFFFFFF12))
      .child(
        h_flex()
          .gap_3()
          .child(stat_label("CPU"))
          .child(Sparkline::new(&monitor.cpu, 100.0).w(px(120.)))
          .child(stat_value(format!("{cpu:.0}%"), px(44.)))
          .child(render_cores(&cores)),
      )
      .child(
        h_flex()
          .gap_3()
          .child(stat_label("RAM"))
          .child(Sparkline::new(&monitor.memory, 100.0).w(px(120.)))
          .child(stat_value(
            format!(
              "{} / {}",
              format_bytes(memory.used_bytes),
              format_bytes(memory.total_bytes)
            ),
            px(140.),
          ))
          .child(stat_label("NET"))
          .child(
            Icon::new(IconName::ArrowDown)
              .size(rems(0.75))
              .text_color(rgb(0x888888)),
          )
          .child(Sparkline::new(&monitor.received, network_max).w(px(56.)))
          .child(stat_value(format_rate(network.rx_per_sec), px(88.)))
          .child(
            Icon::new(IconName::ArrowUp)
              .size(rems(0.75))
              .text_color(rgb(0x888888)),
          )
          .child(Sparkline::new(&monitor.transmitted, network_max).w(px(56.)))
          .child(stat_value(format_rate(network.tx_per_sec), px(88.))),
      )
  }

  fn render_columns(&self, cx: &mut Context<Self>) -> impl IntoElement {
    let header = |label: &'static str, sort: SortKey, width: Option<Pixels>, trailing: bool| {
      let is_active = self.sort == sort;

      h_flex()
        .id(label)
        .cursor_pointer()
        .gap_1()
        .when(trailing, |this| this.justify_end())
        .when_some(width, |this, width| this.w(width).flex_shrink_0())
        .when(width.is_none(), |this| this.flex_1().min_w_0())
        .text_color(if is_active {
          Hsla::from(rgba(0xFFFFFFCC))
        } else {
          Hsla::from(rgb(0x888888))
        })
        .child(label)
        .when(is_active, |this| {
          this.child(
            Icon::new(if self.reversed {
              IconName::ArrowUp
            } else {
              IconName::ArrowDown
            })
            .size(rems(0.7))
            .text_color(rgba(0xFFFFFFCC)),
          )
        })
        .on_click(cx.listener(move |this, _, window, cx| this.sort_by(sort, window, cx)))
    };

    h_flex()
      .px_4()
      .pt_2()
      .pb_1()
      .gap_2()
      .text_xs()
      .child(div().w(EXPANDER_WIDTH).flex_shrink_0())
      .child(header("Name", SortKey::Name, None, false))
      .child(div().w(COUNT_WIDTH).flex_shrink_0())
      .child(header("CPU", SortKey::Cpu, Some(CPU_WIDTH), true))
      .child(header("Memory", SortKey::Memory, Some(MEMORY_WIDTH), true))
      .child(header("PID", SortKey::Pid, Some(PID_WIDTH), true))
      .child(
        div()
          .w(USER_WIDTH)
          .flex_shrink_0()
          .text_color(rgb(0x888888))
          .child("User"),
      )
  }
}

impl Focusable for SystemPanel {
  fn focus_handle(&self, cx: &App) -> FocusHandle {
    match &self.confirmation_prompt {
      Some(prompt) => prompt.read(cx).focus_handle(cx),
      None => self.picker.read(cx).focus_handle(cx),
    }
  }
}

impl Render for SystemPanel {
  fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
    // Called out because the order and the column disagree on purpose: the
    // column shows this interval, the order mostly follows the lifetime average.
    let sort_label = match self.sort {
      SortKey::Cpu if !self.reversed => "cpu (lazy)",
      SortKey::Cpu => "cpu",
      SortKey::Memory => "memory",
      SortKey::Name => "name",
      SortKey::Pid => "pid",
    };

    v_flex()
      .key_context(CONTEXT)
      .size_full()
      .relative()
      .on_action(cx.listener(Self::kill_selected))
      .on_action(cx.listener(Self::cycle_sort))
      .on_action(cx.listener(Self::reverse_sort))
      .on_action(cx.listener(Self::toggle_tree))
      .child(
        picker_input(&self.picker).show_back_button(true).suffix(
          h_flex()
            .gap_1()
            .text_xs()
            .text_color(rgb(0x888888))
            .when(self.mode == ViewMode::Tree, |this| this.child("tree ·"))
            .child(format!("sort: {sort_label}"))
            .child(
              Icon::new(if self.reversed {
                IconName::ArrowUp
              } else {
                IconName::ArrowDown
              })
              .size(rems(0.7))
              .text_color(rgb(0x888888)),
            )
            .into_any_element(),
        ),
      )
      .child(self.render_stats(cx))
      .when_some(self.error.clone(), |this, error| {
        this.child(
          div()
            .px_4()
            .py_1()
            .text_xs()
            .text_color(rgb(0xCC4444))
            .child(error),
        )
      })
      .child(self.render_columns(cx))
      .child(picker_results(&self.picker))
      .when_some(self.confirmation_prompt.clone(), |this, prompt| {
        this.child(render_confirmation_overlay(&prompt))
      })
  }
}

struct ProcessDelegate;

impl PickerDelegate for ProcessDelegate {
  type ListItem = ProcessRow;

  fn render_list_item(
    &self,
    _window: &mut Window,
    _cx: &mut Context<Picker<Self>>,
    item: &Self::ListItem,
    is_selected: bool,
  ) -> impl IntoElement {
    let is_child = item.depth > 0;
    // Deep chains exist - a shell inside a terminal inside a session - but past
    // a few levels the indent costs more room than it explains.
    let indent = px(12. * item.depth.min(5) as f32);

    h_flex()
      .w_full()
      .px_2()
      .py_1()
      .gap_2()
      .rounded_md()
      .text_sm()
      .when(is_selected, |this| this.bg(rgba(0xFFFFFF0F)))
      .child(
        div()
          .w(EXPANDER_WIDTH)
          .flex_shrink_0()
          .child(if item.is_expandable() {
            Icon::new(if item.expanded {
              IconName::ChevronDown
            } else {
              IconName::ChevronRight
            })
            .size(rems(0.75))
            .text_color(rgb(0x888888))
            .into_any_element()
          } else {
            div().into_any_element()
          }),
      )
      .child(
        h_flex()
          .flex_1()
          .min_w_0()
          .gap_2()
          .pl(indent)
          .child(
            div()
              .flex_shrink_0()
              .truncate()
              // Anything nested sits under a row that gives it context, so it is
              // dimmed rather than competing with its parent.
              .when(is_child, |this| this.text_color(rgba(0xFFFFFFAA)))
              .child(item.name.clone()),
          )
          // Takes whatever the name leaves, and is the first thing to be cut
          // when the two together do not fit.
          .when(!item.executable.is_empty(), |this| {
            this.child(
              div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(rgb(0x666666))
                .child(shorten_path(&item.executable)),
            )
          }),
      )
      .child(
        div()
          .w(COUNT_WIDTH)
          .flex_shrink_0()
          .text_xs()
          .text_color(rgb(0x888888))
          .child(match item.count {
            Some(count) => SharedString::from(format!("({count})")),
            None => SharedString::default(),
          }),
      )
      .child(numeric_cell(
        format!("{:.1}%", item.cpu_percent),
        CPU_WIDTH,
        // A process actually doing something should stand out from the hundreds
        // sitting at zero.
        if item.cpu_percent >= 1.0 {
          rgba(0xFFFFFFEE)
        } else {
          rgba(0xFFFFFF77)
        },
      ))
      .child(numeric_cell(
        format_bytes(item.memory_bytes),
        MEMORY_WIDTH,
        rgba(0xFFFFFFCC),
      ))
      .child(numeric_cell(
        item
          .pid
          .map(|pid| pid.to_string())
          .unwrap_or_else(|| "-".to_owned()),
        PID_WIDTH,
        rgba(0xFFFFFF77),
      ))
      .child(
        div()
          .w(USER_WIDTH)
          .flex_shrink_0()
          .truncate()
          .text_xs()
          .text_color(rgb(0x888888))
          .child(item.user.clone()),
      )
  }

  fn update_matches(
    &mut self,
    window: &mut Window,
    cx: &mut Context<Picker<Self>>,
    _query: String,
    _cancel_flag: Arc<AtomicBool>,
    search_id: usize,
    _items: Arc<Vec<Self::ListItem>>,
  ) -> Task<()> {
    // The panel has already filtered and ordered these; the picker only needs to
    // show them as they are.
    cx.defer_in(window, move |picker, _window, cx| {
      picker.complete_search(cx, search_id, None);
    });

    Task::ready(())
  }

  fn sort_items(&self, _cx: &App, _items: &[Self::ListItem], _matches: &mut [(usize, u32)]) {
    // Deliberately empty: the rows are a flattened tree and the default sort by
    // score would separate children from the group they belong to.
  }
}

/// One group of processes sharing a name and a binary, before it is flattened
/// into rows.
struct ProcessGroup<'a> {
  key: GroupKey,
  members: Vec<&'a ProcessInfo>,
  cpu_percent: f32,
  cpu_lifetime_percent: f32,
  memory_bytes: u64,
}

#[allow(clippy::too_many_arguments)]
fn build_rows(
  snapshot: &Snapshot,
  query: &str,
  sort: SortKey,
  reversed: bool,
  grouping: bool,
  mode: ViewMode,
  expanded: &HashSet<RowKey>,
  matcher: Option<&mut nucleo_matcher::Matcher>,
) -> Vec<ProcessRow> {
  match mode {
    ViewMode::Grouped => build_grouped_rows(
      &snapshot.processes,
      query,
      sort,
      reversed,
      grouping,
      expanded,
      matcher,
    ),
    ViewMode::Tree => build_tree_rows(
      &snapshot.processes,
      query,
      sort,
      reversed,
      expanded,
      matcher,
    ),
  }
}

fn build_grouped_rows(
  processes: &[ProcessInfo],
  query: &str,
  sort: SortKey,
  reversed: bool,
  grouping: bool,
  expanded: &HashSet<RowKey>,
  matcher: Option<&mut nucleo_matcher::Matcher>,
) -> Vec<ProcessRow> {
  let mut groups = group_processes(processes, grouping);
  filter_groups(&mut groups, query, matcher);
  sort_groups(&mut groups, sort, reversed);

  let mut rows = Vec::with_capacity(groups.len());
  for group in groups {
    let is_group = group.members.len() > 1;
    let key = RowKey::Group(group.key.clone());
    let is_expanded = is_group && expanded.contains(&key);

    if !is_group {
      let Some(process) = group.members.first() else {
        continue;
      };

      rows.push(ProcessRow {
        key: RowKey::Process(process.pid),
        name: group.key.name,
        executable: group.key.executable,
        pid: Some(process.pid),
        targets: Arc::from([process.pid]),
        count: None,
        expanded: false,
        depth: 0,
        user: process.user.clone(),
        cpu_percent: process.cpu_percent,
        memory_bytes: process.memory_bytes,
      });
      continue;
    }

    let user = group
      .members
      .first()
      .map(|process| process.user.clone())
      .unwrap_or_default();

    rows.push(ProcessRow {
      key,
      name: group.key.name.clone(),
      executable: group.key.executable.clone(),
      pid: None,
      targets: group
        .members
        .iter()
        .map(|process| process.pid)
        .collect::<Vec<_>>()
        .into(),
      count: Some(group.members.len()),
      expanded: is_expanded,
      depth: 0,
      user,
      cpu_percent: group.cpu_percent,
      memory_bytes: group.memory_bytes,
    });

    if !is_expanded {
      continue;
    }

    let mut members = group.members;
    sort_processes(&mut members, sort, reversed);

    rows.extend(members.into_iter().map(|process| ProcessRow {
      key: RowKey::Process(process.pid),
      name: group.key.name.clone(),
      // Every member shares the group's binary, so the children repeating it
      // would be noise; their pid is what tells them apart.
      executable: SharedString::default(),
      pid: Some(process.pid),
      targets: Arc::from([process.pid]),
      count: None,
      expanded: false,
      depth: 1,
      user: process.user.clone(),
      cpu_percent: process.cpu_percent,
      memory_bytes: process.memory_bytes,
    }));
  }

  rows
}

/// What a process and everything under it add up to.
#[derive(Clone, Copy, Default)]
struct Subtree {
  count: usize,
  cpu_percent: f32,
  cpu_lifetime_percent: f32,
  memory_bytes: u64,
}

/// The parent/child structure of a snapshot, worked out once so that building
/// rows from it is just walking.
struct Forest<'a> {
  by_pid: HashMap<Pid, &'a ProcessInfo>,
  children: HashMap<Pid, Vec<Pid>>,
  roots: Vec<Pid>,
  totals: HashMap<Pid, Subtree>,
}

impl<'a> Forest<'a> {
  fn new(processes: &'a [ProcessInfo]) -> Self {
    let by_pid: HashMap<Pid, &ProcessInfo> = processes
      .iter()
      .map(|process| (process.pid, process))
      .collect();

    let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
    for process in processes {
      if by_pid.contains_key(&process.parent_pid) && process.parent_pid != process.pid {
        children
          .entry(process.parent_pid)
          .or_default()
          .push(process.pid);
      }
    }

    // Anything whose parent is not in the snapshot stands on its own. Kernel
    // threads are usually filtered out, so their children surface here rather
    // than being lost.
    let mut roots: Vec<Pid> = processes
      .iter()
      .filter(|process| !by_pid.contains_key(&process.parent_pid))
      .map(|process| process.pid)
      .collect();

    // Pre-order from the roots, which also tells us who is reachable.
    let mut order = Vec::with_capacity(processes.len());
    let mut seen = HashSet::with_capacity(processes.len());
    let mut stack: Vec<Pid> = roots.clone();

    while let Some(pid) = stack.pop() {
      if !seen.insert(pid) {
        continue;
      }
      order.push(pid);
      if let Some(descendants) = children.get(&pid) {
        stack.extend(descendants.iter().copied());
      }
    }

    // Parent ids come from a sweep taken over time, so they can describe a cycle
    // that no root reaches. Those processes would otherwise vanish from the
    // list entirely, so each becomes a root of its own.
    for process in processes {
      if seen.contains(&process.pid) {
        continue;
      }

      roots.push(process.pid);
      let mut stack = vec![process.pid];
      while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
          continue;
        }
        order.push(pid);
        if let Some(descendants) = children.get(&pid) {
          stack.extend(descendants.iter().copied());
        }
      }
    }

    // Children come after their parents in pre-order, so walking it backwards
    // means a subtree is complete before it is folded into its parent.
    let mut totals: HashMap<Pid, Subtree> = processes
      .iter()
      .map(|process| {
        (
          process.pid,
          Subtree {
            count: 1,
            cpu_percent: process.cpu_percent,
            cpu_lifetime_percent: process.cpu_lifetime_percent,
            memory_bytes: process.memory_bytes,
          },
        )
      })
      .collect();

    for pid in order.iter().rev() {
      let Some(total) = totals.get(pid).copied() else {
        continue;
      };
      let Some(parent) = by_pid.get(pid).map(|process| process.parent_pid) else {
        continue;
      };
      if parent == *pid {
        continue;
      }

      if let Some(parent_total) = totals.get_mut(&parent) {
        parent_total.count += total.count;
        parent_total.cpu_percent += total.cpu_percent;
        parent_total.cpu_lifetime_percent += total.cpu_lifetime_percent;
        parent_total.memory_bytes += total.memory_bytes;
      }
    }

    Self {
      by_pid,
      children,
      roots,
      totals,
    }
  }

  fn total(&self, pid: Pid) -> Subtree {
    self.totals.get(&pid).copied().unwrap_or_default()
  }
}

fn build_tree_rows(
  processes: &[ProcessInfo],
  query: &str,
  sort: SortKey,
  reversed: bool,
  expanded: &HashSet<RowKey>,
  matcher: Option<&mut nucleo_matcher::Matcher>,
) -> Vec<ProcessRow> {
  let forest = Forest::new(processes);

  // A match deep in the tree is only findable if the branch leading to it is
  // shown too, so the visible set is the matches plus all of their ancestors.
  let visible =
    (!query.is_empty()).then(|| visible_with_ancestors(&forest, processes, query, matcher));

  let mut rows = Vec::with_capacity(processes.len().min(256));
  let mut level = forest.roots.clone();
  if let Some(visible) = &visible {
    level.retain(|pid| visible.contains(pid));
  }
  sort_tree_level(&mut level, &forest, sort, reversed);

  // An explicit stack rather than recursion: a snapshot can describe a chain
  // thousands of processes deep, and this runs on a background thread.
  let mut stack: Vec<(Pid, usize)> = level.into_iter().rev().map(|pid| (pid, 0)).collect();

  while let Some((pid, depth)) = stack.pop() {
    let Some(process) = forest.by_pid.get(&pid) else {
      continue;
    };

    let total = forest.total(pid);
    let descendants = total.count.saturating_sub(1);
    let key = RowKey::Process(pid);
    // While searching, everything on the way to a match is unfolded; leaving it
    // closed would hide the very thing that was found.
    let is_expanded = descendants > 0 && (visible.is_some() || expanded.contains(&key));

    rows.push(ProcessRow {
      key,
      name: process.name.clone(),
      executable: process.executable.clone(),
      pid: Some(pid),
      targets: Arc::from([pid]),
      count: (descendants > 0).then_some(total.count),
      expanded: is_expanded,
      depth,
      user: process.user.clone(),
      // A row that stands for a subtree reports what the subtree adds up to,
      // the same way a group row reports its members' total.
      cpu_percent: if descendants > 0 {
        total.cpu_percent
      } else {
        process.cpu_percent
      },
      memory_bytes: if descendants > 0 {
        total.memory_bytes
      } else {
        process.memory_bytes
      },
    });

    if !is_expanded {
      continue;
    }

    let Some(children) = forest.children.get(&pid) else {
      continue;
    };

    let mut children = children.clone();
    if let Some(visible) = &visible {
      children.retain(|pid| visible.contains(pid));
    }
    sort_tree_level(&mut children, &forest, sort, reversed);

    stack.extend(children.into_iter().rev().map(|pid| (pid, depth + 1)));
  }

  rows
}

/// The processes whose name matches, plus every ancestor of theirs.
fn visible_with_ancestors(
  forest: &Forest<'_>,
  processes: &[ProcessInfo],
  query: &str,
  matcher: Option<&mut nucleo_matcher::Matcher>,
) -> HashSet<Pid> {
  let mut visible = HashSet::new();

  let matched: Vec<Pid> = match matcher {
    Some(matcher) => {
      let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
      let mut buffer = Vec::new();
      processes
        .iter()
        .filter(|process| {
          pattern
            .score(Utf32Str::new(&process.name, &mut buffer), matcher)
            .is_some()
        })
        .map(|process| process.pid)
        .collect()
    }
    None => {
      let needle = query.to_lowercase();
      processes
        .iter()
        .filter(|process| process.name.to_lowercase().contains(&needle))
        .map(|process| process.pid)
        .collect()
    }
  };

  for pid in matched {
    let mut current = pid;
    // Guarded by the visited set, since a cyclic parent chain would otherwise
    // walk forever.
    while visible.insert(current) {
      let Some(process) = forest.by_pid.get(&current) else {
        break;
      };
      if process.parent_pid == current {
        break;
      }
      current = process.parent_pid;
    }
  }

  visible
}

/// Orders one set of siblings, by what each of their subtrees adds up to so that
/// a busy branch sorts high even when the process at its head is idle.
fn sort_tree_level(level: &mut Vec<Pid>, forest: &Forest<'_>, sort: SortKey, reversed: bool) {
  match sort {
    SortKey::Cpu if !reversed => sort_lazy_cpu(
      level,
      |pid| forest.total(*pid).cpu_percent,
      |pid| forest.total(*pid).cpu_lifetime_percent,
    ),
    SortKey::Cpu => level.sort_by(|a, b| {
      compare_desc(
        forest.total(*b).cpu_percent,
        forest.total(*a).cpu_percent,
        reversed,
      )
    }),
    SortKey::Memory => level.sort_by(|a, b| {
      compare_desc(
        forest.total(*b).memory_bytes,
        forest.total(*a).memory_bytes,
        reversed,
      )
    }),
    SortKey::Name => level.sort_by(|a, b| {
      let name = |pid: &Pid| forest.by_pid.get(pid).map(|process| process.name.clone());
      flip(name(a).cmp(&name(b)), reversed)
    }),
    SortKey::Pid => level.sort_by_key(|pid| if reversed { -*pid } else { *pid }),
  }
}

/// Every process in the tree below `roots`, the roots included, ordered parents
/// before children.
///
/// Built from a snapshot rather than walked live, so a parent dying and its
/// children being reparented partway through cannot lose anyone: the whole list
/// is decided before the first signal is sent.
fn process_tree(processes: &[ProcessInfo], roots: &[Pid]) -> Vec<Pid> {
  let mut children: HashMap<Pid, Vec<Pid>> = HashMap::new();
  for process in processes {
    children
      .entry(process.parent_pid)
      .or_default()
      .push(process.pid);
  }

  let mut ordered = Vec::new();
  let mut seen = HashSet::new();
  let mut queue = VecDeque::new();

  for root in roots {
    if seen.insert(*root) {
      ordered.push(*root);
      queue.push_back(*root);
    }
  }

  // `seen` doubles as cycle protection: parent ids come from a snapshot that was
  // taken over time, so they are not guaranteed to describe an actual tree.
  while let Some(pid) = queue.pop_front() {
    let Some(descendants) = children.get(&pid) else {
      continue;
    };

    for child in descendants {
      if seen.insert(*child) {
        ordered.push(*child);
        queue.push_back(*child);
      }
    }
  }

  // Killing the tree above us is a legitimate thing to ask for, but doing it to
  // ourselves first would abandon the rest of the work.
  let ourselves = std::process::id() as Pid;
  if let Some(index) = ordered.iter().position(|pid| *pid == ourselves) {
    let pid = ordered.remove(index);
    ordered.push(pid);
  }

  ordered
}

fn group_key(process: &ProcessInfo) -> GroupKey {
  GroupKey {
    name: process.name.clone(),
    executable: process.executable.clone(),
  }
}

fn group_processes(processes: &[ProcessInfo], grouping: bool) -> Vec<ProcessGroup<'_>> {
  if !grouping {
    return processes
      .iter()
      .map(|process| ProcessGroup {
        key: group_key(process),
        members: vec![process],
        cpu_percent: process.cpu_percent,
        cpu_lifetime_percent: process.cpu_lifetime_percent,
        memory_bytes: process.memory_bytes,
      })
      .collect();
  }

  let mut indices: HashMap<GroupKey, usize> = HashMap::new();
  let mut groups: Vec<ProcessGroup<'_>> = Vec::new();

  for process in processes {
    let key = group_key(process);

    match indices.get(&key) {
      Some(index) => {
        if let Some(group) = groups.get_mut(*index) {
          group.members.push(process);
          group.cpu_percent += process.cpu_percent;
          group.cpu_lifetime_percent += process.cpu_lifetime_percent;
          group.memory_bytes += process.memory_bytes;
        }
      }
      None => {
        indices.insert(key.clone(), groups.len());
        groups.push(ProcessGroup {
          key,
          members: vec![process],
          cpu_percent: process.cpu_percent,
          cpu_lifetime_percent: process.cpu_lifetime_percent,
          memory_bytes: process.memory_bytes,
        });
      }
    }
  }

  groups
}

/// Keeps only the groups whose name matches the query. Children are never
/// matched on their own - they share their group's name, so a group that
/// survives brings all of its processes with it.
fn filter_groups(
  groups: &mut Vec<ProcessGroup<'_>>,
  query: &str,
  matcher: Option<&mut nucleo_matcher::Matcher>,
) {
  if query.is_empty() {
    return;
  }

  let Some(matcher) = matcher else {
    // Without a matcher to borrow, fall back to a plain substring search rather
    // than showing everything as if nothing had been typed.
    let needle = query.to_lowercase();
    groups.retain(|group| group.key.name.to_lowercase().contains(&needle));
    return;
  };

  let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
  let mut buffer = Vec::new();

  groups.retain(|group| {
    pattern
      .score(Utf32Str::new(&group.key.name, &mut buffer), matcher)
      .is_some()
  });
}

/// How many rows at the top count as the settled leaders.
const LAZY_CPU_LEADERS: usize = 6;
/// What it takes to displace one of those leaders.
const LAZY_CPU_BUSY: f32 = 30.0;
/// What it takes to be lifted out of the tail, when the leaders are quiet.
const LAZY_CPU_FLOOR: f32 = 10.0;
/// At most this many are lifted, so the settled part of the order stays the bulk
/// of it.
const LAZY_CPU_LIMIT: usize = 10;

/// btop's "cpu lazy" ordering.
///
/// Ordering on the interval figure alone makes the whole list churn every tick:
/// hundreds of processes sit at or near zero and trade places on rounding alone,
/// so rows move under the cursor constantly. The backbone of the order is
/// therefore the lifetime average, which barely shifts between samples, and only
/// the processes that are busy *right now* are lifted out of it.
///
/// What counts as busy depends on where a row already sits. Displacing one of
/// the leaders takes real load; further down, anything above the floor is worth
/// surfacing - unless the leaders are busier still, in which case the bar rises
/// to meet them so a loaded machine doesn't hoist half the list.
fn sort_lazy_cpu<T>(items: &mut Vec<T>, current: impl Fn(&T) -> f32, lifetime: impl Fn(&T) -> f32) {
  items.sort_by(|a, b| compare_desc(lifetime(b), lifetime(a), false));

  let leaders = items
    .iter()
    .take(LAZY_CPU_LEADERS)
    .map(&current)
    .fold(LAZY_CPU_FLOOR, f32::max);
  let tail = if leaders > LAZY_CPU_BUSY {
    leaders
  } else {
    LAZY_CPU_FLOOR
  };
  let threshold = |index: usize| {
    if index < LAZY_CPU_LEADERS {
      LAZY_CPU_BUSY
    } else {
      tail
    }
  };

  let any_busy = items
    .iter()
    .enumerate()
    .any(|(index, item)| current(item) > threshold(index));
  if !any_busy {
    return;
  }

  let mut busy = Vec::new();
  let mut rest = Vec::with_capacity(items.len());
  for (index, item) in items.drain(..).enumerate() {
    if busy.len() < LAZY_CPU_LIMIT && current(&item) > threshold(index) {
      busy.push(item);
    } else {
      rest.push(item);
    }
  }

  busy.sort_by(|a, b| compare_desc(current(b), current(a), false));
  items.extend(busy);
  items.extend(rest);
}

fn sort_groups(groups: &mut Vec<ProcessGroup<'_>>, sort: SortKey, reversed: bool) {
  match sort {
    // Reversing asks for the quietest first, which is a deliberate look at the
    // exact numbers, so it orders on them strictly.
    SortKey::Cpu if !reversed => sort_lazy_cpu(
      groups,
      |group| group.cpu_percent,
      |group| group.cpu_lifetime_percent,
    ),
    SortKey::Cpu => groups.sort_by(|a, b| compare_desc(b.cpu_percent, a.cpu_percent, reversed)),
    SortKey::Memory => {
      groups.sort_by(|a, b| compare_desc(b.memory_bytes, a.memory_bytes, reversed))
    }
    SortKey::Name => groups.sort_by(|a, b| flip(a.key.name.cmp(&b.key.name), reversed)),
    SortKey::Pid => groups.sort_by_key(|group| {
      let pid = group
        .members
        .iter()
        .map(|process| process.pid)
        .min()
        .unwrap_or(0);
      if reversed { -pid } else { pid }
    }),
  }
}

fn sort_processes(processes: &mut Vec<&ProcessInfo>, sort: SortKey, reversed: bool) {
  match sort {
    SortKey::Cpu if !reversed => sort_lazy_cpu(
      processes,
      |process| process.cpu_percent,
      |process| process.cpu_lifetime_percent,
    ),
    SortKey::Cpu => processes.sort_by(|a, b| compare_desc(b.cpu_percent, a.cpu_percent, reversed)),
    SortKey::Memory => {
      processes.sort_by(|a, b| compare_desc(b.memory_bytes, a.memory_bytes, reversed))
    }
    SortKey::Name => processes.sort_by(|a, b| flip(a.name.cmp(&b.name), reversed)),
    SortKey::Pid => {
      processes.sort_by_key(|process| if reversed { -process.pid } else { process.pid })
    }
  }
}

/// Orders the larger value first, since that is what someone looking for what is
/// using the machine wants at the top.
fn compare_desc<T: PartialOrd>(a: T, b: T, reversed: bool) -> std::cmp::Ordering {
  let ordering = a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
  flip(ordering, reversed)
}

fn flip(ordering: std::cmp::Ordering, reversed: bool) -> std::cmp::Ordering {
  if reversed {
    ordering.reverse()
  } else {
    ordering
  }
}

fn stat_label(label: &'static str) -> impl IntoElement {
  div()
    .w(px(30.))
    .flex_shrink_0()
    .text_xs()
    .text_color(rgb(0x888888))
    .child(label)
}

fn stat_value(value: String, width: Pixels) -> impl IntoElement {
  div()
    .w(width)
    .flex_shrink_0()
    .text_xs()
    .text_color(rgba(0xFFFFFFCC))
    .child(value)
}

fn numeric_cell(value: String, width: Pixels, color: gpui::Rgba) -> impl IntoElement {
  h_flex()
    .w(width)
    .flex_shrink_0()
    .justify_end()
    .text_xs()
    .text_color(color)
    .child(value)
}

/// One bar per core, tall for busy and short for idle. Kept as plain divs rather
/// than a canvas because there are only ever a few dozen of them.
fn render_cores(cores: &[f32]) -> impl IntoElement {
  h_flex().gap(px(1.)).items_end().children(
    cores
      .iter()
      .map(|usage| {
        let fill = (usage / 100.0).clamp(0.0, 1.0);
        div()
          .w(CORE_BAR_WIDTH)
          .h(SPARKLINE_HEIGHT)
          .flex_shrink_0()
          .flex()
          .flex_col()
          .justify_end()
          .bg(rgba(0xFFFFFF12))
          .child(div().w_full().h(relative(fill)).bg(rgba(0xFFFFFFCC)))
      })
      .collect::<Vec<_>>(),
  )
}

/// A filled area chart of the recent history of one value.
///
/// Painted rather than built out of one div per sample: the panel draws four of
/// these next to each other and refreshes every second, and a couple of hundred
/// extra elements per frame is exactly the kind of cost this panel is meant to
/// avoid.
#[derive(IntoElement)]
struct Sparkline {
  samples: Vec<f32>,
  capacity: usize,
  max: f32,
  style: StyleRefinement,
}

impl Sparkline {
  fn new(history: &History, max: f32) -> Self {
    Self {
      samples: history.iter().collect(),
      capacity: history.capacity(),
      max,
      style: StyleRefinement::default(),
    }
  }
}

impl Styled for Sparkline {
  fn style(&mut self) -> &mut StyleRefinement {
    &mut self.style
  }
}

impl RenderOnce for Sparkline {
  fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
    let Sparkline {
      samples,
      capacity,
      max,
      style,
    } = self;

    let fill: Background = Hsla::from(rgba(0xFFFFFF66)).into();

    div()
      .h(SPARKLINE_HEIGHT)
      .flex_shrink_0()
      .refine_style(&style)
      .bg(rgba(0xFFFFFF0A))
      .child(
        canvas(
          |_bounds, _window, _cx| (),
          move |bounds: Bounds<Pixels>, _, window, _cx| {
            if samples.len() < 2 {
              return;
            }

            // Scaled against the full window rather than the samples collected
            // so far, so a graph that is still filling up grows from the right
            // instead of stretching.
            let columns = capacity.max(samples.len()).max(2) - 1;
            let step = bounds.size.width / columns as f32;
            let baseline = bounds.bottom();
            let max = if max > 0.0 { max } else { 1.0 };

            // Right aligned: the newest sample sits at the right edge and older
            // ones trail off to the left.
            let offset = bounds.size.width - step * (samples.len() - 1) as f32;
            let left = bounds.left() + offset;

            let mut path = Path::new(point(left, baseline));
            for (index, value) in samples.iter().enumerate() {
              let x = left + step * index as f32;
              let height = bounds.size.height * (value / max).clamp(0.0, 1.0);
              path.line_to(point(x, baseline - height));
            }
            path.line_to(point(bounds.right(), baseline));

            window.paint_path(path, fill);
          },
        )
        .size_full(),
      )
  }
}

fn format_bytes(bytes: u64) -> String {
  const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

  let mut value = bytes as f64;
  let mut unit = 0;
  while value >= 1024.0 && unit + 1 < UNITS.len() {
    value /= 1024.0;
    unit += 1;
  }

  if unit == 0 {
    format!("{} {}", bytes, UNITS[unit])
  } else if value >= 100.0 {
    format!("{value:.0} {}", UNITS[unit])
  } else {
    format!("{value:.1} {}", UNITS[unit])
  }
}

fn format_rate(bytes_per_second: u64) -> String {
  format!("{}/s", format_bytes(bytes_per_second))
}

/// Roughly how many characters of path fit beside a name. The font is
/// monospaced, so a character budget is a good enough stand-in for a width, and
/// anything still too long is ellipsised by the layout on top of this.
const PATH_BUDGET: usize = 40;

/// The length of a nix store hash, which is base32 and fixed width.
const NIX_HASH_LEN: usize = 32;

fn home_directory() -> Option<&'static str> {
  static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

  HOME
    .get_or_init(|| dirs::home_dir().map(|home| home.to_string_lossy().into_owned()))
    .as_deref()
}

/// Makes a path scannable at a glance, which mostly means throwing away the
/// parts that are the same for everything.
fn shorten_path(path: &str) -> String {
  let mut shortened = strip_nix_hashes(path);

  if let Some(home) = home_directory()
    && let Some(rest) = shortened.strip_prefix(home)
  {
    shortened = format!("~{rest}");
  }

  trim_leading_components(&shortened, PATH_BUDGET)
}

/// Drops the hash from nix store paths.
///
/// `/nix/store/e8slgs3n…-slack-4.35.126/bin/slack` becomes
/// `/nix/store/slack-4.35.126/bin/slack`: 33 characters of noise gone, and what
/// is left is the package and version, which is the part worth reading.
fn strip_nix_hashes(path: &str) -> String {
  let Some(rest) = path.strip_prefix("/nix/store/") else {
    return path.to_owned();
  };

  let Some(hash_end) = rest.get(..NIX_HASH_LEN).and_then(|hash| {
    // Base32 as nix writes it, so no uppercase and no separators. Requiring the
    // dash as well keeps this from firing on a directory that merely happens to
    // be 32 characters long.
    hash
      .chars()
      .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
      .then_some(NIX_HASH_LEN)
  }) else {
    return path.to_owned();
  };

  match rest[hash_end..].strip_prefix('-') {
    Some(named) => format!("/nix/store/{named}"),
    None => path.to_owned(),
  }
}

/// Drops leading path components until what is left fits, because the end of a
/// path - the package, then the binary - is what identifies it.
fn trim_leading_components(path: &str, budget: usize) -> String {
  if path.chars().count() <= budget {
    return path.to_owned();
  }

  let mut start = 0;
  while let Some(next) = path[start + 1..].find('/').map(|index| start + 1 + index) {
    start = next;
    // The ellipsis this gets prefixed with costs a character of its own.
    if path[start..].chars().count() < budget {
      break;
    }
  }

  if start == 0 {
    // A single component with nothing to drop. Saying so with an ellipsis would
    // claim something was removed; leave it for the layout to cut instead.
    return path.to_owned();
  }

  format!("…{}", &path[start..])
}

#[cfg(test)]
mod tests {
  use super::*;

  fn process(pid: Pid, name: &str, cpu: f32, lifetime: f32, memory: u64) -> ProcessInfo {
    executable_process(
      pid,
      name,
      &format!("/usr/bin/{name}"),
      cpu,
      lifetime,
      memory,
    )
  }

  fn executable_process(
    pid: Pid,
    name: &str,
    executable: &str,
    cpu: f32,
    lifetime: f32,
    memory: u64,
  ) -> ProcessInfo {
    ProcessInfo {
      pid,
      parent_pid: 1,
      name: SharedString::from(name.to_owned()),
      executable: SharedString::from(executable.to_owned()),
      user: SharedString::from("someone"),
      cpu_percent: cpu,
      cpu_lifetime_percent: lifetime,
      memory_bytes: memory,
    }
  }

  fn snapshot(processes: Vec<ProcessInfo>) -> Snapshot {
    Snapshot {
      cpu_percent: 0.0,
      core_percent: vec![0.0],
      memory: Default::default(),
      network: Default::default(),
      processes,
    }
  }

  fn sample() -> Snapshot {
    snapshot(vec![
      process(10, "firefox", 5.0, 2.0, 200),
      process(11, "firefox", 20.0, 3.0, 100),
      process(12, "firefox", 1.0, 1.0, 50),
      process(20, "hyprland", 8.0, 40.0, 400),
      process(30, "sleep", 0.0, 0.0, 10),
    ])
  }

  fn rows(sort: SortKey, reversed: bool, expanded: &[&str], query: &str) -> Vec<ProcessRow> {
    let expanded = expanded
      .iter()
      .map(|name| {
        RowKey::Group(GroupKey {
          name: SharedString::from((*name).to_owned()),
          executable: SharedString::from(format!("/usr/bin/{name}")),
        })
      })
      .collect::<HashSet<_>>();

    build_rows(
      &sample(),
      query,
      sort,
      reversed,
      true,
      ViewMode::Grouped,
      &expanded,
      None,
    )
  }

  fn grouped(snapshot: &Snapshot, query: &str, sort: SortKey) -> Vec<ProcessRow> {
    build_rows(
      snapshot,
      query,
      sort,
      false,
      true,
      ViewMode::Grouped,
      &HashSet::new(),
      None,
    )
  }

  fn tree(snapshot: &Snapshot, query: &str, expanded: &[Pid]) -> Vec<ProcessRow> {
    let expanded = expanded
      .iter()
      .map(|pid| RowKey::Process(*pid))
      .collect::<HashSet<_>>();

    build_rows(
      snapshot,
      query,
      SortKey::Pid,
      false,
      true,
      ViewMode::Tree,
      &expanded,
      None,
    )
  }

  /// Renders the shape of a tree the way `pstree` would, so the assertions read
  /// like the thing they describe.
  fn shape(rows: &[ProcessRow]) -> Vec<String> {
    rows
      .iter()
      .map(|row| {
        format!(
          "{}{}:{}",
          "  ".repeat(row.depth),
          row.name,
          row.pid.unwrap_or(0)
        )
      })
      .collect()
  }

  fn names(rows: &[ProcessRow]) -> Vec<String> {
    rows
      .iter()
      .map(|row| match row.pid {
        Some(pid) => format!("{}:{pid}", row.name),
        None => row.name.to_string(),
      })
      .collect()
  }

  #[test]
  fn groups_shared_names_and_sums_their_usage() {
    let rows = rows(SortKey::Cpu, false, &[], "");
    let firefox = rows
      .iter()
      .find(|row| row.name == "firefox")
      .expect("firefox should be listed");

    assert_eq!(firefox.name, "firefox");
    assert_eq!(firefox.cpu_percent, 26.0);
    assert_eq!(firefox.memory_bytes, 350);
    assert!(
      firefox.pid.is_none(),
      "a group row stands for no single pid"
    );
    assert_eq!(firefox.targets.to_vec(), vec![10, 11, 12]);
  }

  #[test]
  fn a_lone_process_is_not_a_group() {
    let rows = rows(SortKey::Cpu, false, &[], "");
    let hyprland = rows
      .iter()
      .find(|row| row.name == "hyprland")
      .expect("hyprland should be listed");

    assert!(!hyprland.is_expandable());
    assert_eq!(hyprland.pid, Some(20));
    assert_eq!(hyprland.targets.to_vec(), vec![20]);
  }

  #[test]
  fn sorts_by_the_chosen_column() {
    // Cpu is deliberately not in here: it is the lazy ordering, covered below.
    assert_eq!(
      names(&rows(SortKey::Memory, false, &[], "")),
      vec!["hyprland:20", "firefox", "sleep:30"]
    );
    assert_eq!(
      names(&rows(SortKey::Name, false, &[], "")),
      vec!["firefox", "hyprland:20", "sleep:30"]
    );
    assert_eq!(
      names(&rows(SortKey::Pid, false, &[], "")),
      vec!["firefox", "hyprland:20", "sleep:30"]
    );
  }

  #[test]
  fn reversing_orders_on_the_interval_figure_strictly() {
    // Reversed cpu drops the lazy ordering, so this is ascending on what the
    // column actually shows.
    assert_eq!(
      names(&rows(SortKey::Cpu, true, &[], "")),
      vec!["sleep:30", "hyprland:20", "firefox"]
    );
  }

  /// The lazy ordering keeps the list still. `hyprland` leads on its lifetime
  /// average even though `firefox` is using more cpu this instant, because
  /// nothing here is busy enough to be worth reshuffling the list for.
  #[test]
  fn lazy_cpu_leaves_a_quiet_list_alone() {
    assert_eq!(
      names(&rows(SortKey::Cpu, false, &[], "")),
      vec!["hyprland:20", "firefox", "sleep:30"]
    );
  }

  #[test]
  fn lazy_cpu_lifts_a_process_that_is_busy_now() {
    let busy = snapshot(vec![
      process(20, "hyprland", 8.0, 40.0, 400),
      process(30, "compiler", 95.0, 1.0, 10),
      process(40, "sleep", 0.0, 0.0, 10),
    ]);

    let rows = grouped(&busy, "", SortKey::Cpu);

    assert_eq!(
      names(&rows),
      vec!["compiler:30", "hyprland:20", "sleep:40"],
      "a process pegging a core should be lifted over the settled leader"
    );
  }

  #[test]
  fn lazy_cpu_does_not_lift_the_whole_list_when_everything_is_busy() {
    // Every one of these is over the floor, but only what beats the leaders is
    // worth moving; the rest keep their settled order.
    let loaded = snapshot(vec![
      process(1, "alpha", 40.0, 90.0, 10),
      process(2, "beta", 15.0, 80.0, 10),
      process(3, "gamma", 15.0, 70.0, 10),
      process(4, "delta", 15.0, 60.0, 10),
      process(5, "epsilon", 15.0, 50.0, 10),
      process(6, "zeta", 15.0, 40.0, 10),
      process(7, "eta", 15.0, 30.0, 10),
    ]);

    let rows = grouped(&loaded, "", SortKey::Cpu);

    assert_eq!(
      names(&rows),
      vec![
        "alpha:1",
        "beta:2",
        "gamma:3",
        "delta:4",
        "epsilon:5",
        "zeta:6",
        "eta:7"
      ],
      "with the leaders already busy the order should not be disturbed"
    );
  }

  #[test]
  fn expanded_children_follow_their_group_in_order() {
    assert_eq!(
      names(&rows(SortKey::Cpu, false, &["firefox"], "")),
      vec![
        "hyprland:20",
        "firefox",
        "firefox:11",
        "firefox:10",
        "firefox:12",
        "sleep:30",
      ]
    );

    // Under a different sort the children reorder with the group, and stay
    // attached to it.
    assert_eq!(
      names(&rows(SortKey::Memory, false, &["firefox"], "")),
      vec![
        "hyprland:20",
        "firefox",
        "firefox:10",
        "firefox:11",
        "firefox:12",
        "sleep:30",
      ]
    );
  }

  #[test]
  fn filters_on_the_process_name() {
    // Without a matcher this falls back to a substring search, which is what the
    // panel does when the matcher pool is exhausted.
    assert_eq!(
      names(&rows(SortKey::Cpu, false, &[], "fire")),
      vec!["firefox"]
    );
    assert!(names(&rows(SortKey::Cpu, false, &[], "nothing")).is_empty());
  }

  #[test]
  fn a_filtered_group_keeps_its_children() {
    assert_eq!(
      names(&rows(SortKey::Cpu, false, &["firefox"], "fire")),
      vec!["firefox", "firefox:11", "firefox:10", "firefox:12"]
    );
  }

  /// The whole point of grouping on the path: three unrelated apps that all
  /// report a name of `electron` stay three separate rows.
  #[test]
  fn processes_sharing_a_name_but_not_a_binary_are_separate_groups() {
    let electron = snapshot(vec![
      executable_process(1, "electron", "/apps/slack/electron", 5.0, 1.0, 100),
      executable_process(2, "electron", "/apps/slack/electron", 5.0, 1.0, 100),
      executable_process(3, "electron", "/apps/discord/electron", 1.0, 1.0, 100),
      executable_process(4, "electron", "/apps/discord/electron", 1.0, 1.0, 100),
    ]);

    let rows = grouped(&electron, "", SortKey::Cpu);

    assert_eq!(rows.len(), 2, "one group per binary, not one per name");
    assert_eq!(rows[0].executable, "/apps/slack/electron");
    assert_eq!(rows[0].targets.to_vec(), vec![1, 2]);
    assert_eq!(rows[1].executable, "/apps/discord/electron");
    assert_eq!(rows[1].targets.to_vec(), vec![3, 4]);
  }

  #[test]
  fn expanding_one_group_leaves_its_namesake_collapsed() {
    let electron = snapshot(vec![
      executable_process(1, "electron", "/apps/slack/electron", 5.0, 1.0, 100),
      executable_process(2, "electron", "/apps/slack/electron", 5.0, 1.0, 100),
      executable_process(3, "electron", "/apps/discord/electron", 1.0, 1.0, 100),
      executable_process(4, "electron", "/apps/discord/electron", 1.0, 1.0, 100),
    ]);

    let expanded = HashSet::from([RowKey::Group(GroupKey {
      name: SharedString::from("electron"),
      executable: SharedString::from("/apps/slack/electron"),
    })]);

    let rows = build_rows(
      &electron,
      "",
      SortKey::Cpu,
      false,
      true,
      ViewMode::Grouped,
      &expanded,
      None,
    );

    assert_eq!(
      rows.iter().map(|row| row.pid).collect::<Vec<_>>(),
      vec![None, Some(1), Some(2), None],
      "only the slack group should have unfolded"
    );
  }

  #[test]
  fn children_do_not_repeat_the_group_path() {
    let rows = rows(SortKey::Cpu, false, &["firefox"], "");
    let children = rows.iter().filter(|row| row.depth > 0).collect::<Vec<_>>();

    assert!(!children.is_empty());
    assert!(children.iter().all(|row| row.executable.is_empty()));
  }

  fn child(pid: Pid, parent: Pid, name: &str) -> ProcessInfo {
    let mut process = process(pid, name, 0.0, 0.0, 10);
    process.parent_pid = parent;
    process
  }

  /// A session laid out the way a real one is: a supervisor at the top, a
  /// terminal under it, and unrelated processes that merely share a name.
  fn session() -> Snapshot {
    snapshot(vec![
      child(1, 0, "systemd"),
      child(10, 1, "ghostty"),
      child(11, 10, "zsh"),
      child(12, 11, "node"),
      child(20, 1, "slack"),
      child(21, 20, "node"),
    ])
  }

  #[test]
  fn tree_mode_nests_by_parent() {
    assert_eq!(
      shape(&tree(&session(), "", &[1, 10, 11, 20])),
      vec![
        "systemd:1",
        "  ghostty:10",
        "    zsh:11",
        "      node:12",
        "  slack:20",
        "    node:21",
      ]
    );
  }

  #[test]
  fn tree_mode_folds_at_the_rows_that_are_closed() {
    // Only the root is unfolded, so its children show but their children do not.
    assert_eq!(
      shape(&tree(&session(), "", &[1])),
      vec!["systemd:1", "  ghostty:10", "  slack:20"]
    );
  }

  #[test]
  fn a_tree_row_counts_and_sums_its_whole_subtree() {
    let rows = tree(&session(), "", &[]);
    let root = rows.first().expect("the root should be listed");

    assert_eq!(root.name, "systemd");
    assert_eq!(root.count, Some(6), "itself and everything under it");
    // A tree row stands for its subtree, but only signals itself; reaching the
    // descendants is what the prompt's tree options are for.
    assert_eq!(root.targets.to_vec(), vec![1]);
  }

  /// The two `node` processes are unrelated, so unlike grouped mode the tree
  /// keeps them where they actually live.
  #[test]
  fn tree_mode_does_not_collapse_unrelated_namesakes() {
    let rows = tree(&session(), "", &[1, 10, 11, 20]);
    let nodes = rows
      .iter()
      .filter(|row| row.name == "node")
      .collect::<Vec<_>>();

    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].depth, 3, "under ghostty's shell");
    assert_eq!(nodes[1].depth, 2, "under slack");
  }

  #[test]
  fn searching_a_tree_reveals_the_branch_leading_to_a_match() {
    // `zsh` is three levels down and nothing else matches, so its ancestors come
    // along to give it context, and nothing needs unfolding by hand.
    assert_eq!(
      shape(&tree(&session(), "zsh", &[])),
      vec!["systemd:1", "  ghostty:10", "    zsh:11"]
    );
  }

  #[test]
  fn searching_a_tree_keeps_every_branch_that_matches() {
    assert_eq!(
      shape(&tree(&session(), "node", &[])),
      vec![
        "systemd:1",
        "  ghostty:10",
        "    zsh:11",
        "      node:12",
        "  slack:20",
        "    node:21",
      ]
    );
  }

  #[test]
  fn a_parent_loop_still_lists_everyone_in_tree_mode() {
    // No root reaches these, so without the orphan sweep they would vanish.
    let looped = snapshot(vec![child(10, 11, "a"), child(11, 10, "b")]);
    let rows = tree(&looped, "", &[10, 11]);

    assert_eq!(rows.len(), 2, "every process should be listed exactly once");
  }

  #[test]
  fn collects_a_whole_tree_parents_first() {
    let processes = vec![
      child(1, 0, "init"),
      child(10, 1, "shell"),
      child(11, 10, "make"),
      child(12, 11, "cc"),
      child(13, 11, "ld"),
      child(20, 1, "unrelated"),
    ];

    assert_eq!(process_tree(&processes, &[10]), vec![10, 11, 12, 13]);
    assert_eq!(process_tree(&processes, &[11]), vec![11, 12, 13]);
    assert_eq!(
      process_tree(&processes, &[12]),
      vec![12],
      "a leaf is its own whole tree"
    );
  }

  #[test]
  fn a_tree_covers_every_root_without_repeating_anyone() {
    let processes = vec![
      child(1, 0, "init"),
      child(10, 1, "parent"),
      child(11, 10, "shared"),
    ];

    // The second root is already a descendant of the first, so it must not be
    // signalled twice.
    assert_eq!(process_tree(&processes, &[10, 11]), vec![10, 11]);
  }

  #[test]
  fn a_parent_loop_does_not_hang_the_walk() {
    // Parent ids are read across a whole sweep of /proc, so they are not
    // guaranteed to describe a real tree.
    let processes = vec![child(10, 11, "a"), child(11, 10, "b")];

    assert_eq!(process_tree(&processes, &[10]), vec![10, 11]);
  }

  #[test]
  fn we_signal_ourselves_last() {
    let ourselves = std::process::id() as Pid;
    let processes = vec![
      child(10, 1, "parent"),
      child(ourselves, 10, "launch"),
      child(11, 10, "sibling"),
    ];

    let tree = process_tree(&processes, &[10]);

    assert_eq!(
      tree.last(),
      Some(&ourselves),
      "killing our own ancestor should still finish the job first"
    );
    assert_eq!(tree.len(), 3);
  }

  #[test]
  fn strips_nix_store_hashes() {
    assert_eq!(
      strip_nix_hashes("/nix/store/1zqjq8h9wr2f3kd5m0plxc7nvbsyga4j-slack-4.35.126/bin/slack"),
      "/nix/store/slack-4.35.126/bin/slack"
    );

    // Not a hash, so it is left alone.
    assert_eq!(
      strip_nix_hashes("/nix/store/not-a-hash/bin/thing"),
      "/nix/store/not-a-hash/bin/thing"
    );
    assert_eq!(
      strip_nix_hashes("/nix/store/1zqjq8h9wr2f3kd5m0plxc7nvbsyga4j"),
      "/nix/store/1zqjq8h9wr2f3kd5m0plxc7nvbsyga4j",
      "a bare hash with nothing after it has nothing worth showing instead"
    );
    assert_eq!(strip_nix_hashes("/usr/bin/slack"), "/usr/bin/slack");
  }

  #[test]
  fn trims_from_the_front_so_the_binary_survives() {
    assert_eq!(
      trim_leading_components("/usr/bin/slack", 40),
      "/usr/bin/slack"
    );
    assert_eq!(
      trim_leading_components("/one/two/three/four/five/six/seven/eight", 20),
      "…/six/seven/eight"
    );
    // Nothing to trim: a single component longer than the budget is left as is
    // for the layout to ellipsise.
    assert_eq!(
      trim_leading_components("/verylongsinglecomponent", 5),
      "/verylongsinglecomponent"
    );
  }

  #[test]
  fn shortens_a_realistic_nix_path() {
    let shortened =
      shorten_path("/nix/store/1zqjq8h9wr2f3kd5m0plxc7nvbsyga4j-slack-4.35.126/bin/slack");

    assert_eq!(shortened, "/nix/store/slack-4.35.126/bin/slack");
    assert!(shortened.chars().count() <= PATH_BUDGET);
  }

  #[test]
  fn formats_byte_counts() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(999), "999 B");
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(200 * 1024), "200 KiB");
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    assert_eq!(format_rate(1024), "1.0 KiB/s");
  }
}
