use std::time::Duration;

use zellij_tile::prelude::*;

use super::components::{
    Colors, LineToRender, build_pane_ui_line, build_session_ui_line, build_tab_ui_line,
};

#[derive(Debug, Clone, Default)]
pub struct SelectedIndex(pub Option<usize>, pub Option<usize>, pub Option<usize>);

impl SelectedIndex {
    pub fn tabs_are_visible(&self) -> bool {
        self.1.is_some()
    }
    pub fn panes_are_visible(&self) -> bool {
        self.2.is_some()
    }
    pub fn selected_tab_index(&self) -> Option<usize> {
        self.1
    }
    pub fn session_index_is_selected(&self, index: usize) -> bool {
        self.0 == Some(index)
    }
    pub fn result_shrink(&mut self) {
        match self {
            SelectedIndex(Some(_selected_session), None, None) => self.0 = None,
            SelectedIndex(Some(_selected_session), Some(_selected_tab), None) => self.1 = None,
            SelectedIndex(Some(_selected_session), Some(_selected_tab), Some(_selected_pane)) => {
                self.2 = None
            },
            _ => {},
        }
    }
    pub fn reset(&mut self) {
        self.0 = None;
        self.1 = None;
        self.2 = None;
    }
}

#[derive(Debug, Clone)]
pub struct SessionUiInfo {
    /// Raw Zellij session identity used for attach/kill/switch operations.
    pub name: String,
    /// Human-facing title. Canonical control-plane projections may replace the
    /// fallback without changing the session identity.
    pub title: String,
    /// Role is explicit plugin metadata, never a reserved session name.
    pub is_operator_frame: bool,
    pub tabs: Vec<TabUiInfo>,
    pub connected_users: usize,
    pub is_current_session: bool,
    pub creation_time: Duration,
    pub rail_order: u64,
}

impl SessionUiInfo {
    pub fn from_session_info(session_info: &SessionInfo) -> Self {
        SessionUiInfo {
            name: session_info.name.clone(),
            title: operator_frame_title(session_info)
                .map(str::to_owned)
                .unwrap_or_else(|| friendly_session_title(&session_info.name)),
            is_operator_frame: is_internal_host_session(session_info),
            tabs: session_info
                .tabs
                .iter()
                .map(|t| TabUiInfo::new(t, &session_info.panes))
                .collect(),
            connected_users: session_info.connected_clients,
            is_current_session: session_info.is_current_session,
            creation_time: session_info.creation_time,
            rail_order: session_info.rail_order,
        }
    }
    pub fn line_count(&self, selected_index: &SelectedIndex) -> usize {
        let mut line_count = 1;
        if selected_index.tabs_are_visible() {
            match selected_index
                .selected_tab_index()
                .and_then(|i| self.tabs.get(i))
                .map(|t| t.line_count(selected_index))
            {
                Some(line_count_of_selected_tab) => {
                    line_count += line_count_of_selected_tab.saturating_sub(1);
                    line_count += self.tabs.len();
                },
                None => {
                    line_count += self.tabs.len();
                },
            }
        }
        line_count
    }
    pub fn as_line_to_render(
        &self,
        _session_index: u8,
        mut max_cols: usize,
        colors: Colors,
    ) -> LineToRender {
        let mut line_to_render = LineToRender::new(colors);
        let ui_spans = build_session_ui_line(self, colors);
        for span in ui_spans {
            span.render(None, &mut line_to_render, &mut max_cols);
        }
        line_to_render
    }
}

fn friendly_session_title(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() || uuid::Uuid::parse_str(trimmed).is_ok() {
        return "Workspace".to_owned();
    }
    let identity_suffixes = [
        "-impl-", "-work-", "-audi-", "-rese-", "-marb-", "-pola-", "-fwup-",
    ];
    let stem = identity_suffixes
        .iter()
        .filter_map(|marker| trimmed.find(marker))
        .min()
        .map(|index| &trimmed[..index])
        .unwrap_or(trimmed);
    if stem.is_empty()
        || identity_suffixes
            .iter()
            .any(|marker| trimmed.starts_with(marker.trim_start_matches('-')))
    {
        return "Agent workspace".to_owned();
    }
    let title = stem
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if !title.is_empty() {
        title
    } else {
        "Workspace".to_owned()
    }
}

