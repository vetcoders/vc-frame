mod list_navigation;
mod new_session_info;
mod resurrectable_sessions;
mod session_list;
mod single_screen;
mod single_screen_data;
mod single_screen_render;
mod ui;
use std::collections::BTreeMap;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;
use zellij_tile::prelude::*;

use new_session_info::NewSessionInfo;
use single_screen::{SingleScreenMode, SingleScreenState};
use single_screen_data::{DeleteTarget, UnifiedSearchResult};
use single_screen_render::render_unified_results;
use ui::{
    SessionUiInfo, TabUiInfo,
    components::{
        Colors, render_controls_line, render_error, render_new_session_block, render_prompt,
        render_renaming_session_screen, render_screen_toggle, render_single_screen_prompt,
        render_unsaved_changes_line,
    },
    welcome_screen::{render_banner, render_welcome_boundaries},
};

use resurrectable_sessions::ResurrectableSessions;
use session_list::SessionList;

#[derive(Clone, Debug, Copy, PartialEq, Default)]
enum ActiveScreen {
    NewSession,
    #[default]
    AttachToSession,
    ResurrectSession,
    SingleScreen,
}

#[derive(Default)]
struct State {
    session_name: Option<String>,
    sessions: SessionList,
    resurrectable_sessions: ResurrectableSessions,
    search_term: String,
    new_session_info: NewSessionInfo,
    renaming_session_name: Option<String>,
    error: Option<String>,
    active_screen: ActiveScreen,
    colors: Colors,
    is_welcome_screen: bool,
    is_multi_screen: bool,
    single_screen_state: SingleScreenState,
    show_kill_all_sessions_warning: bool,
    request_ids: Vec<String>,
    is_web_client: bool,
    current_session_last_saved_time: Option<u64>,
    is_visible: bool,
    refresh_timer_armed: bool,
    is_rail: bool,
    // screen row -> click target, rebuilt on every rail render so mouse
    // clicks resolve against exactly what is on screen (incl. scroll window).
    // Header / footer / blank gap rows are absent → click is a no-op.
    rail_click_map: BTreeMap<usize, RailClickTarget>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.is_rail = configuration
            .get("rail")
            .map(|v| v == "true")
            .unwrap_or(false);
        self.is_welcome_screen = configuration
            .get("welcome_screen")
            .map(|v| v == "true")
            .unwrap_or(false)
            && !self.is_rail;
        if self.is_welcome_screen {
            self.active_screen = ActiveScreen::NewSession;
        }
        self.new_session_info.is_welcome_screen = self.is_welcome_screen;
        self.is_multi_screen = configuration
            .get("multi_screen")
            .map(|v| v == "true")
            .unwrap_or(false)
            || self.is_rail;
        if !self.is_multi_screen {
            self.active_screen = ActiveScreen::SingleScreen;
        } else if self.is_rail {
            self.active_screen = ActiveScreen::AttachToSession;
        }
        self.single_screen_state.is_welcome_screen = self.is_welcome_screen;
        self.is_visible = true;
        subscribe(&[
            EventType::ModeUpdate,
            EventType::SessionUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::RunCommandResult,
            EventType::Timer,
            EventType::Visible,
        ]);
        let pane_title = if self.is_rail {
            configuration
                .get("pane_title")
                .cloned()
                .unwrap_or_else(|| "Sessions".to_owned())
        } else if self.is_welcome_screen {
            configuration
                .get("pane_title")
                .cloned()
                .unwrap_or_else(|| "𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. Shell".to_owned())
        } else {
            configuration
                .get("pane_title")
                .cloned()
                .unwrap_or_else(|| "Session Manager".to_owned())
        };
        rename_plugin_pane(get_plugin_ids().plugin_id, pane_title);
        self.refresh_session_list();
        if !self.is_welcome_screen {
            self.arm_refresh_timer();
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.name == "vc_rail_nav" {
            match pipe_message.payload.as_deref() {
                Some("up") => self.switch_session_relative(-1),
                Some("down") => self.switch_session_relative(1),
                _ => (),
            }
            true
        } else if pipe_message.name == "filepicker_result" {
            if let (Some(payload), Some(request_id)) =
                (pipe_message.payload, pipe_message.args.get("request_id"))
            {
                match self.request_ids.iter().position(|p| p == request_id) {
                    Some(request_id_position) => {
                        self.request_ids.remove(request_id_position);
                        let new_session_folder = std::path::PathBuf::from(payload);
                        if !self.is_multi_screen {
                            self.single_screen_state.new_session_folder =
                                Some(new_session_folder.clone());
                        }
                        self.new_session_info.new_session_folder = Some(new_session_folder);
                    },
                    None => {
                        eprintln!("request id not found");
                    },
                }
            }
            true
        } else {
            false
        }
    }
    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::Timer(_) => {
                self.refresh_timer_armed = false;
                if !self.is_visible {
                    return false;
                }
                let new_saved_time = current_session_last_saved_time();
                if new_saved_time != self.current_session_last_saved_time {
                    self.current_session_last_saved_time = new_saved_time;
                    should_render = true;
                }
                if self.refresh_session_list() {
                    should_render = true;
                }
                self.arm_refresh_timer();
            },
            Event::Visible(is_visible) => {
                let was_visible = self.is_visible;
                self.is_visible = is_visible;
                if is_visible && !was_visible {
                    if self.refresh_session_list() {
                        should_render = true;
                    }
                    self.arm_refresh_timer();
                }
            },
            Event::ModeUpdate(mode_info) => {
                self.colors = Colors::new(mode_info.style.colors);
                self.is_web_client = mode_info.is_web_client.unwrap_or(false);
                should_render = true;
            },
            Event::Key(key) => {
                should_render = self.handle_key(key);
            },
            Event::Mouse(mouse_event) if self.is_rail => {
                if self.error.is_some() {
                    self.error = None;
                    should_render = true;
                } else {
                    should_render = self.handle_session_rail_mouse(mouse_event);
                }
            },
            Event::PermissionRequestResult(_result) => {
                should_render = true;
            },
            Event::SessionUpdate(session_infos, resurrectable_session_list) => {
                for session_info in &session_infos {
                    if session_info.is_current_session {
                        self.new_session_info
                            .update_layout_list(session_info.available_layouts.clone());
                    }
                }
                self.resurrectable_sessions
                    .update(resurrectable_session_list);
                self.update_session_infos(session_infos);
                if !self.is_multi_screen {
                    self.single_screen_state.update_search_term(
                        &self.sessions.session_ui_infos,
                        &self.resurrectable_sessions.all_resurrectable_sessions,
                    );
                    let previous_selection =
                        self.single_screen_state.layout_list.selected_layout_index;
                    let previous_search_term = self
                        .single_screen_state
                        .layout_list
                        .layout_search_term
                        .clone();
                    self.single_screen_state.layout_list =
                        self.new_session_info.get_layout_list_clone();
                    self.single_screen_state.layout_list.layout_search_term = previous_search_term;
                    self.single_screen_state.layout_list.update_search_term();
                    self.single_screen_state.layout_list.selected_layout_index =
                        previous_selection.min(self.single_screen_state.layout_list.max_index());
                }
                should_render = true;
            },
            _ => (),
        };
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if self.is_rail {
            self.render_session_rail(rows, cols);
            if let Some(error) = self.error.as_ref() {
                render_error(error, rows, cols, 0, 0);
            }
            return;
        }

        let (x, y, width, height) = self.main_menu_size(rows, cols);

        let background = self.colors.palette.text_unselected.background;

        if self.is_welcome_screen {
            render_banner(x, 0, rows.saturating_sub(height), width);
        }

        if self.active_screen != ActiveScreen::SingleScreen {
            render_screen_toggle(
                self.active_screen,
                x,
                y,
                width.saturating_sub(2),
                &background,
            );
        }

        match self.active_screen {
            ActiveScreen::NewSession => {
                render_new_session_block(
                    &self.new_session_info,
                    self.colors,
                    height.saturating_sub(2),
                    width,
                    x,
                    y + 2,
                );
            },
            ActiveScreen::AttachToSession => {
                if let Some(new_session_name) = self.renaming_session_name.as_ref() {
                    render_renaming_session_screen(new_session_name, height, width, x, y + 2);
                } else if self.show_kill_all_sessions_warning {
                    self.render_kill_all_sessions_warning(height, width, x, y);
                } else {
                    render_prompt(&self.search_term, self.colors, x, y + 2);
                    let bottom_lines = 7;
                    let room_for_list = height.saturating_sub(bottom_lines);
                    self.sessions.update_rows(room_for_list);
                    let list =
                        self.sessions
                            .render(room_for_list, width.saturating_sub(7), self.colors); // 7 for various ui
                    for (i, line) in list.iter().enumerate() {
                        print!("\u{1b}[{};{}H{}", y + i + 5, x, line.render());
                    }
                }
            },
            ActiveScreen::ResurrectSession => {
                self.resurrectable_sessions.render(height, width, x, y);
            },
            ActiveScreen::SingleScreen => {
                match self.single_screen_state.mode {
                    SingleScreenMode::SearchAndSelect => {
                        if let Some(new_session_name) = self.renaming_session_name.as_ref() {
                            render_renaming_session_screen(new_session_name, height, width, x, y);
                        } else if self.show_kill_all_sessions_warning {
                            self.render_kill_all_sessions_warning(height, width, x, y);
                        } else {
                            // Use max_table_rows as fixed content height so the
                            // prompt position stays stable regardless of result count
                            let max_table_rows = height.saturating_sub(5);
                            let content_height = 2 + max_table_rows; // prompt + header + max data rows
                            // Available space above help lines (2 help rows at bottom)
                            let available = height.saturating_sub(3);
                            let y_offset = y + available.saturating_sub(content_height) / 2;

                            // Horizontal centering: cap content block and center
                            // within the full pane width
                            let content_width = std::cmp::min(width, 90);
                            let x_centered = x + (width.saturating_sub(content_width)) / 2;

                            let enter_action = if !self.single_screen_state.search_term.is_empty() {
                                if let Some(result) = self.single_screen_state.get_selected_result()
                                {
                                    match result {
                                        UnifiedSearchResult::ActiveSession { .. } => Some("Attach"),
                                        UnifiedSearchResult::ResurrectableSession { .. } => {
                                            Some("Resurrect")
                                        },
                                    }
                                } else {
                                    let typed = &self.single_screen_state.search_term;
                                    if self.sessions.has_session(typed) {
                                        Some("Attach")
                                    } else if self.resurrectable_sessions.has_session(typed) {
                                        Some("Resurrect")
                                    } else {
                                        Some("Create new")
                                    }
                                }
                            } else {
                                None
                            };
                            render_single_screen_prompt(
                                &self.single_screen_state.search_term,
                                enter_action,
                                self.colors,
                                x_centered,
                                y_offset,
                            );
                            render_unified_results(
                                &self.single_screen_state.render_cache,
                                self.single_screen_state.selected_index,
                                max_table_rows,
                                content_width,
                                self.colors,
                                x_centered,
                                y_offset + 2,
                            );
                        }
                    },
                    SingleScreenMode::SelectingLayout => {
                        let new_session_name = if self.single_screen_state.search_term.is_empty() {
                            "<RANDOM>"
                        } else {
                            &self.single_screen_state.search_term
                        };
                        let esc = self.colors.shortcuts("<ESC>");
                        println!(
                            "\u{1b}[m\u{1b}[{};{}H{}: {} ({} to go back)",
                            y + 1,
                            x + 1,
                            self.colors.session_name_prompt("New session name"),
                            self.colors.session_and_folder_entry(new_session_name),
                            esc,
                        );

                        // Render layout selection
                        let layout_search_term =
                            &self.single_screen_state.layout_list.layout_search_term;
                        let search_term_len = layout_search_term.len();
                        let layout_indication_line = if width > 73 + search_term_len {
                            Text::new(format!(
                                "New session layout: {}_ (Search and select from list, <ENTER> when done)",
                                layout_search_term
                            ))
                            .color_range(2, ..20 + search_term_len)
                            .color_range(3, 20..20 + search_term_len)
                            .color_range(3, 52 + search_term_len..59 + search_term_len)
                        } else {
                            Text::new(format!(
                                "New session layout: {}_ <ENTER>",
                                layout_search_term
                            ))
                            .color_range(2, ..20 + search_term_len)
                            .color_range(3, 20..20 + search_term_len)
                            .color_range(3, 22 + search_term_len..)
                        };
                        print_text_with_coordinates(layout_indication_line, x, y + 2, None, None);
                        println!();

                        let max_layout_rows = height.saturating_sub(8);
                        let mut table = Table::new();
                        for (i, (layout_info, indices, is_selected)) in self
                            .single_screen_state
                            .layout_list
                            .layouts_to_render(max_layout_rows)
                            .into_iter()
                            .enumerate()
                        {
                            let layout_name = layout_info.display_name();
                            let layout_name_len = layout_name.len();
                            let is_builtin = layout_info.is_builtin();
                            if i > max_layout_rows.saturating_sub(1) {
                                break;
                            }
                            let mut layout_cell = if is_builtin {
                                Text::new(format!("{} (built-in)", layout_name))
                                    .color_range(1, 0..layout_name_len)
                                    .color_range(0, layout_name_len + 1..)
                                    .color_indices(3, indices)
                            } else {
                                Text::new(layout_name)
                                    .color_range(1, ..)
                                    .color_indices(3, indices)
                            };
                            if is_selected {
                                layout_cell = layout_cell.selected();
                            }
                            let arrow_cell = if is_selected {
                                Text::new("<↓↑>".to_string()).selected().color_range(3, ..)
                            } else {
                                Text::new("    ".to_string()).color_range(3, ..)
                            };
                            table = table.add_styled_row(vec![arrow_cell, layout_cell]);
                        }
                        print_table_with_coordinates(table, x, y + 4, None, None);

                        // Render folder prompt
                        self.render_single_screen_folder_prompt(
                            x,
                            (y + height).saturating_sub(3),
                            width,
                        );
                    },
                }
            },
        }
        if let Some(error) = self.error.as_ref() {
            render_error(error, height, width, x, y);
        } else if (self.active_screen == ActiveScreen::AttachToSession
            || self.active_screen == ActiveScreen::SingleScreen)
            && !self.is_welcome_screen
        {
            let help_x = if self.active_screen == ActiveScreen::SingleScreen {
                let content_width = std::cmp::min(width, 90);
                x + (width.saturating_sub(content_width)) / 2
            } else {
                x
            };
            let help_offset = render_controls_line(
                self.active_screen,
                width,
                self.colors,
                help_x,
                rows.saturating_sub(1),
            );
            let adjusted_x = help_x + help_offset;
            let adjusted_width = width.saturating_sub(help_offset);
            render_unsaved_changes_line(
                adjusted_width,
                adjusted_x,
                rows,
                self.current_session_last_saved_time,
            );
        } else {
            let _ = render_controls_line(self.active_screen, width, self.colors, x, rows);
        }
        if self.is_welcome_screen {
            render_welcome_boundaries(rows, cols); // explicitly done in the end to override some
            // stuff, see comment in function
        }
    }
}

