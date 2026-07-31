mod list_navigation;
mod new_session_info;
mod resurrectable_sessions;
mod session_list;
mod single_screen;
mod single_screen_data;
mod single_screen_render;
mod ui;
use serde::{Deserialize, Deserializer};
use std::collections::{BTreeMap, BTreeSet};
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

const SETTLEMENT_COUNTS_PIPE: &str = "vc_settlement_counts";
const SETTLEMENT_HISTORY_SCHEMA: &str = "vibecrafted.settlement-history.v1";
// Guardian republishes at least every five seconds. Three missed refresh
// windows turn exact counts into lower bounds instead of letting a dead
// producer leave stale values looking authoritative forever.
const SETTLEMENT_FEED_STALE_AFTER_TICKS: u8 = 15;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SettlementPipeOutcome {
    acknowledged: bool,
    should_render: bool,
}

impl SettlementPipeOutcome {
    fn accepted(should_render: bool) -> Self {
        Self {
            acknowledged: true,
            should_render,
        }
    }

    fn rejected(should_render: bool) -> Self {
        Self {
            acknowledged: false,
            should_render,
        }
    }
}

fn acknowledge_cli_pipe(pipe_id: &str) {
    #[cfg(target_arch = "wasm32")]
    unblock_cli_pipe_input(pipe_id);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = pipe_id;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SettlementCounts {
    f: u64,
    x: u64,
    n: u64,
    total: u64,
}

impl SettlementCounts {
    fn has_valid_total(&self) -> bool {
        self.f
            .checked_add(self.x)
            .and_then(|total| total.checked_add(self.n))
            == Some(self.total)
    }

    fn historical_count(&self, bucket: BucketKind) -> u64 {
        match bucket {
            BucketKind::Finalized => self.f,
            BucketKind::Failed => self.x,
            BucketKind::NeedsAttention => self.n,
        }
    }

    fn is_monotonic_from(&self, previous: &Self) -> bool {
        self.f >= previous.f
            && self.x >= previous.x
            && self.n >= previous.n
            && self.total >= previous.total
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SettlementHistory {
    schema: String,
    generation: String,
    sequence: u64,
    historical_transitions: SettlementCounts,
    latest_by_run: SettlementCounts,
    gaps: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    complete_from: Option<u64>,
}

fn deserialize_required_option<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
}

impl SettlementHistory {
    fn parse(payload: &str) -> Option<Self> {
        let snapshot: Self = serde_json::from_str(payload).ok()?;
        let generation = Uuid::parse_str(&snapshot.generation).ok()?;
        let has_canonical_generation = generation.to_string() == snapshot.generation;
        let has_valid_completeness = matches!(
            (snapshot.sequence, snapshot.gaps, snapshot.complete_from),
            (0, 0, None) | (1.., 0, Some(1)) | (_, 1.., None)
        );
        (snapshot.schema == SETTLEMENT_HISTORY_SCHEMA
            && has_canonical_generation
            && snapshot.historical_transitions.has_valid_total()
            && snapshot.latest_by_run.has_valid_total()
            && snapshot.sequence == snapshot.historical_transitions.total
            && snapshot.latest_by_run.total <= snapshot.historical_transitions.total
            && snapshot.latest_by_run.f <= snapshot.historical_transitions.f
            && snapshot.latest_by_run.x <= snapshot.historical_transitions.x
            && snapshot.latest_by_run.n <= snapshot.historical_transitions.n
            && has_valid_completeness)
            .then_some(snapshot)
    }

    fn is_complete(&self) -> bool {
        self.gaps == 0
    }
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
    /// Last-session kill confirmation (Ctrl+o → x when no other live sessions).
    show_kill_last_session_warning: bool,
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
    // Hovered rail row (plugin-relative line). Sessions and f/x/n drawers share
    // the same OS-like highlight so every chrome row feels equally interactive.
    rail_hover_row: Option<usize>,
    // Canonical, append-only settlement truth delivered by Vibecrafted. This
    // never falls back to bucket viewer tabs: before the first valid payload,
    // f/x/n render as unavailable.
    settlement_history: Option<SettlementHistory>,
    // A rejected payload on the canonical transport means the last accepted
    // snapshot is only a lower bound until a valid replay confirms it again.
    // Never leave stale values looking exact after producer corruption,
    // divergence, or a non-monotonic advancement.
    settlement_feed_degraded: bool,
    // The visible session-manager already receives one host timer per second.
    // Reuse that cadence as a freshness lease without adding another timer or
    // coupling plugin rendering to Guardian process state.
    settlement_feed_age_ticks: Option<u8>,
    // Once a producer generation has been superseded, a delayed payload from
    // that retired generation must never roll the rail back.
    retired_settlement_generations: BTreeSet<String>,
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
        if pipe_message.name == SETTLEMENT_COUNTS_PIPE {
            // This is route integrity, not cryptographic authentication:
            // accept only the public local-CLI broadcast Guardian emits. A
            // process running as the same OS user can already mutate both
            // runtimes and therefore remains inside this trust boundary.
            let PipeSource::Cli(pipe_id) = &pipe_message.source else {
                return false;
            };
            if pipe_message.is_private {
                return false;
            }
            let outcome = match pipe_message.payload.as_deref() {
                Some(payload) => self.apply_settlement_history(payload),
                None => SettlementPipeOutcome::rejected(self.mark_settlement_feed_degraded()),
            };
            if outcome.acknowledged {
                acknowledge_cli_pipe(pipe_id);
            }
            outcome.should_render
        } else if pipe_message.name == "vc_rail_nav" {
            match pipe_message.payload.as_deref() {
                Some("up") => self.switch_session_relative(-1),
                Some("down") => self.switch_session_relative(1),
                _ => (),
            }
            true
        } else if pipe_message.name == "vc_kill_current_session" {
            self.kill_current_session_preserving_client();
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
                if self.age_settlement_feed() {
                    should_render = true;
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
                    subscribe(&[EventType::SessionUpdate]);
                    if self.refresh_session_list() {
                        should_render = true;
                    }
                    self.arm_refresh_timer();
                } else if !is_visible && was_visible {
                    // Hidden instances drop the subscription entirely so the
                    // server does not serialize the snapshot into their wasm
                    // memory every second.
                    unsubscribe(&[EventType::SessionUpdate]);
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
                }
                // Always process mouse so hover + click stay live even after an error.
                if self.handle_session_rail_mouse(mouse_event) {
                    should_render = true;
                }
            },
            Event::PermissionRequestResult(_result) => {
                should_render = true;
            },
            Event::SessionUpdate(session_infos, resurrectable_session_list) => {
                // Every tab carries its own rail instance; hidden instances must
                // not pay the full rebuild for each 1s broadcast — the visible
                // transition refreshes them from scratch instead.
                if !self.is_visible {
                    return false;
                }
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
                } else if self.show_kill_last_session_warning {
                    self.render_kill_last_session_warning(height, width, x, y);
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
                        } else if self.show_kill_last_session_warning {
                            self.render_kill_last_session_warning(height, width, x, y);
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
    // The fisheye is the one "you are here" glyph across the whole chrome —
    // the same ◉/○ pair the tab chips carry.
    let status = if session.is_current_session {
        "◉"
    } else {
        "○"
    };
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

/// Chrome tabs of the default layout (`vibecrafted.kdl`): present in every
/// session created without an explicit layout, including bucket sessions. Kept
/// in lockstep with the layout by `chrome_tab_names_match_the_default_layout`.
const CHROME_TAB_NAMES: [&str; 2] = ["Start here", "Shell"];

/// Char offset of `needle` in `haystack`, searching from char offset `from`.
/// Text::color_range speaks char offsets, while `str::find` returns bytes —
/// this bridges the two for rows containing multi-byte glyphs (`●`, `·`).
fn char_offset_of(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let byte_start = if from == 0 {
        0
    } else {
        haystack.char_indices().nth(from).map(|(byte, _)| byte)?
    };
    haystack[byte_start..]
        .find(needle)
        .map(|relative| haystack[..byte_start + relative].chars().count())
}

impl BucketKind {
    fn session_name(&self) -> &'static str {
        match self {
            BucketKind::Finalized => "Finalized runs",
            BucketKind::Failed => "Failed runs",
            BucketKind::NeedsAttention => "Needs attention",
        }
    }
    fn rail_label(&self) -> &'static str {
        match self {
            BucketKind::Finalized => "Finalized",
            BucketKind::Failed => "Failed",
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

/// Resolve hover highlight for a plugin-relative mouse line.
/// Missing map key / negative line → clear (no sticky highlight on chrome gaps).
fn rail_hover_target(line: isize, click_map: &BTreeMap<usize, RailClickTarget>) -> Option<usize> {
    usize::try_from(line)
        .ok()
        .filter(|row| click_map.contains_key(row))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionRailRow {
    kind: SessionRailRowKind,
    text: String,
}

impl SessionRailRow {
    #[cfg(test)]
    fn is_live_process(&self) -> bool {
        matches!(self.kind, SessionRailRowKind::LiveProcess { .. })
    }
    fn is_bucket(&self) -> bool {
        matches!(self.kind, SessionRailRowKind::Bucket { .. })
    }
}

/// Same rhythm as working-session rows (` 1 * name`): hotkey slot + status +
/// immutable historical count + label. Viewer tabs are deliberately demoted
/// to the explicit `tN` suffix: they are useful diagnostics, never f/x/n truth.
fn format_bucket_rail_entry(
    bucket: BucketKind,
    historical_count: Option<u64>,
    history_is_complete: bool,
    viewer_tabs: usize,
    is_current_session: bool,
) -> String {
    let status = if is_current_session { "◉" } else { "○" };
    let historical_count = historical_count
        .map(|count| {
            if history_is_complete {
                count.to_string()
            } else {
                format!("≥{count}")
            }
        })
        .unwrap_or_else(|| "?".to_owned());
    format!(
        " {} {} {:>3} · {} · t{}",
        bucket.hotkey(),
        status,
        historical_count,
        bucket.rail_label(),
        viewer_tabs,
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

// Direct rail navigation (vc_rail_nav pipe): product contract v3 is
// Cmd/Super+Up/Down in every mode (Ctrl+Up/Down mirrors it outside LOCK);
// tab-mode Up/Down and bare arrows on a focused rail also hit this path. The target
// is resolved relative to the *current* session with wrap-around, so every
// rail instance receiving the broadcast computes the same destination and
// the switch stays idempotent. Navigation walks the *working* sessions in
// rail order only — the f/x/n bucket sessions have their own hotkeys and
// must not swallow a step.
fn relative_session_target(sessions: &[SessionUiInfo], offset: isize) -> Option<String> {
    let working = working_session_indices(sessions);
    if working.len() < 2 {
        return None;
    }
    let current_pos = working
        .iter()
        .position(|&index| sessions[index].is_current_session)?;
    let count = working.len() as isize;
    let target_pos = (current_pos as isize + offset).rem_euclid(count) as usize;
    if target_pos == current_pos {
        return None;
    }
    sessions
        .get(working[target_pos])
        .map(|session| session.name.clone())
}

/// Where the client should land after killing the current session: the next
/// working session in nav order first, then any other working session (the
/// nav helper cannot even find "here" when the current session is a bucket),
/// then any other bucket — an f/x/n drawer is still a live session, and
/// hopping into it beats killing the whole client. Only a true "nothing else
/// is alive" returns `None`.
///
/// Navigation may skip buckets; the kill path must not — that asymmetry is
/// deliberate. Collapsing both onto [`relative_session_target`] was how
/// killing a session next to live buckets took the client down with it.
fn kill_fallback_target(sessions: &[SessionUiInfo]) -> Option<String> {
    if let Some(target) = relative_session_target(sessions, 1) {
        return Some(target);
    }
    let current_index = sessions.iter().position(|s| s.is_current_session);
    let is_other = |index: usize| Some(index) != current_index;
    if let Some(index) = working_session_indices(sessions)
        .into_iter()
        .find(|&index| is_other(index))
    {
        return Some(sessions[index].name.clone());
    }
    sessions
        .iter()
        .enumerate()
        .find(|&(index, _)| is_other(index))
        .map(|(_, session)| session.name.clone())
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

fn session_rail_rows_with_truth(
    sessions: &[SessionUiInfo],
    settlement_history: Option<&SettlementHistory>,
    settlement_feed_degraded: bool,
) -> Vec<SessionRailRow> {
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
    rows.extend(bucket_rail_rows(
        sessions,
        settlement_history,
        settlement_feed_degraded,
    ));
    rows
}

#[cfg(test)]
fn session_rail_rows(sessions: &[SessionUiInfo]) -> Vec<SessionRailRow> {
    session_rail_rows_with_truth(sessions, None, false)
}

/// The pinned tail of the rail. Always all three buckets, whether or not their
/// sessions exist yet — a permanent entry point beats one that appears only
/// once something has already failed.
fn bucket_rail_rows(
    sessions: &[SessionUiInfo],
    settlement_history: Option<&SettlementHistory>,
    settlement_feed_degraded: bool,
) -> Vec<SessionRailRow> {
    RAIL_BUCKETS
        .into_iter()
        .map(|bucket| {
            let session_index = sessions
                .iter()
                .position(|session| session.name == bucket.session_name());
            let session = session_index.map(|index| &sessions[index]);
            // The bucket session remains the navigation target. Its non-chrome
            // tab inventory is shown only as secondary `tN` telemetry: tabs can
            // disappear, be finalized elsewhere, or represent stale viewers.
            let viewer_tabs = session
                .map(|session| {
                    session
                        .tabs
                        .iter()
                        .filter(|tab| !CHROME_TAB_NAMES.contains(&tab.name.as_str()))
                        .count()
                })
                .unwrap_or(0);
            let historical_count = settlement_history
                .map(|snapshot| snapshot.historical_transitions.historical_count(bucket));
            let history_is_complete = !settlement_feed_degraded
                && settlement_history.is_some_and(SettlementHistory::is_complete);
            let is_current_session = session.is_some_and(|session| session.is_current_session);
            SessionRailRow {
                kind: SessionRailRowKind::Bucket {
                    bucket,
                    session_index,
                },
                text: format_bucket_rail_entry(
                    bucket,
                    historical_count,
                    history_is_complete,
                    viewer_tabs,
                    is_current_session,
                ),
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

/// Bucket rows lose label letters first. A historical count is either rendered
/// in full or omitted entirely: clipping `18446744073709551615` into a smaller
/// exact-looking number would corrupt the operator truth.
fn fit_bucket_rail_line(text: &str, width: usize) -> String {
    let Some(head_end) = text.find(" · ") else {
        return fit_rail_line(text, width);
    };
    let Some(suffix_start) = text.rfind(" · t") else {
        return fit_rail_line(text, width);
    };
    let head = &text[..head_end];
    let suffix = &text[suffix_start..];
    let head_width = head.width();
    let suffix_width = suffix.width();
    if text.width() <= width {
        return fit_rail_line(text, width);
    }

    if head_width > width {
        let bucket_without_count: String = head.chars().take(4).collect();
        return fit_rail_line(&format!("{bucket_without_count} …"), width);
    }
    if head_width + suffix_width > width {
        return fit_rail_line(head, width);
    }

    let label = &text[head_end..suffix_start];
    let label_width = width - head_width - suffix_width;
    let mut fitted = head.to_owned();
    fitted.push_str(&truncate_to_width(label, label_width));
    fitted.push_str(suffix);
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
    /// Accept a canonical snapshot iff it advances the sequence without ever
    /// decreasing append-only historical counts inside one durable producer
    /// generation. A new canonical generation is an explicit store reset and
    /// may restart at zero; its completeness marker keeps partial replay honest.
    /// Exact replay is idempotent, while stale sequence numbers and same-sequence
    /// divergence are rejected.
    fn apply_settlement_history(&mut self, payload: &str) -> SettlementPipeOutcome {
        let Some(incoming) = SettlementHistory::parse(payload) else {
            return SettlementPipeOutcome::rejected(self.mark_settlement_feed_degraded());
        };
        if self
            .retired_settlement_generations
            .contains(&incoming.generation)
        {
            return SettlementPipeOutcome::rejected(false);
        }
        if let Some(current) = &self.settlement_history {
            if incoming.generation != current.generation {
                if incoming.is_complete()
                    && !incoming
                        .historical_transitions
                        .is_monotonic_from(&current.historical_transitions)
                {
                    return SettlementPipeOutcome::rejected(self.mark_settlement_feed_degraded());
                }
                self.retired_settlement_generations
                    .insert(current.generation.clone());
                self.settlement_history = Some(incoming);
                self.mark_settlement_feed_fresh();
                return SettlementPipeOutcome::accepted(true);
            }
            if incoming.sequence < current.sequence {
                return SettlementPipeOutcome::rejected(false);
            }
            if incoming.sequence == current.sequence {
                if incoming != *current {
                    eprintln!(
                        "rejecting divergent settlement history at sequence {}",
                        incoming.sequence
                    );
                    return SettlementPipeOutcome::rejected(self.mark_settlement_feed_degraded());
                }
                let should_render = std::mem::take(&mut self.settlement_feed_degraded);
                self.settlement_feed_age_ticks = Some(0);
                return SettlementPipeOutcome::accepted(should_render);
            }
            if !incoming
                .historical_transitions
                .is_monotonic_from(&current.historical_transitions)
            {
                return SettlementPipeOutcome::rejected(self.mark_settlement_feed_degraded());
            }
        }
        self.settlement_history = Some(incoming);
        self.mark_settlement_feed_fresh();
        SettlementPipeOutcome::accepted(true)
    }

    fn mark_settlement_feed_fresh(&mut self) {
        self.settlement_feed_degraded = false;
        self.settlement_feed_age_ticks = Some(0);
    }

    fn mark_settlement_feed_degraded(&mut self) -> bool {
        self.settlement_history.is_some()
            && !std::mem::replace(&mut self.settlement_feed_degraded, true)
    }

    fn age_settlement_feed(&mut self) -> bool {
        let Some(age_ticks) = self.settlement_feed_age_ticks.as_mut() else {
            return false;
        };
        *age_ticks = age_ticks.saturating_add(1);
        if *age_ticks < SETTLEMENT_FEED_STALE_AFTER_TICKS {
            return false;
        }
        self.mark_settlement_feed_degraded()
    }

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

        let all_rows = session_rail_rows_with_truth(
            &self.sessions.session_ui_infos,
            self.settlement_history.as_ref(),
            self.settlement_feed_degraded,
        );
        // The buckets are pinned to the bottom of the rail, so only the leading
        // working-session rows take part in scrolling.
        let bucket_row_start = all_rows.iter().position(|row| row.is_bucket()).unwrap_or(0);
        let (rail_rows, bucket_rows) = all_rows.split_at(bucket_row_start);

        // The LIVE number moved to the compact-bar's fleet chip; the header
        // keeps the session count and the current-session anchor — the top
        // bar carries brand │ mode │ tabs only (operator call 2026-07-30),
        // so "where am I" lives here, right above the session list.
        let session_count = working_session_indices(&self.sessions.session_ui_infos).len();
        let mut header_text = format!("SESSIONS {}", session_count);
        let anchor_start = header_text.width() + 3; // " · " before the name
        if let Some(current) = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|s| s.is_current_session)
        {
            header_text.push_str(&format!(" · {}", current.name));
        }
        let header = fit_rail_line(&header_text, cols);
        let header_width = header.width();
        let mut header = Text::new(header);
        if cols >= 8 {
            header = header.color_range(1, 0..8);
        }
        if header_width > anchor_start {
            // The anchor takes the same accent as SESSIONS — one hue for
            // "you are here", per the THEMES_GUIDE doctrine.
            header = header.color_range(1, anchor_start..);
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
            let fitted = fit_rail_line(&rail_row.text, cols);
            let fitted_chars = fitted.chars().count();
            let mut text = Text::new(fitted.clone());
            match rail_row.kind {
                SessionRailRowKind::Session(session_index) => {
                    let is_current = self
                        .sessions
                        .session_ui_infos
                        .get(session_index)
                        .is_some_and(|session| session.is_current_session);
                    if cols >= 4 {
                        // The `*` current-session marker must be the brightest
                        // ink on the line: leave it at the row's base color and
                        // dim the `-` of every other session instead. Painting
                        // both with the same muted emphasis made "you are
                        // here" invisible.
                        // Ordinal digits are chrome, not content — dim them.
                        text = text.color_range(2, 0..2);
                        if is_current {
                            // "You are here" carries the accent on the NAME,
                            // the same ink as the header — one glance finds it.
                            if fitted_chars > 5 {
                                text = text.color_range(1, 5..fitted_chars);
                            }
                        } else {
                            text = text.color_range(2, 3..4);
                        }
                    }
                    // The whole current-session block sits on a full-width
                    // highlight bed — this header row opens it.
                    if is_current || selected_index == Some(session_index) {
                        text = text.selected();
                    }
                },
                SessionRailRowKind::LiveProcess { session_index, .. } => {
                    let in_current_session = self
                        .sessions
                        .session_ui_infos
                        .get(session_index)
                        .is_some_and(|session| session.is_current_session);
                    let is_active_tab_row = fitted.chars().nth(3) == Some('●');
                    if cols >= 4 {
                        // Active tab dot gets the accent, idle dot stays dim;
                        // the trailing "· command +N" diagnostics dim away so
                        // the tab name is the only bright ink on the line.
                        if is_active_tab_row {
                            text = text.color_range(1, 3..4);
                        } else {
                            text = text.color_range(2, 3..4);
                        }
                        if let Some(separator) = char_offset_of(&fitted, " · ", 4) {
                            text = text.color_range(2, separator..fitted_chars);
                        }
                        if in_current_session && is_active_tab_row {
                            // Strongest level of the three: on the highlight
                            // bed, the tab you are actually in carries the
                            // accent on its whole name.
                            let name_end =
                                char_offset_of(&fitted, " · ", 4).unwrap_or(fitted_chars);
                            text = text.color_range(1, 3..name_end);
                        }
                    }
                    // Process rows extend the current-session highlight block
                    // so the whole "you are here" region reads as one shape.
                    if in_current_session {
                        text = text.selected();
                    }
                },
                SessionRailRowKind::Bucket { .. } => {},
            }
            // OS hover: same highlight language for sessions, live tabs, drawers.
            if self.rail_hover_row == Some(row) {
                text = text.selected();
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
            let mut text = Text::new(fit_bucket_rail_line(&bucket_row.text, cols));
            // Same accent column as session status (`◉`/`○` at index 3 in
            // `"01 ◉ name"` / `" f ○ Finalized…"`) — color_range(1, 3..4).
            if cols >= 4 {
                text = text.color_range(1, 3..4);
            }
            if let SessionRailRowKind::Bucket {
                session_index: Some(session_index),
                ..
            } = bucket_row.kind
                && selected_index == Some(session_index)
            {
                text = text.selected();
            }
            if self.rail_hover_row == Some(row) {
                text = text.selected();
            }
            // Empty drawers stay clickable: mouse + f/x/n open the folder
            // (create session with fleet layout when missing).
            self.rail_click_map
                .insert(row, rail_row_click_target(&bucket_row.kind));
            print_text_with_coordinates(text, 0, row, None, None);
            row += 1;
        }
    }
    fn handle_session_rail_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                // Operator contract: arrow = immediate switch, no Enter confirm.
                self.switch_session_relative(1);
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.switch_session_relative(-1);
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.handle_session_rail_selection();
                true
            },
            BareKey::Char('+') | BareKey::Char('=') if key.has_no_modifiers() => {
                // Rail width is operator-tunable: the layout's size=24 is only
                // the starting point. Growing the right edge widens the column.
                resize_focused_pane_with_direction(Resize::Increase, Direction::Right);
                true
            },
            BareKey::Char('-') if key.has_no_modifiers() => {
                resize_focused_pane_with_direction(Resize::Decrease, Direction::Right);
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
                // Keep hover on the row we just activated (OS list selection).
                self.rail_hover_row = Some(row);
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
            Mouse::Hover(line, _column) => {
                // Only clickable rows highlight. Header / blank / footer /
                // out-of-bounds / leave (line < 0 from server) clear hover.
                // Hover is delivered both while focused (SendToTerminal) and
                // while unfocused (UpdateHover → mouse_event) so the rail
                // lights under the cursor without a prior click.
                let next = rail_hover_target(line, &self.rail_click_map);
                if self.rail_hover_row != next {
                    self.rail_hover_row = next;
                    true
                } else {
                    false
                }
            },
            Mouse::ScrollUp(_) => {
                self.sessions.move_session_selection_up();
                true
            },
            Mouse::ScrollDown(_) => {
                self.sessions.move_session_selection_down();
                true
            },
            // Right-click / middle not mapped. Shift+click is client passthrough.
            _ => false,
        }
    }
    /// Open a triage drawer session (Finalized / Failed / Needs attention).
    ///
    /// OS-folder contract: every path does something useful.
    /// - already here → quiet no-op
    /// - exists elsewhere → switch
    /// - missing (count 0) → create with fleet `vibecrafted` layout and attach
    fn jump_to_bucket(&mut self, bucket: BucketKind) {
        let name = bucket.session_name();
        let exists = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|session| session.name == name);
        match exists {
            Some(session) if session.is_current_session => {
                // Already inside this drawer — no error spam.
            },
            Some(_) => {
                switch_session_with_focus(name, None, None);
                self.reset_selected_index();
            },
            None => {
                // Empty drawer: open the folder with the standard session canvas.
                switch_session_with_layout(
                    Some(name),
                    LayoutInfo::BuiltIn("vibecrafted".to_owned()),
                    None,
                );
                self.reset_selected_index();
            },
        }
    }
    fn switch_session_relative(&mut self, offset: isize) {
        if let Some(target_session_name) =
            relative_session_target(&self.sessions.session_ui_infos, offset)
        {
            switch_session_with_focus(&target_session_name, None, None);
        }
    }

    /// Kill the current session without leaving vc-frame when another live
    /// session exists (switch first, then kill the abandoned server). If this
    /// is the last active session, arm a y/n confirmation overlay.
    fn kill_current_session_preserving_client(&mut self) {
        if self.show_kill_last_session_warning {
            // Second Ctrl+o → x (or repeated pipe) acts as explicit confirm.
            self.confirm_kill_last_session();
            return;
        }

        let current_name = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|session| session.is_current_session)
            .map(|session| session.name.clone())
            .or_else(|| self.session_name.clone());
        let Some(current_name) = current_name else {
            self.show_error("No current session to kill.");
            return;
        };

        if let Some(target) = kill_fallback_target(&self.sessions.session_ui_infos) {
            // Hop first so the client stays inside vc-frame, then kill the old server.
            switch_session_with_focus(&target, None, None);
            match kill_sessions(std::slice::from_ref(&current_name)) {
                Ok(()) => {
                    self.sessions
                        .session_ui_infos
                        .retain(|session| session.name != current_name);
                    self.show_kill_last_session_warning = false;
                },
                Err(error) => {
                    self.show_error(&format!("Failed to kill session: {error}"));
                },
            }
            return;
        }

        // Last active session in this window — require explicit confirmation.
        self.show_kill_last_session_warning = true;
        self.show_kill_all_sessions_warning = false;
        // Rail is narrow: use the error banner. Floating manager uses the
        // dedicated y/n overlay (see render_kill_last_session_warning).
        if self.is_rail {
            self.show_error(
                "You are about to close the last active session in this window. Are you sure? y/n \
                 (or Ctrl+o → x again).",
            );
        }
        // Make the plugin pane visible so the prompt is not buried.
        show_self(true);
    }

    fn confirm_kill_last_session(&mut self) {
        self.show_kill_last_session_warning = false;
        let name = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|session| session.is_current_session)
            .map(|session| session.name.clone())
            .or_else(|| self.session_name.clone());
        let Some(name) = name else {
            self.show_error("No current session to kill.");
            return;
        };
        // No other session to hop to — kill this server (client exits with it).
        if let Err(error) = kill_sessions(std::slice::from_ref(&name)) {
            self.show_error(&format!("Failed to kill session: {error}"));
        }
    }

    fn handle_kill_last_session_warning_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Char('y') if key.has_no_modifiers() => {
                self.confirm_kill_last_session();
                true
            },
            BareKey::Char('n') | BareKey::Esc if key.has_no_modifiers() => {
                self.show_kill_last_session_warning = false;
                self.error = None;
                true
            },
            BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                self.show_kill_last_session_warning = false;
                self.error = None;
                true
            },
            _ => true,
        }
    }
    fn handle_session_rail_selection(&mut self) {
        self.ensure_rail_selection();
        if let Some(selected_session_name) = self.sessions.get_selected_session_name() {
            if self.sessions.selected_is_current_session() {
                // Already here — quiet (same contract as jump_to_bucket).
            } else {
                switch_session_with_focus(&selected_session_name, None, None);
                self.reset_selected_index();
            }
        }
    }
    fn handle_key(&mut self, key: KeyWithModifier) -> bool {
        if self.show_kill_last_session_warning {
            return self.handle_kill_last_session_warning_key(key);
        }
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
                            // Already on this session with no tab/pane target — quiet.
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
                                        // Already here — quiet (same contract as rail drawers).
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
                                    // Already here — quiet (same contract as rail drawers).
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

    fn render_kill_last_session_warning(&self, rows: usize, columns: usize, x: usize, y: usize) {
        if rows == 0 || columns == 0 {
            return;
        }
        let warning_description_text =
            "You are about to close the last active session in this window.";
        let confirmation_text = "Are you sure? (y/n)";
        let warning_y_location = y + (rows / 2).saturating_sub(1);
        let confirmation_y_location = y + (rows / 2) + 1;
        let warning_x_location =
            x + columns.saturating_sub(warning_description_text.chars().count()) / 2;
        let confirmation_x_location =
            x + columns.saturating_sub(confirmation_text.chars().count()) / 2;
        print_text_with_coordinates(
            Text::new(warning_description_text).color_range(0, ..),
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

    fn session_launched_at(name: &str, is_current_session: bool, secs: u64) -> SessionUiInfo {
        SessionUiInfo {
            creation_time: Duration::from_secs(secs),
            ..session(name, is_current_session)
        }
    }

    /// Killing next to live buckets must hop into a bucket, never take the
    /// client down: buckets are invisible to navigation, not to the kill path.
    #[test]
    fn kill_next_to_buckets_hops_into_a_bucket_instead_of_dying() {
        let sessions = vec![
            session("work", true),
            session("Finalized runs", false),
            session("Needs attention", false),
        ];
        // Navigation deliberately sees nothing…
        assert_eq!(relative_session_target(&sessions, 1), None);
        // …but the kill path still finds a live session to land on.
        assert_eq!(
            kill_fallback_target(&sessions),
            Some("Finalized runs".to_owned())
        );
    }

    /// When the CURRENT session is a bucket, nav cannot even locate "here";
    /// the kill path must still prefer a working session over dying.
    #[test]
    fn kill_from_inside_a_bucket_prefers_a_working_session() {
        let sessions = vec![
            session("Failed runs", true),
            session("work", false),
            session("Finalized runs", false),
        ];
        assert_eq!(kill_fallback_target(&sessions), Some("work".to_owned()));
    }

    /// Two working sessions: the kill path follows the same wrap-around
    /// order the rail nav uses.
    #[test]
    fn kill_with_working_neighbours_follows_nav_order() {
        let sessions = vec![
            session("alpha", true),
            session("beta", false),
            session("Finalized runs", false),
        ];
        assert_eq!(kill_fallback_target(&sessions), Some("beta".to_owned()));
    }

    /// Only when nothing else is alive may the kill path return None —
    /// that is the one case where the confirmation overlay is honest.
    #[test]
    fn kill_of_the_truly_last_session_returns_none() {
        let sessions = vec![session("only", true)];
        assert_eq!(kill_fallback_target(&sessions), None);
    }

    /// The rail contract: slot = launch order. The session started first holds
    /// slot 01 for as long as it lives — regardless of its name and of which
    /// session the viewing instance considers current.
    #[test]
    fn rail_orders_sessions_by_launch_time_not_name_or_current() {
        let mut rail = SessionList::default();
        rail.set_sessions(
            vec![
                session_launched_at("alpha", true, 200),
                session_launched_at("zeta", false, 100),
            ],
            vec![],
        );
        let names: Vec<&str> = rail
            .session_ui_infos
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["zeta", "alpha"],
            "the earlier-launched session holds the earlier slot, name and current-ness be damned"
        );
    }

    /// Activating a different session must not reshuffle the rail: two views
    /// with different `is_current_session` flags render the same order.
    #[test]
    fn activation_does_not_reshuffle_rail() {
        let order_seen_by = |current: &str| {
            let mut rail = SessionList::default();
            rail.set_sessions(
                vec![
                    session_launched_at("morning", current == "morning", 100),
                    session_launched_at("noon", current == "noon", 200),
                    session_launched_at("evening", current == "evening", 300),
                ],
                vec![],
            );
            rail.session_ui_infos
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order_seen_by("morning"), order_seen_by("evening"));
        assert_eq!(order_seen_by("noon"), vec!["morning", "noon", "evening"]);
    }

    /// ctrl+t Up/Down steps through working sessions in rail order, wrapping
    /// around — and never lands on an f/x/n bucket session.
    #[test]
    fn rail_nav_steps_over_working_sessions_and_skips_buckets() {
        let sessions = vec![
            session_launched_at("early", false, 100),
            session_launched_at("Finalized runs", false, 150),
            session_launched_at("late", true, 200),
            session_launched_at("Needs attention", false, 250),
        ];
        // current = "late" (last working session): down wraps to "early",
        // up steps back to "early" as well (two working sessions total).
        assert_eq!(
            relative_session_target(&sessions, 1).as_deref(),
            Some("early"),
            "down from the last working session must wrap, not hit a bucket"
        );
        assert_eq!(
            relative_session_target(&sessions, -1).as_deref(),
            Some("early")
        );
        // A single working session has nowhere to go.
        let lonely = vec![
            session_launched_at("only", true, 100),
            session_launched_at("Failed runs", false, 200),
        ];
        assert_eq!(relative_session_target(&lonely, 1), None);
    }

    /// When a session dies, the sessions below it move up one slot — the
    /// survivors keep their relative launch order and a newly launched
    /// session appends at the end.
    #[test]
    fn dead_session_compacts_slots_preserving_launch_order() {
        let mut rail = SessionList::default();
        rail.set_sessions(
            vec![
                session_launched_at("first", true, 100),
                session_launched_at("second", false, 200),
                session_launched_at("third", false, 300),
            ],
            vec![],
        );
        // "second" dies; a fresh "fourth" launches later.
        rail.set_sessions(
            vec![
                session_launched_at("third", false, 300),
                session_launched_at("first", true, 100),
                session_launched_at("fourth", false, 400),
            ],
            vec![],
        );
        let names: Vec<&str> = rail
            .session_ui_infos
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["first", "third", "fourth"]);
    }

    #[test]
    fn hidden_instance_ignores_session_update_broadcasts() {
        let mut state = State {
            is_visible: false,
            ..Default::default()
        };
        state
            .sessions
            .set_sessions(vec![session("alpha", true)], vec![]);
        let rendered = state.update(Event::SessionUpdate(vec![], vec![]));
        assert!(!rendered, "hidden rail must not render on broadcast");
        assert_eq!(
            state.sessions.session_ui_infos.len(),
            1,
            "hidden rail must keep stale state instead of rebuilding it"
        );
    }

    #[test]
    fn visible_instance_processes_session_update_broadcasts() {
        let mut state = State {
            is_visible: true,
            ..Default::default()
        };
        state
            .sessions
            .set_sessions(vec![session("alpha", true)], vec![]);
        let rendered = state.update(Event::SessionUpdate(vec![], vec![]));
        assert!(rendered);
        assert!(state.sessions.session_ui_infos.is_empty());
    }

    #[test]
    fn rail_entries_include_ordinal_status_and_name() {
        assert_eq!(
            format_session_rail_entry(&session("alpha", true), 1),
            "01 ◉ alpha"
        );
        assert_eq!(
            format_session_rail_entry(&session("beta", false), 12),
            "12 ○ beta"
        );
    }

    /// The same inventory as seen by two different plugin instances: each one
    /// considers a different session current.
    fn two_views_of_the_same_inventory() -> (SessionList, SessionList) {
        let mut view_from_alpha = SessionList::default();
        view_from_alpha.set_sessions(
            vec![
                session("zeta", false),
                session("alpha", true),
                session("mid", false),
            ],
            vec![],
        );
        let mut view_from_zeta = SessionList::default();
        view_from_zeta.set_sessions(
            vec![
                session("zeta", true),
                session("alpha", false),
                session("mid", false),
            ],
            vec![],
        );
        (view_from_alpha, view_from_zeta)
    }

    fn working_row_texts(sessions: &[SessionUiInfo]) -> Vec<String> {
        session_rail_rows(sessions)
            .into_iter()
            .filter(|row| !row.is_bucket())
            .map(|row| row.text)
            .collect()
    }

    #[test]
    fn session_order_is_independent_of_current_session() {
        let (view_from_alpha, view_from_zeta) = two_views_of_the_same_inventory();
        let names = |list: &SessionList| -> Vec<String> {
            list.session_ui_infos
                .iter()
                .map(|s| s.name.clone())
                .collect()
        };
        assert_eq!(names(&view_from_alpha), vec!["alpha", "mid", "zeta"]);
        assert_eq!(
            names(&view_from_alpha),
            names(&view_from_zeta),
            "session order must not depend on which session is current"
        );
    }

    #[test]
    fn current_session_change_moves_only_the_marker() {
        let (view_from_alpha, view_from_zeta) = two_views_of_the_same_inventory();
        assert_eq!(
            working_row_texts(&view_from_alpha.session_ui_infos),
            vec!["01 ◉ alpha", "02 ○ mid", "03 ○ zeta"]
        );
        assert_eq!(
            working_row_texts(&view_from_zeta.session_ui_infos),
            vec!["01 ○ alpha", "02 ○ mid", "03 ◉ zeta"]
        );
    }

    #[test]
    fn hotkeys_and_rail_rows_target_the_same_sessions_in_every_view() {
        let (view_from_alpha, view_from_zeta) = two_views_of_the_same_inventory();
        for character in ['1', '2', '3'] {
            let target_a =
                rail_ordinal_target(&view_from_alpha.session_ui_infos, character).unwrap();
            let target_b =
                rail_ordinal_target(&view_from_zeta.session_ui_infos, character).unwrap();
            assert_eq!(
                view_from_alpha.session_ui_infos[target_a].name,
                view_from_zeta.session_ui_infos[target_b].name,
                "hotkey {} must resolve to the same session in every view",
                character
            );
        }
        // Click rows carry the session index they display, so the row under a
        // given ordinal and the hotkey for that ordinal must agree.
        let rows = session_rail_rows(&view_from_alpha.session_ui_infos);
        for (ordinal, row) in rows.iter().filter(|row| !row.is_bucket()).enumerate() {
            let SessionRailRowKind::Session(session_index) = &row.kind else {
                panic!("expected a session row, got {:?}", row.kind);
            };
            let hotkey = char::from_digit(ordinal as u32 + 1, 10).unwrap();
            assert_eq!(
                rail_ordinal_target(&view_from_alpha.session_ui_infos, hotkey),
                Some(*session_index),
                "click target and hotkey {} must point at the same session",
                hotkey
            );
        }
    }

    #[test]
    fn buckets_stay_pinned_below_name_sorted_sessions() {
        let mut rail = SessionList::default();
        // Bucket names sort alphabetically before the working sessions, which
        // must not pull them out of their pinned tail position.
        rail.set_sessions(
            vec![
                session("zzz", false),
                session("Finalized runs", false),
                session("aaa", true),
                session("Failed runs", false),
                session("Needs attention", false),
            ],
            vec![],
        );
        let rows = session_rail_rows(&rail.session_ui_infos);
        let bucket_start = rows.iter().position(|row| row.is_bucket()).unwrap();
        assert_eq!(
            bucket_start, 2,
            "only the two working sessions may precede the buckets"
        );
        assert!(rows[bucket_start..].iter().all(|row| row.is_bucket()));
        assert_eq!(rows[0].text, "01 ◉ aaa");
        assert_eq!(rows[1].text, "02 ○ zzz");
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
                "01 ◉ alpha",
                "   ● impl-260718-120000-01000 · claude",
                "   · audit-260718-130000-02000 · codex +1",
                "02 ○ beta",
                // the buckets are always pinned to the tail of the rail
                " f ○   ? · Finalized · t0",
                " x ○   ? · Failed · t0",
                " n ○   ? · Needs attention · t0",
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

    fn settlement_payload(
        sequence: u64,
        historical: (u64, u64, u64),
        latest: (u64, u64, u64),
    ) -> String {
        settlement_payload_for(
            "00000000-0000-4000-8000-000000000001",
            sequence,
            historical,
            latest,
            0,
            (sequence > 0).then_some(1),
        )
    }

    fn settlement_payload_for(
        generation: &str,
        sequence: u64,
        historical: (u64, u64, u64),
        latest: (u64, u64, u64),
        gaps: u64,
        complete_from: Option<u64>,
    ) -> String {
        serde_json::json!({
            "schema": SETTLEMENT_HISTORY_SCHEMA,
            "generation": generation,
            "sequence": sequence,
            "historical_transitions": {
                "f": historical.0,
                "x": historical.1,
                "n": historical.2,
                "total": historical.0 + historical.1 + historical.2,
            },
            "latest_by_run": {
                "f": latest.0,
                "x": latest.1,
                "n": latest.2,
                "total": latest.0 + latest.1 + latest.2,
            },
            "gaps": gaps,
            "complete_from": complete_from,
        })
        .to_string()
    }

    fn settlement_pipe(payload: String) -> PipeMessage {
        PipeMessage {
            source: PipeSource::Cli("settlement-history-test".to_owned()),
            name: SETTLEMENT_COUNTS_PIPE.to_owned(),
            payload: Some(payload),
            args: BTreeMap::new(),
            is_private: false,
        }
    }

    #[test]
    fn canonical_pipe_acknowledgement_is_independent_from_rendering() {
        let mut state = State::default();
        let canonical = settlement_payload(10, (4, 3, 3), (2, 1, 1));

        assert_eq!(
            state.apply_settlement_history(&canonical),
            SettlementPipeOutcome::accepted(true)
        );
        assert_eq!(
            state.apply_settlement_history(&canonical),
            SettlementPipeOutcome::accepted(false)
        );

        let divergent = settlement_payload(10, (5, 2, 3), (3, 1, 1));
        assert_eq!(
            state.apply_settlement_history(&divergent),
            SettlementPipeOutcome::rejected(true)
        );
        assert_eq!(
            state.apply_settlement_history(&canonical),
            SettlementPipeOutcome::accepted(true)
        );

        let stale = settlement_payload(9, (4, 3, 2), (2, 1, 1));
        assert_eq!(
            state.apply_settlement_history(&stale),
            SettlementPipeOutcome::rejected(false)
        );
    }

    #[test]
    fn canonical_history_drives_exact_97_176_408_primary_counts() {
        let snapshot =
            SettlementHistory::parse(&settlement_payload(681, (97, 176, 408), (12, 7, 3)))
                .expect("valid settlement history");
        let rows = session_rail_rows_with_truth(&[session("alpha", true)], Some(&snapshot), false);
        let buckets: Vec<&str> = rows
            .iter()
            .filter(|row| row.is_bucket())
            .map(|row| row.text.as_str())
            .collect();

        assert_eq!(
            buckets,
            vec![
                " f ○  97 · Finalized · t0",
                " x ○ 176 · Failed · t0",
                " n ○ 408 · Needs attention · t0",
            ]
        );
    }

    #[test]
    fn incomplete_history_is_an_explicit_lower_bound_never_an_exact_count() {
        let payload = settlement_payload_for(
            "00000000-0000-4000-8000-000000000001",
            681,
            (97, 176, 408),
            (12, 7, 3),
            4,
            None,
        );
        let snapshot = SettlementHistory::parse(&payload).expect("valid partial history");
        let rows = session_rail_rows_with_truth(&[], Some(&snapshot), false);
        let buckets: Vec<&str> = rows
            .iter()
            .filter(|row| row.is_bucket())
            .map(|row| row.text.as_str())
            .collect();

        assert_eq!(
            buckets,
            vec![
                " f ○ ≥97 · Finalized · t0",
                " x ○ ≥176 · Failed · t0",
                " n ○ ≥408 · Needs attention · t0",
            ]
        );
        assert!(
            SettlementHistory::parse(&settlement_payload_for(
                "00000000-0000-4000-8000-000000000001",
                681,
                (97, 176, 408),
                (12, 7, 3),
                4,
                Some(1),
            ))
            .is_none()
        );
    }

    #[test]
    fn generation_reset_is_partial_and_a_retired_generation_cannot_return() {
        let mut state = State::default();
        let first_generation = settlement_payload_for(
            "00000000-0000-4000-8000-000000000001",
            u64::MAX,
            (u64::MAX, 0, 0),
            (1, 0, 0),
            1,
            None,
        );
        assert!(state.pipe(settlement_pipe(first_generation)));

        let dishonest_exact_reset = settlement_payload_for(
            "00000000-0000-4000-8000-000000000002",
            0,
            (0, 0, 0),
            (0, 0, 0),
            0,
            None,
        );
        assert!(state.pipe(settlement_pipe(dishonest_exact_reset)));
        assert!(state.settlement_feed_degraded);

        let reset_generation = settlement_payload_for(
            "00000000-0000-4000-8000-000000000002",
            0,
            (0, 0, 0),
            (0, 0, 0),
            1,
            None,
        );
        assert!(state.pipe(settlement_pipe(reset_generation)));
        assert!(!state.settlement_feed_degraded);
        assert_eq!(
            state
                .settlement_history
                .as_ref()
                .map(|history| history.sequence),
            Some(0)
        );

        let delayed_retired_generation = settlement_payload_for(
            "00000000-0000-4000-8000-000000000001",
            u64::MAX,
            (u64::MAX, 0, 0),
            (1, 0, 0),
            1,
            None,
        );
        assert!(!state.pipe(settlement_pipe(delayed_retired_generation)));
        assert_eq!(
            state
                .settlement_history
                .as_ref()
                .map(|history| history.generation.as_str()),
            Some("00000000-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn settlement_history_accepts_only_cli_transport_and_canonical_generation() {
        let payload = settlement_payload(1, (1, 0, 0), (1, 0, 0));
        let mut state = State::default();
        let mut keybind = settlement_pipe(payload.clone());
        keybind.source = PipeSource::Keybind;
        assert!(!state.pipe(keybind));
        assert!(state.settlement_history.is_none());

        let mut targeted_private_cli = settlement_pipe(payload.clone());
        targeted_private_cli.is_private = true;
        assert!(!state.pipe(targeted_private_cli));
        assert!(state.settlement_history.is_none());

        let malformed_generation = settlement_payload_for(
            "00000000-0000-4000-8000-00000000000A",
            1,
            (1, 0, 0),
            (1, 0, 0),
            0,
            Some(1),
        );
        assert!(!state.pipe(settlement_pipe(malformed_generation)));
        assert!(state.pipe(settlement_pipe(payload.clone())));

        let mut missing_payload = settlement_pipe(payload);
        missing_payload.payload = None;
        assert!(state.pipe(missing_payload));
        assert!(state.settlement_feed_degraded);
    }

    #[test]
    fn settlement_feed_silence_degrades_exact_truth_until_replay() {
        let mut state = State::default();
        let accepted = settlement_payload(10, (4, 3, 3), (2, 1, 1));
        assert!(state.pipe(settlement_pipe(accepted.clone())));
        assert_eq!(state.settlement_feed_age_ticks, Some(0));

        for _ in 1..SETTLEMENT_FEED_STALE_AFTER_TICKS {
            assert!(!state.age_settlement_feed());
            assert!(!state.settlement_feed_degraded);
        }
        assert!(state.age_settlement_feed());
        assert!(state.settlement_feed_degraded);

        let degraded_rows = session_rail_rows_with_truth(
            &[],
            state.settlement_history.as_ref(),
            state.settlement_feed_degraded,
        );
        assert_eq!(
            degraded_rows
                .iter()
                .filter(|row| row.is_bucket())
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                " f ○  ≥4 · Finalized · t0",
                " x ○  ≥3 · Failed · t0",
                " n ○  ≥3 · Needs attention · t0",
            ]
        );

        // An exact canonical replay is both an integrity confirmation and a
        // heartbeat. It restores exact rendering and renews the lease.
        assert!(state.pipe(settlement_pipe(accepted)));
        assert!(!state.settlement_feed_degraded);
        assert_eq!(state.settlement_feed_age_ticks, Some(0));
    }

    #[test]
    fn settlement_pipe_rejects_stale_decrease_divergence_and_malformed_truth() {
        let mut state = State::default();
        let accepted = settlement_payload(10, (4, 3, 3), (2, 1, 1));
        assert!(state.pipe(settlement_pipe(accepted.clone())));
        let baseline = state.settlement_history.clone();

        // Exact replay is idempotent: no render and no state change.
        assert!(!state.pipe(settlement_pipe(accepted.clone())));
        assert_eq!(state.settlement_history, baseline);
        assert!(!state.settlement_feed_degraded);

        // Same sequence with different valid content is divergence. The last
        // good snapshot remains visible only as a lower bound until replay.
        assert!(state.pipe(settlement_pipe(settlement_payload(
            10,
            (5, 2, 3),
            (3, 1, 1),
        ))));
        assert_eq!(state.settlement_history, baseline);
        assert!(state.settlement_feed_degraded);
        let degraded_rows = session_rail_rows_with_truth(
            &[],
            state.settlement_history.as_ref(),
            state.settlement_feed_degraded,
        );
        assert_eq!(
            degraded_rows
                .iter()
                .filter(|row| row.is_bucket())
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec![
                " f ○  ≥4 · Finalized · t0",
                " x ○  ≥3 · Failed · t0",
                " n ○  ≥3 · Needs attention · t0",
            ]
        );
        assert!(state.pipe(settlement_pipe(accepted.clone())));
        assert!(!state.settlement_feed_degraded);

        // Older snapshots remain stale even if their counters are larger.
        assert!(!state.pipe(settlement_pipe(
            settlement_payload(9, (4, 3, 2), (2, 1, 1),)
        )));
        assert_eq!(state.settlement_history, baseline);
        assert!(!state.settlement_feed_degraded);

        // A new sequence cannot rewrite append-only history downward.
        assert!(state.pipe(settlement_pipe(settlement_payload(
            11,
            (3, 3, 5),
            (2, 1, 1),
        ))));
        assert_eq!(state.settlement_history, baseline);
        assert!(state.settlement_feed_degraded);
        assert!(state.pipe(settlement_pipe(accepted.clone())));
        assert!(!state.settlement_feed_degraded);

        let malformed_total = serde_json::json!({
            "schema": SETTLEMENT_HISTORY_SCHEMA,
            "generation": "00000000-0000-4000-8000-000000000001",
            "sequence": 11,
            "historical_transitions": {"f": 5, "x": 3, "n": 3, "total": 999},
            "latest_by_run": {"f": 2, "x": 1, "n": 1, "total": 4},
            "gaps": 0,
            "complete_from": 1,
        })
        .to_string();
        assert!(state.pipe(settlement_pipe(malformed_total)));
        assert!(state.settlement_feed_degraded);

        let missing_complete_from = serde_json::json!({
            "schema": SETTLEMENT_HISTORY_SCHEMA,
            "generation": "00000000-0000-4000-8000-000000000001",
            "sequence": 11,
            "historical_transitions": {"f": 5, "x": 3, "n": 3, "total": 11},
            "latest_by_run": {"f": 2, "x": 1, "n": 1, "total": 4},
            "gaps": 1,
        })
        .to_string();
        assert!(!state.pipe(settlement_pipe(missing_complete_from)));

        let impossible_latest_bucket = serde_json::json!({
            "schema": SETTLEMENT_HISTORY_SCHEMA,
            "generation": "00000000-0000-4000-8000-000000000001",
            "sequence": 11,
            "historical_transitions": {"f": 0, "x": 8, "n": 3, "total": 11},
            "latest_by_run": {"f": 4, "x": 0, "n": 0, "total": 4},
            "gaps": 0,
            "complete_from": 1,
        })
        .to_string();
        assert!(!state.pipe(settlement_pipe(impossible_latest_bucket)));

        let wrong_schema = serde_json::json!({
            "schema": "vibecrafted.settlement-history.v0",
            "generation": "00000000-0000-4000-8000-000000000001",
            "sequence": 11,
            "historical_transitions": {"f": 5, "x": 3, "n": 3, "total": 11},
            "latest_by_run": {"f": 2, "x": 1, "n": 1, "total": 4},
            "gaps": 0,
            "complete_from": 1,
        })
        .to_string();
        assert!(!state.pipe(settlement_pipe(wrong_schema)));

        let unknown_field = serde_json::json!({
            "schema": SETTLEMENT_HISTORY_SCHEMA,
            "generation": "00000000-0000-4000-8000-000000000001",
            "sequence": 11,
            "historical_transitions": {"f": 5, "x": 3, "n": 3, "total": 11},
            "latest_by_run": {"f": 2, "x": 1, "n": 1, "total": 4},
            "gaps": 0,
            "complete_from": 1,
            "viewer_tabs": 999,
        })
        .to_string();
        assert!(!state.pipe(settlement_pipe(unknown_field)));
        assert_eq!(state.settlement_history, baseline);
        assert!(state.settlement_feed_degraded);

        // A canonical replay proves that the accepted lower bound is current
        // again and restores exact rendering.
        assert!(state.pipe(settlement_pipe(accepted)));
        assert!(!state.settlement_feed_degraded);
    }

    #[test]
    fn needs_attention_viewer_tab_and_finalized_latest_do_not_change_history_count() {
        let snapshot =
            SettlementHistory::parse(&settlement_payload(681, (97, 176, 408), (1, 0, 0)))
                .expect("valid settlement history");
        let rows = session_rail_rows_with_truth(
            &[session("alpha", true), bucket_session("Needs attention", 1)],
            Some(&snapshot),
            false,
        );
        let needs_attention = rows
            .iter()
            .find(|row| {
                matches!(
                    row.kind,
                    SessionRailRowKind::Bucket {
                        bucket: BucketKind::NeedsAttention,
                        ..
                    }
                )
            })
            .expect("needs-attention bucket");

        assert_eq!(
            needs_attention.text, " n ○ 408 · Needs attention · t1",
            "historical n=408 is primary; finalized latest and one viewer tab are not"
        );
    }

    #[test]
    fn viewer_inventory_is_secondary_while_truth_is_unavailable() {
        let rows = session_rail_rows(&[bucket_session("Finalized runs", 2)]);
        let finalized = rows
            .iter()
            .find(|row| {
                matches!(
                    row.kind,
                    SessionRailRowKind::Bucket {
                        bucket: BucketKind::Finalized,
                        ..
                    }
                )
            })
            .expect("finalized bucket");

        assert_eq!(finalized.text, " f ○   ? · Finalized · t2");
        assert!(
            !finalized.text.contains(" f ○   2 "),
            "viewer inventory must never masquerade as settlement truth"
        );
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
    fn bucket_rail_labels_describe_settlement_outcomes() {
        assert_eq!(BucketKind::Finalized.rail_label(), "Finalized");
        assert_eq!(BucketKind::Failed.rail_label(), "Failed");
        assert_eq!(BucketKind::NeedsAttention.rail_label(), "Needs attention");
    }

    #[test]
    fn buckets_are_pinned_below_working_sessions_with_secondary_viewer_inventory() {
        let rows = session_rail_rows(&[
            session("alpha", true),
            bucket_session("Finalized runs", 3),
            session("beta", false),
            bucket_session("Needs attention", 1),
            bucket_session("Failed runs", 2),
        ]);
        let text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();

        // Drawer order is fixed by RAIL_BUCKETS, not by session creation order.
        // Without canonical truth the primary count is unavailable; mutable
        // viewer inventory survives only in the explicit tN suffix.
        assert_eq!(
            text,
            vec![
                "01 ◉ alpha",
                "02 ○ beta",
                " f ○   ? · Finalized · t3",
                " x ○   ? · Failed · t2",
                " n ○   ? · Needs attention · t1",
            ]
        );
    }

    #[test]
    fn secondary_viewer_inventory_ignores_default_layout_chrome_tabs() {
        // A bucket session materialized via `attach --create-background` starts
        // with the default-layout chrome ("Start here" + "Shell"). Those tabs
        // are furniture — the drawer must still read zero runs.
        let mut chrome_only = session("Finalized runs", false);
        chrome_only.tabs = vec![
            TabUiInfo::for_rail_test("Start here", false, "", 0),
            TabUiInfo::for_rail_test("Shell", false, "", 0),
        ];
        let mut chrome_plus_run = session("Needs attention", false);
        chrome_plus_run.tabs = vec![
            TabUiInfo::for_rail_test("Start here", false, "", 0),
            TabUiInfo::for_rail_test("Shell", false, "", 0),
            TabUiInfo::for_rail_test("revi-260723-011839-18000", false, "", 0),
        ];
        let rows = session_rail_rows(&[session("alpha", true), chrome_only, chrome_plus_run]);
        let buckets: Vec<&str> = rows
            .iter()
            .filter(|row| row.is_bucket())
            .map(|row| row.text.as_str())
            .collect();
        assert_eq!(
            buckets,
            vec![
                " f ○   ? · Finalized · t0",
                " x ○   ? · Failed · t0",
                " n ○   ? · Needs attention · t1",
            ]
        );
    }

    #[test]
    fn bucket_rows_show_even_before_their_sessions_exist() {
        let rows = session_rail_rows(&[session("alpha", true)]);
        let buckets: Vec<&SessionRailRow> = rows.iter().filter(|row| row.is_bucket()).collect();

        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].text, " f ○   ? · Finalized · t0");
        assert_eq!(buckets[1].text, " x ○   ? · Failed · t0");
        assert_eq!(buckets[2].text, " n ○   ? · Needs attention · t0");
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
    fn bucket_rail_lines_keep_secondary_viewer_suffix_when_truncated() {
        // Wide enough: untouched, padded like any rail line.
        assert_eq!(
            fit_bucket_rail_line(" n ○ 408 · Needs attention · t12", 34),
            " n ○ 408 · Needs attention · t12  "
        );
        // Narrow: the label loses letters, explicit viewer telemetry survives.
        assert_eq!(
            fit_bucket_rail_line(" n ○ 408 · Needs attention · t12", 24),
            " n ○ 408 · Needs a · t12"
        );
        // Shorter drawers at the same width pad instead of truncating.
        assert_eq!(
            fit_bucket_rail_line(" x ○ 176 · Failed · t2", 24),
            " x ○ 176 · Failed · t2  "
        );
        // Degenerate width: falls back to plain clipping, no suffix games.
        assert_eq!(
            fit_bucket_rail_line(" n ○ 408 · Needs attention · t12", 4),
            " n ○"
        );
        let huge = fit_bucket_rail_line(" f ○ 18446744073709551615 · Finalized · t0", 24);
        assert_eq!(huge.trim_end(), " f ○ …");
        assert!(
            !huge.chars().any(|character| character.is_ascii_digit()),
            "an overflowing count must be omitted, never clipped into a smaller exact number"
        );
        // Non-bucket text is untouched by the suffix rule.
        assert_eq!(fit_bucket_rail_line("01 ◉ alpha", 6), "01 ◉ a");
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
        // Empty drawer: still a bucket target so mouse + f/x/n open the folder
        // (create-with-layout when missing — not a silent dead zone).
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

    #[test]
    fn rail_hover_only_on_clickable_rows_and_clears_elsewhere() {
        let rows = session_rail_rows(&[session("alpha", true)]);
        let mut click_map: BTreeMap<usize, RailClickTarget> = BTreeMap::new();
        for (offset, row) in rows.iter().enumerate() {
            click_map.insert(offset + 1, rail_row_click_target(&row.kind));
        }
        // Data rows (1 = session, 2..=4 = drawers) highlight.
        assert_eq!(rail_hover_target(1, &click_map), Some(1));
        assert_eq!(
            rail_hover_target(2, &click_map),
            Some(2),
            "empty Finalized drawer stays hoverable"
        );
        // Header, blank gap, out-of-bounds, negative leave → clear.
        assert_eq!(rail_hover_target(0, &click_map), None);
        assert_eq!(rail_hover_target(99, &click_map), None);
        assert_eq!(rail_hover_target(-1, &click_map), None);
    }

    #[test]
    fn bucket_and_session_status_share_accent_column_index_3() {
        // Session: "01 ◉ alpha" — status fisheye at display column 3.
        // Bucket:  " f ○  97 · Finalized · t0" — status ring at column 3.
        // The markers are multi-byte, so the accent column is a CHAR index —
        // the same unit color_range(1, 3..4) speaks.
        let session_text = session_rail_rows(&[session("alpha", true)])
            .into_iter()
            .find(|r| matches!(r.kind, SessionRailRowKind::Session(_)))
            .unwrap()
            .text;
        let bucket_text = format_bucket_rail_entry(BucketKind::Finalized, Some(97), true, 0, false);
        assert_eq!(
            session_text.chars().nth(3),
            Some('◉'),
            "session status column"
        );
        assert_eq!(
            bucket_text.chars().nth(3),
            Some('○'),
            "bucket status column (same index as session)"
        );
    }

    #[test]
    fn char_offset_of_speaks_chars_not_bytes() {
        // "   ● tab · cmd" — the dot separator sits after multi-byte `●`.
        let row = "   ● tab · cmd";
        assert_eq!(char_offset_of(row, " · ", 4), Some(8));
        assert_eq!(char_offset_of(row, " · ", 0), Some(8));
        assert_eq!(char_offset_of(row, "missing", 0), None);
        assert_eq!(char_offset_of("ab", "b", 5), None, "from beyond end");
    }
}
