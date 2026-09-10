mod list_navigation;
mod new_session_info;
mod resurrectable_sessions;
mod session_list;
mod single_screen;
mod single_screen_data;
mod single_screen_render;
mod ui;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use uuid::Uuid;
use zellij_tile::prelude::*;

use new_session_info::{NewSessionInfo, execute_switch_session_plan};
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

const VC_CHROME_VISIBILITY_MESSAGE: &str = "vc.status-bar-visibility.v1";
const VC_CHROME_HEARTBEAT_MESSAGE: &str = "vc.fleet-live-count.v1";
// Semantic live-run truth for the dedicated Agent Workspaces canvas:
// Vibecrafted Server `active_runs`, relayed by the vc-frame server's
// session-metadata loop. Never derived from local files, PIDs, or sessions.
const VC_LIVE_RUNS_MESSAGE: &str = "vc.live-runs.v1";
const VC_GUEST_CREATE_REQUEST_KEY: &str = "vc_frame_guest_create_request";
const VC_GUEST_COMMAND_CONTEXT_KEY: &str = "vc_frame_guest_surface";
const VC_FRAME_SELF_EXECUTABLE: &str = "vc-frame:self";

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostHandoff {
    PendingOnSelf,
    CliProject { host: String },
    DetachedNotice,
}
// The producer re-sends at least every five seconds, so three missed windows
// mark the Agent Workspaces projection degraded.
const LIVE_RUNS_FEED_STALE_AFTER_TICKS: u8 = 15;

// Floor for a renderable main-menu frame: anything below is a transient
// startup event, not a legal surface. The menu needs at least a banner row
// plus a content row; kept far below the comfortable chrome minimum
// (tools/repro_chrome.py MIN_COLUMNS) so legal small panes still render.
// The rail path has its own zero-dimension guard and legally lives at
// cols 6-10 — these thresholds must never apply to it.
const MIN_MENU_RENDER_ROWS: usize = 2;
const MIN_MENU_RENDER_COLS: usize = 4;

fn menu_dimensions_are_transient(rows: usize, cols: usize) -> bool {
    rows < MIN_MENU_RENDER_ROWS || cols < MIN_MENU_RENDER_COLS
}

fn should_hide_manager_after_guest_create(frame_host: bool) -> bool {
    !frame_host
}

fn guest_visit_command(session_name: &str, tab_position: Option<usize>) -> CommandToRun {
    let mut args = vec!["visit".to_owned(), session_name.to_owned()];
    if let Some(tab_position) = tab_position {
        args.push("--tab".to_owned());
        args.push(tab_position.saturating_add(1).to_string());
    }
    CommandToRun::new_with_args(VC_FRAME_SELF_EXECUTABLE, args)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
struct AgentRunUiInfo {
    run_id: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    skill: String,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    root: String,
    #[serde(default)]
    repo: String,
    #[serde(default)]
    workspace_title: Option<String>,
    #[serde(default)]
    task_title: Option<String>,
    #[serde(default)]
    plan_title: Option<String>,
    #[serde(default)]
    operator_session: String,
    #[serde(default)]
    health: String,
    #[serde(default)]
    execution_state: String,
    #[serde(default)]
    proof_state: String,
    #[serde(default)]
    delivery_state: String,
    #[serde(default)]
    started_at: String,
}

impl AgentRunUiInfo {
    fn primary_title(&self) -> String {
        if let Some(title) = human_title_field(self.workspace_title.as_deref()) {
            return title;
        }

        let repo = human_title_field(Some(&self.repo)).or_else(|| repository_from_root(&self.root));
        let task = human_title_field(self.task_title.as_deref())
            .or_else(|| human_title_field(self.plan_title.as_deref()))
            .or_else(|| dispatch_task_from_root(&self.root));
        match (repo, task) {
            (Some(repo), Some(task)) if repo != task => format!("{repo} · {task}"),
            (Some(repo), _) => repo,
            (None, Some(task)) => task,
            (None, None) => {
                friendly_path_fallback(&self.root).unwrap_or_else(|| "Agent workspace".to_owned())
            },
        }
    }

    fn status_summary(&self) -> String {
        let agent = nonempty_title(Some(&self.agent)).unwrap_or_else(|| "agent unavailable".into());
        let skill = nonempty_title(Some(&self.skill))
            .or_else(|| nonempty_title(Some(&self.mode)))
            .unwrap_or_else(|| "skill unavailable".into());
        let state = nonempty_title(Some(&self.execution_state))
            .or_else(|| nonempty_title(Some(&self.health)))
            .unwrap_or_else(|| "status unavailable".into());
        format!("{agent} · {skill} · {state}")
    }
}

fn nonempty_title(value: Option<&str>) -> Option<String> {
    value
        .map(sanitize_display_label)
        .filter(|value| !value.is_empty())
}

fn human_title_field(value: Option<&str>) -> Option<String> {
    nonempty_title(value).filter(|value| !looks_like_runtime_identity(value))
}

fn looks_like_runtime_identity(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
        || [
            "impl-", "work-", "audi-", "rese-", "marb-", "pola-", "fwup-",
        ]
        .iter()
        .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
}

fn path_components(path: &str) -> Vec<&str> {
    path.split(['/', '\\'])
        .filter(|component| !component.is_empty())
        .collect()
}

fn is_dispatch_day(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 9
        && bytes[4] == b'_'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || byte.is_ascii_digit())
}

fn humanize_path_component(value: &str) -> Option<String> {
    let value = sanitize_display_label(value);
    if value.is_empty() || Uuid::parse_str(&value).is_ok() {
        return None;
    }
    let humanized = value
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!humanized.is_empty()).then_some(humanized)
}

fn repository_from_root(root: &str) -> Option<String> {
    let components = path_components(root);
    components
        .windows(2)
        .find_map(|window| is_dispatch_day(window[1]).then(|| humanize_path_component(window[0])))
        .flatten()
        .or_else(|| {
            components
                .last()
                .and_then(|value| humanize_path_component(value))
        })
}

fn dispatch_task_from_root(root: &str) -> Option<String> {
    let components = path_components(root);
    components.windows(2).find_map(|window| {
        is_dispatch_day(window[0])
            .then(|| humanize_path_component(window[1]))
            .flatten()
    })
}

fn friendly_path_fallback(root: &str) -> Option<String> {
    path_components(root)
        .into_iter()
        .rev()
        .find_map(humanize_path_component)
}

fn agent_workspace_lines(runs: Option<&[AgentRunUiInfo]>, degraded: bool) -> Vec<String> {
    let mut lines = vec!["AGENT WORKSPACES".to_owned()];
    if degraded {
        lines.push("DEGRADED · showing last accepted server projection".to_owned());
    } else {
        lines.push("LIVE · Vibecrafted Server projection".to_owned());
    }
    lines.push(String::new());
    match runs {
        None => lines.push("UNAVAILABLE · waiting for canonical workspace data".to_owned()),
        Some([]) => lines.push("EMPTY · no active agent runs".to_owned()),
        Some(runs) => {
            for run in runs {
                lines.push(format!("● {}", run.primary_title()));
                lines.push(format!("  {}", run.status_summary()));
                lines.push(format!("  run {}", sanitize_display_label(&run.run_id)));
                lines.push(String::new());
            }
        },
    }
    lines
}