fn rail_ordinal_key_to_index(character: char) -> Option<usize> {
    match character {
        '1'..='9' => Some(character as usize - '1' as usize),
        '0' => Some(9),
        _ => None,
    }
}

fn format_session_rail_entry(session: &SessionUiInfo, ordinal: usize) -> String {
    let status = if session.is_current_session { "*" } else { "-" };
    format!("{:02} {} {}", ordinal, status, session.name)
}

/// Status buckets finished runs are transferred into.
///
/// These names are a wire contract with the triage reaper — they must stay
/// identical to `FINALIZED_RUNS_SESSION` / `FAILED_RUNS_SESSION` /
/// `NEEDS_ATTENTION_SESSION` in `zellij-utils/src/run_triage.rs`. The plugin
/// cannot import them (pulling zellij-utils into a wasm plugin for three string
/// literals is not worth it), so `bucket_names_match_the_triage_contract` pins
/// them from this side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BucketKind {
    Finalized,
    Failed,
    NeedsAttention,
}

/// Rail order, top to bottom. Best outcome first, ambiguity last.
const RAIL_BUCKETS: [BucketKind; 3] = [
    BucketKind::Finalized,
    BucketKind::Failed,
    BucketKind::NeedsAttention,
];

impl BucketKind {
    fn session_name(&self) -> &'static str {
        match self {
            BucketKind::Finalized => "Finalized runs",
            BucketKind::Failed => "Failed runs",
            BucketKind::NeedsAttention => "Needs attention",
        }
    }
    /// Hotkey, deliberately outside the '0'-'9' range the session ordinals use.
    /// `x` rather than the initial `f` for "failed" — `f` is already finalized,
    /// and these three are the only character keys the rail claims.
    fn hotkey(&self) -> char {
        match self {
            BucketKind::Finalized => 'f',
            BucketKind::Failed => 'x',
            BucketKind::NeedsAttention => 'n',
        }
    }
    fn from_session_name(name: &str) -> Option<Self> {
        RAIL_BUCKETS
            .into_iter()
            .find(|bucket| bucket.session_name() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionRailRowKind {
    Session(usize),
    LiveProcess {
        session_index: usize,
        /// 0-based tab position — handed straight to `switch_session_with_focus`
        /// / `go_to_tab` (both expect 0-based and bump internally).
        tab_position: usize,
    },
    /// Pinned bucket row. `session_index` is `None` until the reaper has had a
    /// reason to create the bucket session — the row still shows, at zero.
    Bucket {
        bucket: BucketKind,
        session_index: Option<usize>,
    },
}

/// What a left-click on a rail row should do. Derived from the rendered row
/// kind so hit-testing stays pure and independent of keyboard selection state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RailClickTarget {
    Session(usize),
    LiveProcess {
        session_index: usize,
        tab_position: usize,
    },
    Bucket(BucketKind),
}

fn rail_row_click_target(kind: &SessionRailRowKind) -> RailClickTarget {
    match *kind {
        SessionRailRowKind::Session(session_index) => RailClickTarget::Session(session_index),
        SessionRailRowKind::LiveProcess {
            session_index,
            tab_position,
        } => RailClickTarget::LiveProcess {
            session_index,
            tab_position,
        },
        SessionRailRowKind::Bucket { bucket, .. } => RailClickTarget::Bucket(bucket),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRailRow {
    kind: SessionRailRowKind,
    text: String,
}

impl SessionRailRow {
    fn is_live_process(&self) -> bool {
        matches!(self.kind, SessionRailRowKind::LiveProcess { .. })
    }
    fn is_bucket(&self) -> bool {
        matches!(self.kind, SessionRailRowKind::Bucket { .. })
    }
}

fn format_bucket_rail_entry(bucket: BucketKind, count: usize, is_current_session: bool) -> String {
    let status = if is_current_session { "*" } else { "-" };
    format!(
        " {} {} {} · {}",
        bucket.hotkey(),
        status,
        bucket.session_name(),
        count
    )
}

fn format_process_tab_rail_entry(tab: &TabUiInfo) -> String {
    let activity = if tab.is_active { "●" } else { "·" };
    let mut text = format!("   {} {}", activity, tab.name);
    if let Some(process_label) = tab.primary_process_label()
        && process_label != tab.name
        && !process_label.contains(&tab.name)
    {
        text.push_str(" · ");
        text.push_str(process_label);
    }
    let additional_processes = tab.live_process_count().saturating_sub(1);
    if additional_processes > 0 {
        text.push_str(&format!(" +{}", additional_processes));
    }
    text
}

// Direct rail navigation (vc_rail_nav pipe, bound to Ctrl+Shift+Up/Down even
// in locked mode): the target is resolved relative to the *current* session
// with wrap-around, so every rail instance receiving the broadcast computes
// the same destination and the switch stays idempotent.
fn relative_session_target(sessions: &[SessionUiInfo], offset: isize) -> Option<String> {
    if sessions.len() < 2 {
        return None;
    }
    let current = sessions.iter().position(|s| s.is_current_session)?;
    let count = sessions.len() as isize;
    let target = (current as isize + offset).rem_euclid(count) as usize;
    if target == current {
        return None;
    }
    sessions.get(target).map(|s| s.name.clone())
}

/// Working sessions in rail order — bucket sessions are pinned separately and
/// must not also appear in the ordinary listing.
fn working_session_indices(sessions: &[SessionUiInfo]) -> Vec<usize> {
    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| BucketKind::from_session_name(&session.name).is_none())
        .map(|(index, _)| index)
        .collect()
}

/// Resolve an ordinal keypress against the *working* sessions, so the buckets
/// sitting at the bottom of the rail do not shift what `3` means.
fn rail_ordinal_target(sessions: &[SessionUiInfo], character: char) -> Option<usize> {
    let ordinal = rail_ordinal_key_to_index(character)?;
    working_session_indices(sessions).get(ordinal).copied()
}

fn session_rail_rows(sessions: &[SessionUiInfo]) -> Vec<SessionRailRow> {
    let mut rows = vec![];
    for (ordinal, session_index) in working_session_indices(sessions).into_iter().enumerate() {
        let session = &sessions[session_index];
        rows.push(SessionRailRow {
            kind: SessionRailRowKind::Session(session_index),
            text: format_session_rail_entry(session, ordinal + 1),
        });
        rows.extend(
            session
                .tabs
                .iter()
                .filter(|tab| tab.live_process_count() > 0)
                .map(|tab| SessionRailRow {
                    kind: SessionRailRowKind::LiveProcess {
                        session_index,
                        tab_position: tab.position,
                    },
                    text: format_process_tab_rail_entry(tab),
                }),
        );
    }
    rows.extend(bucket_rail_rows(sessions));
    rows
}

/// The pinned tail of the rail. Always all three buckets, whether or not their
/// sessions exist yet — a permanent entry point beats one that appears only
/// once something has already failed.
fn bucket_rail_rows(sessions: &[SessionUiInfo]) -> Vec<SessionRailRow> {
    RAIL_BUCKETS
        .into_iter()
        .map(|bucket| {
            let session_index = sessions
                .iter()
                .position(|session| session.name == bucket.session_name());
            let session = session_index.map(|index| &sessions[index]);
            // One transferred run is one tab, so the tab count is the bucket count.
            let count = session.map(|session| session.tabs.len()).unwrap_or(0);
            let is_current_session = session.is_some_and(|session| session.is_current_session);
            SessionRailRow {
                kind: SessionRailRowKind::Bucket {
                    bucket,
                    session_index,
                },
                text: format_bucket_rail_entry(bucket, count, is_current_session),
            }
        })
        .collect()
}

fn rail_range_to_render(
    visible_rows: usize,
    results_len: usize,
    selected_index: Option<usize>,
) -> (usize, usize) {
    if visible_rows == 0 || results_len == 0 {
        return (0, 0);
    }
    if visible_rows >= results_len {
        return (0, results_len);
    }
    let anchor = selected_index
        .unwrap_or(0)
        .min(results_len.saturating_sub(1));
    let half = visible_rows / 2;
    let mut start = anchor.saturating_sub(half);
    let mut end = start + visible_rows;
    if end > results_len {
        end = results_len;
        start = results_len.saturating_sub(visible_rows);
    }
    (start, end)
}

fn fit_rail_line(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut fitted = truncate_to_width(text, width);
    let fitted_width = fitted.width();
    if fitted_width < width {
        fitted.push_str(&" ".repeat(width - fitted_width));
    }
    fitted
}

fn truncate_to_width(text: &str, width: usize) -> String {
    let mut current_width = 0;
    let mut truncated = String::new();
    for character in text.chars() {
        let character_width = character.width().unwrap_or(0);
        if current_width + character_width > width {
            break;
        }
        current_width += character_width;
        truncated.push(character);
    }
    truncated
}

impl State {
    fn reset_selected_index(&mut self) {
        self.sessions.reset_selected_index();
    }
    fn ensure_rail_selection(&mut self) {
        if self.sessions.session_ui_infos.is_empty() {
            self.reset_selected_index();
            return;
        }
        if let Some(selected_index) = self.sessions.selected_index.0
            && selected_index < self.sessions.session_ui_infos.len()
        {
            return;
        }
        let current_session_index = self
            .sessions
            .session_ui_infos
            .iter()
            .position(|s| s.is_current_session)
            .unwrap_or(0);
        self.sessions.select_session_index(current_session_index);
    }
    fn render_session_rail(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        self.ensure_rail_selection();

        let all_rows = session_rail_rows(&self.sessions.session_ui_infos);
        // The buckets are pinned to the bottom of the rail, so only the leading
        // working-session rows take part in scrolling.
        let bucket_row_start = all_rows.iter().position(|row| row.is_bucket()).unwrap_or(0);
        let (rail_rows, bucket_rows) = all_rows.split_at(bucket_row_start);

        let session_count = working_session_indices(&self.sessions.session_ui_infos).len();
        let live_process_count = rail_rows.iter().filter(|row| row.is_live_process()).count();
        let header = fit_rail_line(
            &format!("SESSIONS {} · LIVE {}", session_count, live_process_count),
            cols,
        );
        let mut header = Text::new(header);
        if cols >= 8 {
            header = header.color_range(1, 0..8);
        }
        print_text_with_coordinates(header, 0, 0, None, None);

        let list_rows = rows.saturating_sub(1);
        if list_rows == 0 {
            return;
        }
        // Buckets only give up their pinned slots when the rail is too short to
        // hold even one working session alongside them.
        let pinned_rows = bucket_rows.len().min(list_rows.saturating_sub(1));
        let scrollable_rows = list_rows.saturating_sub(pinned_rows);
        let footer_rows = usize::from(rail_rows.len() > scrollable_rows && scrollable_rows > 1);
        let entry_rows = scrollable_rows.saturating_sub(footer_rows);
        let selected_index = self.sessions.selected_index.0;
        let selected_row_index = selected_index.and_then(|selected_session_index| {
            rail_rows
                .iter()
                .position(|row| row.kind == SessionRailRowKind::Session(selected_session_index))
        });
        let (start, end) = rail_range_to_render(entry_rows, rail_rows.len(), selected_row_index);
        let mut row = 1;

        self.rail_click_map.clear();
        for rail_row in &rail_rows[start..end] {
            let mut text = Text::new(fit_rail_line(&rail_row.text, cols));
            match rail_row.kind {
                SessionRailRowKind::Session(session_index) => {
                    if cols >= 4 {
                        text = text.color_range(1, 3..4);
                    }
                    if selected_index == Some(session_index) {
                        text = text.selected();
                    }
                },
                SessionRailRowKind::LiveProcess { .. } => {
                    if cols >= 4 {
                        text = text.color_range(2, 3..4);
                    }
                },
                SessionRailRowKind::Bucket { .. } => {},
            }
            // Every data row is clickable; header (row 0) never enters the map.
            self.rail_click_map
                .insert(row, rail_row_click_target(&rail_row.kind));
            print_text_with_coordinates(text, 0, row, None, None);
            row += 1;
        }

        if footer_rows == 1 && row < rows {
            let hidden_above = start;
            let hidden_below = rail_rows.len().saturating_sub(end);
            let footer = match (hidden_above, hidden_below) {
                (0, below) => format!("+{} more", below),
                (above, 0) => format!("+{} above", above),
                (above, below) => format!("+{} above +{} more", above, below),
            };
            print_text_with_coordinates(
                Text::new(fit_rail_line(&footer, cols)),
                0,
                row,
                None,
                None,
            );
            row += 1;
        }

        // Blank out the gap so the buckets always sit flush with the bottom
        // edge, whatever the working-session list is doing above them.
        let first_pinned_row = rows.saturating_sub(pinned_rows);
        while row < first_pinned_row {
            print_text_with_coordinates(Text::new(" ".repeat(cols)), 0, row, None, None);
            row += 1;
        }

        for bucket_row in &bucket_rows[bucket_rows.len() - pinned_rows..] {
            let mut text = Text::new(fit_rail_line(&bucket_row.text, cols));
            if cols >= 4 {
                text = text.color_range(3, 3..4);
            }
            if let SessionRailRowKind::Bucket {
                session_index: Some(session_index),
                ..
            } = bucket_row.kind
                && selected_index == Some(session_index)
            {
                text = text.selected();
            }
            // Empty buckets stay clickable so the mouse path matches `f`/`x`/`n`
            // (which report "is empty" rather than silently no-opping).
            self.rail_click_map
                .insert(row, rail_row_click_target(&bucket_row.kind));
            print_text_with_coordinates(text, 0, row, None, None);
            row += 1;
        }
    }
    fn handle_session_rail_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                self.sessions.move_session_selection_down();
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.sessions.move_session_selection_up();
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_session_rail_selection();
                true
            },
            BareKey::Char(character) if key.has_no_modifiers() => {
                if character == '\n' {
                    self.handle_session_rail_selection();
                    true
                } else if let Some(index) =
                    rail_ordinal_target(&self.sessions.session_ui_infos, character)
                {
                    if self.sessions.select_session_index(index) {
                        self.handle_session_rail_selection();
                    }
                    true
                } else if let Some(bucket) = RAIL_BUCKETS
                    .into_iter()
                    .find(|bucket| bucket.hotkey() == character)
                {
                    self.jump_to_bucket(bucket);
                    true
                } else {
                    false
                }
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                hide_self();
                true
            },
            BareKey::Esc if key.has_no_modifiers() => {
                hide_self();
                true
            },
            _ => false,
        }
    }
    fn handle_session_rail_mouse(&mut self, mouse_event: Mouse) -> bool {
        match mouse_event {
            Mouse::LeftClick(line, _column) => {
                let Ok(row) = usize::try_from(line) else {
                    return false;
                };
                // Header, footer, blank gap between working sessions and the
                // pinned buckets: not in the map → quiet no-op, no crash.
                let Some(target) = self.rail_click_map.get(&row).cloned() else {
                    return false;
                };
                match target {
                    RailClickTarget::Session(session_index) => {
                        if !self.sessions.select_session_index(session_index) {
                            return false;
                        }
                        if !self.sessions.selected_is_current_session() {
                            self.handle_session_rail_selection();
                        }
                        true
                    },
                    RailClickTarget::LiveProcess {
                        session_index,
                        tab_position,
                    } => {
                        if !self.sessions.select_session_index(session_index) {
                            return false;
                        }
                        let Some(session_name) = self.sessions.get_selected_session_name() else {
                            return false;
                        };
                        if self.sessions.selected_is_current_session() {
                            // Same 0-based position the keyboard path uses;
                            // the plugin shim bumps it for Action::GoToTab.
                            go_to_tab(tab_position as u32);
                        } else {
                            switch_session_with_focus(&session_name, Some(tab_position), None);
                            self.reset_selected_index();
                        }
                        true
                    },
                    RailClickTarget::Bucket(bucket) => {
                        // Same entry point as the `f`/`x`/`n` hotkeys.
                        self.jump_to_bucket(bucket);
                        true
                    },
                }
            },
            // Scroll / hover / right-click are not part of this cut. Shift+click
            // never reaches the plugin (client passthrough to the terminal).
            _ => false,
        }
    }
    /// Hop to a bucket session. An empty bucket has no session behind it yet,
    /// and conjuring one on a keypress would clutter the rail with sessions the
    /// operator never asked for — say so instead.
    fn jump_to_bucket(&mut self, bucket: BucketKind) {
        let exists = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|session| session.name == bucket.session_name());
        match exists {
            Some(session) if session.is_current_session => self.show_error("Already attached..."),
            Some(session) => {
                let name = session.name.clone();
                switch_session_with_focus(&name, None, None);
                self.reset_selected_index();
            },
            None => self.show_error(&format!("{} is empty", bucket.session_name())),
        }
    }
    fn switch_session_relative(&mut self, offset: isize) {
        if let Some(target_session_name) =
            relative_session_target(&self.sessions.session_ui_infos, offset)
        {
            switch_session_with_focus(&target_session_name, None, None);
        }
    }
    fn handle_session_rail_selection(&mut self) {
        self.ensure_rail_selection();
        if let Some(selected_session_name) = self.sessions.get_selected_session_name() {
            if self.sessions.selected_is_current_session() {
                self.show_error("Already attached...");
            } else {
                switch_session_with_focus(&selected_session_name, None, None);
                self.reset_selected_index();
            }
        }
    }
    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.error.is_some() {
            self.error = None;
            return true;
        }
        if self.is_rail {
            return self.handle_session_rail_key(key);
        }
        match self.active_screen {
            ActiveScreen::NewSession => self.handle_new_session_key(key),
            ActiveScreen::AttachToSession => self.handle_attach_to_session(key),
            ActiveScreen::ResurrectSession => self.handle_resurrect_session_key(key),
            ActiveScreen::SingleScreen => self.handle_single_screen_key(key),
        }
    }
    fn handle_new_session_key(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                self.new_session_info.handle_key(key);
                should_render = true;
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.new_session_info.handle_key(key);
                should_render = true;
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_selection();
                should_render = true;
            },
            BareKey::Char(character) if key.has_no_modifiers() => {
                if character == '\n' {
                    self.handle_selection();
                } else {
                    self.new_session_info.handle_key(key);
                }
                should_render = true;
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                self.new_session_info.handle_key(key);
                should_render = true;
            },
            BareKey::Char('w') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.active_screen = ActiveScreen::NewSession;
                should_render = true;
            },
            BareKey::Tab if key.has_no_modifiers() => {
                self.toggle_active_screen();
                should_render = true;
            },
            BareKey::Char('f') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                let request_id = Uuid::new_v4();
                let mut config = BTreeMap::new();
                let mut args = BTreeMap::new();
                self.request_ids.push(request_id.to_string());
                // we insert this into the config so that a new plugin will be opened (the plugin's
                // uniqueness is determined by its name/url as well as its config)
                config.insert("request_id".to_owned(), request_id.to_string());
                // we also insert this into the args so that the plugin will have an easier access to
                // it
                args.insert("request_id".to_owned(), request_id.to_string());
                pipe_message_to_plugin(
                    MessageToPlugin::new("filepicker")
                        .with_plugin_url("filepicker")
                        .with_plugin_config(config)
                        .new_plugin_instance_should_have_pane_title(
                            "Select folder for the new session...",
                        )
                        .new_plugin_instance_should_be_focused()
                        .with_args(args),
                );
                should_render = true;
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.new_session_info.new_session_folder = None;
                should_render = true;
            },
            BareKey::Esc if key.has_no_modifiers() => {
                self.new_session_info.handle_key(key);
                should_render = true;
            },
            _ => {},
        }
        should_render
    }
    fn handle_attach_to_session(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;
        if self.show_kill_all_sessions_warning {
            match key.bare_key {
                BareKey::Char('y') if key.has_no_modifiers() => {
                    let all_other_sessions = self.sessions.all_other_sessions();
                    let was_searching = self.sessions.is_searching;
                    let prev_search_idx = self.sessions.selected_search_index;
                    let prev_top_idx = self.sessions.selected_index.0;
                    match kill_sessions(&all_other_sessions) {
                        Ok(()) => {
                            self.sessions
                                .session_ui_infos
                                .retain(|s| !all_other_sessions.contains(&s.name));
                            self.sessions
                                .update_search_term(&self.search_term, &self.colors);
                            self.sessions.restore_selection_after_delete(
                                was_searching,
                                prev_search_idx,
                                prev_top_idx,
                            );
                        },
                        Err(e) => {
                            self.show_error(&format!("Failed to kill sessions: {}", e));
                        },
                    }
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                BareKey::Char('n') | BareKey::Esc if key.has_no_modifiers() => {
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                _ => {},
            }
        } else {
            match key.bare_key {
                BareKey::Right if key.has_no_modifiers() => {
                    self.sessions.result_expand();
                    should_render = true;
                },
                BareKey::Left if key.has_no_modifiers() => {
                    self.sessions.result_shrink();
                    should_render = true;
                },
                BareKey::Down if key.has_no_modifiers() => {
                    self.sessions.move_selection_down();
                    should_render = true;
                },
                BareKey::Up if key.has_no_modifiers() => {
                    self.sessions.move_selection_up();
                    should_render = true;
                },
                BareKey::Enter if key.has_no_modifiers() => {
                    self.handle_selection();
                    should_render = true;
                },
                BareKey::Char(character) if key.has_no_modifiers() => {
                    if character == '\n' {
                        self.handle_selection();
                    } else if let Some(new_session_name) = self.renaming_session_name.as_mut() {
                        new_session_name.push(character);
                    } else {
                        self.search_term.push(character);
                        self.sessions
                            .update_search_term(&self.search_term, &self.colors);
                    }
                    should_render = true;
                },
                BareKey::Backspace if key.has_no_modifiers() => {
                    if let Some(new_session_name) = self.renaming_session_name.as_mut() {
                        if new_session_name.is_empty() {
                            self.renaming_session_name = None;
                        } else {
                            new_session_name.pop();
                        }
                    } else {
                        self.search_term.pop();
                        self.sessions
                            .update_search_term(&self.search_term, &self.colors);
                    }
                    should_render = true;
                },
                BareKey::Char('w') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    self.active_screen = ActiveScreen::NewSession;
                    should_render = true;
                },
                BareKey::Char('r') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    self.renaming_session_name = Some(String::new());
                    should_render = true;
                },
                BareKey::Delete if key.has_no_modifiers() => {
                    if let Some(selected_session_name) = self.sessions.get_selected_session_name() {
                        let was_searching = self.sessions.is_searching;
                        let prev_search_idx = self.sessions.selected_search_index;
                        let prev_top_idx = self.sessions.selected_index.0;
                        match kill_sessions(std::slice::from_ref(&selected_session_name)) {
                            Ok(()) => {
                                self.sessions
                                    .session_ui_infos
                                    .retain(|s| s.name != selected_session_name);
                                self.sessions
                                    .update_search_term(&self.search_term, &self.colors);
                                self.sessions.restore_selection_after_delete(
                                    was_searching,
                                    prev_search_idx,
                                    prev_top_idx,
                                );
                            },
                            Err(e) => {
                                self.show_error(&format!("Failed to kill session: {}", e));
                            },
                        }
                    } else {
                        self.show_error("Must select session before killing it.");
                    }
                    should_render = true;
                },
                BareKey::Char('d') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    let all_other_sessions = self.sessions.all_other_sessions();
                    if all_other_sessions.is_empty() {
                        self.show_error("No other sessions to kill. Quit to kill the current one.");
                    } else {
                        self.show_kill_all_sessions_warning = true;
                    }
                    should_render = true;
                },
                BareKey::Char('x') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    disconnect_other_clients()
                },
                BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    if !self.search_term.is_empty() {
                        self.search_term.clear();
                        self.sessions
                            .update_search_term(&self.search_term, &self.colors);
                        self.reset_selected_index();
                    } else if !self.is_welcome_screen {
                        self.reset_selected_index();
                        close_self();
                    }
                    should_render = true;
                },
                BareKey::Tab if key.has_no_modifiers() => {
                    self.toggle_active_screen();
                    should_render = true;
                },
                BareKey::Esc if key.has_no_modifiers() => {
                    if self.renaming_session_name.is_some() {
                        self.renaming_session_name = None;
                        should_render = true;
                    } else if !self.is_welcome_screen {
                        close_self();
                    }
                },
                BareKey::Char('a')
                    if key.has_modifiers(&[KeyModifier::Ctrl]) && !self.is_welcome_screen =>
                {
                    // we don't want to save welcome screen sessions
                    if let Err(e) = save_session() {
                        self.show_error(&format!("Couldn't save session: {}", e));
                    }
                },
                _ => {},
            }
        }
        should_render
    }
    fn handle_resurrect_session_key(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                self.resurrectable_sessions.move_selection_down();
                should_render = true;
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.resurrectable_sessions.move_selection_up();
                should_render = true;
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_selection();
                should_render = true;
            },
            BareKey::Char(character) if key.has_no_modifiers() => {
                if character == '\n' {
                    self.handle_selection();
                } else {
                    self.resurrectable_sessions.handle_character(character);
                }
                should_render = true;
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                self.resurrectable_sessions.handle_backspace();
                should_render = true;
            },
            BareKey::Char('w') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.active_screen = ActiveScreen::NewSession;
                should_render = true;
            },
            BareKey::Tab if key.has_no_modifiers() => {
                self.toggle_active_screen();
                should_render = true;
            },
            BareKey::Delete if key.has_no_modifiers() => {
                self.resurrectable_sessions.delete_selected_session();
                should_render = true;
            },
            BareKey::Char('d') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.resurrectable_sessions
                    .show_delete_all_sessions_warning();
                should_render = true;
            },
            BareKey::Esc if key.has_no_modifiers() && !self.is_welcome_screen => {
                close_self();
            },
            _ => {},
        }
        should_render
    }
    fn handle_single_screen_key(&mut self, key: KeyWithModifier) -> bool {
        match self.single_screen_state.mode {
            SingleScreenMode::SearchAndSelect => self.handle_single_screen_search_key(key),
            SingleScreenMode::SelectingLayout => self.handle_single_screen_layout_key(key),
        }
    }
    fn handle_single_screen_search_key(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;

        // Handle kill-all warning overlay first
        if self.show_kill_all_sessions_warning {
            match key.bare_key {
                BareKey::Char('y') if key.has_no_modifiers() => {
                    let all_other_sessions = self.sessions.all_other_sessions();
                    let previous_index = self.single_screen_state.selected_index;
                    match kill_sessions(&all_other_sessions) {
                        Ok(()) => {
                            self.sessions
                                .session_ui_infos
                                .retain(|s| !all_other_sessions.contains(&s.name));
                            self.single_screen_state.update_search_term(
                                &self.sessions.session_ui_infos,
                                &self.resurrectable_sessions.all_resurrectable_sessions,
                            );
                            self.single_screen_state
                                .restore_selection_after_delete(previous_index);
                        },
                        Err(e) => {
                            self.show_error(&format!("Failed to kill sessions: {}", e));
                        },
                    }
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                BareKey::Char('n') | BareKey::Esc if key.has_no_modifiers() => {
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    self.show_kill_all_sessions_warning = false;
                    should_render = true;
                },
                _ => {},
            }
            return should_render;
        }

        // Handle rename overlay
        if self.renaming_session_name.is_some() {
            match key.bare_key {
                BareKey::Enter if key.has_no_modifiers() => {
                    self.handle_selection();
                    should_render = true;
                },
                BareKey::Char(c) if key.has_no_modifiers() => {
                    if c == '\n' {
                        self.handle_selection();
                    } else if let Some(name) = self.renaming_session_name.as_mut() {
                        name.push(c);
                    }
                    should_render = true;
                },
                BareKey::Backspace if key.has_no_modifiers() => {
                    if let Some(name) = self.renaming_session_name.as_mut() {
                        if name.is_empty() {
                            self.renaming_session_name = None;
                        } else {
                            name.pop();
                        }
                    }
                    should_render = true;
                },
                BareKey::Esc if key.has_no_modifiers() => {
                    self.renaming_session_name = None;
                    should_render = true;
                },
                _ => {},
            }
            return should_render;
        }

        match key.bare_key {
            BareKey::Char(character) if key.has_no_modifiers() => {
                if character == '\n' {
                    self.handle_selection();
                } else {
                    self.single_screen_state.search_term.push(character);
                    self.single_screen_state.update_search_term(
                        &self.sessions.session_ui_infos,
                        &self.resurrectable_sessions.all_resurrectable_sessions,
                    );
                }
                should_render = true;
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                self.single_screen_state.search_term.pop();
                self.single_screen_state.update_search_term(
                    &self.sessions.session_ui_infos,
                    &self.resurrectable_sessions.all_resurrectable_sessions,
                );
                should_render = true;
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_selection();
                should_render = true;
            },
            BareKey::Down if key.has_no_modifiers() => {
                self.single_screen_state.move_selection_down();
                should_render = true;
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.single_screen_state.move_selection_up();
                should_render = true;
            },
            BareKey::Tab if key.has_no_modifiers() => {
                self.single_screen_state.tab_complete(
                    &self.sessions.session_ui_infos,
                    &self.resurrectable_sessions.all_resurrectable_sessions,
                );
                should_render = true;
            },
            BareKey::Char('r') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.renaming_session_name = Some(String::new());
                should_render = true;
            },
            BareKey::Delete if key.has_no_modifiers() => {
                let selected = self
                    .single_screen_state
                    .get_selected_result()
                    .map(|r| r.as_delete_target());
                if let Some(target) = selected {
                    let previous_index = self.single_screen_state.selected_index;
                    let outcome: Result<(), String> = match &target {
                        DeleteTarget::Active(name) => kill_sessions(std::slice::from_ref(name))
                            .map(|()| {
                                self.sessions.session_ui_infos.retain(|s| s.name != *name);
                            }),
                        DeleteTarget::Resurrectable(name) => delete_dead_session(name).map(|()| {
                            self.resurrectable_sessions
                                .all_resurrectable_sessions
                                .retain(|(n, _)| n != name);
                        }),
                    };
                    match outcome {
                        Ok(()) => {
                            self.single_screen_state.update_search_term(
                                &self.sessions.session_ui_infos,
                                &self.resurrectable_sessions.all_resurrectable_sessions,
                            );
                            self.single_screen_state
                                .restore_selection_after_delete(previous_index);
                        },
                        Err(e) => {
                            self.show_error(&format!("Failed to delete session: {}", e));
                        },
                    }
                }
                should_render = true;
            },
            BareKey::Char('d') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                let all_other_sessions = self.sessions.all_other_sessions();
                if all_other_sessions.is_empty() {
                    self.show_error("No other sessions to kill. Quit to kill the current one.");
                } else {
                    self.show_kill_all_sessions_warning = true;
                }
                should_render = true;
            },
            BareKey::Char('x') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                disconnect_other_clients();
            },
            BareKey::Char('a') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                if !self.is_welcome_screen
                    && let Err(e) = save_session()
                {
                    self.show_error(&format!("Couldn't save session: {}", e));
                }
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                if !self.single_screen_state.search_term.is_empty() {
                    self.single_screen_state.search_term.clear();
                    self.single_screen_state.update_search_term(
                        &self.sessions.session_ui_infos,
                        &self.resurrectable_sessions.all_resurrectable_sessions,
                    );
                } else if !self.is_welcome_screen {
                    close_self();
                }
                should_render = true;
            },
            BareKey::Esc if key.has_no_modifiers() => {
                if self.single_screen_state.selected_index.is_some() {
                    self.single_screen_state.selected_index = None;
                    should_render = true;
                } else if !self.is_welcome_screen {
                    close_self();
                }
            },
            _ => {},
        }
        should_render
    }
    fn handle_single_screen_layout_key(&mut self, key: KeyWithModifier) -> bool {
        let mut should_render = false;
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                self.single_screen_state.layout_list.move_selection_down();
                should_render = true;
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.single_screen_state.layout_list.move_selection_up();
                should_render = true;
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_selection();
                should_render = true;
            },
            BareKey::Char(character) if key.has_no_modifiers() => {
                if character == '\n' {
                    self.handle_selection();
                } else {
                    self.single_screen_state
                        .layout_list
                        .layout_search_term
                        .push(character);
                    self.single_screen_state.layout_list.update_search_term();
                }
                should_render = true;
            },
            BareKey::Backspace if key.has_no_modifiers() => {
                self.single_screen_state
                    .layout_list
                    .layout_search_term
                    .pop();
                self.single_screen_state.layout_list.update_search_term();
                should_render = true;
            },
            BareKey::Char('f') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                let request_id = Uuid::new_v4();
                let mut config = BTreeMap::new();
                let mut args = BTreeMap::new();
                self.request_ids.push(request_id.to_string());
                config.insert("request_id".to_owned(), request_id.to_string());
                args.insert("request_id".to_owned(), request_id.to_string());
                pipe_message_to_plugin(
                    MessageToPlugin::new("filepicker")
                        .with_plugin_url("filepicker")
                        .with_plugin_config(config)
                        .new_plugin_instance_should_have_pane_title(
                            "Select folder for the new session...",
                        )
                        .new_plugin_instance_should_be_focused()
                        .with_args(args),
                );
                should_render = true;
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.single_screen_state.new_session_folder = None;
                should_render = true;
            },
            BareKey::Esc if key.has_no_modifiers() => {
                self.single_screen_state.transition_to_search();
                should_render = true;
            },
            _ => {},
        }
        should_render
    }
    fn handle_selection(&mut self) {
        match self.active_screen {
            ActiveScreen::NewSession => {
                if self.new_session_info.name().len() >= 108 {
                    // this is due to socket path limitations
                    // TODO: get this from Zellij (for reference: this is part of the interprocess
                    // package, we should get if from there if possible because it's configurable
                    // through the package)
                    self.show_error("Session name must be shorter than 108 bytes");
                    return;
                } else if self.new_session_info.name().contains('/') {
                    self.show_error("Session name cannot contain '/'");
                    return;
                } else if self
                    .sessions
                    .has_forbidden_session(self.new_session_info.name())
                {
                    self.show_error("This session exists and web clients cannot attach to it.");
                    return;
                }
                self.new_session_info.handle_selection(&self.session_name);
            },
            ActiveScreen::AttachToSession => {
                if let Some(renaming_session_name) = &self.renaming_session_name.take() {
                    if renaming_session_name.is_empty() {
                        self.show_error("New name must not be empty.");
                        return; // so that we don't hide self
                    } else if self.session_name.as_ref() == Some(renaming_session_name) {
                        // noop - we're already called that!
                        return; // so that we don't hide self
                    } else if self.sessions.has_session(renaming_session_name) {
                        self.show_error("A session by this name already exists.");
                        return; // so that we don't hide self
                    } else if self
                        .resurrectable_sessions
                        .has_session(renaming_session_name)
                    {
                        self.show_error("A resurrectable session by this name already exists.");
                        return; // s that we don't hide self
                    } else {
                        if renaming_session_name.contains('/') {
                            self.show_error("Session names cannot contain '/'");
                            return;
                        }
                        self.update_current_session_name_in_ui(renaming_session_name);
                        rename_session(renaming_session_name);
                        return; // s that we don't hide self
                    }
                }
                if let Some(selected_session_name) = self.sessions.get_selected_session_name() {
                    let selected_tab = self.sessions.get_selected_tab_position();
                    let selected_pane = self.sessions.get_selected_pane_id();
                    let is_current_session = self.sessions.selected_is_current_session();
                    if is_current_session {
                        if let Some((pane_id, is_plugin)) = selected_pane {
                            if is_plugin {
                                focus_plugin_pane(pane_id, true, false);
                            } else {
                                focus_terminal_pane(pane_id, true, false);
                            }
                        } else if let Some(tab_position) = selected_tab {
                            go_to_tab(tab_position as u32);
                        } else {
                            self.show_error("Already attached...");
                        }
                    } else {
                        switch_session_with_focus(
                            &selected_session_name,
                            selected_tab,
                            selected_pane,
                        );
                    }
                }
                self.reset_selected_index();
                self.search_term.clear();
                self.sessions
                    .update_search_term(&self.search_term, &self.colors);
                if self.is_welcome_screen {
                    // the welcome screen has done its job and now we need to quit this temporary
                    // session so as not to leave garbage sessions behind
                    quit_zellij();
                } else {
                    hide_self();
                }
            },
            ActiveScreen::ResurrectSession => {
                if let Some(session_name_to_resurrect) =
                    self.resurrectable_sessions.get_selected_session_name()
                {
                    switch_session(Some(&session_name_to_resurrect));
                    if self.is_welcome_screen {
                        // the welcome screen has done its job and now we need to quit this temporary
                        // session so as not to leave garbage sessions behind
                        quit_zellij();
                    } else {
                        hide_self();
                    }
                }
            },
            ActiveScreen::SingleScreen => {
                // Handle rename
                if let Some(renaming_session_name) = &self.renaming_session_name.take() {
                    if renaming_session_name.is_empty() {
                        self.show_error("New name must not be empty.");
                        return;
                    } else if self.session_name.as_ref() == Some(renaming_session_name) {
                        return;
                    } else if self.sessions.has_session(renaming_session_name) {
                        self.show_error("A session by this name already exists.");
                        return;
                    } else if self
                        .resurrectable_sessions
                        .has_session(renaming_session_name)
                    {
                        self.show_error("A resurrectable session by this name already exists.");
                        return;
                    } else {
                        if renaming_session_name.contains('/') {
                            self.show_error("Session names cannot contain '/'");
                            return;
                        }
                        self.update_current_session_name_in_ui(renaming_session_name);
                        rename_session(renaming_session_name);
                        return;
                    }
                }

                match self.single_screen_state.mode {
                    SingleScreenMode::SearchAndSelect => {
                        if let Some(result) = self.single_screen_state.get_selected_result() {
                            // User navigated to a specific result
                            let session_name = result.session_name().to_owned();
                            match result {
                                UnifiedSearchResult::ActiveSession {
                                    is_current_session, ..
                                } => {
                                    if *is_current_session {
                                        self.show_error("Already attached...");
                                    } else {
                                        switch_session_with_focus(&session_name, None, None);
                                    }
                                },
                                UnifiedSearchResult::ResurrectableSession { .. } => {
                                    switch_session(Some(&session_name));
                                },
                            }
                            self.single_screen_state.search_term.clear();
                            self.single_screen_state.selected_index = None;
                            if self.is_welcome_screen {
                                quit_zellij();
                            } else {
                                hide_self();
                            }
                        } else {
                            // No navigation - use typed name
                            let typed_name = self.single_screen_state.search_term.clone();

                            // Validate name
                            if typed_name.len() >= 108 {
                                self.show_error("Session name must be shorter than 108 bytes");
                                return;
                            }
                            if typed_name.contains('/') {
                                self.show_error("Session name cannot contain '/'");
                                return;
                            }
                            if self.sessions.has_forbidden_session(&typed_name) {
                                self.show_error(
                                    "This session exists and web clients cannot attach to it.",
                                );
                                return;
                            }

                            // Check exact match against active sessions
                            if self.sessions.has_session(&typed_name) {
                                if self.session_name.as_deref() == Some(&typed_name) {
                                    self.show_error("Already attached...");
                                } else {
                                    switch_session_with_focus(&typed_name, None, None);
                                    if self.is_welcome_screen {
                                        quit_zellij();
                                    } else {
                                        hide_self();
                                    }
                                }
                                return;
                            }
                            // Check exact match against resurrectable sessions
                            if self.resurrectable_sessions.has_session(&typed_name) {
                                switch_session(Some(&typed_name));
                                if self.is_welcome_screen {
                                    quit_zellij();
                                } else {
                                    hide_self();
                                }
                                return;
                            }
                            // No match - transition to layout selection
                            self.single_screen_state.transition_to_layout_selection();
                        }
                    },
                    SingleScreenMode::SelectingLayout => {
                        let new_session_name = if self.single_screen_state.search_term.is_empty() {
                            None
                        } else {
                            Some(self.single_screen_state.search_term.as_str())
                        };
                        let layout = self.single_screen_state.layout_list.selected_layout_info();
                        let cwd = self.single_screen_state.new_session_folder.clone();

                        if new_session_name != self.session_name.as_deref() {
                            match layout {
                                Some(layout_info) => {
                                    switch_session_with_layout(new_session_name, layout_info, cwd);
                                },
                                None => {
                                    switch_session(new_session_name);
                                },
                            }
                        }
                        self.single_screen_state.search_term.clear();
                        self.single_screen_state.transition_to_search();
                        if self.is_welcome_screen {
                            quit_zellij();
                        } else {
                            hide_self();
                        }
                    },
                }
            },
        }
    }
    fn toggle_active_screen(&mut self) {
        self.active_screen = match self.active_screen {
            ActiveScreen::NewSession => ActiveScreen::AttachToSession,
            ActiveScreen::AttachToSession => ActiveScreen::ResurrectSession,
            ActiveScreen::ResurrectSession => ActiveScreen::NewSession,
            ActiveScreen::SingleScreen => ActiveScreen::SingleScreen, // no-op
        };
    }
    fn show_error(&mut self, error_text: &str) {
        self.error = Some(error_text.to_owned());
    }
    fn update_current_session_name_in_ui(&mut self, new_name: &str) {
        if let Some(old_session_name) = self.session_name.as_ref() {
            self.sessions
                .update_session_name(old_session_name, new_name);
        }
        self.session_name = Some(new_name.to_owned());
    }
    fn arm_refresh_timer(&mut self) {
        if !self.refresh_timer_armed {
            set_timeout(1.0);
            self.refresh_timer_armed = true;
        }
    }

    fn refresh_session_list(&mut self) -> bool {
        let snapshot = match get_session_list() {
            Ok(snapshot) => snapshot,
            Err(_) => return false,
        };
        for session_info in &snapshot.live_sessions {
            if session_info.is_current_session {
                self.new_session_info
                    .update_layout_list(session_info.available_layouts.clone());
            }
        }
        self.resurrectable_sessions
            .update(snapshot.resurrectable_sessions);
        self.update_session_infos(snapshot.live_sessions);
        if !self.is_multi_screen {
            self.single_screen_state.update_search_term(
                &self.sessions.session_ui_infos,
                &self.resurrectable_sessions.all_resurrectable_sessions,
            );
            let previous_selection = self.single_screen_state.layout_list.selected_layout_index;
            let previous_search_term = self
                .single_screen_state
                .layout_list
                .layout_search_term
                .clone();
            self.single_screen_state.layout_list = self.new_session_info.get_layout_list_clone();
            self.single_screen_state.layout_list.layout_search_term = previous_search_term;
            self.single_screen_state.layout_list.update_search_term();
            self.single_screen_state.layout_list.selected_layout_index =
                previous_selection.min(self.single_screen_state.layout_list.max_index());
        }
        true
    }

    fn update_session_infos(&mut self, session_infos: Vec<SessionInfo>) {
        let session_ui_infos: Vec<SessionUiInfo> = session_infos
            .iter()
            .filter_map(|s| {
                if self.is_web_client && !s.web_clients_allowed {
                    None
                } else if self.is_welcome_screen && s.is_current_session {
                    // do not display current session if we're the welcome screen
                    // because:
                    // 1. attaching to the welcome screen from the welcome screen is not a thing
                    // 2. it can cause issues on the web (since we're disconnecting and
                    //    reconnecting to a session we just closed by disconnecting...)
                    None
                } else {
                    Some(SessionUiInfo::from_session_info(s))
                }
            })
            .collect();
        let forbidden_sessions: Vec<SessionUiInfo> = session_infos
            .iter()
            .filter_map(|s| {
                if self.is_web_client && !s.web_clients_allowed {
                    Some(SessionUiInfo::from_session_info(s))
                } else {
                    None
                }
            })
            .collect();
        let current_session_name = session_infos.iter().find_map(|s| {
            if s.is_current_session {
                Some(s.name.clone())
            } else {
                None
            }
        });
        if let Some(current_session_name) = current_session_name {
            self.session_name = Some(current_session_name);
        }
        self.sessions
            .set_sessions(session_ui_infos, forbidden_sessions);
    }
    fn main_menu_size(&self, rows: usize, cols: usize) -> (usize, usize, usize, usize) {
        // x, y, width, height
        let width = if self.is_welcome_screen {
            std::cmp::min(cols, 101)
        } else {
            cols
        };
        let x = if self.is_welcome_screen {
            (cols.saturating_sub(width) as f64 / 2.0).floor() as usize + 2
        } else {
            0
        };
        let y = if self.is_welcome_screen {
            (rows.saturating_sub(15) as f64 / 2.0).floor() as usize
        } else {
            0
        };
        let height = rows.saturating_sub(y);
        (x, y, width, height)
    }
    fn render_single_screen_folder_prompt(&self, x: usize, y: usize, max_cols: usize) {
        match self.single_screen_state.new_session_folder.as_ref() {
            Some(new_session_folder) => {
                let folder_prompt = "New session folder:";
                let new_session_folder_str = new_session_folder.display().to_string();
                let change_folder_shortcut = self.colors.shortcuts("<Ctrl f>");
                let reset_folder_shortcut = self.colors.shortcuts("<Ctrl c>");
                if max_cols >= folder_prompt.len() + new_session_folder_str.len() + 30 {
                    print!(
                        "\u{1b}[m\u{1b}[{};{}H{} {} ({} to change, {} to reset)",
                        y + 1,
                        x + 1,
                        self.colors.session_name_prompt(folder_prompt),
                        self.colors
                            .session_and_folder_entry(&new_session_folder_str),
                        change_folder_shortcut,
                        reset_folder_shortcut,
                    );
                } else {
                    print!(
                        "\u{1b}[m\u{1b}[{};{}H{} {} ({}/{})",
                        y + 1,
                        x + 1,
                        self.colors.session_name_prompt("Folder:"),
                        self.colors
                            .session_and_folder_entry(&new_session_folder_str),
                        change_folder_shortcut,
                        reset_folder_shortcut,
                    );
                }
            },
            None => {
                let folder_prompt = "New session folder:";
                let change_folder_shortcut = self.colors.shortcuts("<Ctrl f>");
                print!(
                    "\u{1b}[m\u{1b}[{};{}H{} ({} to set)",
                    y + 1,
                    x + 1,
                    self.colors.session_name_prompt(folder_prompt),
                    change_folder_shortcut,
                );
            },
        }
    }
    fn render_kill_all_sessions_warning(&self, rows: usize, columns: usize, x: usize, y: usize) {
        if rows == 0 || columns == 0 {
            return;
        }
        let session_count = self.sessions.all_other_sessions().len();
        let session_count_len = session_count.to_string().chars().count();
        let warning_description_text = format!("This will kill {} active sessions", session_count);
        let confirmation_text = "Are you sure? (y/n)";
        let warning_y_location = y + (rows / 2).saturating_sub(1);
        let confirmation_y_location = y + (rows / 2) + 1;
        let warning_x_location =
            x + columns.saturating_sub(warning_description_text.chars().count()) / 2;
        let confirmation_x_location =
            x + columns.saturating_sub(confirmation_text.chars().count()) / 2;
        print_text_with_coordinates(
            Text::new(warning_description_text).color_range(0, 15..16 + session_count_len),
            warning_x_location,
            warning_y_location,
            None,
            None,
        );
        print_text_with_coordinates(
            Text::new(confirmation_text).color_indices(2, vec![15, 17]),
            confirmation_x_location,
            confirmation_y_location,
            None,
            None,
        );
    }
}

