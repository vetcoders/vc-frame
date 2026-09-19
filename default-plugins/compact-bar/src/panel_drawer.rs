//! Current-tab panel inventory for the compact-bar Panels chip and drawer.
//!
//! The server already owns pane identity (`PaneManifest` / `PaneInfo`). This
//! module does not keep a second registry: it projects the last server snapshot
//! into rows the chip can count and the drawer can focus.

use zellij_tile::prelude::*;

pub const CONFIG_IS_PANEL_DRAWER: &str = "is_panel_drawer";
pub const PANEL_DRAWER_TITLE: &str = "Panels";
pub const MSG_TOGGLE_PANEL_DRAWER: &str = "vc_panel_drawer";

/// Compact-bar / tab-bar / status-bar / session-manager rail — not user panels.
const CHROME_PLUGIN_URLS: [&str; 12] = [
    "vc-frame:compact-bar",
    "zellij:compact-bar",
    "compact-bar",
    "vc-frame:status-bar",
    "zellij:status-bar",
    "status-bar",
    "vc-frame:tab-bar",
    "zellij:tab-bar",
    "tab-bar",
    "vc-frame:session-manager",
    "zellij:session-manager",
    "session-manager",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    Terminal,
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelRow {
    pub id: u32,
    pub is_plugin: bool,
    pub title: String,
    pub kind: PanelKind,
    pub state: String,
    pub hidden: bool,
    pub is_floating: bool,
}

impl PanelRow {
    pub fn pane_id(&self) -> PaneId {
        if self.is_plugin {
            PaneId::Plugin(self.id)
        } else {
            PaneId::Terminal(self.id)
        }
    }

    pub fn visibility_label(&self) -> &'static str {
        if self.hidden { "hidden" } else { "visible" }
    }

    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            PanelKind::Terminal => "terminal",
            PanelKind::Plugin => "plugin",
        }
    }

    pub fn list_line(&self) -> String {
        format!(
            "{} · {} · {} · {}",
            self.title,
            self.kind_label(),
            self.state,
            self.visibility_label()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawerCommand {
    Hide,
    Focus(PaneId),
    Redraw,
    None,
}

#[derive(Debug, Default, Clone)]
pub struct PanelDrawer {
    pub rows: Vec<PanelRow>,
    pub selected: usize,
}

impl PanelDrawer {
    pub fn replace_rows(&mut self, rows: Vec<PanelRow>) -> bool {
        let changed = self.rows != rows;
        if !changed {
            return false;
        }
        self.rows = rows;
        if self.rows.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
        true
    }

    pub fn selected_row(&self) -> Option<&PanelRow> {
        self.rows.get(self.selected)
    }

    pub fn move_selection(&mut self, delta: isize) -> bool {
        if self.rows.is_empty() {
            return false;
        }
        let len = self.rows.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len) as usize;
        if next == self.selected {
            return false;
        }
        self.selected = next;
        true
    }

    pub fn select_index(&mut self, index: usize) -> Option<PaneId> {
        if index >= self.rows.len() {
            return None;
        }
        self.selected = index;
        Some(self.rows[index].pane_id())
    }

    pub fn handle_key(&mut self, key: &KeyWithModifier) -> DrawerCommand {
        if !key.has_no_modifiers() {
            return DrawerCommand::None;
        }
        match key.bare_key {
            BareKey::Esc | BareKey::Char('q') => DrawerCommand::Hide,
            BareKey::Enter => self
                .selected_row()
                .map(|row| DrawerCommand::Focus(row.pane_id()))
                .unwrap_or(DrawerCommand::None),
            BareKey::Up | BareKey::Char('k') => {
                if self.move_selection(-1) {
                    DrawerCommand::Redraw
                } else {
                    DrawerCommand::None
                }
            },
            BareKey::Down | BareKey::Char('j') => {
                if self.move_selection(1) {
                    DrawerCommand::Redraw
                } else {
                    DrawerCommand::None
                }
            },
            _ => DrawerCommand::None,
        }
    }

    /// List rows start after a two-line header.
    pub fn handle_click(&mut self, line: isize) -> DrawerCommand {
        if line < 2 {
            return DrawerCommand::None;
        }
        let index = (line as usize).saturating_sub(2);
        self.select_index(index)
            .map(DrawerCommand::Focus)
            .unwrap_or(DrawerCommand::None)
    }
}

pub fn inventory_for_tab(
    manifest: &PaneManifest,
    tab_position: usize,
    own_plugin_id: Option<u32>,
    floating_visible: bool,
) -> Vec<PanelRow> {
    let Some(panes) = manifest.panes.get(&tab_position) else {
        return Vec::new();
    };
    let mut rows: Vec<PanelRow> = panes
        .iter()
        .filter(|pane| include_pane(pane, own_plugin_id))
        .map(|pane| row_from_pane(pane, floating_visible))
        .collect();
    rows.sort_by_key(|row| {
        (
            row.hidden,
            !row.is_floating,
            row.is_plugin,
            row.id,
            row.title.clone(),
        )
    });
    rows
}

pub fn detect_panel_drawer(manifest: &PaneManifest, floating_visible: bool) -> (Option<u32>, bool) {
    for panes in manifest.panes.values() {
        for pane in panes {
            if pane.is_plugin
                && pane.title == PANEL_DRAWER_TITLE
                && pane
                    .plugin_url
                    .as_deref()
                    .is_some_and(|url| CHROME_PLUGIN_URLS[..3].contains(&url))
            {
                let hidden = pane_is_hidden(pane, floating_visible);
                return (Some(pane.id), !hidden);
            }
        }
    }
    (None, false)
}

pub fn current_tab_position(active_tab_idx: usize) -> usize {
    active_tab_idx.saturating_sub(1)
}

pub fn floating_panes_visible(tabs: &[TabInfo]) -> bool {
    tabs.iter()
        .find(|tab| tab.active)
        .map(|tab| tab.are_floating_panes_visible)
        .unwrap_or(true)
}

pub fn panel_drawer_coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("72%".to_owned()),
        Some("6%".to_owned()),
        Some("26%".to_owned()),
        Some("80%".to_owned()),
        Some(true),
        Some(false),
    )
}