fn project_canonical_session_titles(
    runs: Option<&[AgentRunUiInfo]>,
    sessions: &mut [SessionUiInfo],
) {
    let Some(runs) = runs else {
        return;
    };
    for session in sessions {
        if let Some(run) = runs
            .iter()
            .find(|run| !run.operator_session.is_empty() && run.operator_session == session.name)
        {
            session.title = run.primary_title();
        }
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
    // Full-canvas Agent Workspaces view. It consumes the same server-owned
    // projection as the rail and never scans runtime directories or invents
    // mock workers.
    workspace_dashboard: bool,
    agent_runs: Option<Vec<AgentRunUiInfo>>,
    // A frame host owns the chrome once and projects other sessions into one
    // replaceable terminal pane. Guest servers keep their PTYs; this plugin
    // only swaps the interactive visitor process.
    frame_host: bool,
    workspace_surface: bool,
    visited_guest_name: Option<String>,
    pending_guest_visit: Option<PendingGuestRequest>,
    // A create result must acknowledge this generation before it can become
    // a projection. SessionUpdate is discovery, never a create receipt.
    pending_guest_create: Option<(String, PendingGuestRequest)>,
    host_session_name: Option<String>,
    current_session_is_host: bool,
    own_plugin_id: Option<u32>,
    // screen row -> click target, rebuilt on every rail render so mouse
    // clicks resolve against exactly what is on screen (incl. scroll window).
    // Header / footer / blank gap rows are absent → click is a no-op.
    rail_click_map: BTreeMap<usize, RailClickTarget>,
    // Hovered rail row (plugin-relative line).
    rail_hover_row: Option<usize>,
    // Vibecrafted Server Live census (`vc.live-runs.v1`) feeds the dedicated
    // Agent Workspaces canvas, never the session rail.
    live_runs_feed_degraded: bool,
    live_runs_feed_age_ticks: Option<u8>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.own_plugin_id = Some(get_plugin_ids().plugin_id);
        self.workspace_surface =
            configuration.get("workspace_surface").map(String::as_str) == Some("true");
        if self.workspace_surface {
            return;
        }
        self.is_rail = configuration
            .get("rail")
            .map(|v| v == "true")
            .unwrap_or(false);
        self.workspace_dashboard = configuration
            .get("workspace_dashboard")
            .map(|value| value == "true")
            .unwrap_or(false);
        self.frame_host = self.is_rail
            && configuration
                .get("frame_host")
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
        // Ordinary rails start parked. The host rail must stay awake so
        // launcher `project-workspace` pipes and guest pane discovery work
        // without a chrome-heartbeat that no isolated client sends.
        self.is_visible = !self.is_rail || self.frame_host;
        let mut subscriptions = vec![
            EventType::ModeUpdate,
            EventType::Key,
            EventType::Mouse,
            EventType::RunCommandResult,
            EventType::Timer,
            EventType::Visible,
            EventType::CustomMessage,
        ];
        if self.frame_host {
            subscriptions.push(EventType::PaneUpdate);
        }
        if self.is_visible {
            subscriptions.push(EventType::SessionUpdate);
        }
        subscribe(&subscriptions);
        let pane_title = if self.workspace_dashboard {
            configuration
                .get("pane_title")
                .cloned()
                .unwrap_or_else(|| "Agent Workspaces".to_owned())
        } else if self.is_rail {
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
        if self.is_visible {
            self.refresh_session_list();
        }
        if self.is_visible && !self.is_welcome_screen {
            self.arm_refresh_timer();
        }
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if self.workspace_surface {
            return false;
        }
        if self.frame_host
            && pipe_message.name == VC_GUEST_SURFACE_MESSAGE
            && let PipeSource::Cli(ref pipe_id) = pipe_message.source
            && let (Some(request_id), Some(GuestSurfaceRequest::Project { session, tab })) = (
                pipe_message.args.get("request_id"),
                pipe_message
                    .payload
                    .as_deref()
                    .and_then(parse_guest_surface_payload),
            )
        {
            let ids = get_plugin_ids();
            block_cli_pipe_input(pipe_id);
            let pane_id = self.activate_session_request(
                &session,
                tab,
                request_id,
                Some(pipe_id),
                pipe_message.args.get("pipe_client_id").map(String::as_str),
            );
            if pane_id.is_some() {
                // Screen owns the final acknowledgment after the visitor
                // receives guest output. Keep this exact pipe pending.
                return true;
            }
            let receipt = WorkspaceProjectionReceipt {
                request_id: request_id.clone(),
                client_id: ids.client_id,
                plugin_id: ids.plugin_id,
                guest: session,
                tab,
                pane_id,
                status: if pane_id.is_some() {
                    ProjectionStatus::Handled
                } else {
                    ProjectionStatus::Unavailable
                },
                detail: self.error.clone().unwrap_or_default(),
            };
            cli_pipe_output(pipe_id, &(serde_json::to_string(&receipt).unwrap() + "\n"));
            unblock_cli_pipe_input(pipe_id);
            return true;
        }
        if pipe_message.name == "vc_rail_nav" {
            match pipe_message.payload.as_deref() {
                Some("up") => self.switch_session_relative(-1),
                Some("down") => self.switch_session_relative(1),
                _ => (),
            }
            true
        } else if pipe_message.name == "vc_kill_current_session" {
            self.kill_current_session_preserving_client();
            true
        } else if pipe_message.name == VC_GUEST_SURFACE_MESSAGE {
            pipe_message
                .payload
                .as_deref()
                .map(|payload| self.handle_guest_surface_message(payload))
                .unwrap_or(false)
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
        if self.workspace_surface {
            return false;
        }
        let mut should_render = false;
        match event {
            Event::Timer(_) => {
                self.refresh_timer_armed = false;
                if !self.is_visible {
                    return false;
                }
                if self.age_live_runs_feed() {
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
                if self.is_rail {
                    return false;
                }
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
            Event::CustomMessage(message, payload)
                if self.is_rail && message == VC_CHROME_VISIBILITY_MESSAGE =>
            {
                match payload.as_str() {
                    "true" => {
                        let was_visible = self.is_visible;
                        self.is_visible = true;
                        if !was_visible {
                            subscribe(&[EventType::SessionUpdate]);
                            if self.refresh_session_list() {
                                should_render = true;
                            }
                            self.arm_refresh_timer();
                        }
                    },
                    "false" => {
                        self.is_visible = false;
                        unsubscribe(&[EventType::SessionUpdate]);
                    },
                    _ => {},
                }
            },
            Event::CustomMessage(message, _payload)
                if self.is_rail && message == VC_CHROME_HEARTBEAT_MESSAGE =>
            {
                let was_visible = self.is_visible;
                self.is_visible = true;
                if !was_visible {
                    subscribe(&[EventType::SessionUpdate]);
                    if self.refresh_session_list() {
                        should_render = true;
                    }
                    self.arm_refresh_timer();
                }
            },
            Event::CustomMessage(message, payload)
                if (self.is_rail || self.workspace_dashboard)
                    && message == VC_LIVE_RUNS_MESSAGE =>
            {
                should_render = self.apply_live_runs_payload(&payload);
            },
            Event::CustomMessage(message, payload) if message == VC_GUEST_SURFACE_MESSAGE => {
                should_render = self.handle_guest_surface_message(&payload);
            },
            Event::RunCommandResult(exit_code, stdout, stderr, context)
                if context.contains_key(VC_GUEST_CREATE_CONTEXT_KEY) =>
            {
                should_render = self.handle_guest_create_result(
                    exit_code,
                    &stdout,
                    &stderr,
                    context.get(VC_GUEST_CREATE_CONTEXT_KEY).map(String::as_str),
                    context.get(VC_GUEST_CREATE_REQUEST_KEY).map(String::as_str),
                );
            },
            // The synchronous open response owns the replacement pane ID.
            // Delayed CommandPaneOpened/Exited events from prior visits must
            // never overwrite it (including held panes after visitor exit).
            Event::PaneUpdate(_) if self.frame_host => {
                self.try_visit_pending_guest();
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
                let session_display_changed = self.update_session_infos(session_infos);
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
                should_render = !self.is_rail || session_display_changed;
            },
            _ => (),
        };
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if self.workspace_surface {
            print!("Select a workspace from Sessions.");
            return;
        }
        if self.workspace_dashboard {
            self.render_agent_workspaces(rows, cols);
            return;
        }
        if self.is_rail {
            self.render_session_rail(rows, cols);
            if let Some(error) = self.error.as_ref() {
                render_error(error, rows, cols, 0, 0);
            }
            return;
        }

        // Transient initial resize events arrive with rows/cols at or near
        // zero before the real layout lands; painting the main menu on those
        // frames is what makes the view visibly jump at startup. The rail
        // branch above keeps its own long-standing zero-dimension guard.
        if menu_dimensions_are_transient(rows, cols) {
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

/// The rail reads its allocated width and picks one of three faces
/// (audit topology 2026-08-05). Sharp thresholds, no hysteresis — the
/// operator resizes the panel, the runtime never does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RailWidthMode {
    /// Full render: `SESSIONS N · name` header, full names, counters.
    /// The default operator layout allocates `size=24`, so Wide is the
    /// byte-for-byte baseline.
    Wide,
    /// Header drops the current-session anchor, names truncate through
    /// `fit_rail_line`, the ◉/○ indicators stay.
    Normal,
    /// Iconic strip: ordinal + state indicator only, ultra-short header,
    /// no long text — built at row level, never by truncating prose into
    /// mincemeat.
    Dense,
}

/// Wide is today's baseline: the default operator layout gives the rail 24
/// columns (see compact-bar/src/line.rs rail note).
const RAIL_WIDE_MIN_COLS: usize = 24;
/// Below this the header/name mix stops being readable and the rail switches
/// to the iconic strip.
const RAIL_NORMAL_MIN_COLS: usize = 14;

impl RailWidthMode {
    fn from_cols(cols: usize) -> Self {
        if cols >= RAIL_WIDE_MIN_COLS {
            RailWidthMode::Wide
        } else if cols >= RAIL_NORMAL_MIN_COLS {
            RailWidthMode::Normal
        } else {
            RailWidthMode::Dense
        }
    }
}

/// Header text per width mode. Wide anchors "you are here" with the current
/// session name; Normal keeps the count only; Dense is an ultra-short badge.
fn rail_header_text(
    mode: RailWidthMode,
    session_count: usize,
    current_session_name: Option<&str>,
) -> String {
    match mode {
        RailWidthMode::Wide => {
            let mut text = format!("SESSIONS {}", session_count);
            if let Some(name) = current_session_name {
                text.push_str(&format!(" · {}", sanitize_display_label(name)));
            }
            text
        },
        RailWidthMode::Normal => format!("SESSIONS {}", session_count),
        RailWidthMode::Dense => format!("S{}", session_count),
    }
}

fn format_session_rail_entry(
    session: &SessionUiInfo,
    ordinal: usize,
    mode: RailWidthMode,
) -> String {
    // The fisheye is the one "you are here" glyph across the whole chrome —
    // the same ◉/○ pair the tab chips carry.
    let status = if session.is_current_session {
        "◉"
    } else {
        "○"
    };
    if mode == RailWidthMode::Dense {
        // Iconic row: ordinal keeps the 1..9/0 hotkey affordance visible,
        // the fisheye carries the state — no name to shred.
        return format!("{:02} {}", ordinal, status);
    }
    // Sanitize the name *before* pad/fit so control bytes and multi-space
    // runs cannot change display width frame-to-frame (rail flicker).
    format!(
        "{:02} {} {}",
        ordinal,
        status,
        sanitize_display_label(&session.title)
    )
}

/// Strip control/line-break characters and collapse internal whitespace so a
/// rail/help row paints at a stable grid width every tick.
fn sanitize_display_label(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut prev_space = false;
    for ch in input.chars() {
        // Newlines/tabs/other whitespace → single space (never a hard break
        // that would reflow the one-row rail cell). Other controls vanish.
        let as_space = ch.is_whitespace()
            || ch == '\n'
            || ch == '\r'
            || ch == '\t'
            || ch == '\u{2028}'
            || ch == '\u{2029}';
        if as_space {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        if ch.is_control() {
            continue;
        }
        prev_space = false;
        out.push(ch);
    }
    // Trim trailing space from the collapse pass.
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Kill/delete contract for laptops (macOS): ⌥⌫ is primary. Forward-delete
/// (Del) remains accepted for full keyboards but is never advertised in help
/// — it is scarce on MacBooks and steals sequences under some hosts.
fn is_kill_or_delete_key(key: &KeyWithModifier) -> bool {
    (key.bare_key == BareKey::Backspace && key.has_only_modifiers(&[KeyModifier::Alt]))
        || (key.bare_key == BareKey::Delete && key.has_no_modifiers())
}

/// Char offset of `needle` in `haystack`, searching from char offset `from`.
/// Text::color_range speaks char offsets, while `str::find` returns bytes —
/// this bridges the two for rows containing multi-byte glyphs (`◉`, `·`).
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionRailRowKind {
    Session(usize),
    LiveProcess {
        session_index: usize,
        /// 0-based tab position — handed straight to `switch_session_with_focus`
        /// / `go_to_tab` (both expect 0-based and bump internally).
        tab_position: usize,
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
}

fn format_process_tab_rail_entry(tab: &TabUiInfo, mode: RailWidthMode) -> String {
    let activity = if tab.is_active { "◉" } else { "·" };
    if mode == RailWidthMode::Dense {
        // Iconic strip: the indented activity dot alone carries the state —
        // truncating "name · command +N" into mincemeat is not an option.
        return format!("   {}", activity);
    }
    let tab_name = sanitize_display_label(&tab.name);
    let mut text = format!("   {} {}", activity, tab_name);
    if let Some(process_label) = tab.primary_process_label() {
        let process_label = stable_process_label(process_label);
        if process_label != tab_name && !process_label.contains(&tab_name) {
            text.push_str(" · ");
            text.push_str(&process_label);
        }
    }
    let additional_processes = tab.live_process_count().saturating_sub(1);
    if additional_processes > 0 {
        text.push_str(&format!(" +{}", additional_processes));
    }
    text
}

/// Remove a leading braille animation frame while preserving the meaningful
/// process status that follows it. Terminal-local progress stays visible, but
/// spinner-only title churn cannot repaint the whole session rail.
fn stable_process_label(input: &str) -> String {
    let sanitized = sanitize_display_label(input);
    let mut chars = sanitized.chars();
    let Some(first) = chars.next() else {
        return sanitized;
    };
    let remainder = chars.as_str().trim_start();
    if ('\u{2800}'..='\u{28ff}').contains(&first) && !remainder.is_empty() {
        remainder.to_owned()
    } else {
        sanitized
    }
}

// Direct rail navigation (vc_rail_nav pipe): product contract v3 is
// Cmd/Super+Up/Down in every mode (Ctrl+Up/Down mirrors it outside LOCK);
// tab-mode Up/Down and bare arrows on a focused rail also hit this path. The target
// is resolved relative to the *current* session with wrap-around, so every
// rail instance receiving the broadcast computes the same destination and
// the switch stays idempotent. Navigation walks the *working* sessions in
// rail order only.
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
/// session in rail order first, then any other live session. Only a true
/// "nothing else is alive" returns `None`.
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

/// Every live session in rail order. Historical synthetic drawer names are
/// intentionally ordinary sessions: an upgrade must not hide or destroy
/// same-name user work.
fn working_session_indices(sessions: &[SessionUiInfo]) -> Vec<usize> {
    sessions
        .iter()
        .enumerate()
        .map(|(index, _)| index)
        .collect()
}

/// Resolve an ordinal keypress against the visible sessions.
fn rail_ordinal_target(sessions: &[SessionUiInfo], character: char) -> Option<usize> {
    let ordinal = rail_ordinal_key_to_index(character)?;
    working_session_indices(sessions).get(ordinal).copied()
}

fn session_rail_rows_with_truth(
    sessions: &[SessionUiInfo],
    mode: RailWidthMode,
) -> Vec<SessionRailRow> {
    session_rail_session_rows(sessions, mode)
}

/// Stable rail projection used to suppress redraws when only terminal
/// animation frames changed.
fn session_rail_session_rows(
    sessions: &[SessionUiInfo],
    mode: RailWidthMode,
) -> Vec<SessionRailRow> {
    let mut rows = vec![];
    for (ordinal, session_index) in working_session_indices(sessions).into_iter().enumerate() {
        let session = &sessions[session_index];
        rows.push(SessionRailRow {
            kind: SessionRailRowKind::Session(session_index),
            text: format_session_rail_entry(session, ordinal + 1, mode),
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
                    text: format_process_tab_rail_entry(tab, mode),
                }),
        );
    }
    rows
}

#[cfg(test)]
fn session_rail_rows(sessions: &[SessionUiInfo]) -> Vec<SessionRailRow> {
    session_rail_rows_with_truth(sessions, RailWidthMode::Wide)
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
    /// Ingest the server-owned `vc.live-runs.v1` projection for the dedicated
    /// Agent Workspaces canvas. The session rail remains run-agnostic.
    fn apply_live_runs_payload(&mut self, payload: &str) -> bool {
        #[derive(Deserialize)]
        struct LiveRunsFeed {
            schema: String,
            runs: Vec<AgentRunUiInfo>,
        }
        let previous = (self.live_runs_feed_degraded, self.agent_runs.clone());
        let parsed: Option<LiveRunsFeed> = serde_json::from_str(payload)
            .ok()
            .filter(|feed: &LiveRunsFeed| feed.schema == VC_LIVE_RUNS_MESSAGE);
        let Some(feed) = parsed else {
            // Preserve the last accepted server projection, but mark it stale.
            return self.mark_live_runs_feed_degraded();
        };
        let mut runs = feed.runs;
        runs.sort_by(|left, right| {
            left.started_at
                .cmp(&right.started_at)
                .then_with(|| left.run_id.cmp(&right.run_id))
        });
        self.agent_runs = Some(runs);
        project_canonical_session_titles(
            self.agent_runs.as_deref(),
            &mut self.sessions.session_ui_infos,
        );
        project_canonical_session_titles(
            self.agent_runs.as_deref(),
            &mut self.sessions.forbidden_sessions,
        );
        self.live_runs_feed_degraded = false;
        self.live_runs_feed_age_ticks = Some(0);
        (self.live_runs_feed_degraded, self.agent_runs.clone()) != previous
    }

    fn mark_live_runs_feed_degraded(&mut self) -> bool {
        self.agent_runs.is_some() && !std::mem::replace(&mut self.live_runs_feed_degraded, true)
    }

    fn age_live_runs_feed(&mut self) -> bool {
        let Some(age_ticks) = self.live_runs_feed_age_ticks.as_mut() else {
            return false;
        };
        *age_ticks = age_ticks.saturating_add(1);
        if *age_ticks < LIVE_RUNS_FEED_STALE_AFTER_TICKS {
            return false;
        }
        self.mark_live_runs_feed_degraded()
    }

    fn render_agent_workspaces(&self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        for (row, line) in
            agent_workspace_lines(self.agent_runs.as_deref(), self.live_runs_feed_degraded)
                .into_iter()
                .take(rows)
                .enumerate()
        {
            let text = fit_rail_line(&line, cols);
            print_text_with_coordinates(Text::new(text), 0, row, None, None);
        }
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
        let mode = RailWidthMode::from_cols(cols);

        let rail_rows = session_rail_rows_with_truth(&self.sessions.session_ui_infos, mode);

        // The LIVE number lives in the bottom status-bar's fleet chip; the
        // header keeps the session count and the current-session anchor — the
        // top bar carries brand   mode   tabs only, so "where am I" lives
        // here, right above the session list.
        let session_count = working_session_indices(&self.sessions.session_ui_infos).len();
        let current_session_name = self
            .sessions
            .session_ui_infos
            .iter()
            .find(|s| s.is_current_session)
            .map(|s| s.title.as_str());
        // Anchor offset only exists in Wide — the other modes drop the name.
        let anchor_start = format!("SESSIONS {}", session_count).width() + 3; // " · "
        let header_text = rail_header_text(mode, session_count, current_session_name);
        let header = fit_rail_line(&header_text, cols);
        let header_width = header.width();
        let header_chars = header.chars().count();
        let mut header = Text::new(header);
        match mode {
            RailWidthMode::Wide | RailWidthMode::Normal => {
                if cols >= 8 {
                    header = header.color_range(1, 0..8);
                }
                if mode == RailWidthMode::Wide && header_width > anchor_start {
                    // The anchor takes the same accent as SESSIONS — one hue
                    // for "you are here", per the THEMES_GUIDE doctrine.
                    header = header.color_range(1, anchor_start..);
                }
            },
            RailWidthMode::Dense => {
                // The whole badge is the accent; the range never exceeds the
                // fitted text (trailing pad spaces carry no visible ink).
                header = header.color_range(1, 0..header_chars);
            },
        }
        print_text_with_coordinates(header, 0, 0, None, None);
        self.rail_click_map.clear();

        let chrome_rows = 1;
        let list_rows = rows.saturating_sub(chrome_rows);
        if list_rows == 0 {
            return;
        }
        let footer_rows = usize::from(rail_rows.len() > list_rows && list_rows > 1);
        let entry_rows = list_rows.saturating_sub(footer_rows);
        let selected_index = self.sessions.selected_index.0;
        let selected_row_index = selected_index.and_then(|selected_session_index| {
            rail_rows
                .iter()
                .position(|row| row.kind == SessionRailRowKind::Session(selected_session_index))
        });
        let (start, end) = rail_range_to_render(entry_rows, rail_rows.len(), selected_row_index);
        let mut row = chrome_rows;

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
                            if mode == RailWidthMode::Dense {
                                // No name in the iconic strip — the accent
                                // lands on the fisheye itself.
                                text = text.color_range(1, 3..4);
                            } else if fitted_chars > 5 {
                                // "You are here" carries the accent on the
                                // NAME, the same ink as the header — one
                                // glance finds it.
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
                    let is_active_tab_row = fitted.chars().nth(3) == Some('◉');
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
                        if in_current_session && is_active_tab_row && mode != RailWidthMode::Dense {
                            // Strongest level of the three: on the highlight
                            // bed, the tab you are actually in carries the
                            // accent on its whole name. (Dense has no name —
                            // the dot already took the accent above.)
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
        }
    }
    fn handle_session_rail_key(&mut self, key: KeyWithModifier) -> bool {
        // Bare arrows are deliberately NOT handled here. Session switching by
        // arrow lives only in the ^T tab-mode keybinds (vc_rail_nav pipe) and
        // the always-on Super chords; a focused rail consuming raw arrows made
        // LOCK mode switch sessions, since LOCK routes keys to the focused pane.
        match key.bare_key {
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
                // Header and footer are absent from the map: quiet no-op.
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
                            self.activate_session(&session_name, Some(tab_position));
                            self.reset_selected_index();
                        }
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
    fn switch_session_relative(&mut self, offset: isize) {
        if let Some(target_session_name) =
            relative_session_target(&self.sessions.session_ui_infos, offset)
        {
            self.activate_session(&target_session_name, None);
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
            if self.visited_guest_name.as_deref() == Some(selected_session_name.as_str())
                || (!self.frame_host && self.sessions.selected_is_current_session())
            {
                // Already here — keep the session switch idempotent.
            } else {
                self.activate_session(&selected_session_name, None);
                self.reset_selected_index();
            }
        }
    }

    fn activate_session(&mut self, session_name: &str, tab_position: Option<usize>) {
        self.pending_guest_create = None;
        if !self.frame_host {
            switch_session_with_focus(session_name, tab_position, None);
            return;
        }
        self.activate_session_request(
            session_name,
            tab_position,
            &Uuid::new_v4().to_string(),
            None,
            None,
        );
    }

    fn activate_session_request(
        &mut self,
        session_name: &str,
        tab_position: Option<usize>,
        request_id: &str,
        pipe_id: Option<&str>,
        pipe_client: Option<&str>,
    ) -> Option<u32> {
        self.pending_guest_create = None;
        self.pending_guest_visit = None;
        let mut context = BTreeMap::from([
            (
                VC_GUEST_COMMAND_CONTEXT_KEY.to_owned(),
                session_name.to_owned(),
            ),
            ("vc_workspace_request".to_owned(), request_id.to_owned()),
            ("vc_workspace_guest".to_owned(), session_name.to_owned()),
            (
                "vc_workspace_tab".to_owned(),
                tab_position.map(|tab| tab.to_string()).unwrap_or_default(),
            ),
        ]);
        if let Some(pipe_id) = pipe_id {
            context.insert("vc_workspace_pipe".to_owned(), pipe_id.to_owned());
        }
        if let Some(pipe_client) = pipe_client {
            context.insert("vc_workspace_pipe_client".to_owned(), pipe_client.to_owned());
        }
        // The Screen owner resolves the registered surface and reserves this exact
        // generation. This placeholder argument is never pane authority.
        match open_command_pane_in_place_of_pane_id(
            PaneId::Terminal(0),
            guest_visit_command(session_name, tab_position),
            true,
            context,
        ) {
            Some(PaneId::Terminal(new_pane_id)) => {
                self.visited_guest_name = Some(session_name.to_owned());
                self.error = None;
                Some(new_pane_id)
            },
            _ => {
                self.show_error(
                    "Projection refused or unavailable: owner did not commit this request.",
                );
                None
            },
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
                _ if is_kill_or_delete_key(&key) => {
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
            _ if is_kill_or_delete_key(&key) => {
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
            _ if is_kill_or_delete_key(&key) => {
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
                let existing = self.live_workspace_names();
                if let Some(plan) = self
                    .new_session_info
                    .handle_selection(&self.session_name, &existing)
                {
                    self.apply_new_workspace_plan(plan);
                }
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
                        let existing = self.live_workspace_names();
                        self.apply_new_workspace_plan(plan_new_workspace(
                            self.is_welcome_screen,
                            self.session_name.as_deref(),
                            new_session_name,
                            layout,
                            cwd,
                            &existing,
                        ));
                        self.single_screen_state.search_term.clear();
                        self.single_screen_state.transition_to_search();
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
        let session_display_changed = self.update_session_infos(snapshot.live_sessions);
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
        !self.is_rail || session_display_changed
    }

    fn live_workspace_names(&self) -> Vec<String> {
        self.sessions
            .session_ui_infos
            .iter()
            .map(|session| session.name.clone())
            .chain(self.session_name.clone())
            .collect()
    }

    fn apply_new_workspace_plan(&mut self, plan: NewWorkspacePlan) {
        match plan {
            NewWorkspacePlan::SwitchSession { .. } => execute_switch_session_plan(plan),
            NewWorkspacePlan::CreateGuestWorkspace { name, layout, cwd } => {
                self.spawn_guest_workspace(name, layout, cwd);
            },
            NewWorkspacePlan::RefuseDuplicate { .. } => {
                if let Some(message) = plan.duplicate_message() {
                    self.show_error(&message);
                }
            },
        }
    }

    fn spawn_guest_workspace(&mut self, name: String, layout: LayoutInfo, cwd: Option<PathBuf>) {
        let argv = guest_create_argv(&name, &layout);
        let args: Vec<&str> = argv.iter().map(String::as_str).collect();
        let mut context = BTreeMap::new();
        context.insert(VC_GUEST_CREATE_CONTEXT_KEY.to_owned(), name.clone());
        let request_id = Uuid::new_v4().to_string();
        context.insert(VC_GUEST_CREATE_REQUEST_KEY.to_owned(), request_id.clone());
        self.pending_guest_visit = None;
        self.pending_guest_create = Some((
            request_id,
            PendingGuestRequest {
                session: name,
                tab: None,
            },
        ));
        if let Some(cwd) = cwd {
            run_command_with_env_variables_and_cwd(&args, BTreeMap::new(), cwd, context);
        } else {
            run_command(&args, context);
        }
    }

    fn plan_host_handoff(&self) -> HostHandoff {
        if self.frame_host {
            HostHandoff::PendingOnSelf
        } else if self.current_session_is_host {
            match self.session_name.clone() {
                Some(host) => HostHandoff::CliProject { host },
                None => HostHandoff::DetachedNotice,
            }
        } else {
            HostHandoff::DetachedNotice
        }
    }

    fn apply_host_handoff(&mut self, guest: &str, tab: Option<usize>) {
        match self.plan_host_handoff() {
            HostHandoff::PendingOnSelf => {
                self.pending_guest_visit = Some(PendingGuestRequest {
                    session: guest.to_owned(),
                    tab,
                });
            },
            HostHandoff::CliProject { host } => {
                let argv = project_workspace_argv(&host, guest, tab);
                let args: Vec<&str> = argv.iter().map(String::as_str).collect();
                run_command(&args, BTreeMap::new());
                self.pending_guest_visit = None;
            },
            HostHandoff::DetachedNotice => {
                self.show_error(&format!(
                    "Created workspace `{guest}` as a detached guest. Project it into a running host with:\n  vc-frame --session <host> project-workspace {guest}"
                ));
            },
        }
    }

    fn maybe_visit_pending_guest(&mut self, session_infos: &[SessionInfo]) {
        let Some(pending) = self.pending_guest_visit.clone() else {
            return;
        };
        if !session_infos
            .iter()
            .any(|session| session.name == pending.session)
        {
            return;
        }
        if !self.frame_host {
            return;
        }
        self.try_visit_pending_guest();
    }

    fn try_visit_pending_guest(&mut self) {
        if !self.frame_host {
            return;
        }
        let Some(pending) = self.pending_guest_visit.clone() else {
            return;
        };
        #[cfg(target_family = "wasm")]
        self.activate_session(&pending.session, pending.tab);
        #[cfg(not(target_family = "wasm"))]
        let _ = pending;
    }

    fn handle_guest_create_result(
        &mut self,
        exit_code: Option<i32>,
        stdout: &[u8],
        stderr: &[u8],
        created_name: Option<&str>,
        request_id: Option<&str>,
    ) -> bool {
        let Some((expected_id, pending)) = self.pending_guest_create.as_ref() else {
            return false;
        };
        if request_id != Some(expected_id.as_str())
            || created_name != Some(pending.session.as_str())
        {
            return false;
        }
        let (_, pending) = self.pending_guest_create.take().unwrap();
        let failed = exit_code.is_none_or(|code| code != 0);
        if !failed {
            self.apply_host_handoff(&pending.session, pending.tab);
            if should_hide_manager_after_guest_create(self.frame_host) {
                hide_self();
            }
            return true;
        }
        let detail = [stderr, stdout]
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
            .find(|text| !text.is_empty())
            .unwrap_or_else(|| format!("exit {}", exit_code.unwrap_or(-1)));
        let workspace = created_name.unwrap_or("workspace");
        self.show_error(&format!(
            "Failed to create workspace `{workspace}`: {detail}"
        ));
        show_self(true);
        true
    }

    fn publish_guest_surface(&self, session_infos: &[SessionInfo]) {
        if !self.frame_host {
            return;
        }
        let Some(guest_name) = self.visited_guest_name.as_deref() else {
            return;
        };
        let Some(guest) = session_infos
            .iter()
            .find(|session| session.name == guest_name)
        else {
            return;
        };
        let payload = serde_json::json!({
            "session": guest.name,
            "status": guest.name,
            "host_plugin_id": self.own_plugin_id,
            "tabs": guest.tabs.iter().map(|tab| {
                serde_json::json!({
                    "name": tab.name,
                    "active": tab.active,
                    "position": tab.position,
                })
            }).collect::<Vec<_>>(),
        });
        let encoded = payload.to_string();
        #[cfg(target_family = "wasm")]
        pipe_message_to_plugin(
            MessageToPlugin::new(VC_GUEST_SURFACE_MESSAGE)
                .with_plugin_url(VC_COMPACT_BAR_PLUGIN_ALIAS)
                .with_payload(encoded),
        );
        #[cfg(not(target_family = "wasm"))]
        let _ = encoded;
    }

    fn handle_guest_surface_message(&mut self, payload: &str) -> bool {
        let Some(request) = parse_guest_surface_payload(payload) else {
            return false;
        };
        let (session, tab) = match request {
            GuestSurfaceRequest::Project { session, tab } => (session, tab),
            GuestSurfaceRequest::ActivateTab { session, tab } => (session, Some(tab)),
            GuestSurfaceRequest::Surface { .. } => return false,
        };
        if !host_owns_guest_surface_routing(self.frame_host) {
            return false;
        }
        self.pending_guest_create = None;
        self.pending_guest_visit = Some(PendingGuestRequest {
            session: session.clone(),
            tab,
        });
        self.try_visit_pending_guest();
        true
    }

    fn update_session_infos(&mut self, session_infos: Vec<SessionInfo>) -> bool {
        let previous_rail_projection = self.is_rail.then(|| {
            session_rail_rows_with_truth(&self.sessions.session_ui_infos, RailWidthMode::Wide)
        });
        let current_hosts: Vec<&SessionInfo> = session_infos
            .iter()
            .filter(|session| session.is_current_session && is_internal_host_session(session))
            .collect();
        self.host_session_name = match current_hosts.as_slice() {
            [host] => Some(host.name.clone()),
            _ => None,
        };
        self.current_session_is_host = session_infos
            .iter()
            .any(|session| session.is_current_session && is_internal_host_session(session));
        self.maybe_visit_pending_guest(&session_infos);
        self.publish_guest_surface(&session_infos);
        let mut session_ui_infos: Vec<SessionUiInfo> = session_infos
            .iter()
            .filter_map(|s| {
                if is_internal_host_session(s) || (self.is_web_client && !s.web_clients_allowed) {
                    None
                } else if self.is_welcome_screen && s.is_current_session {
                    // do not display current session if we're the welcome screen
                    // because:
                    // 1. attaching to the welcome screen from the welcome screen is not a thing
                    // 2. it can cause issues on the web (since we're disconnecting and
                    //    reconnecting to a session we just closed by disconnecting...)
                    None
                } else {
                    let mut ui = SessionUiInfo::from_session_info(s);
                    if self.frame_host {
                        ui.is_current_session =
                            self.visited_guest_name.as_deref() == Some(ui.name.as_str());
                    }
                    Some(ui)
                }
            })
            .collect();
        let mut forbidden_sessions: Vec<SessionUiInfo> = session_infos
            .iter()
            .filter_map(|s| {
                if self.is_web_client && !s.web_clients_allowed {
                    Some(SessionUiInfo::from_session_info(s))
                } else {
                    None
                }
            })
            .collect();
        project_canonical_session_titles(self.agent_runs.as_deref(), &mut session_ui_infos);
        project_canonical_session_titles(self.agent_runs.as_deref(), &mut forbidden_sessions);
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
        previous_rail_projection.is_none_or(|previous| {
            previous
                != session_rail_rows_with_truth(
                    &self.sessions.session_ui_infos,
                    RailWidthMode::Wide,
                )
        })
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

    #[test]
    fn transient_menu_dimensions_are_guarded() {
        assert!(menu_dimensions_are_transient(0, 80));
        assert!(menu_dimensions_are_transient(1, 80));
        assert!(menu_dimensions_are_transient(2, 3));
        assert!(menu_dimensions_are_transient(0, 0));
    }

    #[test]
    fn legal_menu_dimensions_are_not_transient() {
        assert!(!menu_dimensions_are_transient(2, 4));
        assert!(!menu_dimensions_are_transient(24, 80));
        // The rail path never consults this predicate: it keeps its own
        // zero-dimension guard and legally renders at cols 6-10.
    }

    #[test]
    fn rail_width_mode_thresholds_are_sharp() {
        assert_eq!(RailWidthMode::from_cols(24), RailWidthMode::Wide);
        assert_eq!(RailWidthMode::from_cols(23), RailWidthMode::Normal);
        assert_eq!(RailWidthMode::from_cols(14), RailWidthMode::Normal);
        assert_eq!(RailWidthMode::from_cols(13), RailWidthMode::Dense);
        assert_eq!(RailWidthMode::from_cols(6), RailWidthMode::Dense);
        assert_eq!(RailWidthMode::from_cols(80), RailWidthMode::Wide);
    }

    #[test]
    fn rail_header_speaks_three_faces() {
        // Wide anchors the current session name.
        assert_eq!(
            rail_header_text(RailWidthMode::Wide, 3, Some("alpha")),
            "SESSIONS 3 · alpha"
        );
        // Normal keeps the count, drops the anchor.
        assert_eq!(
            rail_header_text(RailWidthMode::Normal, 3, Some("alpha")),
            "SESSIONS 3"
        );
        // Dense is an ultra-short badge.
        assert_eq!(
            rail_header_text(RailWidthMode::Dense, 3, Some("alpha")),
            "S3"
        );
        // No current session: Wide degrades to the count alone.
        assert_eq!(rail_header_text(RailWidthMode::Wide, 2, None), "SESSIONS 2");
    }

    #[test]
    fn dense_rows_are_iconic_and_fit_narrow_columns() {
        let entry = format_session_rail_entry(
            &session("very-long-session-name", true),
            1,
            RailWidthMode::Dense,
        );
        assert_eq!(entry, "01 ◉");
        let entry = format_session_rail_entry(&session("other", false), 7, RailWidthMode::Dense);
        assert_eq!(entry, "07 ○");
        // Every dense row fits the narrowest legal rail without shredding.
        for cols in [4, 6, 8, 13] {
            assert!(fit_rail_line(&entry, cols).width() <= cols);
        }
    }

    #[test]
    fn dense_and_wide_build_the_same_row_inventory() {
        // Same sessions, same number and kinds of rows in both faces — the
        // click-map is built per row in render, so kind parity here proves
        // every dense row stays clickable.
        let mut alpha = session("alpha", true);
        alpha.tabs = vec![TabUiInfo::for_rail_test("build", true, "cargo", 1)];
        let sessions = [alpha, session("beta", false)];
        let wide = session_rail_rows_with_truth(&sessions, RailWidthMode::Wide);
        let dense = session_rail_rows_with_truth(&sessions, RailWidthMode::Dense);
        assert_eq!(wide.len(), dense.len());
        for (wide_row, dense_row) in wide.iter().zip(dense.iter()) {
            assert_eq!(wide_row.kind, dense_row.kind);
        }
    }

    #[test]
    fn normal_rows_keep_names_and_indicators() {
        let entry = format_session_rail_entry(&session("alpha", true), 1, RailWidthMode::Normal);
        assert_eq!(entry, "01 ◉ alpha");
        let entry = format_session_rail_entry(&session("beta", false), 2, RailWidthMode::Normal);
        assert_eq!(entry, "02 ○ beta");
    }

    #[test]
    fn rail_hides_internal_host_and_marks_visited_guest_current() {
        let mut state = State::default();
        state.frame_host = true;
        state.visited_guest_name = Some("workspace-a".to_owned());
        let mut plugins = BTreeMap::new();
        plugins.insert(
            1,
            PluginInfo {
                location: "session-manager".to_owned(),
                configuration: BTreeMap::from([("frame_host".to_owned(), "true".to_owned())]),
            },
        );
        let host = SessionInfo {
            name: "frame-host".to_owned(),
            plugins,
            is_current_session: true,
            ..SessionInfo::default()
        };
        let guest = SessionInfo {
            name: "workspace-a".to_owned(),
            ..SessionInfo::default()
        };
        state.update_session_infos(vec![host, guest]);
        let names: Vec<&str> = state
            .sessions
            .session_ui_infos
            .iter()
            .map(|session| session.name.as_str())
            .collect();
        assert_eq!(names, vec!["workspace-a"]);
        assert!(state.sessions.session_ui_infos[0].is_current_session);
    }

    fn session(name: &str, is_current_session: bool) -> SessionUiInfo {
        SessionUiInfo {
            name: name.to_owned(),
            title: name.to_owned(),
            tabs: vec![],
            connected_users: 1,
            is_current_session,
            creation_time: Duration::ZERO,
        }
    }

    /// Product key-contract v3: bare arrows switch sessions ONLY through the
    /// ^T tab-mode keybinds (vc_rail_nav pipe). A focused rail pane must not
    /// consume them — LOCK routes raw keys to the focused pane, so a rail
    /// arrow handler becomes a hidden mode-proof session switcher.
    #[test]
    fn rail_ignores_bare_arrow_keys() {
        let mut state = State::default();
        state.sessions.session_ui_infos = vec![session("solo", true)];
        assert!(!state.handle_session_rail_key(KeyWithModifier::new(BareKey::Up)));
        assert!(!state.handle_session_rail_key(KeyWithModifier::new(BareKey::Down)));
    }

    fn session_launched_at(name: &str, is_current_session: bool, secs: u64) -> SessionUiInfo {
        SessionUiInfo {
            creation_time: Duration::from_secs(secs),
            ..session(name, is_current_session)
        }
    }

    fn agent_run(payload: &str) -> AgentRunUiInfo {
        serde_json::from_str(payload).unwrap()
    }

    #[test]
    fn human_workspace_title_precedence_is_deterministic() {
        let explicit = agent_run(
            r#"{"run_id":"impl-raw","workspace_title":"Cancer Trial Review","repo":"vc-frame","task_title":"FUX","root":"/tmp/ignored"}"#,
        );
        assert_eq!(explicit.primary_title(), "Cancer Trial Review");

        let repo_and_task =
            agent_run(r#"{"run_id":"impl-raw","repo":"vc-frame","plan_title":"Unified layout"}"#);
        assert_eq!(repo_and_task.primary_title(), "vc-frame · Unified layout");

        let dispatched = agent_run(
            r#"{"run_id":"impl-260827-132005-98719","root":"/Users/operator/.vibecrafted/worktrees/vetcoders/vc-frame/2026_0827/FUX"}"#,
        );
        assert_eq!(dispatched.primary_title(), "vc frame · FUX");
        assert_ne!(dispatched.primary_title(), dispatched.run_id);

        let raw_explicit = agent_run(
            r#"{"run_id":"impl-raw","workspace_title":"impl-260827-132005-98719","repo":"vc-frame","task_title":"FUX"}"#,
        );
        assert_eq!(raw_explicit.primary_title(), "vc-frame · FUX");
    }

    #[test]
    fn agent_workspaces_has_real_empty_unavailable_and_degraded_states() {
        assert!(agent_workspace_lines(None, false)[3].starts_with("UNAVAILABLE"));
        assert!(agent_workspace_lines(Some(&[]), false)[3].starts_with("EMPTY"));
        assert!(agent_workspace_lines(Some(&[]), true)[1].starts_with("DEGRADED"));

        let run = agent_run(
            r#"{"run_id":"impl-260827-132005-98719","repo":"vc-frame","task_title":"FUX","agent":"codex","skill":"implement","execution_state":"running"}"#,
        );
        let lines = agent_workspace_lines(Some(std::slice::from_ref(&run)), false);
        assert_eq!(lines[3], "● vc-frame · FUX");
        assert_eq!(lines[4], "  codex · implement · running");
        assert!(lines[5].contains(&run.run_id));
        assert!(!lines[3].contains(&run.run_id));
    }

    #[test]
    fn canonical_operator_session_title_overlays_raw_session_identity() {
        let run = agent_run(
            r#"{"run_id":"impl-raw","operator_session":"fux-impl-raw","repo":"vc-frame","task_title":"FUX"}"#,
        );
        let mut sessions = vec![session("fux-impl-raw", true)];
        project_canonical_session_titles(Some(std::slice::from_ref(&run)), &mut sessions);
        assert_eq!(sessions[0].name, "fux-impl-raw");
        assert_eq!(sessions[0].title, "vc-frame · FUX");
        assert_eq!(
            format_session_rail_entry(&sessions[0], 1, RailWidthMode::Wide),
            "01 ◉ vc-frame · FUX"
        );
    }

    #[test]
    fn session_updates_preserve_selection_by_identity_after_resort() {
        let mut list = SessionList::default();
        list.set_sessions(
            vec![
                session_launched_at("alpha", true, 10),
                session_launched_at("beta", false, 20),
            ],
            vec![],
        );
        list.select_session_index(1);
        list.set_sessions(
            vec![
                session_launched_at("new-first", false, 1),
                session_launched_at("alpha", true, 10),
                session_launched_at("beta", false, 20),
            ],
            vec![],
        );
        assert_eq!(list.get_selected_session_name().as_deref(), Some("beta"));
    }

    /// Historical synthetic names are ordinary workspaces. They remain
    /// visible and navigable so upgrades cannot hide same-name user data.
    #[test]
    fn historical_bucket_names_are_ordinary_sessions() {
        let sessions = vec![
            session("work", true),
            session("Finalized runs", false),
            session("Needs attention", false),
        ];
        assert_eq!(
            relative_session_target(&sessions, 1),
            Some("Finalized runs".to_owned())
        );
        let rows = session_rail_rows(&sessions);
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .all(|row| matches!(row.kind, SessionRailRowKind::Session(_)))
        );
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

    /// ctrl+t Up/Down steps through every workspace in rail order, wrapping.
    #[test]
    fn rail_nav_includes_historical_bucket_names() {
        let sessions = vec![
            session_launched_at("early", false, 100),
            session_launched_at("Finalized runs", false, 150),
            session_launched_at("late", true, 200),
            session_launched_at("Needs attention", false, 250),
        ];
        assert_eq!(
            relative_session_target(&sessions, 1).as_deref(),
            Some("Needs attention")
        );
        assert_eq!(
            relative_session_target(&sessions, -1).as_deref(),
            Some("Finalized runs")
        );
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
            format_session_rail_entry(&session("alpha", true), 1, RailWidthMode::Wide),
            "01 ◉ alpha"
        );
        assert_eq!(
            format_session_rail_entry(&session("beta", false), 12, RailWidthMode::Wide),
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
        for (ordinal, row) in rows.iter().enumerate() {
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
    fn historical_bucket_names_sort_like_other_sessions() {
        let mut rail = SessionList::default();
        rail.set_sessions(
            vec![
                session_launched_at("zzz", false, 5),
                session_launched_at("Finalized runs", false, 3),
                session_launched_at("aaa", true, 1),
                session_launched_at("Failed runs", false, 2),
                session_launched_at("Needs attention", false, 4),
            ],
            vec![],
        );
        let rows = session_rail_rows(&rail.session_ui_infos);
        assert_eq!(rows[0].text, "01 ◉ aaa");
        assert_eq!(rows[1].text, "02 ○ Failed runs");
        assert_eq!(rows[2].text, "03 ○ Finalized runs");
        assert_eq!(rows[3].text, "04 ○ Needs attention");
        assert_eq!(rows[4].text, "05 ○ zzz");
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
                "   ◉ impl-260718-120000-01000 · claude",
                "   · audit-260718-130000-02000 · codex +1",
                "02 ○ beta",
            ]
        );
        assert_eq!(rows.iter().filter(|row| row.is_live_process()).count(), 2);
    }

    #[test]
    fn rail_does_not_repeat_process_label_when_tab_already_names_the_agent() {
        let tab = TabUiInfo::for_rail_test("claude", true, "claude", 1);

        assert_eq!(
            format_process_tab_rail_entry(&tab, RailWidthMode::Wide),
            "   ◉ claude"
        );
    }

    #[test]
    fn rail_strips_spinner_frame_but_keeps_meaningful_process_progress() {
        let spinning = TabUiInfo::for_rail_test("resume-codex", true, "⣧ vc", 1);
        let progressing = TabUiInfo::for_rail_test("resume-codex", true, "⣇ indexing workspace", 1);

        assert_eq!(
            format_process_tab_rail_entry(&spinning, RailWidthMode::Wide),
            "   ◉ resume-codex · vc"
        );
        assert_eq!(
            format_process_tab_rail_entry(&progressing, RailWidthMode::Wide),
            "   ◉ resume-codex · indexing workspace"
        );
    }

    #[test]
    fn spinner_frame_changes_do_not_change_the_rail_projection() {
        let mut first = session("vc-frame", true);
        first.tabs = vec![TabUiInfo::for_rail_test("resume-codex", true, "⣧ vc", 1)];
        let mut next = session("vc-frame", true);
        next.tabs = vec![TabUiInfo::for_rail_test("resume-codex", true, "⣇ vc", 1)];

        assert_eq!(
            session_rail_session_rows(&[first], RailWidthMode::Wide),
            session_rail_session_rows(&[next], RailWidthMode::Wide)
        );
    }

    #[test]
    fn meaningful_progress_changes_still_change_the_rail_projection() {
        let mut first = session("vc-frame", true);
        first.tabs = vec![TabUiInfo::for_rail_test("resume-codex", true, "⣧ vc", 1)];
        let mut next = session("vc-frame", true);
        next.tabs = vec![TabUiInfo::for_rail_test(
            "resume-codex",
            true,
            "⣇ applying patch",
            1,
        )];

        assert_ne!(
            session_rail_session_rows(&[first], RailWidthMode::Wide),
            session_rail_session_rows(&[next], RailWidthMode::Wide)
        );
    }

    #[test]
    fn live_runs_feed_remains_available_to_the_agent_workspaces_canvas() {
        let mut state = State {
            is_rail: false,
            workspace_dashboard: true,
            ..Default::default()
        };
        assert!(state.update(Event::CustomMessage(
            VC_LIVE_RUNS_MESSAGE.to_owned(),
            r#"{"schema":"vc.live-runs.v1","server_url":"https://observer.example:8443","runs":[{"run_id":"a"},{"run_id":"b"}]}"#.to_owned(),
        )));
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(2));
        assert!(!state.live_runs_feed_degraded);

        // Same census again: no repaint for an unchanged truth.
        assert!(!state.update(Event::CustomMessage(
            VC_LIVE_RUNS_MESSAGE.to_owned(),
            r#"{"schema":"vc.live-runs.v1","server_url":"https://observer.example:8443","runs":[{"run_id":"a"},{"run_id":"b"}]}"#.to_owned(),
        )));

        // A corrupt payload marks the canvas feed degraded without changing
        // the last canonical server projection.
        assert!(state.update(Event::CustomMessage(
            VC_LIVE_RUNS_MESSAGE.to_owned(),
            r#"{"schema":"someone-elses.schema","runs":[]}"#.to_owned(),
        )));
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(2));
        assert!(state.live_runs_feed_degraded);
    }

    #[test]
    fn live_runs_feed_degrades_after_missed_refresh_windows() {
        let mut state = State {
            is_rail: true,
            ..Default::default()
        };
        assert!(
            state
                .apply_live_runs_payload(r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"a"}]}"#)
        );
        for _ in 1..LIVE_RUNS_FEED_STALE_AFTER_TICKS {
            assert!(!state.age_live_runs_feed());
            assert!(!state.live_runs_feed_degraded);
        }
        assert!(state.age_live_runs_feed());
        assert!(state.live_runs_feed_degraded);

        // A fresh payload restores exact truth.
        assert!(
            state
                .apply_live_runs_payload(r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"a"}]}"#)
        );
        assert!(!state.live_runs_feed_degraded);
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
    fn sanitize_display_label_strips_controls_and_collapses_whitespace() {
        assert_eq!(sanitize_display_label("  hello\n\tworld  "), "hello world");
        assert_eq!(sanitize_display_label("Main\u{0007}"), "Main");
        // Stable width: control-laden and clean labels pad to the same grid.
        let dirty = fit_rail_line(&sanitize_display_label("Main\n\r  "), 10);
        let clean = fit_rail_line(&sanitize_display_label("Main"), 10);
        assert_eq!(dirty.width(), clean.width());
        assert_eq!(dirty.width(), 10);
    }

    #[test]
    fn format_session_rail_entry_sanitizes_name_for_stable_columns() {
        let mut session = session("alpha", true);
        session.name = "alpha\nbeta".to_owned();
        let text = format_session_rail_entry(&session, 1, RailWidthMode::Wide);
        assert!(!text.contains('\n'));
        assert!(
            text.contains("alpha beta") || text.contains("alphabeta") || text.contains("alpha")
        );
        // After sanitize newline becomes space collapse → "alpha beta"
        assert_eq!(sanitize_display_label("alpha\nbeta"), "alpha beta");
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
    fn rail_row_click_target_maps_session_and_tab() {
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

        // row 1: alpha session, row 2: its live worker tab, row 3: beta
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
        // Data rows highlight; chrome and gaps do not.
        assert_eq!(rail_hover_target(1, &click_map), Some(1));
        assert_eq!(rail_hover_target(2, &click_map), None);
        // Header, blank gap, out-of-bounds, negative leave → clear.
        assert_eq!(rail_hover_target(0, &click_map), None);
        assert_eq!(rail_hover_target(99, &click_map), None);
        assert_eq!(rail_hover_target(-1, &click_map), None);
    }

    #[test]
    fn char_offset_of_speaks_chars_not_bytes() {
        // "   ◉ tab · cmd" — the dot separator sits after multi-byte `◉`.
        let row = "   ◉ tab · cmd";
        assert_eq!(char_offset_of(row, " · ", 4), Some(8));
        assert_eq!(char_offset_of(row, " · ", 0), Some(8));
        assert_eq!(char_offset_of(row, "missing", 0), None);
        assert_eq!(char_offset_of("ab", "b", 5), None, "from beyond end");
    }

    #[test]
    fn guest_visit_command_preserves_session_boundaries_and_one_based_cli_tab() {
        let command = guest_visit_command("my session", Some(2));
        assert_eq!(
            command.path,
            std::path::PathBuf::from(VC_FRAME_SELF_EXECUTABLE)
        );
        assert_eq!(command.args, ["visit", "my session", "--tab", "3"]);
    }

    #[test]
    fn ordinary_manager_ignores_guest_tab_activation() {
        let mut state = State::default();
        state.frame_host = false;
        assert!(!state.handle_guest_surface_message(&activate_guest_tab_payload("workspace-a", 1)));
        assert!(state.visited_guest_name.is_none());
        assert!(state.pending_guest_visit.is_none());
    }

    #[test]
    fn ordinary_manager_ignores_project_and_does_not_reconnect() {
        let mut state = State::default();
        state.frame_host = false;
        assert!(!state.handle_guest_surface_message(&project_guest_payload("workspace-b", None)));
        assert!(state.visited_guest_name.is_none());
    }

    #[test]
    fn host_project_pipe_sets_pending_when_guest_pane_is_missing() {
        let mut state = State::default();
        state.frame_host = true;
        assert!(state.handle_guest_surface_message(&project_guest_payload("workspace-a", None)));
        assert_eq!(
            state
                .pending_guest_visit
                .as_ref()
                .map(|pending| pending.session.as_str()),
            Some("workspace-a")
        );
        assert_eq!(
            state
                .pending_guest_visit
                .as_ref()
                .and_then(|pending| pending.tab),
            None
        );
    }

    #[test]
    fn host_project_pipe_retains_requested_tab() {
        let mut state = State::default();
        state.frame_host = true;
        assert!(state.handle_guest_surface_message(&project_guest_payload("workspace-b", Some(2))));
        assert_eq!(
            state.pending_guest_visit,
            Some(PendingGuestRequest {
                session: "workspace-b".to_owned(),
                tab: Some(2),
            })
        );
    }

    #[test]
    fn failed_guest_create_clears_pending_and_surfaces_the_error() {
        let mut state = State::default();
        state.pending_guest_create = Some((
            "create-new".to_owned(),
            PendingGuestRequest {
                session: "workspace-a".to_owned(),
                tab: Some(1),
            },
        ));
        assert!(state.handle_guest_create_result(
            Some(1),
            b"",
            b"Session already exists",
            Some("workspace-a"),
            Some("create-new"),
        ));
        assert!(state.pending_guest_visit.is_none());
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or("")
                .contains("Failed to create workspace `workspace-a`")
        );
    }

    #[test]
    fn floating_manager_handoff_uses_current_host_only() {
        let mut state = State::default();
        state.frame_host = false;
        state.current_session_is_host = false;
        state.host_session_name = Some("frame-host-a".to_owned());
        assert_eq!(
            state.plan_host_handoff(),
            HostHandoff::DetachedNotice,
            "first-listed foreign host must not receive projection"
        );
        state.current_session_is_host = true;
        state.session_name = Some("frame-host-b".to_owned());
        assert_eq!(
            state.plan_host_handoff(),
            HostHandoff::CliProject {
                host: "frame-host-b".to_owned(),
            }
        );
        state.frame_host = true;
        assert_eq!(state.plan_host_handoff(), HostHandoff::PendingOnSelf);
        state.frame_host = false;
        state.current_session_is_host = false;
        state.host_session_name = None;
        assert_eq!(state.plan_host_handoff(), HostHandoff::DetachedNotice);
    }

    #[test]
    fn successful_guest_create_hands_off_retained_tab() {
        let mut state = State::default();
        state.frame_host = true;
        state.pending_guest_create = Some((
            "create-new".to_owned(),
            PendingGuestRequest {
                session: "workspace-b".to_owned(),
                tab: Some(2),
            },
        ));
        assert!(state.handle_guest_create_result(
            Some(0),
            b"",
            b"",
            Some("workspace-b"),
            Some("create-new")
        ));
        assert_eq!(
            state.pending_guest_visit,
            Some(PendingGuestRequest {
                session: "workspace-b".to_owned(),
                tab: Some(2),
            })
        );
    }
    #[test]
    fn discovery_and_old_create_results_cannot_acknowledge_a_new_creation() {
        let mut state = State {
            frame_host: true,
            ..Default::default()
        };
        let pending = PendingGuestRequest {
            session: "workspace-b".to_owned(),
            tab: Some(2),
        };
        state.pending_guest_create = Some(("new-generation".to_owned(), pending.clone()));
        let mut discovered = SessionInfo::default();
        discovered.name = pending.session.clone();
        state.maybe_visit_pending_guest(&[discovered]);
        assert!(state.pending_guest_visit.is_none());
        assert!(state.visited_guest_name.is_none());
        for (name, id) in [
            ("workspace-b", "old-generation"),
            ("workspace-a", "new-generation"),
        ] {
            for status in [0, 1] {
                assert!(!state.handle_guest_create_result(
                    Some(status),
                    b"",
                    b"",
                    Some(name),
                    Some(id)
                ));
                assert_eq!(state.pending_guest_create.as_ref().unwrap().1, pending);
                assert!(state.pending_guest_visit.is_none());
            }
        }
        assert!(state.handle_guest_create_result(
            Some(1),
            b"",
            b"duplicate name",
            Some("workspace-b"),
            Some("new-generation")
        ));
        assert!(state.pending_guest_create.is_none());
        assert!(state.pending_guest_visit.is_none());
        assert!(state.visited_guest_name.is_none());
    }
    #[test]
    fn newer_project_supersedes_an_in_flight_create_and_ignores_old_pane_events() {
        let mut state = State {
            frame_host: true,
            ..Default::default()
        };
        state.pending_guest_create = Some((
            "old".to_owned(),
            PendingGuestRequest {
                session: "workspace-b".to_owned(),
                tab: Some(1),
            },
        ));
        state.handle_guest_surface_message(&project_guest_payload("workspace-a", Some(2)));
        assert!(!state.handle_guest_create_result(
            Some(0),
            b"",
            b"",
            Some("workspace-b"),
            Some("old")
        ));
        assert_eq!(
            state.pending_guest_visit.as_ref().unwrap().session,
            "workspace-a"
        );
        assert_eq!(state.pending_guest_visit.as_ref().unwrap().tab, Some(2));
        let context = BTreeMap::from([(
            VC_GUEST_COMMAND_CONTEXT_KEY.to_owned(),
            "workspace-b".to_owned(),
        )]);
        state.update(Event::CommandPaneOpened(3, context.clone()));
        state.update(Event::CommandPaneExited(3, Some(1), context));
        assert!(state.visited_guest_name.is_none());
    }
}
