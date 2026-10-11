//! Concise command labels for the existing compact-bar renderer.
//! Stored tab/pane names remain untouched: explicit names override native OSC
//! only when the user actually supplied them. Closing the workspace Shell
//! creates a fresh shell while the workspace envelope is still alive.
//!
//! Activity: a program's OSC title changes without any PaneUpdate, so while a
//! tab carries a command label the producer samples that pane's title on its
//! own clock and projects the native spinner and sanitized conversation topic.
//! Topics are display facts, never stored tab names or command arguments.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use zellij_tile::prelude::*;

const DEBOUNCE_SECS: f64 = 1.5;
/// One clock drives both the debounce and activity sampling.
const TICK_SECS: f64 = 0.3;
const MAX_LABEL_LEN: usize = 24;
/// Text-only protocol limits; a native spinner may add two display cells.
const MAX_DISPLAY_LABEL_WIDTH: usize = 80;
const MAX_DISPLAY_LABEL_BYTES: usize = 320;
const DISPLAY_LABEL_MESSAGE: &str = "vc_tab_command_label";
const DISPLAY_REGISTER_MESSAGE: &str = "vc_tab_command_labels_register";
const PROTECTED_EXACT: &[&str] = &["Start here", "Shell"];
const PROTECTED_PREFIXES: &[&str] = &["scaf-", "resume-", "marbles-"];

const AGENT_TOKENS: &[(&str, &str)] = &[
    ("grok", "grok"),
    ("codex", "codex"),
    ("claude", "claude"),
    ("agy", "agy"),
    ("junie", "junie"),
    ("gemini", "gemini"),
    ("voc", "Voc"),
    ("vc-o", "Voc"),
    ("vibecrafted", "vc"),
    ("rmcp-mux", "mux"),
    ("aicx", "aicx"),
    ("lbrx-stt", "stt"),
    ("mlx", "mlx"),
];

const SHELLS: &[&str] = &[
    "zsh", "bash", "fish", "sh", "nu", "dash", "tcsh", "ksh", "csh",
];

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    panes: HashMap<usize, Vec<PaneInfo>>,
    pane_commands: HashMap<u32, Vec<String>>,
    compact_bars: Vec<u32>,
    shell_tab: Option<usize>,
    shell_cwd: Option<PathBuf>,
    /// Last display projection (stored name, label) per tab id — never
    /// ownership of a stored tab or pane name.
    auto_labels: HashMap<usize, (String, String)>,
    /// Projections waiting out the debounce window, with the clock value at
    /// which each was first wanted.
    pending: HashMap<usize, ((String, String), f64)>,
    /// Native spinner frame and topic last published per labeled tab.
    activity: HashMap<usize, char>,
    native_topics: HashMap<usize, String>,
    /// Sum of fired timer intervals — the plugin's own clock.
    clock: f64,
    timer_armed: bool,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        subscribe(&[
            EventType::TabUpdate,
            EventType::PaneUpdate,
            EventType::CommandChanged,
            EventType::CwdChanged,
            EventType::Timer,
        ]);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::TabUpdate(tabs) => {
                if self.observe_shell_tabs(&tabs) {
                    self.shell_tab = new_tab(
                        Some("Shell".to_owned()),
                        self.shell_cwd.as_ref().map(|p| p.display().to_string()),
                    );
                }
                self.auto_labels
                    .retain(|id, _| tabs.iter().any(|t| t.tab_id == *id));
                self.pending
                    .retain(|id, _| tabs.iter().any(|t| t.tab_id == *id));
                self.activity
                    .retain(|id, _| tabs.iter().any(|t| t.tab_id == *id));
                self.native_topics
                    .retain(|id, _| tabs.iter().any(|t| t.tab_id == *id));
                self.tabs = tabs;
                self.recompute_and_arm();
            },
            Event::PaneUpdate(manifest) => {
                self.record_panes(manifest);
                // Change events are deltas, not a startup snapshot. Hydrate once
                // per newly seen terminal through the existing live OS query.
                self.hydrate_commands_with(|id| {
                    get_pane_running_command(PaneId::Terminal(id)).ok()
                });
                if self.shell_cwd.is_none() {
                    self.shell_cwd = self
                        .shell_terminal_id()
                        .and_then(|id| get_pane_cwd(PaneId::Terminal(id)).ok());
                }
                self.recompute_and_arm();
            },
            Event::CwdChanged(PaneId::Terminal(id), cwd, _)
                if self.shell_terminal_id() == Some(id) =>
            {
                self.shell_cwd = Some(cwd);
            },
            Event::CommandChanged(PaneId::Terminal(id), command, _, _) => {
                self.pane_commands.insert(id, command);
                self.recompute_and_arm();
            },
            Event::Timer(elapsed) => {
                self.timer_armed = false;
                self.clock += elapsed;
                for id in self.take_due_labels() {
                    self.publish(id);
                }
                let changed = self.sample_activity_with(|id| {
                    get_pane_info(PaneId::Terminal(id)).map(|pane| pane.title)
                });
                for id in changed {
                    self.publish(id);
                }
                self.arm_tick();
            },
            _ => {},
        }
        false
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if message.name == DISPLAY_REGISTER_MESSAGE
            && let PipeSource::Plugin(id) = message.source
        {
            if !self.compact_bars.contains(&id) {
                self.compact_bars.push(id);
            }
            // The renderer supplies its actual runtime id, including the
            // session canvas which is absent from the pane manifest.
            for (tab_id, (base, label)) in self.desired_labels() {
                send_label(id, tab_id, &base, &self.display_label(tab_id, &label));
            }
        }
        false
    }

    fn render(&mut self, _rows: usize, _cols: usize) {}
}

