use std::time::Duration;

use zellij_tile::prelude::*;

use super::components::{
    build_pane_ui_line, build_session_ui_line, build_tab_ui_line, Colors, LineToRender,
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
}

impl TabUiInfo {
    pub fn new(tab_info: &TabInfo, pane_manifest: &PaneManifest) -> Self {
        let panes = pane_manifest
            .panes
            .get(&tab_info.position)
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
        TabUiInfo {
            name: tab_info.name.clone(),
            panes,
            position: tab_info.position,
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