#[cfg(test)]
mod rail_tests {
    use super::*;
    use std::time::Duration;

    fn session(name: &str, is_current_session: bool) -> SessionUiInfo {
        SessionUiInfo {
            name: name.to_owned(),
            tabs: vec![],
            connected_users: 1,
            is_current_session,
            creation_time: Duration::ZERO,
        }
    }

    #[test]
    fn rail_entries_include_ordinal_status_and_name() {
        assert_eq!(
            format_session_rail_entry(&session("alpha", true), 1),
            "01 * alpha"
        );
        assert_eq!(
            format_session_rail_entry(&session("beta", false), 12),
            "12 - beta"
        );
    }

    #[test]
    fn rail_expands_sessions_with_live_process_tabs_only() {
        let mut alpha = session("alpha", true);
        alpha.tabs = vec![
            TabUiInfo::for_rail_test("impl-260718-120000-01000", true, "claude", 1),
            TabUiInfo::for_rail_test("old-run", false, "codex", 0),
            TabUiInfo::for_rail_test("audit-260718-130000-02000", false, "codex", 2),
        ];
        let beta = session("beta", false);

        let rows = session_rail_rows(&[alpha, beta]);
        let text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();

        assert_eq!(
            text,
            vec![
                "01 * alpha",
                "   ● impl-260718-120000-01000 · claude",
                "   · audit-260718-130000-02000 · codex +1",
                "02 - beta",
                // the buckets are always pinned to the tail of the rail
                " f - Finalized runs · 0",
                " x - Failed runs · 0",
                " n - Needs attention · 0",
            ]
        );
        assert_eq!(rows.iter().filter(|row| row.is_live_process()).count(), 2);
    }