impl State {
    /// A close observed through either UI or CLI uses the same TabUpdate.
    /// Clear ownership before requesting a replacement, so stale snapshots
    /// cannot produce duplicates. A stopped/empty workspace never resurrects.
    fn observe_shell_tabs(&mut self, tabs: &[TabInfo]) -> bool {
        let envelope = ["Start here", "Agents"]
            .iter()
            .all(|name| tabs.iter().any(|t| t.name == *name));
        if !envelope {
            self.shell_tab = None;
            return false;
        }
        if let Some(shell) = tabs.iter().find(|t| t.name == "Shell") {
            self.shell_tab = Some(shell.tab_id);
            return false;
        }
        if self
            .shell_tab
            .is_some_and(|id| !tabs.iter().any(|t| t.tab_id == id))
        {
            self.shell_tab = None;
            return true;
        }
        false
    }

    fn shell_terminal_id(&self) -> Option<u32> {
        let tab = self
            .tabs
            .iter()
            .find(|t| Some(t.tab_id) == self.shell_tab)?;
        self.panes
            .get(&tab.position)?
            .iter()
            .find(|p| !p.is_plugin && p.is_selectable)
            .map(|p| p.id)
    }

    fn record_panes(&mut self, manifest: PaneManifest) {
        let terminal_ids: Vec<u32> = manifest
            .panes
            .values()
            .flatten()
            .filter(|p| !p.is_plugin)
            .map(|p| p.id)
            .collect();
        self.pane_commands.retain(|id, _| terminal_ids.contains(id));
        self.panes = manifest.panes;
    }

    fn hydrate_commands_with(&mut self, mut query: impl FnMut(u32) -> Option<Vec<String>>) {
        for pane in self.panes.values().flatten().filter(|p| !p.is_plugin) {
            if let std::collections::hash_map::Entry::Vacant(slot) =
                self.pane_commands.entry(pane.id)
                && let Some(command) = query(pane.id)
            {
                slot.insert(command);
            }
        }
    }

    fn publish_label(&self, tab_id: usize, base: &str, label: &str) {
        for bar in &self.compact_bars {
            send_label(*bar, tab_id, base, label);
        }
    }

    /// Re-send one applied projection with its current activity frame.
    fn publish(&self, tab_id: usize) {
        if let Some((base, label)) = self.auto_labels.get(&tab_id) {
            self.publish_label(tab_id, base, &self.display_label(tab_id, label));
        }
    }

    fn display_label(&self, tab_id: usize, label: &str) -> String {
        // Registration may replay a new desired command before debounce; it
        // must not inherit the applied command's topic or native frame.
        if self
            .auto_labels
            .get(&tab_id)
            .is_none_or(|(_, applied)| applied != label)
        {
            return label.to_owned();
        }
        let label = self
            .native_topics
            .get(&tab_id)
            .map(|topic| format!("{label} · {topic}"))
            .unwrap_or_else(|| label.to_owned());
        match self.activity.get(&tab_id) {
            Some(frame) => format!("{frame} {label}"),
            None => label,
        }
    }

    fn recompute_and_arm(&mut self) {
        let desired = self.desired_labels();
        let stale: Vec<usize> = self
            .auto_labels
            .keys()
            .copied()
            .filter(|id| !desired.contains_key(id))
            .collect();
        for id in stale {
            self.publish_label(id, "", "");
            self.auto_labels.remove(&id);
            self.activity.remove(&id);
            self.native_topics.remove(&id);
        }
        self.pending
            .retain(|id, (projection, _)| desired.get(id) == Some(projection));
        for (id, projection) in desired {
            if self.auto_labels.get(&id) != Some(&projection) && !self.pending.contains_key(&id) {
                self.pending.insert(id, (projection, self.clock));
            }
        }
        self.arm_tick();
    }

