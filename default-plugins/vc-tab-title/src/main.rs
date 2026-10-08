//! Concise command labels for the existing compact-bar renderer.
//! Stored tab/pane names remain untouched: explicit names override native OSC
//! only when the user actually supplied them. Closing the workspace Shell
//! creates a fresh shell while the workspace envelope is still alive.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use zellij_tile::prelude::*;

const DEBOUNCE_SECS: f64 = 1.5;
const MAX_LABEL_LEN: usize = 24;
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
    /// Last display projection, never ownership of a stored tab or pane name.
    auto_labels: HashMap<usize, (String, String)>,
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
            Event::CwdChanged(PaneId::Terminal(id), cwd, _) => {
                if self.shell_terminal_id() == Some(id) {
                    self.shell_cwd = Some(cwd);
                }
            },
            Event::CommandChanged(PaneId::Terminal(id), command, _, _) => {
                self.pane_commands.insert(id, command);
                self.recompute_and_arm();
            },
            Event::Timer(_) => {
                self.timer_armed = false;
                self.apply_stable_labels();
            },
            _ => {},
        }
        false
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if message.name == DISPLAY_REGISTER_MESSAGE {
            if let PipeSource::Plugin(id) = message.source {
                if !self.compact_bars.contains(&id) {
                    self.compact_bars.push(id);
                }
                // The renderer supplies its actual runtime id, including the
                // session canvas which is absent from the pane manifest.
                for (tab_id, label) in self.desired_labels() {
                    let base = &self.tabs.iter().find(|t| t.tab_id == tab_id).unwrap().name;
                    pipe_message_to_plugin(
                        MessageToPlugin::new(DISPLAY_LABEL_MESSAGE)
                            .with_destination_plugin_id(id)
                            .with_payload(format!("{tab_id}\n{base}\n{label}")),
                    );
                }
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
            if !self.pane_commands.contains_key(&pane.id) {
                if let Some(command) = query(pane.id) {
                    self.pane_commands.insert(pane.id, command);
                }
            }
        }
    }

    fn publish_label(&self, tab_id: usize, base: &str, label: &str) {
        for bar in &self.compact_bars {
            pipe_message_to_plugin(
                MessageToPlugin::new(DISPLAY_LABEL_MESSAGE)
                    .with_destination_plugin_id(*bar)
                    .with_payload(format!("{tab_id}\n{base}\n{label}")),
            );
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
        }
        self.pending
            .retain(|id, label| desired.get(id) == Some(label));
        for (id, label) in desired {
            let base = &self.tabs.iter().find(|t| t.tab_id == id).unwrap().name;
            if self.auto_labels.get(&id) != Some(&(base.clone(), label.clone())) {
                self.pending.insert(id, label);
            }
        }
        if !self.pending.is_empty() && !self.timer_armed {
            set_timeout(DEBOUNCE_SECS);
            self.timer_armed = true;
        }
    }

    fn apply_stable_labels(&mut self) {
        let desired = self.desired_labels();
        let pending = std::mem::take(&mut self.pending);
        for (id, label) in pending {
            if desired.get(&id) == Some(&label) && !self.compact_bars.is_empty() {
                let base = &self.tabs.iter().find(|t| t.tab_id == id).unwrap().name;
                self.publish_label(id, base, &label);
                self.auto_labels.insert(id, (base.clone(), label));
            } else if let Some(new_label) = desired.get(&id) {
                self.pending.insert(id, new_label.clone());
            }
        }
        if !self.pending.is_empty() {
            set_timeout(DEBOUNCE_SECS);
            self.timer_armed = true;
        }
    }

    fn desired_labels(&self) -> HashMap<usize, String> {
        self.tabs
            .iter()
            .filter(|tab| tab.name == "Shell" || is_soft_name(&tab.name, tab.tab_id, None))
            .filter_map(|tab| self.label_for_tab(tab).map(|label| (tab.tab_id, label)))
            .collect()
    }

    fn label_for_tab(&self, tab: &TabInfo) -> Option<String> {
        let panes = self.panes.get(&tab.position)?;
        let pane = panes
            .iter()
            .find(|p| !p.is_plugin && p.is_selectable && p.is_focused)
            .or_else(|| panes.iter().find(|p| !p.is_plugin && p.is_selectable))?;
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
        let mut state = State::default();
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
        let mut queries = 0;
        state.hydrate_commands_with(|id| {
            assert_eq!(id, 7);
            queries += 1;
            Some(argv(&["node", "/bin/codex", "resume", "private"]))
        });
        state.hydrate_commands_with(|_| panic!("must not create a second command producer"));
        assert_eq!(queries, 1);
        assert_eq!(state.desired_labels().get(&2).unwrap(), "codex");
        state
            .pane_commands
            .insert(7, argv(&["cargo", "build", "--token", "secret"]));
        assert_eq!(state.desired_labels().get(&2).unwrap(), "cargo build");
        state.pane_commands.insert(7, argv(&["/bin/zsh", "-l"]));
        assert_eq!(state.desired_labels().get(&2).unwrap(), "Shell");
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
