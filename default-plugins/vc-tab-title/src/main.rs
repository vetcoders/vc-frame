//! vc-tab-title — background plugin that auto-titles tabs from the running
//! command of their focused terminal pane.
//!
//! Contract (operator doctrine):
//! - Protected names are never touched: "Start here", "Shell", spawn names
//!   (`scaf-*`, `resume-*`, `marbles-*`, run-id shaped), and anything the user
//!   set manually (any name that is neither the default "Tab #N" nor a label
//!   this plugin applied earlier).
//! - Only "soft" names are replaced: "Tab #N", "shell", or our own previous
//!   auto-label.
//! - The label comes from the foreground child of the focused pane's shell
//!   (never the PID-1 shell itself); a bare shell falls back to basename(cwd).
//! - Agent labels are sticky: when an agent process exits and the pane drops
//!   back to a bare shell, the last agent label stays (less flicker).
//! - Renames are debounced so short-lived commands do not flash the tab bar.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use zellij_tile::prelude::*;

const DEBOUNCE_SECS: f64 = 1.5;
const MAX_LABEL_LEN: usize = 12;

/// Names that are never auto-renamed, even if they somehow look soft.
const PROTECTED_EXACT: &[&str] = &["Start here", "Shell"];

/// Spawn/workflow tab names owned by the Vibecrafted dispatcher.
const PROTECTED_PREFIXES: &[&str] = &["scaf-", "resume-", "marbles-"];

/// Agent allowlist in priority order: (argv token, tab label).
/// Tokens match a basename exactly or as a `<token>-`/`<token>.` prefix,
/// so `aicx-mcp` maps to `aicx` and `claude` inside a node argv still hits.
const AGENT_TOKENS: &[(&str, &str)] = &[
    ("grok", "grok"),
    ("codex", "codex"),
    ("claude", "claude"),
    ("agy", "agy"),
    ("junie", "junie"),
    ("gemini", "gemini"),
    ("voc", "voc"),
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
    /// Panes per tab position, from the last PaneUpdate.
    panes: HashMap<usize, Vec<PaneInfo>>,
    /// Latest known command per terminal pane id: (argv, is_foreground_child).
    pane_commands: HashMap<u32, (Vec<String>, bool)>,
    /// Latest known cwd per terminal pane id.
    pane_cwds: HashMap<u32, PathBuf>,
    /// Labels this plugin applied, keyed by stable tab id. A tab whose current
    /// name matches its entry here is still "ours" and may be renamed again.
    auto_labels: HashMap<usize, String>,
    /// Labels computed but not yet applied (waiting out the debounce window).
    pending: HashMap<usize, String>,
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
                let live_tab_ids: Vec<usize> = tabs.iter().map(|t| t.tab_id).collect();
                self.auto_labels.retain(|id, _| live_tab_ids.contains(id));
                self.pending.retain(|id, _| live_tab_ids.contains(id));
                self.tabs = tabs;
                self.recompute_and_arm();
            },
            Event::PaneUpdate(pane_manifest) => {
                let mut live_terminal_ids: Vec<u32> = Vec::new();
                for panes in pane_manifest.panes.values() {
                    for pane in panes {
                        if !pane.is_plugin {
                            live_terminal_ids.push(pane.id);
                        }
                    }
                }
                self.pane_commands
                    .retain(|id, _| live_terminal_ids.contains(id));
                self.pane_cwds
                    .retain(|id, _| live_terminal_ids.contains(id));
                self.panes = pane_manifest.panes;
                self.recompute_and_arm();
            },
            Event::CommandChanged(
                PaneId::Terminal(terminal_id),
                command,
                is_foreground,
                _focused_client_ids,
            ) => {
                self.pane_commands
                    .insert(terminal_id, (command, is_foreground));
                self.recompute_and_arm();
            },
            Event::CwdChanged(PaneId::Terminal(terminal_id), cwd, _focused_client_ids) => {
                self.pane_cwds.insert(terminal_id, cwd);
                self.recompute_and_arm();
            },
            Event::Timer(_) => {
                self.timer_armed = false;
                self.apply_stable_labels();
            },
            _ => {},
        }
        false // background-only plugin, never renders
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // Background-only plugin. Never rendered. Intentionally empty.
    }
}

impl State {
    /// Recompute desired labels and arm the debounce timer when a rename is
    /// wanted. Labels are applied later, in the Timer handler, and only if
    /// they are still wanted then — short-lived commands never hit the tab bar.
    fn recompute_and_arm(&mut self) {
        let desired = self.desired_labels();
        // Drop pending renames that are no longer wanted.
        self.pending
            .retain(|id, label| desired.get(id) == Some(label));
        for (tab_id, label) in desired {
            self.pending.insert(tab_id, label);
        }
        if !self.pending.is_empty() && !self.timer_armed {
            set_timeout(DEBOUNCE_SECS);
            self.timer_armed = true;
        }
    }