fn include_pane(pane: &PaneInfo, own_plugin_id: Option<u32>) -> bool {
    if !pane.is_selectable {
        return false;
    }
    if pane.is_plugin && Some(pane.id) == own_plugin_id {
        return false;
    }
    if pane.is_plugin
        && pane
            .plugin_url
            .as_deref()
            .is_some_and(|url| CHROME_PLUGIN_URLS.contains(&url))
    {
        return false;
    }
    if pane.is_plugin && pane.title == PANEL_DRAWER_TITLE {
        return false;
    }
    true
}

fn pane_is_hidden(pane: &PaneInfo, floating_visible: bool) -> bool {
    pane.is_suppressed || (pane.is_floating && !floating_visible)
}

fn row_from_pane(pane: &PaneInfo, floating_visible: bool) -> PanelRow {
    let kind = if pane.is_plugin {
        PanelKind::Plugin
    } else {
        PanelKind::Terminal
    };
    PanelRow {
        id: pane.id,
        is_plugin: pane.is_plugin,
        title: pane_title(pane),
        kind,
        state: pane_state(pane),
        hidden: pane_is_hidden(pane, floating_visible),
        is_floating: pane.is_floating,
    }
}

fn pane_title(pane: &PaneInfo) -> String {
    let title = pane.title.trim();
    if title.is_empty() {
        if pane.is_plugin {
            "plugin".to_owned()
        } else {
            "terminal".to_owned()
        }
    } else {
        title.to_owned()
    }
}