    /// Pending projections that stayed wanted for the whole debounce window
    /// become applied labels; short-lived commands never reach the bar.
    fn take_due_labels(&mut self) -> Vec<usize> {
        let due: Vec<usize> = self
            .pending
            .iter()
            .filter(|(_, (_, since))| self.clock - since >= DEBOUNCE_SECS - 1e-6)
            .map(|(id, _)| *id)
            .collect();
        for id in &due {
            if let Some((projection, _)) = self.pending.remove(id) {
                // A new command starts without the previous one's frame.
                self.activity.remove(id);
                self.native_topics.remove(id);
                self.auto_labels.insert(*id, projection);
            }
        }
        due
    }

    /// Sample native presentation on the existing clock. A topic-only change
    /// or clear must publish even when the braille frame stays unchanged.
    fn sample_activity_with(
        &mut self,
        mut title_of: impl FnMut(u32) -> Option<String>,
    ) -> Vec<usize> {
        let labeled: Vec<usize> = self
            .auto_labels
            .iter()
            .filter(|(_, (_, label))| label != "Shell")
            .map(|(id, _)| *id)
            .collect();
        let mut changed = Vec::new();
        for tab_id in labeled {
            let before = self.display_label(tab_id, &self.auto_labels[&tab_id].1);
            let label = &self.auto_labels[&tab_id].1;
            let pane = self
                .tabs
                .iter()
                .find(|t| t.tab_id == tab_id)
                .filter(|tab| self.label_for_tab(tab).as_ref() == Some(label))
                .and_then(|tab| self.label_pane(tab))
                .filter(|pane| !pane.exited && !pane.is_held);
            let title = pane
                .and_then(|pane| title_of(pane.id))
                .and_then(|title| sanitize_native_title(&title));
            let frame = title.as_deref().and_then(activity_frame);
            // The native title is the topic authority. argv is only used to
            // refuse the server's full-command fallback, never to derive a topic.
            let topic = pane
                .and_then(|pane| self.pane_commands.get(&pane.id))
                .filter(|command| matches!(classify_command(command), CommandClass::Agent(_)))
                .and_then(|command| {
                    let command_title = sanitize_native_title(&command.join(" "))?;
                    title.as_deref().and_then(|title| {
                        (title
                            .strip_prefix(|c: char| ('\u{2801}'..='\u{28ff}').contains(&c))
                            .and_then(|rest| rest.strip_prefix(' '))
                            .unwrap_or(title)
                            != command_title
                                .strip_prefix(|c: char| ('\u{2801}'..='\u{28ff}').contains(&c))
                                .and_then(|rest| rest.strip_prefix(' '))
                                .unwrap_or(&command_title))
                        .then(|| native_topic(title, label))
                        .flatten()
                    })
                });
            match frame {
                Some(frame) => {
                    self.activity.insert(tab_id, frame);
                },
                None => {
                    self.activity.remove(&tab_id);
                },
            }
            match topic {
                Some(topic) => {
                    self.native_topics.insert(tab_id, topic);
                },
                None => {
                    self.native_topics.remove(&tab_id);
                },
            }
            if before != self.display_label(tab_id, label) {
                changed.push(tab_id);
            }
        }
        changed
    }

    /// Tick while anything waits out the debounce or a label can show activity.
    fn arm_tick(&mut self) {
        let sampling = self.auto_labels.values().any(|(_, label)| label != "Shell");
        if (sampling || !self.pending.is_empty()) && !self.timer_armed {
            set_timeout(TICK_SECS);
            self.timer_armed = true;
        }
    }

    /// tab id -> (stored name the label was computed over, label). The stored
    /// name travels with the label so the renderer can refuse a projection
    /// over a tab that was renamed in the meantime.
    fn desired_labels(&self) -> HashMap<usize, (String, String)> {
        self.tabs
            .iter()
            .filter(|tab| tab.name == "Shell" || is_soft_name(&tab.name, tab.tab_id, None))
            .filter_map(|tab| {
                self.label_for_tab(tab)
                    .map(|label| (tab.tab_id, (tab.name.clone(), label)))
            })
            .collect()
    }

