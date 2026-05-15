use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::ui::{
    components::{minimize_lines, Colors, LineToRender, ListItem},
    SelectedIndex, SessionUiInfo,
};

macro_rules! render_assets {
    ($assets:expr, $line_count_to_remove:expr, $selected_index:expr, $to_render_until_selected: expr, $to_render_after_selected:expr, $has_deeper_selected_assets:expr, $max_cols:expr, $colors:expr) => {{
        let (start_index, anchor_asset_index, end_index, line_count_to_remove) =
            minimize_lines($assets.len(), $line_count_to_remove, $selected_index);
        let mut truncated_result_count_above = start_index;
        let mut truncated_result_count_below = $assets.len().saturating_sub(end_index);
        let mut current_index = 1;
        if let Some(assets_to_render_before_selected) = $assets.get(start_index..anchor_asset_index)
        {
            for asset in assets_to_render_before_selected {
                let mut asset: LineToRender =
                    asset.as_line_to_render(current_index, $max_cols, $colors);
                asset.add_truncated_results(truncated_result_count_above);
                truncated_result_count_above = 0;
                current_index += 1;
                $to_render_until_selected.push(asset);
            }
        }
        if let Some(selected_asset) = $assets.get(anchor_asset_index) {
            if $selected_index.is_some() && !$has_deeper_selected_assets {
                let mut selected_asset: LineToRender =
                    selected_asset.as_line_to_render(current_index, $max_cols, $colors);
                selected_asset.make_selected(true);
                selected_asset.add_truncated_results(truncated_result_count_above);
                if anchor_asset_index + 1 >= end_index {
                    selected_asset.add_truncated_results(truncated_result_count_below);
                }
                current_index += 1;
                $to_render_until_selected.push(selected_asset);
            } else {
                $to_render_until_selected.push(selected_asset.as_line_to_render(
                    current_index,
                    $max_cols,
                    $colors,
                ));
                current_index += 1;
            }
        }
        if let Some(assets_to_render_after_selected) =
            $assets.get(anchor_asset_index + 1..end_index)
        {
            for asset in assets_to_render_after_selected.iter().rev() {
                let mut asset: LineToRender =
                    asset.as_line_to_render(current_index, $max_cols, $colors);
                asset.add_truncated_results(truncated_result_count_below);
                truncated_result_count_below = 0;
                current_index += 1;
                $to_render_after_selected.insert(0, asset.into());
            }
        }
        line_count_to_remove
    }};
}

#[derive(Debug, Default)]
pub struct SessionList {
    pub session_ui_infos: Vec<SessionUiInfo>,
    pub forbidden_sessions: Vec<SessionUiInfo>,
    pub selected_index: SelectedIndex,
    pub selected_search_index: Option<usize>,
    pub search_results: Vec<SearchResult>,
    pub is_searching: bool,
}