/// Truthful when the server snapshot carries a command, hold, or exit.
/// "agent" is a label from url/title, not a live heartbeat.
fn pane_state(pane: &PaneInfo) -> String {
    if pane.exited {
        return match pane.exit_status {
            Some(code) => format!("exited {code}"),
            None => "exited".to_owned(),
        };
    }
    if pane.is_held {
        return "held".to_owned();
    }
    if pane.is_plugin {
        let haystack = format!(
            "{} {}",
            pane.title,
            pane.plugin_url.as_deref().unwrap_or("")
        )
        .to_ascii_lowercase();
        if haystack.contains("agent") {
            return "agent".to_owned();
        }
        return plugin_state_label(pane.plugin_url.as_deref());
    }
    pane.terminal_command
        .as_deref()
        .map(short_command)
        .filter(|command| !command.is_empty())
        .unwrap_or_else(|| "running".to_owned())
}

fn plugin_state_label(url: Option<&str>) -> String {
    url.and_then(|url| url.rsplit([':', '/', '@']).next())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_owned())
        .unwrap_or_else(|| "plugin".to_owned())
}

fn short_command(command: &str) -> String {
    command
        .split_whitespace()
        .next()
        .and_then(|token| token.rsplit('/').next())
        .unwrap_or(command)
        .to_owned()
}

