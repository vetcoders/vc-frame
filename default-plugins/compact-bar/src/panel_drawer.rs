//! Current-tab panel inventory for the compact-bar Panels chip and drawer.
//!
//! The server already owns pane identity (`PaneManifest` / `PaneInfo`). This
//! module does not keep a second registry: it projects the last server snapshot
//! into rows the chip can count and the drawer can focus.

use zellij_tile::prelude::*;

pub const CONFIG_IS_PANEL_DRAWER: &str = "is_panel_drawer";
/// The drawer's own title — the shared constant the server pager also uses.
pub const PANEL_DRAWER_TITLE: &str = PANELS_DRAWER_TITLE;
pub const MSG_TOGGLE_PANEL_DRAWER: &str = "vc_panel_drawer";

/// Panels-layer scope of a row, exactly as the server published it in
/// `PaneInfo::panel_scope`. `Unknown` is a Panels row whose snapshot carries no
/// scope (a legacy producer): rendered as unknown, never guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelScopeLabel {
    Global,
    Project(String),
    Unbound,
    Unknown,
}

impl PanelScopeLabel {
    pub fn label(&self) -> String {
        match self {
            PanelScopeLabel::Global => "Global".to_owned(),
            PanelScopeLabel::Project(guest) => format!("Project {guest}"),
            PanelScopeLabel::Unbound => "Unbound".to_owned(),
            PanelScopeLabel::Unknown => "scope unknown".to_owned(),
        }
    }
}

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
    /// Panels scope from the server snapshot (see `scope_label`); None for
    /// rows that are not Panels-layer panes (tiled, plain suppressed).
    pub scope: Option<PanelScopeLabel>,
    /// `(i, N)` pager position among the visible floating panels, 1-based,
    /// in the same order the server pager steps through them.
    pub pager: Option<(usize, usize)>,
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

    pub fn pager_label(&self) -> Option<String> {
        self.pager.map(|(index, total)| format!("{index}/{total}"))
    }

    pub fn list_line(&self) -> String {
        let mut line = format!(
            "{} · {} · {} · {}",
            self.title,
            self.kind_label(),
            self.state,
            self.visibility_label()
        );
        if let Some(scope) = &self.scope {
            line.push_str(" · ");
            line.push_str(&scope.label());
        }
        if let Some(pager) = self.pager_label() {
            line.push_str(" · ");
            line.push_str(&pager);
        }
        line
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
    number_visible_panels(&mut rows);
    rows
}

/// Visible floating rows sort first by (kind, id) — the server pager order —
/// so their `i/N` is their rank among themselves.
fn number_visible_panels(rows: &mut [PanelRow]) {
    let total = rows
        .iter()
        .filter(|row| row.is_floating && !row.hidden)
        .count();
    let mut index = 0;
    for row in rows.iter_mut() {
        if row.is_floating && !row.hidden {
            index += 1;
            row.pager = Some((index, total));
        } else {
            row.pager = None;
        }
    }
}

/// Scope label of a row from the server-published `PaneInfo::panel_scope`.
/// A published scope always wins — a scope-hidden Project pane is suppressed
/// and non-floating, yet still belongs to its guest. A floating row without a
/// published scope comes from a producer that predates the field: Unknown.
/// Anything else is not a Panels row and carries no scope.
pub fn scope_label(pane: &PaneInfo) -> Option<PanelScopeLabel> {
    match &pane.panel_scope {
        Some(PanelScope::Global) => Some(PanelScopeLabel::Global),
        Some(PanelScope::Project(guest)) => Some(PanelScopeLabel::Project(guest.clone())),
        Some(PanelScope::Unbound) => Some(PanelScopeLabel::Unbound),
        None if pane.is_floating => Some(PanelScopeLabel::Unknown),
        None => None,
    }
}