impl SessionList {
    pub fn set_sessions(
        &mut self,
        mut session_ui_infos: Vec<SessionUiInfo>,
        mut forbidden_sessions: Vec<SessionUiInfo>,
    ) {
        session_ui_infos.sort_unstable_by(|a, b| {
            if a.is_current_session {
                std::cmp::Ordering::Less
            } else if b.is_current_session {
                std::cmp::Ordering::Greater
            } else {
                a.name.cmp(&b.name)
            }
        });
        forbidden_sessions.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        self.session_ui_infos = session_ui_infos;
        self.forbidden_sessions = forbidden_sessions;
    }
    pub fn render(&self, max_rows: usize, max_cols: usize, colors: Colors) -> Vec<LineToRender> {
        if self.is_searching {
            self.render_search_results(max_rows, max_cols)
        } else {
            self.render_list(max_rows, max_cols, colors)
        }
    }
    fn render_search_results(&self, max_rows: usize, max_cols: usize) -> Vec<LineToRender> {
        let mut lines_to_render = vec![];
        for (i, result) in self.search_results.iter().enumerate() {
            if lines_to_render.len() + result.lines_to_render() <= max_rows {
                let mut result_lines = result.render(max_cols);
                if Some(i) == self.selected_search_index {
                    let mut render_arrows = true;
                    for line_to_render in result_lines.iter_mut() {
                        line_to_render.make_selected_as_search(render_arrows);
                        render_arrows = false;
                    }
                }
                lines_to_render.append(&mut result_lines);
            } else {
                break;
            }
        }
        lines_to_render
    }
    fn render_list(&self, max_rows: usize, max_cols: usize, colors: Colors) -> Vec<LineToRender> {
        let mut lines_to_render_until_selected = vec![];
        let mut lines_to_render_after_selected = vec![];
        let total_lines_to_render = self.total_lines_to_render();
        let line_count_to_remove = total_lines_to_render.saturating_sub(max_rows);
        let line_count_to_remove = self.render_sessions(
            &mut lines_to_render_until_selected,
            &mut lines_to_render_after_selected,
            line_count_to_remove,
            max_cols,
            colors,
        );
        let line_count_to_remove = self.render_tabs(
            &mut lines_to_render_until_selected,
            &mut lines_to_render_after_selected,
            line_count_to_remove,
            max_cols,
            colors,
        );
        self.render_panes(
            &mut lines_to_render_until_selected,
            &mut lines_to_render_after_selected,
            line_count_to_remove,
            max_cols,
            colors,
        );
        let mut lines_to_render = lines_to_render_until_selected;
        lines_to_render.append(&mut lines_to_render_after_selected);
        lines_to_render
    }
    fn render_sessions(
        &self,
        to_render_until_selected: &mut Vec<LineToRender>,
        to_render_after_selected: &mut Vec<LineToRender>,
        line_count_to_remove: usize,
        max_cols: usize,
        colors: Colors,
    ) -> usize {
        render_assets!(
            self.session_ui_infos,
            line_count_to_remove,
            self.selected_index.0,
            to_render_until_selected,
            to_render_after_selected,
            self.selected_index.1.is_some(),
            max_cols,
            colors
        )
    }
    fn render_tabs(
        &self,
        to_render_until_selected: &mut Vec<LineToRender>,
        to_render_after_selected: &mut Vec<LineToRender>,
        line_count_to_remove: usize,
        max_cols: usize,
        colors: Colors,
    ) -> usize {
        if self.selected_index.1.is_none() {
            return line_count_to_remove;
        }
        if let Some(tabs_in_session) = self
            .selected_index
            .0
            .and_then(|i| self.session_ui_infos.get(i))
            .map(|s| &s.tabs)
        {
            render_assets!(
                tabs_in_session,
                line_count_to_remove,
                self.selected_index.1,
                to_render_until_selected,
                to_render_after_selected,
                self.selected_index.2.is_some(),
                max_cols,
                colors
            )
        } else {
            line_count_to_remove
        }
    }
    fn render_panes(
        &self,
        to_render_until_selected: &mut Vec<LineToRender>,
        to_render_after_selected: &mut Vec<LineToRender>,
        line_count_to_remove: usize,
        max_cols: usize,
        colors: Colors,
    ) -> usize {
        if self.selected_index.2.is_none() {
            return line_count_to_remove;
        }
        if let Some(panes_in_session) = self
            .selected_index
            .0
            .and_then(|i| self.session_ui_infos.get(i))
            .map(|s| &s.tabs)
            .and_then(|tabs| {
                self.selected_index
                    .1
                    .and_then(|i| tabs.get(i))
                    .map(|t| &t.panes)
            })
        {
            render_assets!(
                panes_in_session,
                line_count_to_remove,
                self.selected_index.2,
                to_render_until_selected,
                to_render_after_selected,
                false,
                max_cols,
                colors
            )
        } else {
            line_count_to_remove
        }
    }
    fn total_lines_to_render(&self) -> usize {
        self.session_ui_infos
            .iter()
            .enumerate()
            .fold(0, |acc, (index, s)| {
                if self.selected_index.session_index_is_selected(index) {
                    acc + s.line_count(&self.selected_index)
                } else {
                    acc + 1
                }
            })
    }
    pub fn update_search_term(&mut self, search_term: &str, colors: &Colors) {
        let mut flattened_assets = self.flatten_assets(colors);
        let mut matches = vec![];
        let matcher = SkimMatcherV2::default().use_cache(true);
        for (list_item, session_name, tab_position, pane_id, is_current_session) in
            flattened_assets.drain(..)
        {
            if let Some((score, indices)) = matcher.fuzzy_indices(&list_item.name, search_term) {
                matches.push(SearchResult::new(
                    score,
                    indices,
                    list_item,
                    session_name,
                    tab_position,
                    pane_id,
                    is_current_session,
                ));
            }
        }
        matches.sort_by(|a, b| b.score.cmp(&a.score));
        self.search_results = matches;
        self.is_searching = !search_term.is_empty();
        self.selected_search_index = Some(0);
    }
    fn flatten_assets(
        &self,
        colors: &Colors,
    ) -> Vec<(ListItem, String, Option<usize>, Option<(u32, bool)>, bool)> {
        // list_item, session_name, tab_position, (pane_id, is_plugin), is_current_session
        let mut list_items = vec![];
        for session in &self.session_ui_infos {
            let session_name = session.name.clone();
            let is_current_session = session.is_current_session;
            list_items.push((
                ListItem::from_session_info(session, *colors),
                session_name.clone(),
                None,
                None,
                is_current_session,
            ));
            for tab in &session.tabs {
                let tab_position = tab.position;
                list_items.push((
                    ListItem::from_tab_info(session, tab, *colors),
                    session_name.clone(),
                    Some(tab_position),
                    None,
                    is_current_session,
                ));
                for pane in &tab.panes {
                    let pane_id = (pane.pane_id, pane.is_plugin);
                    list_items.push((
                        ListItem::from_pane_info(session, tab, pane, *colors),
                        session_name.clone(),
                        Some(tab_position),
                        Some(pane_id),
                        is_current_session,
                    ));
                }
            }
        }
        list_items
    }
    pub fn get_selected_session_name(&self) -> Option<String> {
        if self.is_searching {
            self.selected_search_index
                .and_then(|i| self.search_results.get(i))
                .map(|s| s.session_name.clone())
        } else {
            self.selected_index
                .0
                .and_then(|i| self.session_ui_infos.get(i))
                .map(|s_i| s_i.name.clone())
        }
    }
    pub fn selected_is_current_session(&self) -> bool {
        if self.is_searching {
            self.selected_search_index
                .and_then(|i| self.search_results.get(i))
                .map(|s| s.is_current_session)
                .unwrap_or(false)
        } else {
            self.selected_index
                .0
                .and_then(|i| self.session_ui_infos.get(i))
                .map(|s_i| s_i.is_current_session)
                .unwrap_or(false)
        }
    }
    pub fn get_selected_tab_position(&self) -> Option<usize> {
        if self.is_searching {
            self.selected_search_index
                .and_then(|i| self.search_results.get(i))
                .and_then(|s| s.tab_position)
        } else {
            self.selected_index
                .0
                .and_then(|i| self.session_ui_infos.get(i))
                .and_then(|s_i| {
                    self.selected_index
                        .1
                        .and_then(|i| s_i.tabs.get(i))
                        .map(|t| t.position)
                })
        }
    }
    pub fn get_selected_pane_id(&self) -> Option<(u32, bool)> {
        // (pane_id, is_plugin)
        if self.is_searching {
            self.selected_search_index
                .and_then(|i| self.search_results.get(i))
                .and_then(|s| s.pane_id)
        } else {
            self.selected_index
                .0
                .and_then(|i| self.session_ui_infos.get(i))
                .and_then(|s_i| {
                    self.selected_index
                        .1
                        .and_then(|i| s_i.tabs.get(i))
                        .and_then(|t| {
                            self.selected_index
                                .2
                                .and_then(|i| t.panes.get(i))
                                .map(|p| (p.pane_id, p.is_plugin))
                        })
                })
        }
    }
    pub fn move_selection_down(&mut self) {
        if self.is_searching {
            match self.selected_search_index.as_mut() {
                Some(search_index) => {
                    *search_index = search_index.saturating_add(1);
                },
                None => {
                    if !self.search_results.is_empty() {
                        self.selected_search_index = Some(0);
                    }
                },
            }
        } else {
            match self.selected_index {
                SelectedIndex(None, None, None) => {
                    if !self.session_ui_infos.is_empty() {
                        self.selected_index.0 = Some(0);
                    }
                },
                SelectedIndex(Some(selected_session), None, None) => {
                    if self.session_ui_infos.len() > selected_session + 1 {
                        self.selected_index.0 = Some(selected_session + 1);
                    } else {
                        self.selected_index.0 = None;
                        self.selected_index.1 = None;
                        self.selected_index.2 = None;
                    }
                },
                SelectedIndex(Some(selected_session), Some(selected_tab), None) => {
                    if self
                        .get_session(selected_session)
                        .map(|s| s.tabs.len() > selected_tab + 1)
                        .unwrap_or(false)
                    {
                        self.selected_index.1 = Some(selected_tab + 1);
                    } else {
                        self.selected_index.1 = Some(0);
                    }
                },
                SelectedIndex(Some(selected_session), Some(selected_tab), Some(selected_pane)) => {
                    if self
                        .get_session(selected_session)
                        .and_then(|s| s.tabs.get(selected_tab))
                        .map(|t| t.panes.len() > selected_pane + 1)
                        .unwrap_or(false)
                    {
                        self.selected_index.2 = Some(selected_pane + 1);
                    } else {
                        self.selected_index.2 = Some(0);
                    }
                },
                _ => {},
            }
        }
    }
    pub fn move_selection_up(&mut self) {
        if self.is_searching {
            match self.selected_search_index.as_mut() {
                Some(search_index) => {
                    *search_index = search_index.saturating_sub(1);
                },
                None => {
                    if !self.search_results.is_empty() {
                        self.selected_search_index = Some(0);
                    }
                },
            }
        } else {
            match self.selected_index {
                SelectedIndex(None, None, None) => {
                    if !self.session_ui_infos.is_empty() {
                        self.selected_index.0 = Some(self.session_ui_infos.len().saturating_sub(1))
                    }
                },
                SelectedIndex(Some(selected_session), None, None) => {
                    if selected_session > 0 {
                        self.selected_index.0 = Some(selected_session - 1);
                    } else {
                        self.selected_index.0 = None;
                    }
                },
                SelectedIndex(Some(selected_session), Some(selected_tab), None) => {
                    if selected_tab > 0 {
                        self.selected_index.1 = Some(selected_tab - 1);
                    } else {
                        let tab_count = self
                            .get_session(selected_session)
                            .map(|s| s.tabs.len())
                            .unwrap_or(0);
                        self.selected_index.1 = Some(tab_count.saturating_sub(1))
                    }
                },
                SelectedIndex(Some(selected_session), Some(selected_tab), Some(selected_pane)) => {
                    if selected_pane > 0 {
                        self.selected_index.2 = Some(selected_pane - 1);
                    } else {
                        let pane_count = self
                            .get_session(selected_session)
                            .and_then(|s| s.tabs.get(selected_tab))
                            .map(|t| t.panes.len())
                            .unwrap_or(0);
                        self.selected_index.2 = Some(pane_count.saturating_sub(1))
                    }
                },
                _ => {},
            }
        }
    }
    fn get_session(&self, index: usize) -> Option<&SessionUiInfo> {
        self.session_ui_infos.get(index)
    }
    pub fn result_expand(&mut self) {
        // we can't move this to SelectedIndex because the borrow checker is mean
        match self.selected_index {
            SelectedIndex(Some(selected_session), None, None) => {
                let selected_session_has_tabs = self
                    .get_session(selected_session)
                    .map(|s| !s.tabs.is_empty())
                    .unwrap_or(false);
                if selected_session_has_tabs {
                    self.selected_index.1 = Some(0);
                }
            },
            SelectedIndex(Some(selected_session), Some(selected_tab), None) => {
                let selected_tab_has_panes = self
                    .get_session(selected_session)
                    .and_then(|s| s.tabs.get(selected_tab))
                    .map(|t| !t.panes.is_empty())
                    .unwrap_or(false);
                if selected_tab_has_panes {
                    self.selected_index.2 = Some(0);
                }
            },
            _ => {},
        }
    }
    pub fn result_shrink(&mut self) {
        self.selected_index.result_shrink();
    }
    pub fn update_rows(&mut self, rows: usize) {
        if let Some(search_result_rows_until_selected) = self.selected_search_index.map(|i| {
            self.search_results
                .iter()
                .enumerate()
                .take(i + 1)
                .fold(0, |acc, s| acc + s.1.lines_to_render())
        }) {
            if search_result_rows_until_selected > rows
                || self.selected_search_index >= Some(self.search_results.len())
            {
                self.selected_search_index = None;
            }
        }
    }
    pub fn reset_selected_index(&mut self) {
        self.selected_index.reset();
    }
    /// After deleting one or more entries (and re-running `update_search_term`
    /// to rebuild `search_results`), put the cursor back on a sensible
    /// neighbour at the same numeric row -- the entry that took the deleted
    /// row's slot, or the last entry if the deleted row was at the end. The
    /// caller passes the indices captured **before** the deletion so this
    /// method can clamp them to the new list/search lengths.
    pub fn restore_selection_after_delete(
        &mut self,
        was_searching: bool,
        prev_search_idx: Option<usize>,
        prev_top_idx: Option<usize>,
    ) {
        if was_searching {
            let len = self.search_results.len();
            self.selected_search_index = clamp_index_after_delete(prev_search_idx, len);
        } else {
            // Tab / pane subselectors point into a session that has just
            // shifted in the list; clear them so the top-level index alone
            // describes the new selection.
            self.selected_index.1 = None;
            self.selected_index.2 = None;
            self.selected_index.0 =
                clamp_index_after_delete(prev_top_idx, self.session_ui_infos.len());
        }
    }
    pub fn has_session(&self, session_name: &str) -> bool {
        self.session_ui_infos.iter().any(|s| s.name == session_name)
    }
    pub fn has_forbidden_session(&self, session_name: &str) -> bool {
        self.forbidden_sessions
            .iter()
            .any(|s| s.name == session_name)
    }
    pub fn update_session_name(&mut self, old_name: &str, new_name: &str) {
        self.session_ui_infos
            .iter_mut()
            .find(|s| s.name == old_name)
            .map(|s| s.name = new_name.to_owned());
    }
    pub fn all_other_sessions(&self) -> Vec<String> {
        self.session_ui_infos
            .iter()
            .filter_map(|s| {
                if !s.is_current_session {
                    Some(s.name.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Clamp a pre-delete index to a post-delete length. Returns the same
/// numeric row when it still exists (so the cursor lands on whatever entry
/// took the deleted row's slot), the last index when the deletion happened
/// at the tail, or `None` when the list is now empty.
pub fn clamp_index_after_delete(prev_index: Option<usize>, new_len: usize) -> Option<usize> {
    if new_len == 0 {
        return None;
    }
    Some(prev_index.unwrap_or(0).min(new_len - 1))
}

#[derive(Debug)]
pub struct SearchResult {
    score: i64,
    indices: Vec<usize>,
    list_item: ListItem,
    session_name: String,
    tab_position: Option<usize>,
    pane_id: Option<(u32, bool)>,
    is_current_session: bool,
}

impl SearchResult {
    pub fn new(
        score: i64,
        indices: Vec<usize>,
        list_item: ListItem,
        session_name: String,
        tab_position: Option<usize>,
        pane_id: Option<(u32, bool)>,
        is_current_session: bool,
    ) -> Self {
        SearchResult {
            score,
            indices,
            list_item,
            session_name,
            tab_position,
            pane_id,
            is_current_session,
        }
    }
    pub fn lines_to_render(&self) -> usize {
        self.list_item.line_count()
    }
    pub fn render(&self, max_width: usize) -> Vec<LineToRender> {
        self.list_item.render(Some(self.indices.clone()), max_width)
    }
}