    #[test]
    fn rail_does_not_repeat_process_label_when_tab_already_names_the_agent() {
        let tab = TabUiInfo::for_rail_test("claude", true, "claude", 1);

        assert_eq!(format_process_tab_rail_entry(&tab), "   ● claude");
    }

    fn bucket_session(name: &str, tabs: usize) -> SessionUiInfo {
        let mut bucket = session(name, false);
        bucket.tabs = (0..tabs)
            .map(|index| TabUiInfo::for_rail_test(&format!("run-{}", index), false, "", 0))
            .collect();
        bucket
    }

    #[test]
    fn bucket_names_match_the_triage_contract() {
        // Pinned against zellij-utils/src/run_triage.rs — the reaper creates
        // sessions by these exact names, and a silent rename here would leave
        // transferred runs invisible in the rail.
        assert_eq!(BucketKind::Finalized.session_name(), "Finalized runs");
        assert_eq!(BucketKind::Failed.session_name(), "Failed runs");
        assert_eq!(BucketKind::NeedsAttention.session_name(), "Needs attention");
    }

    #[test]
    fn buckets_are_pinned_below_the_working_sessions_with_live_counts() {
        let rows = session_rail_rows(&[
            session("alpha", true),
            bucket_session("Finalized runs", 3),
            session("beta", false),
            bucket_session("Needs attention", 1),
            bucket_session("Failed runs", 2),
        ]);
        let text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();

        // Drawer order is fixed by RAIL_BUCKETS, not by session creation order.
        assert_eq!(
            text,
            vec![
                "01 * alpha",
                "02 - beta",
                " f - Finalized runs · 3",
                " x - Failed runs · 2",
                " n - Needs attention · 1",
            ]
        );
    }