pub fn render_drawer(rows: usize, cols: usize, drawer: &PanelDrawer) {
    let header = Text::new("Panels  · Esc closes · Enter focuses");
    print_text_with_coordinates(header, 0, 0, Some(cols), Some(1));
    if drawer.rows.is_empty() {
        print_text_with_coordinates(
            Text::new("No panels in this tab."),
            0,
            2,
            Some(cols),
            Some(1),
        );
        return;
    }
    let items: Vec<NestedListItem> = drawer
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mut item = NestedListItem::new(row.list_line());
            if index == drawer.selected {
                item = item.selected();
            }
            item
        })
        .collect();
    print_nested_list_with_coordinates(items, 0, 2, Some(cols), Some(rows.saturating_sub(2)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn terminal(id: u32, title: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: title.to_owned(),
            is_plugin: false,
            is_selectable: true,
            ..PaneInfo::default()
        }
    }

    fn plugin(id: u32, title: &str, url: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: title.to_owned(),
            is_plugin: true,
            is_selectable: true,
            plugin_url: Some(url.to_owned()),
            ..PaneInfo::default()
        }
    }

    fn manifest(tabs: &[(usize, Vec<PaneInfo>)]) -> PaneManifest {
        PaneManifest {
            panes: tabs.iter().cloned().collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn hidden_suppressed_pane_is_listed() {
        let mut hidden = terminal(7, "❯_ Quick cmd");
        hidden.is_suppressed = true;
        hidden.is_floating = true;
        let mut chrome = plugin(1, "compact-bar", "vc-frame:compact-bar");
        chrome.is_selectable = false;
        let listed = inventory_for_tab(
            &manifest(&[(0, vec![chrome, hidden.clone()])]),
            0,
            Some(1),
            true,
        );
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].is_plugin, listed[0].id), (false, 7));
        assert!(listed[0].hidden);
        assert_eq!(listed[0].visibility_label(), "hidden");
    }

    #[test]
    fn floating_layer_hidden_marks_floats_hidden_without_dropping_them() {
        let mut quick = terminal(3, "❯_ Quick cmd");
        quick.is_floating = true;
        let tiled = terminal(2, "shell");
        let listed = inventory_for_tab(&manifest(&[(0, vec![tiled, quick])]), 0, None, false);
        assert_eq!(listed.len(), 2);
        let quick_row = listed.iter().find(|row| row.id == 3).unwrap();
        assert!(quick_row.hidden);
        let tiled_row = listed.iter().find(|row| row.id == 2).unwrap();
        assert!(!tiled_row.hidden);
    }

    #[test]
    fn several_quick_cmd_panels_stay_distinct_rows() {
        let mut a = terminal(10, "❯_ Quick cmd");
        a.is_floating = true;
        let mut b = terminal(11, "❯_ Quick cmd");
        b.is_floating = true;
        let listed = inventory_for_tab(&manifest(&[(0, vec![a, b])]), 0, None, true);
        assert_eq!(listed.len(), 2);
        assert_eq!((listed[0].is_plugin, listed[0].id), (false, 10));
        assert_eq!((listed[1].is_plugin, listed[1].id), (false, 11));
    }

    #[test]
    fn identical_manifests_keep_stable_ids() {
        let pane = terminal(4, "vim");
        let first = inventory_for_tab(&manifest(&[(0, vec![pane.clone()])]), 0, None, true);
        let second = inventory_for_tab(&manifest(&[(0, vec![pane])]), 0, None, true);
        assert_eq!(
            (first[0].is_plugin, first[0].id),
            (second[0].is_plugin, second[0].id)
        );
        assert_eq!((first[0].is_plugin, first[0].id), (false, 4));
    }

    #[test]
    fn inventory_is_tab_scoped() {
        let tab0 = terminal(1, "one");
        let tab1 = terminal(2, "two");
        let snap = manifest(&[(0, vec![tab0]), (1, vec![tab1])]);
        let on_zero = inventory_for_tab(&snap, 0, None, true);
        let on_one = inventory_for_tab(&snap, 1, None, true);
        assert_eq!(on_zero.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(on_one.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn chrome_and_own_plugin_are_excluded() {
        let mut bar = plugin(8, "compact-bar", "vc-frame:compact-bar");
        bar.is_selectable = false;
        let drawer = plugin(9, PANEL_DRAWER_TITLE, "vc-frame:compact-bar");
        let user = terminal(5, "zsh");
        let listed =
            inventory_for_tab(&manifest(&[(0, vec![bar, drawer, user])]), 0, Some(9), true);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, 5);
    }

    #[test]
    fn selection_maps_to_exact_pane_id() {
        let mut drawer = PanelDrawer::default();
        let mut hidden = terminal(7, "hidden-term");
        hidden.is_suppressed = true;
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a"), hidden])]),
            0,
            None,
            true,
        ));
        assert_eq!(drawer.select_index(0), Some(PaneId::Terminal(1)));
        let hidden_idx = drawer
            .rows
            .iter()
            .position(|row| row.id == 7)
            .expect("hidden pane must be selectable");
        assert_eq!(drawer.select_index(hidden_idx), Some(PaneId::Terminal(7)));
    }

    #[test]
    fn escape_closes_drawer_without_a_focus_command() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a")])]),
            0,
            None,
            true,
        ));
        let key = KeyWithModifier::new(BareKey::Esc);
        assert_eq!(drawer.handle_key(&key), DrawerCommand::Hide);
        let enter = KeyWithModifier::new(BareKey::Enter);
        assert_eq!(
            drawer.handle_key(&enter),
            DrawerCommand::Focus(PaneId::Terminal(1))
        );
    }

    #[test]
    fn agent_label_is_taken_from_url_not_invented_liveness() {
        let agent = plugin(12, "live-agent", "vc-frame:agent-workspace");
        let listed = inventory_for_tab(&manifest(&[(0, vec![agent])]), 0, None, true);
        assert_eq!(listed[0].kind, PanelKind::Plugin);
        assert_eq!(listed[0].state, "agent");
    }

    #[test]
    fn held_and_exited_process_state_is_truthful() {
        let mut held = terminal(1, "build");
        held.is_held = true;
        held.terminal_command = Some("cargo test".to_owned());
        let mut exited = terminal(2, "build");
        exited.exited = true;
        exited.exit_status = Some(2);
        let listed = inventory_for_tab(&manifest(&[(0, vec![held, exited])]), 0, None, true);
        assert_eq!(
            listed
                .iter()
                .find(|row| row.id == 1)
                .map(|row| row.state.as_str()),
            Some("held")
        );
        assert_eq!(
            listed
                .iter()
                .find(|row| row.id == 2)
                .map(|row| row.state.as_str()),
            Some("exited 2")
        );
    }
}