pub fn detect_panel_drawer(manifest: &PaneManifest, floating_visible: bool) -> (Option<u32>, bool) {
    for panes in manifest.panes.values() {
        for pane in panes {
            if pane.is_plugin
                && pane.title == PANEL_DRAWER_TITLE
                && pane
                    .plugin_url
                    .as_deref()
                    .and_then(panels_chrome_plugin)
                    .is_some_and(|chrome| chrome == "compact-bar")
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

/// The shared Panels predicate (`zellij_utils::data::is_panels_layer_pane`, the
/// one the server pager uses) — plus this plugin's own instance, which is
/// chrome under any URL.
fn include_pane(pane: &PaneInfo, own_plugin_id: Option<u32>) -> bool {
    if pane.is_plugin && Some(pane.id) == own_plugin_id {
        return false;
    }
    pane.is_panels_layer_pane()
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
        scope: scope_label(pane),
        pager: None,
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
    fn visible_floating_panels_carry_i_of_n_in_pager_order() {
        let tiled = terminal(1, "shell");
        let mut b = terminal(9, "claude");
        b.is_floating = true;
        let mut a = terminal(4, "codex");
        a.is_floating = true;
        let mut agent = plugin(2, "agent", "vc-frame:agent-workspace");
        agent.is_floating = true;
        let mut hidden = terminal(5, "project-b");
        hidden.is_floating = true;
        hidden.is_suppressed = true;
        let listed = inventory_for_tab(
            &manifest(&[(0, vec![tiled, b, agent, a, hidden])]),
            0,
            None,
            true,
        );
        let pager = |id: u32, is_plugin: bool| {
            listed
                .iter()
                .find(|row| row.id == id && row.is_plugin == is_plugin)
                .and_then(|row| row.pager)
        };
        assert_eq!(pager(4, false), Some((1, 3)));
        assert_eq!(pager(9, false), Some((2, 3)));
        assert_eq!(pager(2, true), Some((3, 3)));
        assert_eq!(pager(5, false), None, "hidden panels are skipped");
        assert_eq!(pager(1, false), None, "tiled panes are not panels");
        let codex = listed.iter().find(|row| row.id == 4).unwrap();
        assert!(codex.list_line().ends_with(" · 1/3"));
    }

    #[test]
    fn hidden_floating_layer_numbers_no_panel() {
        let mut a = terminal(3, "claude");
        a.is_floating = true;
        let listed = inventory_for_tab(&manifest(&[(0, vec![a])]), 0, None, false);
        assert_eq!(listed[0].pager, None);
        assert_eq!(listed[0].pager_label(), None);
    }

    #[test]
    fn scope_label_comes_from_the_published_scope_and_never_guesses() {
        let mut global = terminal(1, "claude");
        global.is_floating = true;
        global.panel_scope = Some(PanelScope::Global);
        let mut project = terminal(2, "codex");
        project.is_floating = true;
        project.panel_scope = Some(PanelScope::Project("workspace-a".to_owned()));
        // Scope-hidden Project: suppressed and non-floating, ownership known.
        let mut hidden_project = terminal(3, "gemini");
        hidden_project.is_suppressed = true;
        hidden_project.panel_scope = Some(PanelScope::Project("workspace-b".to_owned()));
        let mut unbound = terminal(4, "shell");
        unbound.is_floating = true;
        unbound.panel_scope = Some(PanelScope::Unbound);
        // Legacy producer: floating, no scope field.
        let mut legacy = terminal(5, "old");
        legacy.is_floating = true;
        let tiled = terminal(6, "tiled");

        assert_eq!(scope_label(&global), Some(PanelScopeLabel::Global));
        assert_eq!(
            scope_label(&project),
            Some(PanelScopeLabel::Project("workspace-a".to_owned()))
        );
        assert_eq!(
            scope_label(&hidden_project),
            Some(PanelScopeLabel::Project("workspace-b".to_owned())),
            "hidden Project ownership does not depend on is_floating"
        );
        assert_eq!(scope_label(&unbound), Some(PanelScopeLabel::Unbound));
        assert_eq!(scope_label(&legacy), Some(PanelScopeLabel::Unknown));
        assert_eq!(scope_label(&tiled), None, "tiled panes are not panels");

        let listed = inventory_for_tab(
            &manifest(&[(
                0,
                vec![global, project, hidden_project, unbound, legacy, tiled],
            )]),
            0,
            None,
            true,
        );
        let line = |id: u32| {
            listed
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.list_line())
                .unwrap()
        };
        assert_eq!(
            line(1),
            "claude · terminal · running · visible · Global · 1/4"
        );
        assert_eq!(
            line(2),
            "codex · terminal · running · visible · Project workspace-a · 2/4"
        );
        assert_eq!(
            line(3),
            "gemini · terminal · running · hidden · Project workspace-b"
        );
        assert_eq!(
            line(4),
            "shell · terminal · running · visible · Unbound · 3/4"
        );
        assert_eq!(
            line(5),
            "old · terminal · running · visible · scope unknown · 4/4"
        );
        assert_eq!(line(6), "tiled · terminal · running · visible");
    }

    #[test]
    fn terminal_named_panels_is_counted_while_the_plugin_drawer_is_not() {
        // The same manifest the server pager sees: a real terminal renamed
        // "Panels", the drawer plugin titled "Panels", floating chrome, and a
        // user plugin. Drawer i/N must equal the shared-predicate inventory.
        let mut named_terminal = terminal(3, PANEL_DRAWER_TITLE);
        named_terminal.is_floating = true;
        let mut other_terminal = terminal(7, "codex");
        other_terminal.is_floating = true;
        let mut drawer = plugin(4, PANEL_DRAWER_TITLE, "vc-frame:compact-bar");
        drawer.is_floating = true;
        let mut config = plugin(5, "Config", "zellij:status-bar");
        config.is_floating = true;
        let mut agent = plugin(6, "agent", "vc-frame:agent-workspace");
        agent.is_floating = true;
        let panes = vec![
            named_terminal,
            other_terminal,
            drawer.clone(),
            config.clone(),
            agent,
        ];
        let listed = inventory_for_tab(&manifest(&[(0, panes.clone())]), 0, None, true);
        let numbered: Vec<(bool, u32, (usize, usize))> = listed
            .iter()
            .filter_map(|row| row.pager.map(|pager| (row.is_plugin, row.id, pager)))
            .collect();
        assert_eq!(
            numbered,
            vec![(false, 3, (1, 3)), (false, 7, (2, 3)), (true, 6, (3, 3))],
            "terminal 'Panels' is page-able; the drawer and chrome are not"
        );
        let eligible: Vec<(bool, u32)> = panes
            .iter()
            .filter(|pane| pane.is_panels_layer_pane())
            .map(|pane| (pane.is_plugin, pane.id))
            .collect();
        assert_eq!(
            eligible,
            vec![(false, 3), (false, 7), (true, 6)],
            "the drawer inventory is the shared predicate, nothing more"
        );
        assert!(!drawer.is_panels_layer_pane());
        assert!(!config.is_panels_layer_pane());
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