    /// Apply pending labels that survived the debounce window unchanged.
    fn apply_stable_labels(&mut self) {
        let desired = self.desired_labels();
        let pending = std::mem::take(&mut self.pending);
        for (tab_id, label) in pending {
            if desired.get(&tab_id) == Some(&label) {
                rename_tab_with_id(tab_id as u64, &label);
                self.auto_labels.insert(tab_id, label);
            } else if let Some(new_label) = desired.get(&tab_id) {
                // Changed mid-window: keep waiting for it to settle.
                self.pending.insert(tab_id, new_label.clone());
            }
        }
        if !self.pending.is_empty() {
            set_timeout(DEBOUNCE_SECS);
            self.timer_armed = true;
        }
    }

    /// The full map of renames we currently want: tab id -> new label.
    /// A tab is absent when it is protected, user-named, already correct,
    /// or when we have nothing better to offer.
    fn desired_labels(&self) -> HashMap<usize, String> {
        let mut desired = HashMap::new();
        for tab in &self.tabs {
            let previous_auto_label = self.auto_labels.get(&tab.tab_id).map(|s| s.as_str());
            if !is_soft_name(&tab.name, tab.position, previous_auto_label) {
                continue;
            }
            let Some(label) = self.label_for_tab(tab) else {
                continue;
            };
            if label == tab.name {
                continue;
            }
            // Sticky agent labels: a dead agent's tab keeps its name instead
            // of flashing back to a shell/cwd label.
            if previous_auto_label == Some(tab.name.as_str())
                && is_agent_label(&tab.name)
                && !is_agent_label(&label)
            {
                continue;
            }
            desired.insert(tab.tab_id, label);
        }
        desired
    }

    /// Compute the label for a tab from its focused terminal pane.
    fn label_for_tab(&self, tab: &TabInfo) -> Option<String> {
        let panes = self.panes.get(&tab.position)?;
        let pane = panes
            .iter()
            .find(|p| !p.is_plugin && p.is_focused)
            .or_else(|| panes.iter().find(|p| !p.is_plugin))?;
        let (command, is_foreground) = self.pane_commands.get(&pane.id)?;
        let label = if *is_foreground && !command.is_empty() {
            match classify_command(command) {
                CommandClass::Agent(label) => label.to_string(),
                CommandClass::Shell => self.cwd_label(pane.id),
                CommandClass::Other(name) => name,
            }
        } else {
            self.cwd_label(pane.id)
        };
        let label = truncate_label(&label);
        if label.is_empty() { None } else { Some(label) }
    }

    fn cwd_label(&self, terminal_id: u32) -> String {
        self.pane_cwds
            .get(&terminal_id)
            .and_then(|cwd| cwd.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "shell".to_string())
    }
}

enum CommandClass {
    /// A known fleet/infra process from the allowlist.
    Agent(&'static str),
    /// A shell with no interesting child.
    Shell,
    /// Anything else: labeled by its executable basename.
    Other(String),
}

/// Map an argv to a tab label class. Scans every token (basename, lowercased)
/// against the agent allowlist so interpreter-wrapped agents still match
/// (`node /usr/local/bin/claude ...` -> claude).
fn classify_command(command: &[String]) -> CommandClass {
    if command.is_empty() {
        return CommandClass::Shell;
    }
    for token in command {
        let token = token_basename(token);
        for (name, label) in AGENT_TOKENS {
            if token_matches(&token, name) {
                return CommandClass::Agent(label);
            }
        }
        if token.starts_with("vc-") {
            return CommandClass::Agent("vc");
        }
    }
    // Loose pass: MLX infra runs as `python …mlx…`, so the marker can sit
    // anywhere inside a script/module token, not on a name boundary.
    for token in command {
        if token_basename(token).contains("mlx") {
            return CommandClass::Agent("mlx");
        }
    }
    let exe = token_basename(&command[0]);
    if SHELLS.contains(&exe.as_str()) {
        return CommandClass::Shell;
    }
    CommandClass::Other(exe)
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
/// this tab's position, a bare "shell", or the label we applied ourselves.
/// Protected names and anything the user typed are never soft.
fn is_soft_name(name: &str, tab_position: usize, previous_auto_label: Option<&str>) -> bool {
    if PROTECTED_EXACT.contains(&name)
        || PROTECTED_PREFIXES.iter().any(|p| name.starts_with(p))
        || looks_like_run_id(name)
    {
        return false;
    }
    name == format!("Tab #{}", tab_position + 1)
        || name == "shell"
        || previous_auto_label == Some(name)
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

fn is_agent_label(label: &str) -> bool {
    AGENT_TOKENS.iter().any(|(_, l)| *l == label)
}

fn truncate_label(label: &str) -> String {
    let label = label.trim();
    label.chars().take(MAX_LABEL_LEN).collect()
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
        assert!(is_soft_name("Tab #3", 2, None));
        // "Tab #3" on a tab that moved to position 0 is no longer the default
        // name for that position — but our own label always stays soft.
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
        assert_eq!(truncate_label("family-onko-portal"), "family-onko-");
        assert_eq!(truncate_label("  codex  "), "codex");
    }
}