    /// The terminal a tab's label speaks for: its focused terminal, else its first.
    fn label_pane(&self, tab: &TabInfo) -> Option<&PaneInfo> {
        let panes = self.panes.get(&tab.position)?;
        panes
            .iter()
            .find(|p| !p.is_plugin && p.is_selectable && p.is_focused)
            .or_else(|| panes.iter().find(|p| !p.is_plugin && p.is_selectable))
    }

    fn label_for_tab(&self, tab: &TabInfo) -> Option<String> {
        let pane = self.label_pane(tab)?;
        if pane.exited || pane.is_held {
            return Some("Shell".to_string());
        }
        let command = self.pane_commands.get(&pane.id)?;
        Some(truncate_label(&match classify_command(command) {
            CommandClass::Agent(label) => label.to_string(),
            CommandClass::Shell => "Shell".to_string(),
            CommandClass::Other(label) => label,
        }))
    }
}

/// One projection to one renderer: `tab_id\nstored_name\nlabel`. An empty
/// name and label clears the projection for that tab.
fn send_label(renderer: u32, tab_id: usize, base: &str, label: &str) {
    pipe_message_to_plugin(
        MessageToPlugin::new(DISPLAY_LABEL_MESSAGE)
            .with_destination_plugin_id(renderer)
            .with_payload(format!("{tab_id}\n{base}\n{label}")),
    );
}

/// A native spinner frame: the title starts with one braille cell and a space
/// (Codex-style `⠦ task | project`). Only the frame leaves this function.
fn activity_frame(title: &str) -> Option<char> {
    let mut chars = title.chars();
    let first = chars.next()?;
    (('\u{2801}'..='\u{28ff}').contains(&first) && chars.next() == Some(' ')).then_some(first)
}