#[derive(Debug, Clone)]
pub struct TabUiInfo {
    pub tab_id: usize,
    pub name: String,
    pub panes: Vec<PaneUiInfo>,
    pub position: usize,
    pub is_active: bool,
    pub has_bell_notification: bool,
    /// Mirrors native topbar attention: exited with nonzero OR unknown status.
    /// Neither case is evidence of a semantic failure inside a live provider.
    pub has_exited_attention: bool,
    pub has_held_pane: bool,
    live_processes: Vec<LiveProcessUiInfo>,
    // Latest native census selection, not identity carried by the producer pipe.
    primary_terminal_is_live: Option<bool>,
}

impl TabUiInfo {
    pub fn new(tab_info: &TabInfo, pane_manifest: &PaneManifest) -> Self {
        let pane_infos = pane_manifest.panes.get(&tab_info.position);
        let panes = pane_infos
            .map(|p| {
                p.iter()
                    .filter_map(|pane_info| {
                        if pane_info.is_selectable && !pane_info.is_suppressed {
                            Some(PaneUiInfo {
                                name: pane_info.title.clone(),
                                exit_code: pane_info.exit_status,
                                pane_id: pane_info.id,
                                is_plugin: pane_info.is_plugin,
                            })
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        let live_processes = pane_infos
            .map(|panes| {
                panes
                    .iter()
                    .filter(|pane| !pane.is_plugin && !pane.exited && !pane.is_held)
                    .map(LiveProcessUiInfo::from_pane_info)
                    .collect()
            })
            .unwrap_or_default();
        let primary_terminal_is_live = pane_infos.map(|panes| {
            panes
                .iter()
                .find(|pane| !pane.is_plugin && pane.is_selectable && pane.is_focused)
                .or_else(|| {
                    panes
                        .iter()
                        .find(|pane| !pane.is_plugin && pane.is_selectable)
                })
                .is_some_and(|pane| !pane.exited && !pane.is_held)
        });
        TabUiInfo {
            tab_id: tab_info.tab_id,
            name: tab_info.name.clone(),
            panes,
            position: tab_info.position,
            is_active: tab_info.active,
            has_bell_notification: tab_info.has_bell_notification || tab_info.is_flashing_bell,
            has_exited_attention: pane_infos.is_some_and(|panes| {
                panes
                    .iter()
                    .any(|pane| !pane.is_plugin && pane.exited && pane.exit_status != Some(0))
            }),
            has_held_pane: pane_infos
                .is_some_and(|panes| panes.iter().any(|pane| !pane.is_plugin && pane.is_held)),
            live_processes,
            primary_terminal_is_live,
        }
    }
    pub fn live_process_count(&self) -> usize {
        self.live_processes.len()
    }
    pub fn primary_process_label(&self) -> Option<&str> {
        self.live_processes
            .iter()
            .find(|process| process.is_focused)
            .or_else(|| self.live_processes.first())
            .map(|process| process.label.as_str())
    }
    pub fn native_tab_activity(&self, projected_activity: Option<bool>) -> bool {
        let native = self
            .live_processes
            .iter()
            .any(|process| crate::native_activity_announced(&process.label));
        // The tab-level pipe carries no pane id. An idle selected terminal must
        // never erase another live terminal's announcement. In a multi-pane tab,
        // native census clears aggregate activity; a busy projection can add it.
        // A known held/exited selection cannot keep a stale projected busy flag.
        if self.primary_terminal_is_live == Some(false) {
            native
        } else if self.live_processes.len() <= 1 {
            projected_activity.unwrap_or(native)
        } else {
            native || projected_activity.unwrap_or(false)
        }
    }
    #[cfg(test)]
    pub fn for_rail_test(
        name: &str,
        is_active: bool,
        process_label: &str,
        live_process_count: usize,
    ) -> Self {
        Self {
            tab_id: 0,
            name: name.to_owned(),
            panes: vec![],
            position: 0,
            is_active,
            has_bell_notification: false,
            has_exited_attention: false,
            has_held_pane: false,
            primary_terminal_is_live: Some(live_process_count > 0),
            live_processes: (0..live_process_count)
                .map(|index| LiveProcessUiInfo {
                    label: process_label.to_owned(),
                    is_focused: index == 0,
                })
                .collect(),
        }
    }
    pub fn line_count(&self, selected_index: &SelectedIndex) -> usize {
        let mut line_count = 1;
        if selected_index.panes_are_visible() {
            line_count += self.panes.len()
        }
        line_count
    }
    pub fn as_line_to_render(
        &self,
        _session_index: u8,
        mut max_cols: usize,
        colors: Colors,
    ) -> LineToRender {
        let mut line_to_render = LineToRender::new(colors);
        let ui_spans = build_tab_ui_line(self, colors);
        for span in ui_spans {
            span.render(None, &mut line_to_render, &mut max_cols);
        }
        line_to_render
    }
}

#[derive(Debug, Clone)]
struct LiveProcessUiInfo {
    label: String,
    is_focused: bool,
}

impl LiveProcessUiInfo {
    fn from_pane_info(pane_info: &PaneInfo) -> Self {
        let title = pane_info.title.trim();
        let label = if !title.is_empty() && !is_generic_pane_title(title) {
            title.to_owned()
        } else {
            pane_info
                .terminal_command
                .as_deref()
                .and_then(command_basename)
                .unwrap_or_else(|| "terminal".to_owned())
        };
        Self {
            label,
            is_focused: pane_info.is_focused,
        }
    }
}

fn is_generic_pane_title(title: &str) -> bool {
    title == "Terminal" || title.starts_with("Pane #")
}

fn command_basename(command: &str) -> Option<String> {
    let executable = command.split_whitespace().next()?;
    let basename = executable.rsplit('/').next().unwrap_or(executable);
    (!basename.is_empty()).then(|| basename.to_owned())
}

#[derive(Debug, Clone)]
pub struct PaneUiInfo {
    pub name: String,
    pub exit_code: Option<i32>,
    pub pane_id: u32,
    pub is_plugin: bool,
}

impl PaneUiInfo {
    pub fn as_line_to_render(
        &self,
        _session_index: u8,
        mut max_cols: usize,
        colors: Colors,
    ) -> LineToRender {
        let mut line_to_render = LineToRender::new(colors);
        let ui_spans = build_pane_ui_line(self, colors);
        for span in ui_spans {
            span.render(None, &mut line_to_render, &mut max_cols);
        }
        line_to_render
    }
}

#[cfg(test)]
mod process_projection_tests {
    use super::*;

    #[test]
    fn host_titles_use_role_and_distinguish_recovery() {
        let host_plugin = PluginInfo {
            location: "session-manager".to_owned(),
            configuration: [("frame_host".to_owned(), "true".to_owned())].into(),
        };
        let mut host = SessionInfo {
            name: "vc-host".to_owned(),
            plugins: [(1, host_plugin)].into(),
            ..Default::default()
        };
        assert_eq!(
            SessionUiInfo::from_session_info(&host).title,
            "Operator Frame"
        );
        host.name = "vc-host-recovered".to_owned();
        assert_eq!(
            SessionUiInfo::from_session_info(&host).title,
            "Operator Frame (recovered)"
        );
        host.plugins.clear();
        assert_eq!(
            SessionUiInfo::from_session_info(&host).title,
            "Vc Host Recovered"
        );
    }

    #[test]
    fn raw_session_identity_is_never_the_primary_title() {
        assert_eq!(
            friendly_session_title("fux-impl-260827-132005-98719"),
            "Fux"
        );
        assert_eq!(
            friendly_session_title("impl-260827-132005-98719"),
            "Agent workspace"
        );
        assert_eq!(
            friendly_session_title("50042b41-2eda-4da5-87c7-472eae3726df"),
            "Workspace"
        );
    }

    fn terminal(title: &str, command: Option<&str>) -> PaneInfo {
        PaneInfo {
            title: title.to_owned(),
            terminal_command: command.map(str::to_owned),
            is_selectable: true,
            ..Default::default()
        }
    }

    #[test]
    fn live_process_projection_ignores_plugins_exited_and_held_panes() {
        let tab_info = TabInfo {
            position: 0,
            name: "impl-260718-120000-01000".to_owned(),
            active: true,
            ..Default::default()
        };
        let mut live = terminal("claude", Some("/Users/me/.local/bin/claude"));
        live.is_focused = true;
        let mut plugin = terminal("Sessions", None);
        plugin.is_plugin = true;
        let mut exited = terminal("old-codex", Some("codex"));
        exited.exited = true;
        let mut held = terminal("failed-run", Some("dispatcher.sh"));
        held.is_held = true;
        let manifest = PaneManifest {
            panes: [(0, vec![plugin, exited, held, live])].into(),
        };

        let projected = TabUiInfo::new(&tab_info, &manifest);

        assert!(projected.is_active);
        assert_eq!(projected.live_process_count(), 1);
        assert_eq!(projected.primary_process_label(), Some("claude"));
    }

    #[test]
    fn live_process_label_falls_back_to_command_basename() {
        let tab_info = TabInfo {
            position: 0,
            name: "agent".to_owned(),
            ..Default::default()
        };
        let manifest = PaneManifest {
            panes: [(
                0,
                vec![terminal("", Some("/opt/homebrew/bin/codex --resume"))],
            )]
            .into(),
        };

        let projected = TabUiInfo::new(&tab_info, &manifest);

        assert_eq!(projected.primary_process_label(), Some("codex"));
    }

    #[test]
    fn unlabeled_live_terminal_still_keeps_its_tab_visible() {
        let tab_info = TabInfo {
            position: 0,
            name: "operator".to_owned(),
            ..Default::default()
        };
        let manifest = PaneManifest {
            panes: [(0, vec![terminal("", None)])].into(),
        };

        let projected = TabUiInfo::new(&tab_info, &manifest);

        assert_eq!(projected.live_process_count(), 1);
        assert_eq!(projected.primary_process_label(), Some("terminal"));
    }

    #[test]
    fn native_attention_only_uses_terminal_exit_and_held_facts() {
        let tab = TabInfo {
            position: 7,
            ..Default::default()
        };
        for (plugin, exited, exit_status, held, attention, paused) in [
            (false, false, Some(9), false, false, false),
            (false, true, Some(0), false, false, false),
            (false, true, Some(9), false, true, false),
            (false, true, None, false, true, false),
            (false, false, None, true, false, true),
            (false, true, Some(0), true, false, true),
            (true, true, Some(9), true, false, false),
        ] {
            let manifest = PaneManifest {
                panes: [(
                    7,
                    vec![PaneInfo {
                        is_plugin: plugin,
                        exited,
                        exit_status,
                        is_held: held,
                        ..Default::default()
                    }],
                )]
                .into(),
            };
            let projected = TabUiInfo::new(&tab, &manifest);
            assert_eq!(projected.has_exited_attention, attention);
            assert_eq!(projected.has_held_pane, paused);
        }
        let mut bell = tab;
        bell.has_bell_notification = true;
        assert!(TabUiInfo::new(&bell, &PaneManifest::default()).has_bell_notification);
        bell.has_bell_notification = false;
        bell.is_flashing_bell = true;
        assert!(TabUiInfo::new(&bell, &PaneManifest::default()).has_bell_notification);
    }
}