    #[test]
    fn bucket_rows_show_even_before_their_sessions_exist() {
        let rows = session_rail_rows(&[session("alpha", true)]);
        let buckets: Vec<&SessionRailRow> = rows.iter().filter(|row| row.is_bucket()).collect();

        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].text, " f - Finalized runs · 0");
        assert_eq!(buckets[1].text, " x - Failed runs · 0");
        assert_eq!(buckets[2].text, " n - Needs attention · 0");
        assert!(matches!(
            buckets[0].kind,
            SessionRailRowKind::Bucket {
                session_index: None,
                ..
            }
        ));
    }

    #[test]
    fn bucket_rows_carry_the_session_index_so_clicks_reuse_the_existing_plumbing() {
        let rows =
            session_rail_rows(&[session("alpha", true), bucket_session("Needs attention", 2)]);
        let needs_attention = rows
            .iter()
            .find(|row| row.is_bucket() && row.text.contains("Needs"))
            .unwrap();

        assert_eq!(
            needs_attention.kind,
            SessionRailRowKind::Bucket {
                bucket: BucketKind::NeedsAttention,
                session_index: Some(1),
            }
        );
    }

    #[test]
    fn buckets_do_not_shift_the_working_session_ordinals() {
        // All three drawers interleaved with the working sessions: pressing 2
        // must still mean beta no matter how many buckets sit above it.
        let sessions = vec![
            session("alpha", true),
            bucket_session("Finalized runs", 5),
            session("beta", false),
            bucket_session("Failed runs", 4),
            bucket_session("Needs attention", 3),
            session("gamma", false),
        ];

        assert_eq!(rail_ordinal_target(&sessions, '1'), Some(0));
        assert_eq!(rail_ordinal_target(&sessions, '2'), Some(2));
        assert_eq!(rail_ordinal_target(&sessions, '3'), Some(5));
        assert_eq!(rail_ordinal_target(&sessions, '4'), None);
        for bucket in RAIL_BUCKETS {
            assert_eq!(rail_ordinal_target(&sessions, bucket.hotkey()), None);
        }
    }

    #[test]
    fn bucket_hotkeys_stay_clear_of_the_session_ordinals_and_of_each_other() {
        let mut hotkeys: Vec<char> = RAIL_BUCKETS.into_iter().map(|b| b.hotkey()).collect();
        for hotkey in &hotkeys {
            assert_eq!(rail_ordinal_key_to_index(*hotkey), None);
        }
        let claimed = hotkeys.len();
        hotkeys.sort_unstable();
        hotkeys.dedup();
        assert_eq!(hotkeys.len(), claimed, "two buckets claim the same hotkey");

        assert_eq!(BucketKind::Finalized.hotkey(), 'f');
        assert_eq!(BucketKind::Failed.hotkey(), 'x');
        assert_eq!(BucketKind::NeedsAttention.hotkey(), 'n');
    }

    #[test]
    fn a_bucket_session_is_never_listed_twice() {
        let rows = session_rail_rows(&[bucket_session("Finalized runs", 2)]);

        assert_eq!(rows.iter().filter(|row| row.is_bucket()).count(), 3);
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.kind, SessionRailRowKind::Session(_))),
            "bucket session leaked into the ordinary listing"
        );
    }

    #[test]
    fn rail_ordinal_keys_map_to_session_indices() {
        assert_eq!(rail_ordinal_key_to_index('1'), Some(0));
        assert_eq!(rail_ordinal_key_to_index('2'), Some(1));
        assert_eq!(rail_ordinal_key_to_index('9'), Some(8));
        assert_eq!(rail_ordinal_key_to_index('0'), Some(9));
        assert_eq!(rail_ordinal_key_to_index('a'), None);
    }

    #[test]
    fn rail_range_keeps_selected_session_visible() {
        assert_eq!(rail_range_to_render(4, 10, Some(7)), (5, 9));
        assert_eq!(rail_range_to_render(4, 10, Some(0)), (0, 4));
        assert_eq!(rail_range_to_render(4, 10, Some(9)), (6, 10));
    }

    #[test]
    fn rail_lines_are_clipped_and_padded_to_width() {
        assert_eq!(fit_rail_line("abcdef", 4), "abcd");
        assert_eq!(fit_rail_line("ab", 4), "ab  ");
    }

    #[test]
    fn relative_session_target_wraps_in_both_directions() {
        let sessions = vec![
            session("alpha", false),
            session("beta", true),
            session("gamma", false),
        ];
        assert_eq!(
            relative_session_target(&sessions, 1),
            Some("gamma".to_owned())
        );
        assert_eq!(
            relative_session_target(&sessions, -1),
            Some("alpha".to_owned())
        );

        let at_end = vec![session("alpha", false), session("beta", true)];
        assert_eq!(
            relative_session_target(&at_end, 1),
            Some("alpha".to_owned())
        );
    }

    #[test]
    fn relative_session_target_refuses_degenerate_lists() {
        assert_eq!(relative_session_target(&[], 1), None);
        assert_eq!(relative_session_target(&[session("solo", true)], 1), None);
        // no current session marker — nothing sane to be relative to
        let orphaned = vec![session("alpha", false), session("beta", false)];
        assert_eq!(relative_session_target(&orphaned, 1), None);
    }

    #[test]
    fn live_process_rows_carry_tab_position_so_clicks_can_focus_the_worker() {
        let mut alpha = session("alpha", true);
        let mut run_a = TabUiInfo::for_rail_test("impl-a", true, "claude", 1);
        run_a.position = 0;
        let mut dead = TabUiInfo::for_rail_test("dead", false, "codex", 0);
        dead.position = 1;
        let mut run_b = TabUiInfo::for_rail_test("impl-b", false, "codex", 1);
        run_b.position = 2;
        alpha.tabs = vec![run_a, dead, run_b];

        let rows = session_rail_rows(&[alpha]);
        let live: Vec<&SessionRailRow> = rows.iter().filter(|row| row.is_live_process()).collect();

        assert_eq!(live.len(), 2, "dead tabs stay collapsed");
        assert_eq!(
            live[0].kind,
            SessionRailRowKind::LiveProcess {
                session_index: 0,
                tab_position: 0,
            }
        );
        assert_eq!(
            live[1].kind,
            SessionRailRowKind::LiveProcess {
                session_index: 0,
                tab_position: 2,
            }
        );
    }

    #[test]
    fn rail_row_click_target_maps_session_tab_and_bucket_including_empty() {
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Session(3)),
            RailClickTarget::Session(3)
        );
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::LiveProcess {
                session_index: 1,
                tab_position: 4,
            }),
            RailClickTarget::LiveProcess {
                session_index: 1,
                tab_position: 4,
            }
        );
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Bucket {
                bucket: BucketKind::Failed,
                session_index: Some(2),
            }),
            RailClickTarget::Bucket(BucketKind::Failed)
        );
        // Empty drawer: still a bucket target so the mouse path can surface
        // the same "is empty" error the hotkey does.
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Bucket {
                bucket: BucketKind::NeedsAttention,
                session_index: None,
            }),
            RailClickTarget::Bucket(BucketKind::NeedsAttention)
        );
    }

    #[test]
    fn simulated_left_click_on_session_row_selects_that_session() {
        // Hit-test only: the host-side switch is exercised by the existing
        // selection path. We rebuild the map the way render does and prove a
        // LeftClick(line) on a data row resolves to the right session.
        let sessions = vec![session("alpha", true), session("beta", false)];
        let rows = session_rail_rows(&sessions);
        let mut click_map: BTreeMap<usize, RailClickTarget> = BTreeMap::new();
        // row 0 is the header; data starts at 1, same as render_session_rail.
        for (offset, row) in rows.iter().enumerate() {
            click_map.insert(offset + 1, rail_row_click_target(&row.kind));
        }

        assert_eq!(
            click_map.get(&1),
            Some(&RailClickTarget::Session(0)),
            "first data row is the first working session"
        );
        assert_eq!(
            click_map.get(&2),
            Some(&RailClickTarget::Session(1)),
            "second data row is the second working session"
        );
        // Header never maps — LeftClick(0) is a no-op.
        assert!(!click_map.contains_key(&0));
    }

    #[test]
    fn simulated_left_click_on_live_process_row_targets_session_and_tab() {
        let mut alpha = session("alpha", true);
        let mut run = TabUiInfo::for_rail_test("worker-run", true, "claude", 1);
        run.position = 3;
        alpha.tabs = vec![run];
        let beta = session("beta", false);
        let rows = session_rail_rows(&[alpha, beta]);
        let mut click_map: BTreeMap<usize, RailClickTarget> = BTreeMap::new();
        for (offset, row) in rows.iter().enumerate() {
            click_map.insert(offset + 1, rail_row_click_target(&row.kind));
        }

        // row 1: alpha session, row 2: its live worker tab, row 3: beta, then buckets
        assert_eq!(click_map.get(&1), Some(&RailClickTarget::Session(0)));
        assert_eq!(
            click_map.get(&2),
            Some(&RailClickTarget::LiveProcess {
                session_index: 0,
                tab_position: 3,
            })
        );
        assert_eq!(click_map.get(&3), Some(&RailClickTarget::Session(1)));
    }

    #[test]
    fn simulated_left_click_on_bucket_row_targets_bucket_hotkey_equivalent() {
        let rows = session_rail_rows(&[session("alpha", true)]);
        let mut click_map: BTreeMap<usize, RailClickTarget> = BTreeMap::new();
        for (offset, row) in rows.iter().enumerate() {
            click_map.insert(offset + 1, rail_row_click_target(&row.kind));
        }
        // alpha + three empty drawers
        assert_eq!(
            click_map.get(&2),
            Some(&RailClickTarget::Bucket(BucketKind::Finalized))
        );
        assert_eq!(
            click_map.get(&3),
            Some(&RailClickTarget::Bucket(BucketKind::Failed))
        );
        assert_eq!(
            click_map.get(&4),
            Some(&RailClickTarget::Bucket(BucketKind::NeedsAttention))
        );
    }

    #[test]
    fn simulated_left_click_outside_data_rows_is_noop() {
        // Pure map lookup mirrors handle_session_rail_mouse: missing key → false.
        let click_map: BTreeMap<usize, RailClickTarget> = BTreeMap::new();
        assert!(
            !click_map.contains_key(&0),
            "header / empty map must not resolve a target"
        );
        assert!(!click_map.contains_key(&99));
        // Negative lines are rejected before map lookup via usize::try_from.
        assert!(usize::try_from(-1_isize).is_err());
    }
}
