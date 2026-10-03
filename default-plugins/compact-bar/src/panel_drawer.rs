//! Current-tab panel inventory for the compact-bar Panels chip and drawer.
//!
//! The server already owns pane identity (`PaneManifest` / `PaneInfo`). This
//! module does not keep a second registry: it projects the last server snapshot
//! into rows the chip can count and the drawer can focus.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
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

/// The drawer's list filter, rendered as the `[Global] [Project]` chips in the
/// header. Global is the full agent-panel switcher across every tab; Project
/// keeps the panels bound to the guest currently projected into this host's
/// Workspace tab plus the panels of the tab the drawer floats over. Panels
/// are never moved or pinned behind the operator — this is a view filter only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DrawerScope {
    #[default]
    Global,
    Project,
}

impl DrawerScope {
    pub fn label(self) -> &'static str {
        match self {
            DrawerScope::Global => "Global",
            DrawerScope::Project => "Project",
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            DrawerScope::Global => DrawerScope::Project,
            DrawerScope::Project => DrawerScope::Global,
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
    pub is_focused: bool,
    /// 0-based position of the tab this panel lives in — the Global switcher
    /// spans tabs, so a row must carry its origin for filtering and focus.
    pub tab_position: usize,
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
    SetScope(DrawerScope),
    Redraw,
    None,
}

#[derive(Debug, Default, Clone)]
pub struct PanelDrawer {
    pub rows: Vec<PanelRow>,
    pub selected: usize,
    pub details_expanded: bool,
    pub scope: DrawerScope,
    viewport_start: usize,
    viewport_len: usize,
    /// Character-column ranges of the `[Global]` / `[Project]` header chips in
    /// the last painted frame, `None` when the header was clipped. Click
    /// hit-testing resolves against exactly what is on screen.
    scope_chip_columns: Option<((usize, usize), (usize, usize))>,
}

impl PanelDrawer {
    pub fn replace_rows(&mut self, rows: Vec<PanelRow>) -> bool {
        let changed = self.rows != rows;
        if !changed {
            return false;
        }
        let selected_id = self.selected_row().map(PanelRow::pane_id);
        self.rows = rows;
        if self.rows.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self
                .rows
                .iter()
                .position(|row| Some(row.pane_id()) == selected_id)
                .unwrap_or_else(|| self.selected.min(self.rows.len() - 1));
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
            BareKey::Char('d') => {
                self.details_expanded = !self.details_expanded;
                DrawerCommand::Redraw
            },
            BareKey::Char('f') => DrawerCommand::SetScope(self.scope.toggled()),
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

    /// The header chips are clickable on line 0; list rows start after the
    /// two-line header. Clicking a chip sets that scope; clicking a list row
    /// focuses its panel.
    pub fn handle_click(&mut self, line: isize, col: usize) -> DrawerCommand {
        if line == 0 {
            if let Some((global, project)) = self.scope_chip_columns {
                if col >= global.0 && col < global.1 {
                    return DrawerCommand::SetScope(DrawerScope::Global);
                }
                if col >= project.0 && col < project.1 {
                    return DrawerCommand::SetScope(DrawerScope::Project);
                }
            }
            return DrawerCommand::None;
        }
        if line < 2 {
            return DrawerCommand::None;
        }
        let offset = (line as usize).saturating_sub(2);
        if offset >= self.viewport_len {
            return DrawerCommand::None;
        }
        let index = self.viewport_start + offset;
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
        .map(|pane| row_from_pane(pane, tab_position, floating_visible))
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

/// Every tab's inventory concatenated in tab-position order. Per-tab sorting
/// and the per-tab `i/N` pager are kept — the server pager is per-tab, and a
/// cross-tab renumbering would lie about it.
pub fn inventory_global(
    manifest: &PaneManifest,
    own_plugin_id: Option<u32>,
    floating_visible: bool,
) -> Vec<PanelRow> {
    let mut tab_positions: Vec<usize> = manifest.panes.keys().copied().collect();
    tab_positions.sort_unstable();
    let mut rows = Vec::new();
    for tab_position in tab_positions {
        rows.extend(inventory_for_tab(
            manifest,
            tab_position,
            own_plugin_id,
            floating_visible,
        ));
    }
    rows
}

/// The drawer's scoped inventory. `workspace_tab` is the host tab carrying the
/// shared VC Guest surface; its visitor command is the only server-committed
/// "current project" identity available to a non-canvas plugin.
pub fn inventory_for_scope(
    manifest: &PaneManifest,
    current_tab: usize,
    workspace_tab: Option<usize>,
    own_plugin_id: Option<u32>,
    floating_visible: bool,
    scope: DrawerScope,
) -> Vec<PanelRow> {
    match scope {
        DrawerScope::Global => inventory_global(manifest, own_plugin_id, floating_visible),
        DrawerScope::Project => {
            let guest = workspace_tab.and_then(|tab| projected_guest_in_tab(manifest, tab));
            inventory_global(manifest, own_plugin_id, floating_visible)
                .into_iter()
                .filter(|row| match &guest {
                    Some(guest) => {
                        matches!(&row.scope, Some(PanelScopeLabel::Project(row_guest)) if row_guest == guest)
                            || row.tab_position == current_tab
                    },
                    // No projected guest: the current tab IS the project surface.
                    None => row.tab_position == current_tab,
                })
                .collect()
        },
    }
}

/// The visitor terminal records its reservation as
/// `<exe> --workspace-projection <WorkspaceProjectionReady json> visit …`.
/// The guest inside that JSON is the server-committed identity — pane titles
/// are volatile (OSC renames) and the exe name is build-specific.
pub fn projected_guest_in_tab(manifest: &PaneManifest, tab_position: usize) -> Option<String> {
    manifest.panes.get(&tab_position)?.iter().find_map(|pane| {
        if pane.is_plugin {
            return None;
        }
        let command = pane.terminal_command.as_deref()?;
        let marker = command.find("--workspace-projection")?;
        let json_start = command[marker..].find('{')? + marker;
        let json = json_object_at(command, json_start)?;
        serde_json::from_str::<serde_json::Value>(json)
            .ok()?
            .get("guest")?
            .as_str()
            .map(str::to_owned)
    })
}

/// Slice the JSON object starting at `start`, matching braces outside string
/// literals. Returns None on unbalanced input — never a guessed prefix.
fn json_object_at(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        let byte = *byte;
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            },
            _ => {},
        }
    }
    None
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

/// Returns the (i, N) pager position of the currently focused visible floating panel, if any.
pub fn active_pager(rows: &[PanelRow]) -> Option<(usize, usize)> {
    rows.iter()
        .find(|r| r.is_floating && !r.hidden && r.is_focused)
        .and_then(|r| r.pager)
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

pub fn detect_panel_drawer(
    manifest: &PaneManifest,
    tab_position: usize,
    floating_visible: bool,
) -> (Option<u32>, bool) {
    if let Some(panes) = manifest.panes.get(&tab_position) {
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
        Some("40%".to_owned()),
        Some("15%".to_owned()),
        Some("58%".to_owned()),
        Some("78%".to_owned()),
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

fn row_from_pane(pane: &PaneInfo, tab_position: usize, floating_visible: bool) -> PanelRow {
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
        is_focused: pane.is_focused,
        tab_position,
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

/// Explicit ellipsis at a grapheme/cell boundary; never a sliced scope label.
fn fit_panel_text(text: &str, cols: usize) -> String {
    if text.width() <= cols {
        return text.to_owned();
    }
    if cols == 0 {
        return String::new();
    }
    let mut result = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if width + next > cols - 1 {
            break;
        }
        result.push_str(grapheme);
        width += next;
    }
    result.push('…');
    result
}

fn wrap_panel_text(text: &str, cols: usize) -> Vec<String> {
    if cols == 0 {
        return vec![];
    }
    let mut lines = vec![];
    let mut line = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if width + next > cols && !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            width = 0;
        }
        if next > cols {
            lines.push("…".into());
        } else {
            line.push_str(grapheme);
            width += next;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

impl PanelDrawer {
    /// A flat list with optional details, kept within the actual pane bounds.
    /// The same viewport owns paint and mouse hit-testing.
    fn lines(&mut self, rows: usize, cols: usize) -> Vec<(String, bool)> {
        let details = if self.details_expanded {
            self.selected_row()
                .map(|row| {
                    let mut lines = vec!["Details · d folds".to_owned()];
                    lines.extend(wrap_panel_text(&row.list_line(), cols));
                    lines
                })
                .unwrap_or_default()
        } else {
            vec![]
        };
        let detail_count = details.len().min(rows.saturating_sub(3));
        self.viewport_len = rows.saturating_sub(2 + detail_count);
        self.viewport_start =
            self.selected.checked_div(self.viewport_len).unwrap_or(0) * self.viewport_len;
        let end = (self.viewport_start + self.viewport_len).min(self.rows.len());
        let position = format!(
            "Panels · {}/{}",
            if self.rows.is_empty() {
                0
            } else {
                self.selected + 1
            },
            self.rows.len()
        );
        let global_chip = if self.scope == DrawerScope::Global {
            format!("[●{}]", DrawerScope::Global.label())
        } else {
            format!("[ {} ]", DrawerScope::Global.label())
        };
        let project_chip = if self.scope == DrawerScope::Project {
            format!("[●{}]", DrawerScope::Project.label())
        } else {
            format!("[ {} ]", DrawerScope::Project.label())
        };
        let header = format!("{position} · {global_chip} {project_chip}");
        // Chips are clickable only when the header painted unclipped; the
        // ranges are character columns (the header is ASCII + `·`/`●`, width 1).
        self.scope_chip_columns = if header.width() <= cols {
            let global_start = position.chars().count() + 3;
            let global_end = global_start + global_chip.chars().count();
            let project_start = global_end + 1;
            let project_end = project_start + project_chip.chars().count();
            Some(((global_start, global_end), (project_start, project_end)))
        } else {
            None
        };
        let mut lines = vec![
            (header, false),
            ("↑↓ Enter · f filter · d details · Esc".to_owned(), false),
        ];
        if self.rows.is_empty() {
            let empty = match self.scope {
                DrawerScope::Global => "No panels.",
                DrawerScope::Project => "No panels in this project.",
            };
            lines.push((empty.into(), false));
        } else {
            for index in self.viewport_start..end {
                let row = &self.rows[index];
                lines.push((
                    format!("{} {}", if row.hidden { "○" } else { "●" }, row.title),
                    index == self.selected,
                ));
            }
            if detail_count > 0 {
                while lines.len() < rows - detail_count {
                    lines.push((String::new(), false));
                }
                lines.extend(
                    details
                        .into_iter()
                        .take(detail_count)
                        .map(|line| (line, false)),
                );
            }
        }
        lines.truncate(rows);
        lines
            .into_iter()
            .map(|(line, selected)| (fit_panel_text(&line, cols), selected))
            .collect()
    }
}

pub fn render_drawer(rows: usize, cols: usize, drawer: &mut PanelDrawer) {
    for (y, (line, selected)) in drawer.lines(rows, cols).into_iter().enumerate() {
        let text = Text::new(line);
        print_text_with_coordinates(
            if selected { text.selected() } else { text },
            0,
            y,
            Some(cols),
            Some(1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn long_inventory_scrolls_and_mouse_uses_the_painted_page() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(
                0,
                (0..30)
                    .map(|id| terminal(id, &format!("agent {id}")))
                    .collect(),
            )]),
            0,
            None,
            true,
        ));
        drawer.selected = 24;
        let lines = drawer.lines(8, 40);
        assert_eq!(lines.len(), 8);
        assert_eq!(lines[2], ("● agent 24".into(), true));
        assert_eq!(
            drawer.handle_click(2, 1),
            DrawerCommand::Focus(PaneId::Terminal(24))
        );
        assert_eq!(drawer.handle_click(8, 1), DrawerCommand::None);
        drawer.handle_key(&KeyWithModifier::new(BareKey::Char('d')));
        let lines = drawer.lines(8, 40);
        assert!(lines.iter().any(|(line, _)| line == "Details · d folds"));
        let details_y = lines
            .iter()
            .position(|(line, _)| line == "Details · d folds")
            .unwrap();
        assert_eq!(
            drawer.handle_click(details_y as isize, 1),
            DrawerCommand::None,
            "details are not panel rows"
        );
    }

    #[test]
    fn narrow_and_short_views_stay_in_bounds_with_unicode_ellipsis() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(
                0,
                vec![terminal(1, "Voc ZEN 世界 e\u{301} very long title")],
            )]),
            0,
            None,
            true,
        ));
        for expanded in [false, true] {
            drawer.details_expanded = expanded;
            for rows in 0..12 {
                for cols in 0..50 {
                    let lines = drawer.lines(rows, cols);
                    assert!(lines.len() <= rows);
                    assert!(lines.iter().all(|(line, _)| line.width() <= cols));
                }
            }
        }
        assert_eq!(fit_panel_text("世界 hello", 4), "世…");
        assert_eq!(fit_panel_text("e\u{301}hello", 2), "e\u{301}…");
    }

    #[test]
    fn drawer_discovery_never_reuses_another_tabs_layer() {
        let drawer = plugin(7, PANEL_DRAWER_TITLE, "vc-frame:compact-bar");
        let snap = manifest(&[(1, vec![drawer])]);
        assert_eq!(detect_panel_drawer(&snap, 0, true), (None, false));
        assert_eq!(detect_panel_drawer(&snap, 1, true), (Some(7), true));
    }

    #[test]
    fn refreshed_inventory_keeps_the_selected_pane_when_rows_reorder() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a"), terminal(2, "b")])]),
            0,
            None,
            true,
        ));
        drawer.select_index(1);
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(2, "b")])]),
            0,
            None,
            true,
        ));
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a"), terminal(2, "b")])]),
            0,
            None,
            true,
        ));
        assert_eq!(drawer.selected_row().unwrap().id, 2);
    }

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
    fn active_pager_tracks_focused_floating_panel() {
        let tiled = terminal(1, "shell");
        let mut b = terminal(9, "claude");
        b.is_floating = true;
        let mut a = terminal(4, "codex");
        a.is_floating = true;
        a.is_focused = true;
        let listed = inventory_for_tab(&manifest(&[(0, vec![tiled, b, a])]), 0, None, true);
        assert_eq!(active_pager(&listed), Some((1, 2)));
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

    fn visitor(id: u32, guest: &str) -> PaneInfo {
        PaneInfo {
            id,
            title: "VC Guest".to_owned(),
            is_plugin: false,
            is_selectable: true,
            terminal_command: Some(format!(
                "vc-frame --workspace-projection {{\"request_id\":\"r\",\"host\":\"h\",\"client_id\":1,\"plugin_id\":2,\"guest\":\"{guest}\",\"tab\":null,\"pane_id\":0}} visit {guest}"
            )),
            ..PaneInfo::default()
        }
    }

    fn scoped_panel(id: u32, title: &str, scope: PanelScope) -> PaneInfo {
        let mut pane = terminal(id, title);
        pane.is_floating = true;
        pane.panel_scope = Some(scope);
        pane
    }

    #[test]
    fn global_inventory_spans_tabs_in_position_order_with_tab_identity() {
        let snap = manifest(&[
            (1, vec![terminal(2, "two")]),
            (0, vec![terminal(1, "one")]),
        ]);
        let rows = inventory_global(&snap, None, true);
        assert_eq!(
            rows.iter()
                .map(|row| (row.id, row.tab_position))
                .collect::<Vec<_>>(),
            vec![(1, 0), (2, 1)],
            "Global follows tab order and every row knows its origin tab"
        );
    }

    #[test]
    fn projected_guest_is_parsed_from_the_projection_command_not_the_title() {
        let mut visitor_pane = visitor(2, "workspace-a");
        visitor_pane.title = "renamed by OSC".to_owned();
        let snap = manifest(&[(1, vec![visitor_pane, plugin(3, "rail", "vc-frame:session-manager")])]);
        assert_eq!(
            projected_guest_in_tab(&snap, 1).as_deref(),
            Some("workspace-a")
        );
        assert_eq!(projected_guest_in_tab(&snap, 0), None);
        let plain = manifest(&[(0, vec![terminal(1, "shell")])]);
        assert_eq!(projected_guest_in_tab(&plain, 0), None);
        // A truncated or unbalanced command is never a guessed guest.
        let mut broken = visitor(4, "workspace-b");
        broken.terminal_command = Some("vc-frame --workspace-projection {\"guest\":".to_owned());
        assert_eq!(
            projected_guest_in_tab(&manifest(&[(0, vec![broken])]), 0),
            None
        );
    }

    #[test]
    fn project_scope_keeps_the_current_guest_and_the_current_tab() {
        let current_tab_shell = terminal(1, "shell");
        let guest_panel = scoped_panel(3, "agent-a", PanelScope::Project("workspace-a".into()));
        let other_guest_panel = scoped_panel(4, "agent-b", PanelScope::Project("workspace-b".into()));
        let pinned_elsewhere = scoped_panel(5, "pinned", PanelScope::Global);
        let snap = manifest(&[
            (0, vec![current_tab_shell]),
            (1, vec![visitor(2, "workspace-a"), guest_panel]),
            (2, vec![other_guest_panel, pinned_elsewhere]),
        ]);
        let global = inventory_for_scope(&snap, 0, Some(1), None, true, DrawerScope::Global);
        assert_eq!(global.len(), 5, "Global is the full switcher");
        let project = inventory_for_scope(&snap, 0, Some(1), None, true, DrawerScope::Project);
        assert_eq!(
            project.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![1, 3],
            "Project = current tab + panels bound to the projected guest"
        );
        // Focus from a Global row on another tab keeps its tab identity.
        let row = global.iter().find(|row| row.id == 4).unwrap();
        assert_eq!(row.tab_position, 2);
    }

    #[test]
    fn project_scope_without_a_projected_guest_is_the_current_tab() {
        let snap = manifest(&[
            (0, vec![terminal(1, "shell")]),
            (1, vec![scoped_panel(2, "agent", PanelScope::Unbound)]),
        ]);
        let project = inventory_for_scope(&snap, 0, Some(1), None, true, DrawerScope::Project);
        assert_eq!(project.iter().map(|row| row.id).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn scope_chips_toggle_with_f_and_click_on_the_painted_columns() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a")])]),
            0,
            None,
            true,
        ));
        assert_eq!(drawer.scope, DrawerScope::Global);
        assert_eq!(
            drawer.handle_key(&KeyWithModifier::new(BareKey::Char('f'))),
            DrawerCommand::SetScope(DrawerScope::Project)
        );

        let lines = drawer.lines(10, 80);
        assert!(lines[0].0.contains("[●Global]"));
        assert!(lines[0].0.contains("[ Project ]"));
        assert!(lines[1].0.contains("f filter"));
        // Mouse columns are character columns; the header holds multibyte
        // glyphs, so measure in chars, not bytes.
        let char_col = |line: &str, needle: &str| line[..line.find(needle).unwrap()].chars().count();
        let project_col = char_col(&lines[0].0, "[ Project ]");
        assert_eq!(
            drawer.handle_click(0, project_col + 2),
            DrawerCommand::SetScope(DrawerScope::Project),
            "clicking the Project chip selects it"
        );
        drawer.scope = DrawerScope::Project;
        let lines = drawer.lines(10, 80);
        assert!(lines[0].0.contains("[●Project]"));
        let global_col = char_col(&lines[0].0, "[ Global ]");
        assert_eq!(
            drawer.handle_click(0, global_col),
            DrawerCommand::SetScope(DrawerScope::Global)
        );
        assert_eq!(
            drawer.handle_click(0, 0),
            DrawerCommand::None,
            "the count itself is not a chip"
        );
        // A clipped header carries no clickable chips.
        drawer.lines(10, 8);
        assert_eq!(drawer.handle_click(0, 5), DrawerCommand::None);
    }

    #[test]
    fn a_panels_row_dies_with_its_pane() {
        let mut drawer = PanelDrawer::default();
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a"), terminal(2, "b")])]),
            0,
            None,
            true,
        ));
        assert_eq!(drawer.rows.len(), 2);
        // The server manifest without pane 2 = the panel is closed; the
        // switcher entry must not outlive it.
        drawer.replace_rows(inventory_for_tab(
            &manifest(&[(0, vec![terminal(1, "a")])]),
            0,
            None,
            true,
        ));
        assert_eq!(drawer.rows.len(), 1);
        assert!(drawer.rows.iter().all(|row| row.id == 1));
    }
}