/// Parse ANSI once for both the native frame and topic, then remove controls
/// and bidi formatting and collapse whitespace for the one-line protocol.
fn sanitize_native_title(title: &str) -> Option<String> {
    let clean = String::from_utf8(strip_ansi_escapes::strip(title).ok()?).ok()?;
    let clean: String = clean.chars().filter(|c| {
        (!c.is_control() || c.is_whitespace())
            && !matches!(*c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
    }).collect();
    Some(clean.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// A meaningful sanitized title, bounded in terminal cells and UTF-8 bytes.
fn native_topic(clean: &str, label: &str) -> Option<String> {
    let text = clean
        .strip_prefix(|c: char| ('\u{2801}'..='\u{28ff}').contains(&c))
        .and_then(|rest| rest.strip_prefix(' '))
        .unwrap_or(clean);
    let head = text.split('|').next().unwrap_or(text).trim().to_lowercase();
    let provider = label.to_lowercase();
    if text.is_empty()
        || head == provider
        || AGENT_TOKENS
            .iter()
            .any(|(_, agent)| head.eq_ignore_ascii_case(agent))
        || matches!(
            head.as_str(),
            "shell" | "terminal" | "codex cli" | "claude code"
        )
        || ["idle", "running", "working"]
            .iter()
            .any(|state| head == format!("{provider} {state}"))
    {
        return None;
    }
    let max_width = MAX_DISPLAY_LABEL_WIDTH.saturating_sub(label.width() + 3);
    let max_bytes = MAX_DISPLAY_LABEL_BYTES.saturating_sub(label.len() + " · ".len());
    let mut out = String::new();
    let mut width = 0;
    for c in text.chars() {
        let next_width = c.width().unwrap_or(0);
        if width + next_width > max_width || out.len() + c.len_utf8() > max_bytes {
            break;
        }
        width += next_width;
        out.push(c);
    }
    let out = out.trim_end();
    (width > 0 && !out.is_empty()).then(|| out.to_owned())
}

#[derive(Debug, PartialEq, Eq)]
enum CommandClass {
    Agent(&'static str),
    Shell,
    Other(String),
}

/// Inspect executable/script metadata, never prompt or option argument tokens.
fn classify_command(command: &[String]) -> CommandClass {
    let Some(first) = command.first() else {
        return CommandClass::Shell;
    };
    let exe = token_basename(first);
    let interpreter = matches!(exe.as_str(), "node" | "nodejs" | "bun" | "deno" | "ruby")
        || exe.starts_with("python");
    let identity = if interpreter {
        command
            .get(if command.get(1).is_some_and(|s| s == "-m") {
                2
            } else {
                1
            })
            .filter(|s| !s.starts_with('-'))
            .map(|s| token_basename(s))
            .unwrap_or_else(|| exe.clone())
    } else {
        exe.clone()
    };
    for (name, label) in AGENT_TOKENS {
        if token_matches(&identity, name) {
            return CommandClass::Agent(label);
        }
    }
    if identity.starts_with("vc-") {
        return CommandClass::Agent("vc");
    }
    if interpreter && identity.contains("mlx") {
        return CommandClass::Agent("mlx");
    }
    if SHELLS.contains(&exe.as_str()) {
        return CommandClass::Shell;
    }
    // Only established public subcommands are safe activity metadata. Arbitrary
    // argv[1] may be a prompt, credential, URL, filename, or custom private task.
    let subcommand = command.get(1).filter(|sub| match exe.as_str() {
        "cargo" => matches!(
            sub.as_str(),
            "build" | "check" | "test" | "run" | "clippy" | "fmt" | "clean" | "update"
        ),
        "git" => matches!(
            sub.as_str(),
            "status"
                | "diff"
                | "log"
                | "show"
                | "fetch"
                | "pull"
                | "push"
                | "commit"
                | "rebase"
                | "merge"
        ),
        "npm" | "pnpm" | "yarn" => {
            matches!(sub.as_str(), "install" | "build" | "test" | "run" | "dev")
        },
        "uv" => matches!(sub.as_str(), "run" | "sync" | "pip" | "build" | "lock"),
        "docker" => matches!(
            sub.as_str(),
            "build" | "run" | "compose" | "pull" | "push" | "ps"
        ),
        _ => false,
    });
    CommandClass::Other(subcommand.map(|sub| format!("{exe} {sub}")).unwrap_or(exe))
}

/// Basename of a path-ish argv token, lowercased, login-shell dash stripped.
fn token_basename(token: &str) -> String {
    token
        .rsplit('/')
        .next()
        .unwrap_or(token)
        .trim_start_matches('-')
        .to_lowercase()
}

/// `aicx` matches `aicx` and `aicx-mcp`/`aicx.py`, but not `aicxfoo`.
fn token_matches(token: &str, name: &str) -> bool {
    token == name
        || token
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'))
}

/// A name is soft (safe to auto-replace) when it is the default "Tab #N" for
/// this tab's stable id, a bare "shell", or the label we applied ourselves.
/// Protected names and anything the user typed are never soft.
fn is_soft_name(name: &str, tab_id: usize, previous_auto_label: Option<&str>) -> bool {
    if PROTECTED_EXACT.contains(&name)
        || PROTECTED_PREFIXES.iter().any(|p| name.starts_with(p))
        || looks_like_run_id(name)
    {
        return false;
    }
    name == format!("Tab #{}", tab_id + 1) || name == "shell" || previous_auto_label == Some(name)
}

/// Vibecrafted spawn names carry run ids shaped like `work-260722-075023-97000`;
/// any `<word>-<6 digits>-` name is treated as dispatcher-owned.
fn looks_like_run_id(name: &str) -> bool {
    let mut parts = name.split('-');
    let Some(head) = parts.next() else {
        return false;
    };
    let Some(stamp) = parts.next() else {
        return false;
    };
    !head.is_empty()
        && head.chars().all(|c| c.is_ascii_alphabetic())
        && stamp.len() == 6
        && stamp.chars().all(|c| c.is_ascii_digit())
}

fn truncate_label(label: &str) -> String {
    label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(MAX_LABEL_LEN)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agents_match_directly_and_through_interpreters() {
        for (cmd, expected) in [
            (argv(&["grok", "--resume"]), "grok"),
            (argv(&["codex", "exec", "-"]), "codex"),
            (
                argv(&["node", "/usr/local/bin/claude", "--continue"]),
                "claude",
            ),
            (argv(&["vibecrafted", "workflow", "claude"]), "vc"),
            (argv(&["voc"]), "Voc"),
            (argv(&["vc-o"]), "Voc"),
            (argv(&["/opt/bin/aicx-mcp"]), "aicx"),
            (argv(&["rmcp-mux", "--port", "9"]), "mux"),
            (argv(&["python3", "run_mlx-server.py"]), "mlx"),
            (argv(&["lbrx-stt", "--listen"]), "stt"),
        ] {
            match classify_command(&cmd) {
                CommandClass::Agent(label) => assert_eq!(label, expected, "cmd: {:?}", cmd),
                _ => panic!("expected agent label {} for {:?}", expected, cmd),
            }
        }
    }

    #[test]
    fn bare_shells_classify_as_shell() {
        for cmd in [argv(&["-zsh"]), argv(&["/bin/bash", "-l"]), argv(&["fish"])] {
            assert!(matches!(classify_command(&cmd), CommandClass::Shell));
        }
    }

    #[test]
    fn unknown_commands_fall_back_to_exe_basename() {
        match classify_command(&argv(&["/usr/bin/htop", "-d", "10"])) {
            CommandClass::Other(name) => assert_eq!(name, "htop"),
            _ => panic!("expected Other"),
        }
    }

    #[test]
    fn token_prefix_matching_is_boundary_sensitive() {
        assert!(token_matches("aicx-mcp", "aicx"));
        assert!(token_matches("aicx.py", "aicx"));
        assert!(!token_matches("aicxfoo", "aicx"));
        // "grokking" must not match "grok"
        assert!(!token_matches("grokking", "grok"));
    }

    #[test]
    fn protected_names_are_never_soft() {
        for name in [
            "Start here",
            "Shell",
            "scaf-260722-073900-12345",
            "resume-260717-201752-39000",
            "marbles-260709-1",
            "work-260722-075023-97000",
            "revi-260717-201752-39000",
            "debug-pensieve", // user-named
        ] {
            assert!(!is_soft_name(name, 0, None), "{} must be protected", name);
        }
    }

    #[test]
    fn soft_names_allow_auto_rename() {
        assert!(is_soft_name("Tab #1", 0, None));
        // A moved tab keeps the default name derived from its stable id. The
        // caller passes that id, so reordering cannot turn a default into a
        // falsely protected user name.
        assert!(is_soft_name("Tab #3", 2, None));
        assert!(!is_soft_name("Tab #3", 0, None));
        assert!(is_soft_name("shell", 5, None));
        assert!(is_soft_name("codex", 1, Some("codex")));
        assert!(!is_soft_name("codex", 1, None)); // same text typed by a user
    }

    #[test]
    fn run_id_shapes_are_dispatcher_owned() {
        assert!(looks_like_run_id("work-260722-075023-97000"));
        assert!(looks_like_run_id("revi-260717"));
        assert!(!looks_like_run_id("my-tab"));
        assert!(!looks_like_run_id("vc-frame"));
        assert!(!looks_like_run_id("shell"));
    }

    #[test]
    fn labels_are_truncated() {
        assert_eq!(truncate_label("family-onko-portal"), "family-onko-portal");
        assert_eq!(truncate_label("a".repeat(30).as_str()).len(), MAX_LABEL_LEN);
        assert_eq!(truncate_label("  codex  "), "codex");
    }

    fn tab(id: usize, name: &str) -> TabInfo {
        TabInfo {
            tab_id: id,
            position: id,
            name: name.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn startup_hydrates_once_and_commands_return_to_shell() {
        let mut state = State {
            tabs: vec![tab(2, "Shell")],
            ..Default::default()
        };
        state.panes.insert(
            2,
            vec![PaneInfo {
                id: 7,
                is_selectable: true,
                is_focused: true,
                ..Default::default()
            }],
        );
        let mut queries = 0;
        state.hydrate_commands_with(|id| {
            assert_eq!(id, 7);
            queries += 1;
            Some(argv(&["node", "/bin/codex", "resume", "private"]))
        });
        state.hydrate_commands_with(|_| panic!("must not create a second command producer"));
        assert_eq!(queries, 1);
        let label = |state: &State| state.desired_labels().get(&2).unwrap().1.clone();
        assert_eq!(label(&state), "codex");
        assert_eq!(state.desired_labels().get(&2).unwrap().0, "Shell");
        state
            .pane_commands
            .insert(7, argv(&["cargo", "build", "--token", "secret"]));
        assert_eq!(label(&state), "cargo build");
        state.pane_commands.insert(7, argv(&["/bin/zsh", "-l"]));
        assert_eq!(label(&state), "Shell");
        assert_eq!(state.tabs[0].name, "Shell");
        state.tabs[0].name = "My work".into();
        assert!(state.desired_labels().is_empty());
    }

    #[test]
    fn arbitrary_arguments_are_not_identity_or_activity() {
        assert_eq!(
            classify_command(&argv(&["echo", "codex", "private"])),
            CommandClass::Other("echo".into())
        );
        assert_eq!(
            classify_command(&argv(&["node", "-e", "codex private"])),
            CommandClass::Other("node".into())
        );
        assert_eq!(
            classify_command(&argv(&["npm", "private-task"])),
            CommandClass::Other("npm".into())
        );
    }

    #[test]
    fn shell_close_rebirth_is_once_and_does_not_resurrect_workspace() {
        let mut state = State::default();
        let envelope = vec![tab(0, "Start here"), tab(1, "Agents")];
        let mut initial = envelope.clone();
        initial.push(tab(2, "Shell"));
        assert!(!state.observe_shell_tabs(&initial));
        assert!(state.observe_shell_tabs(&envelope));
        assert!(!state.observe_shell_tabs(&envelope));
        initial[2].tab_id = 4;
        assert!(!state.observe_shell_tabs(&initial));
        assert!(!state.observe_shell_tabs(&[]));
        assert!(!state.observe_shell_tabs(&envelope));
        assert!(!state.observe_shell_tabs(&initial));
        assert!(!state.observe_shell_tabs(&[tab(1, "Agents")]));
        assert!(!state.observe_shell_tabs(&envelope));
    }
    fn shell_with_agent(state: &mut State) {
        state.tabs = vec![tab(2, "Shell")];
        state.panes.insert(
            2,
            vec![PaneInfo {
                id: 7,
                is_selectable: true,
                is_focused: true,
                ..Default::default()
            }],
        );
        state
            .pane_commands
            .insert(7, argv(&["node", "/usr/local/bin/codex"]));
    }

    fn want(state: &mut State) {
        // recompute_and_arm without the host timer call.
        for (id, projection) in state.desired_labels() {
            if state.auto_labels.get(&id) != Some(&projection) && !state.pending.contains_key(&id) {
                state.pending.insert(id, (projection, state.clock));
            }
        }
    }

    #[test]
    fn labels_wait_out_the_debounce_on_the_plugin_clock() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        want(&mut state);
        for _ in 0..4 {
            state.clock += TICK_SECS;
            assert!(
                state.take_due_labels().is_empty(),
                "too early at {}",
                state.clock
            );
        }
        state.clock += TICK_SECS;
        assert_eq!(state.take_due_labels(), vec![2]);
        assert_eq!(
            state.auto_labels.get(&2),
            Some(&("Shell".to_owned(), "codex".to_owned()))
        );
        assert!(state.pending.is_empty());
    }

    #[test]
    fn activity_preserves_native_frame_and_topic() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        state
            .auto_labels
            .insert(2, ("Shell".into(), "codex".into()));
        let mut title = "⠦ private task title | project".to_owned();
        assert_eq!(state.sample_activity_with(|_| Some(title.clone())), vec![2]);
        assert_eq!(
            state.display_label(2, "codex"),
            "⠦ codex · private task title | project"
        );
        // Same frame: nothing to republish.
        assert!(
            state
                .sample_activity_with(|_| Some(title.clone()))
                .is_empty()
        );
        title = "⠧ another private title".into();
        assert_eq!(state.sample_activity_with(|_| Some(title.clone())), vec![2]);
        assert_eq!(
            state.display_label(2, "codex"),
            "⠧ codex · another private title"
        );
        // Idle: the program drops its spinner, the tab drops the frame.
        assert_eq!(
            state.sample_activity_with(|_| Some("codex idle | project".into())),
            vec![2]
        );
        assert_eq!(state.display_label(2, "codex"), "codex");
        assert!(!state.display_label(2, "codex").contains("private"));
    }

    #[test]
    fn native_topic_changes_clear_and_preserve_spinner_without_command_changes() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        state
            .auto_labels
            .insert(2, ("Shell".into(), "codex".into()));
        assert_eq!(
            state
                .sample_activity_with(|_| Some("⠦ Przyjmij rolę Integratora | vibecrafted".into())),
            vec![2]
        );
        assert_eq!(
            state.display_label(2, "codex"),
            "⠦ codex · Przyjmij rolę Integratora | vibecrafted"
        );
        assert_eq!(
            state.sample_activity_with(|_| Some("⠦ Nowy temat: żółw 🐢".into())),
            vec![2]
        );
        assert_eq!(
            state.display_label(2, "codex"),
            "⠦ codex · Nowy temat: żółw 🐢"
        );
        assert!(
            state
                .sample_activity_with(|_| Some("⠦ Nowy temat: żółw 🐢".into()))
                .is_empty()
        );
        assert_eq!(
            state.sample_activity_with(|_| Some("⠦ codex".into())),
            vec![2]
        );
        assert_eq!(state.display_label(2, "codex"), "⠦ codex");
        assert_eq!(state.sample_activity_with(|_| Some(String::new())), vec![2]);
        assert_eq!(state.display_label(2, "codex"), "codex");
    }

    #[test]
    fn native_topic_sanitizes_ansi_controls_and_bounds_unicode() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        state
            .auto_labels
            .insert(2, ("Shell".into(), "codex".into()));
        assert_eq!(
            state.sample_activity_with(|_| Some(
                "\u{1b}[31m⠦ Żółw\u{1b}[0m\n  🐢\u{7}\u{202e} | projekt".into()
            )),
            vec![2]
        );
        assert_eq!(
            state.display_label(2, "codex"),
            "⠦ codex · Żółw 🐢 | projekt"
        );
        state.sample_activity_with(|_| Some(format!("⠦ {}", "界".repeat(90))));
        let display = state.display_label(2, "codex");
        assert!(display.width() <= MAX_DISPLAY_LABEL_WIDTH + 2);
        assert!(display.len() <= MAX_DISPLAY_LABEL_BYTES + 4);
        assert!(display.ends_with('界'));
        state.sample_activity_with(|_| Some(format!("Temat {}", "\u{301}".repeat(400))));
        let display = state.display_label(2, "codex");
        assert!(display.len() <= MAX_DISPLAY_LABEL_BYTES);
        assert!(display.width() <= MAX_DISPLAY_LABEL_WIDTH);
        for generic in [
            "",
            "codex",
            "codex | project",
            "claude",
            "grok",
            "Codex CLI",
            "Claude Code",
            "Shell",
            "terminal",
        ] {
            assert!(native_topic(generic, "codex").is_none(), "{generic}");
        }
    }

    #[test]
    fn native_topic_never_uses_command_arguments_or_outlives_its_provider() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        state
            .auto_labels
            .insert(2, ("Shell".into(), "codex".into()));
        state
            .pane_commands
            .insert(7, argv(&["codex", "--prompt", "private fixture prompt"]));
        for argument in [
            "private\nfixture prompt",
            "private\tfixture prompt",
            "private\u{1b}[31mfixture\u{1b}[0m prompt",
            "private\u{202e}fixture prompt",
        ] {
            state
                .pane_commands
                .insert(7, argv(&["codex", "--prompt", argument]));
            state.sample_activity_with(|_| Some(format!("⠦ codex --prompt {argument}")));
            assert_eq!(state.display_label(2, "codex"), "⠦ codex");
        }
        state
            .pane_commands
            .insert(7, argv(&["codex", "--prompt", "private fixture prompt"]));
        state.sample_activity_with(|_| Some("⠦ codex --prompt private fixture prompt".into()));
        assert_eq!(state.display_label(2, "codex"), "⠦ codex");
        state.sample_activity_with(|_| Some("Native conversation".into()));
        assert_eq!(state.display_label(2, "claude"), "claude");
        assert_eq!(
            state.display_label(2, "codex"),
            "codex · Native conversation"
        );
        // Before command debounce completes, the old provider loses its topic.
        state.pane_commands.insert(7, argv(&["cargo", "build"]));
        state.sample_activity_with(|_| Some("cargo build --some-private-option".into()));
        assert_eq!(state.display_label(2, "codex"), "codex");
        state
            .auto_labels
            .insert(2, ("Shell".into(), "cargo build".into()));
        state.sample_activity_with(|_| Some("Never a conversation".into()));
        assert_eq!(state.display_label(2, "cargo build"), "cargo build");
        state.pane_commands.insert(7, argv(&["codex"]));
        state
            .auto_labels
            .insert(2, ("Shell".into(), "codex".into()));
        state.sample_activity_with(|_| Some("Another conversation".into()));
        state.panes.get_mut(&2).unwrap()[0].exited = true;
        state.sample_activity_with(|_| Some("Stale title".into()));
        assert_eq!(state.display_label(2, "codex"), "codex");
    }

    #[test]
    fn plain_shell_labels_are_never_sampled() {
        let mut state = State::default();
        shell_with_agent(&mut state);
        state
            .auto_labels
            .insert(2, ("Shell".into(), "Shell".into()));
        let mut queried = false;
        assert!(
            state
                .sample_activity_with(|_| {
                    queried = true;
                    Some("⠦ busy".into())
                })
                .is_empty()
        );
        assert!(!queried);
    }

    #[test]
    fn activity_frame_is_a_leading_braille_cell_only() {
        assert_eq!(activity_frame("⠦ task | proj"), Some('⠦'));
        assert_eq!(activity_frame("⠦task"), None);
        assert_eq!(activity_frame("task ⠦ x"), None);
        assert_eq!(activity_frame("✳ Claude Code"), None);
        assert_eq!(activity_frame(""), None);
    }

    #[test]
    fn failed_startup_query_is_retryable() {
        let mut state = State::default();
        state.panes.insert(
            0,
            vec![PaneInfo {
                id: 7,
                ..Default::default()
            }],
        );
        state.hydrate_commands_with(|_| None);
        assert!(!state.pane_commands.contains_key(&7));
        state.hydrate_commands_with(|_| Some(argv(&["cargo", "build"])));
        assert_eq!(
            state.pane_commands.get(&7).unwrap(),
            &argv(&["cargo", "build"])
        );
    }
}
