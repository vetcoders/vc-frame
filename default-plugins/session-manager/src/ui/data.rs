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
    pub name: String,
    pub tabs: Vec<TabUiInfo>,
    pub connected_users: usize,
    pub is_current_session: bool,
    pub creation_time: Duration,
}

impl SessionUiInfo {
    pub fn from_session_info(session_info: &SessionInfo) -> Self {
        SessionUiInfo {
            name: session_info.name.clone(),
            tabs: session_info
                .tabs
                .iter()
                .map(|t| TabUiInfo::new(t, &session_info.panes))
                .collect(),
            connected_users: session_info.connected_clients,
            is_current_session: session_info.is_current_session,
            creation_time: session_info.creation_time,
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

#[derive(Debug, Clone)]
pub struct TabUiInfo {
    pub name: String,
    pub panes: Vec<PaneUiInfo>,
    pub position: usize,
    pub is_active: bool,
    live_processes: Vec<LiveProcessUiInfo>,
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
        TabUiInfo {
            name: tab_info.name.clone(),
            panes,
            position: tab_info.position,
            is_active: tab_info.active,
            live_processes,
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
    #[cfg(test)]
    pub fn for_rail_test(
        name: &str,
        is_active: bool,
        process_label: &str,
        live_process_count: usize,
    ) -> Self {
        Self {
            name: name.to_owned(),
            panes: vec![],
            position: 0,
            is_active,
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
}
