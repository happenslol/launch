use std::{
  collections::{BTreeMap, HashMap, HashSet},
  mem,
  os::unix::{fs::PermissionsExt, process::CommandExt},
  path::{Path, PathBuf},
  process::{self, Stdio},
};

use anyhow::{Context as _, Result, bail};
use freedesktop_desktop_entry::{DesktopEntry, Iter, default_paths};
use gpui::{App, Entity, Global, Resource, Task, prelude::*};
use tracing::debug;

use crate::{config::ConfigState, db::DB, launcher::RootItem, util::ResultExt};

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

  pub fn lookup(&mut self, items: Vec<(String, Option<String>)>, cx: &mut Context<Self>) {
    let items: Vec<(String, Option<String>)> = items
      .into_iter()
      .filter(|(name, _)| !self.cache.contains_key(name))
      .collect();

    if items.is_empty() {
      return;
    }

    cx.spawn(async move |this, cx| {
      let entries = cx
        .background_spawn(async move {
          let mut entries = HashMap::new();
          for (name, theme_path) in &items {
            if let Some(path) = get_icon(name, theme_path.as_deref()) {
              entries.insert(name.clone(), Resource::Path(path.into()));
            }
          }
          entries
        })
        .await;

      if !entries.is_empty() {
        this
          .update(cx, |this, cx| {
            this.cache.extend(entries);
            cx.notify();
          })
          .log_err();
      }
    })
    .detach();
  }

  pub fn refresh(&mut self, locales: Vec<String>, cx: &mut Context<Self>) {
    if self.refresh_task.is_some() {
      return;
    }

    self.refresh_task = Some(cx.spawn(async move |this, cx| {
      let (db_entries, cache_entries) = cx
        .background_spawn(async move {
          let entries: Vec<_> = Iter::new(default_paths()).entries(Some(&locales)).collect();

          let mut db_entries = HashMap::new();
          let mut cache_entries = HashMap::new();

          for entry in &entries {
            let Some(icon_name) = entry.icon() else {
              continue;
            };

            let Some(icon_path) = get_icon(icon_name, None) else {
              continue;
            };

            db_entries
              .entry(icon_name.to_string())
              .or_insert_with(|| icon_path.clone());

            let resource = Resource::Path(icon_path.into());

            cache_entries
              .entry(icon_name.to_string())
              .or_insert_with(|| resource.clone());

            let name = entry.name(&locales).unwrap_or_default();
            cache_entries
              .entry(name.to_lowercase())
              .or_insert_with(|| resource.clone());

            cache_entries
              .entry(entry.appid.to_lowercase())
              .or_insert_with(|| resource.clone());

            if let Some(generic_name) = entry.generic_name(&locales) {
              cache_entries
                .entry(generic_name.to_lowercase())
                .or_insert_with(|| resource.clone());
            }
          }

          (db_entries, cache_entries)
        })
        .await;

      DB.store_desktop_entry_icon_paths(&db_entries);

      this
        .update(cx, |this, cx| {
          this.cache.extend(cache_entries);
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
    if !is_launchable(&entry) || result.contains_key(&entry.appid) {
      continue;
    }

    // Without a name there is nothing to search for or draw, so the entry could
    // only ever be picked by accident.
    let Some(name) = entry.name(locales) else {
      continue;
    };
    let name = name.to_string();

    if let Some(icon) = entry.icon() {
      icon_names.insert(icon.to_string());
    }

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

pub fn get_icon(name: &str, theme_path: Option<&str>) -> Option<PathBuf> {
  // Some apps put an absolute path in icon_name directly
  let path = Path::new(name);
  if path.is_absolute() && path.is_file() {
    return Some(path.to_path_buf());
  }

  // SNI's IconThemePath should be searched before the standard XDG dirs
  if let Some(theme_path) = theme_path
    && let Some(found) = find_in_theme_path(theme_path, name)
  {
    return Some(found);
  }

  freedesktop_icons::lookup(name)
    .with_cache()
    .with_scale(1)
    .with_size(24)
    .find()
}

fn find_in_theme_path(dir: &str, name: &str) -> Option<PathBuf> {
  // Most apps drop icons directly in the theme path, no theme structure
  for ext in ["svg", "png"] {
    let path = PathBuf::from(dir).join(format!("{name}.{ext}"));
    if path.is_file() {
      return Some(path);
    }
  }

  // Otherwise scan, preferring SVG
  let mut svg_match: Option<PathBuf> = None;
  let mut png_match: Option<PathBuf> = None;
  for entry in walkdir::WalkDir::new(dir)
    .max_depth(5)
    .into_iter()
    .filter_map(|e| e.ok())
  {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
      continue;
    };
    if stem != name {
      continue;
    }
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
      continue;
    };
    match ext {
      "svg" if svg_match.is_none() => svg_match = Some(path.to_path_buf()),
      "png" if png_match.is_none() => png_match = Some(path.to_path_buf()),
      _ => {}
    }
  }
  svg_match.or(png_match)
}

/// Whether an entry describes something the launcher can actually start.
///
/// Entries that fail this would still show up as results, and picking one would
/// do nothing at all - which reads as the launcher being broken rather than the
/// entry being unlaunchable.
fn is_launchable(entry: &DesktopEntry) -> bool {
  // `NoDisplay` means "runnable, but not from a menu" (URL handlers, MIME-only
  // helpers); `Hidden` means the entry has been deleted by a user override and
  // is to be treated as if it were not there.
  if entry.no_display() || entry.hidden() {
    return false;
  }

  // `Type=Link` and `Type=Directory` entries name no program to run. Entries
  // that omit `Type` are given the benefit of the doubt, since the `Exec` check
  // below already rejects the ones that could not be started anyway.
  if entry.type_().is_some_and(|kind| kind != "Application") {
    return false;
  }

  if entry.exec().is_none() {
    return false;
  }

  // `TryExec` names a program whose presence stands in for the app being
  // installed. Packages that are removed without their desktop file being
  // cleaned up are the usual reason it is missing.
  entry
    .try_exec()
    .is_none_or(|program| find_executable(program).is_some())
}

/// One argument of an `Exec` line, before its field codes are resolved.
struct ExecArg {
  text: String,
  /// The spec forbids field codes inside a quoted argument, so an argument that
  /// came out of quotes is passed to the app exactly as written.
  quoted: bool,
}

/// Splits an `Exec` value into its arguments, honouring the desktop entry
/// spec's quoting rules.
///
/// This stands in for [`DesktopEntry::parse_exec`], which splits on whitespace
/// alone: it tears a quoted path containing spaces into several arguments, and
/// rejects outright any entry whose first argument is quoted while its last is
/// not - a common shape, e.g. `Exec="/path/with spaces/app" %U`.
fn split_exec(exec: &str) -> Result<Vec<ExecArg>> {
  let mut args: Vec<ExecArg> = Vec::new();
  let mut text = String::new();
  let mut quoted = false;
  let mut started = false;
  let mut characters = exec.chars();

  while let Some(character) = characters.next() {
    match character {
      ' ' | '\t' => {
        if started {
          args.push(ExecArg {
            text: mem::take(&mut text),
            quoted,
          });
          quoted = false;
          started = false;
        }
      }
      '"' => {
        started = true;
        quoted = true;
        loop {
          match characters.next() {
            Some('"') => break,
            // Inside quotes a backslash escapes only these four characters.
            // Everywhere else it stands for itself, which matters on Windows-style
            // paths and on the many entries that quote a `sh -c` argument.
            Some('\\') => match characters.next() {
              Some(escaped @ ('"' | '`' | '$' | '\\')) => text.push(escaped),
              Some(other) => {
                text.push('\\');
                text.push(other);
              }
              None => bail!("Exec ends with a backslash inside a quoted argument"),
            },
            Some(other) => text.push(other),
            None => bail!("Exec has an unterminated quoted argument"),
          }
        }
      }
      other => {
        started = true;
        text.push(other);
      }
    }
  }

  if started {
    args.push(ExecArg { text, quoted });
  }

  if args.is_empty() {
    bail!("Exec is empty");
  }

  Ok(args)
}

/// Resolves the field codes in one unquoted `Exec` argument.
///
/// Returns the arguments it stands for: none if the argument drops out, and
/// occasionally two, since `%i` expands to a flag and its value.
fn expand_field_codes(argument: &str, entry: &DesktopEntry, locales: &[String]) -> Vec<String> {
  // `%i` is the one field code that expands to more than one argument, so it is
  // only meaningful on its own.
  if argument == "%i" {
    return match entry.icon() {
      Some(icon) => vec!["--icon".to_string(), icon.to_string()],
      None => Vec::new(),
    };
  }

  let mut expanded = String::with_capacity(argument.len());
  let mut characters = argument.chars();

  while let Some(character) = characters.next() {
    if character != '%' {
      expanded.push(character);
      continue;
    }

    match characters.next() {
      Some('%') => expanded.push('%'),
      // The launcher always starts apps empty-handed, so an argument asking for
      // a file or a URL has nothing to stand in for and is dropped whole.
      Some('f' | 'F' | 'u' | 'U') => return Vec::new(),
      Some('c') => {
        if let Some(name) = entry.name(locales) {
          expanded.push_str(&name);
        }
      }
      // `entry.path` is where the desktop file itself lives, as opposed to
      // `entry.path()`, which is the `Path` key used as a working directory.
      Some('k') => expanded.push_str(&entry.path.to_string_lossy()),
      // Deprecated codes (%d %D %n %N %v %m) and anything unrecognised are
      // dropped rather than handed to the app as a literal.
      Some(_) | None => {}
    }
  }

  if expanded.is_empty() {
    Vec::new()
  } else {
    vec![expanded]
  }
}

/// Terminal emulators the launcher knows how to hand a command to, most
/// preferred first, each with the arguments that have to precede that command.
///
/// `xdg-terminal-exec` leads because it is the freedesktop proposal for exactly
/// this and defers to whichever terminal the user configured.
const KNOWN_TERMINALS: &[(&str, &[&str])] = &[
  ("xdg-terminal-exec", &[]),
  ("ghostty", &["-e"]),
  ("kitty", &[]),
  ("foot", &[]),
  ("alacritty", &["-e"]),
  ("wezterm", &["start", "--"]),
  ("gnome-terminal", &["--"]),
  ("konsole", &["-e"]),
  ("xterm", &["-e"]),
];

/// The command prefix that runs a program inside a terminal, for entries with
/// `Terminal=true`. `configured` is the user's `apps.terminal` setting, which
/// wins over anything found on `PATH`.
fn terminal_command(configured: Option<Vec<String>>) -> Result<Vec<String>> {
  if let Some(configured) = configured.filter(|configured| !configured.is_empty()) {
    return Ok(configured);
  }

  // `$TERMINAL` is only a program name, so it still has to be one whose flag for
  // running a command we know. An unknown one is assumed to take the command
  // straight away, which is the more common convention of the two.
  let preferred = std::env::var("TERMINAL")
    .ok()
    .filter(|name| !name.is_empty());
  let preferred = preferred.as_deref().map(|name| {
    let arguments = KNOWN_TERMINALS
      .iter()
      .find(|(known, _)| *known == name)
      .map_or(&[][..], |(_, arguments)| *arguments);
    (name, arguments)
  });

  preferred
    .into_iter()
    .chain(KNOWN_TERMINALS.iter().copied())
    .find(|(name, _)| find_executable(name).is_some())
    .map(|(name, arguments)| {
      let mut command = Vec::with_capacity(arguments.len() + 1);
      command.push(name.to_string());
      command.extend(arguments.iter().map(|argument| argument.to_string()));
      command
    })
    .context("No terminal emulator found; set `terminal` under `[apps]` in the config")
}

/// Resolves a program name the way `execvp` would: as a path if it contains a
/// slash, otherwise by walking `PATH`.
fn find_executable(program: &str) -> Option<PathBuf> {
  let is_executable = |path: &Path| {
    std::fs::metadata(path)
      .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
  };

  if program.contains('/') {
    let path = Path::new(program);
    return is_executable(path).then(|| path.to_path_buf());
  }

  std::env::split_paths(&std::env::var_os("PATH")?)
    .map(|directory| directory.join(program))
    .find(|path| is_executable(path))
}

/// Builds the command line for a desktop entry: its `Exec`, wrapped in a
/// terminal when the entry asks for one.
fn command_line(
  entry: &DesktopEntry,
  locales: &[String],
  configured_terminal: Option<Vec<String>>,
) -> Result<Vec<String>> {
  let exec = entry.exec().context("Entry has no Exec")?;

  let mut command: Vec<String> = Vec::new();
  for argument in split_exec(exec)? {
    if argument.quoted {
      command.push(argument.text);
    } else {
      command.extend(expand_field_codes(&argument.text, entry, locales));
    }
  }

  if command.is_empty() {
    bail!("Exec expanded to no arguments");
  }

  if entry.terminal() {
    let mut wrapped = terminal_command(configured_terminal)?;
    wrapped.extend(command);
    return Ok(wrapped);
  }

  Ok(command)
}

/// Starts a program detached from the launcher.
///
/// The child gets its own process group, so a signal aimed at the launcher or at
/// the terminal it was started from does not reach it, and `/dev/null` for stdio,
/// so it cannot scribble over the launcher's own output.
///
/// Spawning goes through `async_process` rather than `std::process` because the
/// launcher is a long-lived daemon: `async_process` keeps a single reaper thread
/// that collects each child once it exits, so dropping the handle here does not
/// leave a zombie behind for the rest of the session.
pub fn spawn_detached(command: &[String], working_directory: Option<&str>) -> Result<()> {
  let (program, arguments) = command.split_first().context("Empty command line")?;
  debug!(?command, ?working_directory, "Spawning");

  // The program is passed by name, not as the path it resolves to, so that it
  // reaches the app as its own `argv[0]` - which is what some of them derive
  // their window class from.
  let mut inner = process::Command::new(program);
  inner.args(arguments).process_group(0);

  if let Some(directory) = working_directory.filter(|directory| !directory.is_empty()) {
    inner.current_dir(directory);
  }

  // The stdio has to be set through the `async_process` wrapper: its `spawn`
  // replaces whatever the inner command configured with `Stdio::inherit`.
  async_process::Command::from(inner)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .with_context(|| format!("Spawning `{program}`"))?;

  Ok(())
}

pub fn open_url(url: &str) -> Result<()> {
  spawn_detached(&["xdg-open".to_string(), url.to_string()], None)
}

/// The argument list a desktop entry starts as, terminal wrapping and all.
///
/// This is resolved separately from spawning so that an entry which cannot be
/// turned into a command at all is reported before anything else is attempted,
/// and so that the D-Bus activation path has the fallback ready in hand.
pub fn command_for(entry: &DesktopEntry, locales: &[String], cx: &App) -> Result<Vec<String>> {
  command_line(entry, locales, ConfigState::get(cx).apps.terminal)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Builds an entry from the given `Desktop Entry` group body.
  fn entry(body: &str) -> DesktopEntry {
    DesktopEntry::from_str::<&str>(
      Path::new("/usr/share/applications/test.desktop"),
      &format!("[Desktop Entry]\nType=Application\n{body}"),
      None,
    )
    .expect("entry should parse")
  }

  /// The `Exec` of `entry`, split and with its field codes resolved.
  fn arguments(body: &str) -> Vec<String> {
    let entry = entry(body);
    let exec = entry.exec().expect("entry should have an Exec");
    split_exec(exec)
      .expect("Exec should split")
      .into_iter()
      .flat_map(|argument| {
        if argument.quoted {
          vec![argument.text]
        } else {
          expand_field_codes(&argument.text, &entry, &[])
        }
      })
      .collect()
  }

  #[test]
  fn plain_exec_splits_on_whitespace() {
    assert_eq!(arguments("Exec=kitty -1"), ["kitty", "-1"]);
  }

  #[test]
  fn quoted_argument_keeps_its_spaces() {
    assert_eq!(
      arguments(r#"Exec="/opt/My App/bin/app" --flag"#),
      ["/opt/My App/bin/app", "--flag"]
    );
  }

  #[test]
  fn quoted_first_argument_does_not_reject_the_entry() {
    // `DesktopEntry::parse_exec` fails on this shape, which is why it is not used.
    assert_eq!(
      arguments(r#"Exec="/nix/store/hash-pkg/bin/.wrapped" --handle-uri %u"#),
      ["/nix/store/hash-pkg/bin/.wrapped", "--handle-uri"]
    );
  }

  #[test]
  fn backslash_inside_quotes_escapes_the_four_special_characters() {
    // Two levels of escaping apply: the desktop file's own `\\` -> `\`, then the
    // quoting rules of `Exec`, where a backslash escapes `"`, backtick, `$` and
    // itself.
    assert_eq!(
      arguments(r#"Exec=sh -c "echo \\"quoted\\" for \\$HOME""#),
      ["sh", "-c", r#"echo "quoted" for $HOME"#]
    );
  }

  #[test]
  fn backslash_before_anything_else_stands_for_itself() {
    assert_eq!(arguments(r#"Exec=app "C:\\\\dir""#), ["app", r#"C:\dir"#]);
  }

  #[test]
  fn unterminated_quote_is_an_error() {
    assert!(split_exec(r#"foo "bar"#).is_err());
  }

  #[test]
  fn file_and_url_codes_drop_their_whole_argument() {
    assert_eq!(arguments("Exec=firefox %u"), ["firefox"]);
    assert_eq!(arguments("Exec=nvim %F"), ["nvim"]);
    assert_eq!(arguments("Exec=app --file=%f --flag"), ["app", "--flag"]);
  }

  #[test]
  fn deprecated_and_unknown_codes_are_not_passed_on() {
    assert_eq!(arguments("Exec=app %d %z --flag"), ["app", "--flag"]);
  }

  #[test]
  fn double_percent_is_a_literal_percent() {
    assert_eq!(arguments("Exec=app 100%%"), ["app", "100%"]);
  }

  #[test]
  fn icon_code_expands_to_a_flag_and_a_value() {
    assert_eq!(
      arguments("Exec=app %i\nIcon=my-icon"),
      ["app", "--icon", "my-icon"]
    );
    // With no `Icon` there is nothing for the flag to point at.
    assert_eq!(arguments("Exec=app %i"), ["app"]);
  }

  #[test]
  fn name_and_desktop_file_codes_expand() {
    assert_eq!(arguments("Exec=app %c\nName=Test App"), ["app", "Test App"]);
    assert_eq!(
      arguments("Exec=app %k"),
      ["app", "/usr/share/applications/test.desktop"]
    );
  }

  #[test]
  fn field_codes_inside_quotes_are_left_alone() {
    assert_eq!(arguments(r#"Exec=app "%u""#), ["app", "%u"]);
  }

  #[test]
  fn entries_without_a_runnable_program_are_not_offered() {
    assert!(is_launchable(&entry("Exec=app\nName=App")));
    assert!(!is_launchable(&entry("Exec=app\nName=App\nNoDisplay=true")));
    assert!(!is_launchable(&entry("Exec=app\nName=App\nHidden=true")));
    assert!(!is_launchable(&entry("Name=App")));
    assert!(!is_launchable(&entry(
      "Exec=app\nName=App\nTryExec=/nonexistent/program"
    )));
  }

  #[test]
  fn configured_terminal_wins_over_detection() {
    let configured = vec!["myterm".to_string(), "-x".to_string()];
    assert_eq!(
      terminal_command(Some(configured.clone())).expect("configured terminal"),
      configured
    );
    // An empty list is treated as unset rather than as an empty command, so
    // detection takes over - which either finds a terminal or fails outright.
    assert_ne!(terminal_command(Some(Vec::new())).ok(), Some(Vec::new()));
  }

  #[test]
  fn terminal_entries_are_wrapped_in_a_terminal() {
    let entry = entry("Exec=nvim %F\nName=Neovim\nTerminal=true");
    let terminal = vec!["myterm".to_string(), "-e".to_string()];
    assert_eq!(
      command_line(&entry, &[], Some(terminal)).expect("command line"),
      ["myterm", "-e", "nvim"]
    );
  }

  #[test]
  fn non_terminal_entries_are_not_wrapped() {
    let entry = entry("Exec=firefox %u\nName=Firefox");
    let terminal = vec!["myterm".to_string(), "-e".to_string()];
    assert_eq!(
      command_line(&entry, &[], Some(terminal)).expect("command line"),
      ["firefox"]
    );
  }

  /// The PIDs of our own children that have exited but not been reaped.
  fn zombie_children() -> Vec<i32> {
    let us = std::process::id();
    let mut zombies = Vec::new();

    for entry in std::fs::read_dir("/proc").expect("/proc should be readable") {
      let Ok(entry) = entry else { continue };
      let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
        continue;
      };
      let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
        continue;
      };
      // The command name is parenthesised and may itself contain spaces and
      // parentheses, so the numbered fields start after its closing bracket.
      let Some(fields) = stat.rsplit_once(") ").map(|(_, fields)| fields) else {
        continue;
      };
      let mut fields = fields.split(' ');
      let state = fields.next();
      let parent = fields.next().and_then(|parent| parent.parse::<u32>().ok());

      if state == Some("Z") && parent == Some(us) {
        zombies.push(pid);
      }
    }

    zombies
  }

  #[test]
  fn a_missing_program_is_reported_instead_of_looking_like_a_launch() {
    // The old fork/exec pair could not report this: the exec failed in the
    // child, while the parent had already treated the fork as a launch.
    spawn_detached(&["launch-no-such-program".to_string()], None)
      .expect_err("a missing program should fail to spawn");
  }

  #[test]
  fn spawned_children_do_not_pile_up_as_zombies() {
    spawn_detached(&["true".to_string()], None).expect("`true` should spawn");

    // Reaping happens on `async_process`'s own thread, so give it a moment.
    for _ in 0..200 {
      if zombie_children().is_empty() {
        return;
      }
      std::thread::sleep(std::time::Duration::from_millis(10));
    }

    panic!("children were left unreaped: {:?}", zombie_children());
  }

  /// The process group named by a `/proc/<pid>/stat` line.
  fn process_group(stat: &str) -> Option<u32> {
    // The command name is parenthesised and may itself contain spaces and
    // parentheses, so the numbered fields start after its closing bracket.
    let fields = stat.rsplit_once(") ")?.1;
    // After the command name come the state, the parent, and then the group.
    fields.split(' ').nth(2)?.parse().ok()
  }

  /// The process group of the running test process.
  fn our_process_group() -> u32 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
    process_group(&stat).expect("stat should have a process group")
  }

  #[test]
  fn children_run_in_the_given_directory_and_their_own_process_group() {
    let directory = std::env::temp_dir().join(format!("launch-spawn-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("test directory should be creatable");
    let marker = directory.join("group");

    spawn_detached(
      &[
        "sh".to_string(),
        "-c".to_string(),
        // Written relative to the working directory, so landing in the right
        // place is itself part of what is being checked. The group is read out
        // of `/proc` rather than with `ps`, which is absent from the sandbox
        // the tests are also run in.
        "cp /proc/$$/stat group".to_string(),
      ],
      Some(&directory.to_string_lossy()),
    )
    .expect("`sh` should spawn");

    let mut written = None;
    for _ in 0..200 {
      if let Ok(contents) = std::fs::read_to_string(&marker)
        && let Some(group) = process_group(&contents)
      {
        written = Some(group);
        break;
      }
      std::thread::sleep(std::time::Duration::from_millis(10));
    }

    std::fs::remove_dir_all(&directory).ok();

    let group = written.expect("the child should have written its process group");
    assert_ne!(
      group,
      our_process_group(),
      "the child should not share our process group"
    );
  }

  #[test]
  fn link_entries_are_not_offered() {
    let link = DesktopEntry::from_str::<&str>(
      Path::new("/usr/share/applications/link.desktop"),
      "[Desktop Entry]\nType=Link\nName=Link\nURL=https://example.com\n",
      None,
    )
    .expect("entry should parse");
    assert!(!is_launchable(&link));
  }
}
