mod list_navigation;
mod new_session_info;
mod resurrectable_sessions;
mod session_list;
mod single_screen;
mod single_screen_data;
mod single_screen_render;
mod ui;
mod workspace_surface;
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
use workspace_surface::{
    SURFACE_SPINNER_FRAMES, SurfaceClickTarget, SurfaceOverview, SurfaceTone,
    workspace_surface_overview_lines,
};

#[derive(Clone, Debug, Copy, PartialEq, Default)]
enum ActiveScreen {
    NewSession,
    #[default]
    AttachToSession,
    ResurrectSession,
    SingleScreen,
}

const VC_CHROME_VISIBILITY_MESSAGE: &str = "vc.status-bar-visibility.v1";
// Semantic live-run truth for the dedicated Agent Workspaces canvas:
// Vibecrafted Server `active_runs`, relayed by the vc-frame server's
// session-metadata loop. Never derived from local files, PIDs, or sessions.
const VC_LIVE_RUNS_MESSAGE: &str = "vc.live-runs.v1";
const VC_GUEST_CREATE_REQUEST_KEY: &str = "vc_frame_guest_create_request";
const VC_GUEST_COMMAND_CONTEXT_KEY: &str = "vc_frame_guest_surface";
const VC_OPEN_PROJECT_CONTEXT_KEY: &str = "vc_frame_open_project";

/// What a pinned Operator Frame row opens. Config stays inert: there is no
/// config canvas in this repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRowPlan {
    /// Workspace tab, whose center pane is the VC Guest overview.
    FocusGuestOverview,
    /// Home tab, which renders the server census already on the rail.
    OpenCensus,
    /// The floating `vibecrafted doctor` pane Start Here already launches.
    Doctor,
    /// Start Here's folder picker, then `vc-start resume --repo`.
    ChooseProject,
    Inert,
}

fn host_row_plan(row: HostRow) -> HostRowPlan {
    match row {
        HostRow::Dashboard => HostRowPlan::FocusGuestOverview,
        HostRow::ActiveRuns => HostRowPlan::OpenCensus,
        HostRow::Doctor => HostRowPlan::Doctor,
        HostRow::Projects => HostRowPlan::ChooseProject,
        HostRow::Config => HostRowPlan::Inert,
    }
}

/// Same argv Start Here uses for Help & diagnostics.
fn doctor_pane_argv() -> Vec<String> {
    [
        "vc-frame",
        "action",
        "new-pane",
        "--floating",
        "--name",
        "Vibecrafted Help & diagnostics",
        "--width",
        "72%",
        "--height",
        "70%",
        "--",
        "bash",
        "-lc",
        "vibecrafted doctor; printf '\\nPress Enter to close diagnostics…'; read -r _",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

/// Same folder picker Start Here uses, chosen on the host at runtime so a
/// wasm plugin does not bake in a platform.
fn project_chooser_argv() -> Vec<String> {
    vec![
        "bash".to_owned(),
        "-lc".to_owned(),
        concat!(
            "if [ -x /usr/bin/osascript ]; then ",
            "/usr/bin/osascript -e 'POSIX path of (choose folder with prompt \"Open a Vibecrafted project\")'; ",
            "elif command -v zenity >/dev/null 2>&1; then ",
            "zenity --file-selection --directory --title=\"Open a Vibecrafted project\"; ",
            "else printf \"Project folder path: \"; read -r path; printf \"%s\" \"$path\"; fi"
        )
        .to_owned(),
    ]
}

fn resume_project_argv(path: &str) -> Vec<String> {
    vec![
        "vc-start".to_owned(),
        "resume".to_owned(),
        "--repo".to_owned(),
        path.to_owned(),
    ]
}

fn run_owned_argv(argv: &[String], context: BTreeMap<String, String>) {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    run_command(&refs, context);
}
const VC_FRAME_SELF_EXECUTABLE: &str = "vc-frame:self";

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostHandoff {
    PendingOnSelf,
    CliProject { host: String },
    DetachedNotice,
}
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

fn should_hide_manager_after_guest_create(frame_host: bool, workspace_surface: bool) -> bool {
    // Floating managers hide after a successful create. The host rail stays,
    // and the tiled VC Guest surface must never hide: it IS the pane the new
    // guest is about to be projected into.
    !frame_host && !workspace_surface
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
    /// Server census bucket: `current`, `stalled`, or `recent`.
    /// Absent on older feeds, which are the current/active list.
    #[serde(default)]
    census_bucket: String,
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

/// Server census names. A missing bucket is current: older feeds only carried
/// `active_runs`. Stopped is not "needs attention".
fn census_bucket(run: &AgentRunUiInfo) -> &'static str {
    match run.census_bucket.as_str() {
        "stalled" => "stalled",
        "recent" => "recent",
        _ => "current",
    }
}

fn census_rank(run: &AgentRunUiInfo) -> u8 {
    match census_bucket(run) {
        "stalled" => 1,
        "recent" => 2,
        _ => 0,
    }
}

fn is_current_census_run(run: &AgentRunUiInfo) -> bool {
    census_bucket(run) == "current"
}

const CENSUS_SECTIONS: &[(&str, &str)] = &[
    ("Current", "current"),
    ("Stalled", "stalled"),
    ("Recent", "recent"),
];

fn append_census_run(lines: &mut Vec<String>, run: &AgentRunUiInfo, marker: &str) {
    lines.push(format!("{marker}● {}", run.primary_title()));
    lines.push(format!("  {}", run.status_summary()));
    lines.push(format!("  run {}", sanitize_display_label(&run.run_id)));
}

fn append_census_sections(lines: &mut Vec<String>, runs: &[&AgentRunUiInfo]) {
    for (label, bucket) in CENSUS_SECTIONS {
        lines.push((*label).to_owned());
        let members: Vec<_> = runs
            .iter()
            .copied()
            .filter(|run| census_bucket(run) == *bucket)
            .collect();
        if members.is_empty() {
            lines.push("  —".to_owned());
        } else {
            for run in members {
                append_census_run(lines, run, "");
            }
        }
        lines.push(String::new());
    }
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
            let refs: Vec<&AgentRunUiInfo> = runs.iter().collect();
            append_census_sections(&mut lines, &refs);
        },
    }
    lines
}

fn home_runs_in_scope<'a>(
    runs: &'a [AgentRunUiInfo],
    scope: &AgentPanelScope,
) -> Vec<&'a AgentRunUiInfo> {
    let mut scoped: Vec<&AgentRunUiInfo> = runs
        .iter()
        .filter(|run| scope.includes(Some(run.operator_session.as_str())))
        .collect();
    // Stable: Current, then Stalled, then Recent. Within a bucket the feed
    // order (started_at, run id) is kept.
    scoped.sort_by_key(|run| census_rank(run));
    scoped
}

/// Home's agent panel: scope first, then feed truth. An unknown feed stays
/// UNAVAILABLE; it is never rendered as zero agents.
fn home_agent_panel_lines(
    runs: Option<&[AgentRunUiInfo]>,
    degraded: bool,
    scope: &AgentPanelScope,
    selected: usize,
    notice: Option<&str>,
) -> Vec<String> {
    let mut lines = vec![
        format!("⌂ {VC_HOME_TAB_NAME}"),
        format!("{} · Tab switches", scope.label()),
        if degraded {
            "DEGRADED · showing last accepted server projection".to_owned()
        } else {
            "LIVE · Vibecrafted Server projection".to_owned()
        },
    ];
    if let Some(notice) = notice {
        lines.push(notice.to_owned());
    }
    lines.push(String::new());
    match runs {
        None => lines.push("UNAVAILABLE · waiting for canonical workspace data".to_owned()),
        Some(runs) => {
            let in_scope = home_runs_in_scope(runs, scope);
            lines.push(format!(
                "{} of {} agents in scope",
                in_scope.len(),
                runs.len()
            ));
            if in_scope.is_empty() {
                lines.push("EMPTY · no agent in this scope".to_owned());
            }
            let mut flat_index = 0usize;
            for (label, bucket) in CENSUS_SECTIONS {
                lines.push((*label).to_owned());
                let members: Vec<_> = in_scope
                    .iter()
                    .copied()
                    .filter(|run| census_bucket(run) == *bucket)
                    .collect();
                if members.is_empty() {
                    lines.push("  —".to_owned());
                    continue;
                }
                for run in members {
                    let marker = if flat_index == selected { "▸" } else { " " };
                    flat_index += 1;
                    lines.push(format!("{marker}● {}", run.primary_title()));
                    lines.push(format!("    {}", run.status_summary()));
                    lines.push(format!(
                        "    workspace {}",
                        nonempty_title(Some(&run.operator_session))
                            .unwrap_or_else(|| "none linked".to_owned())
                    ));
                }
            }
        },
    }
    lines.push(String::new());
    lines.push(format!(
        "Enter: open in {VC_SHARED_WORKSPACE_TAB_NAME} · rail h / Ctrl+t 1: {VC_HOME_TAB_NAME}"
    ));
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
    // Empty-host overview state (workspace_surface): while no guest is
    // projected, the pane renders live workspaces, the runs census and quick
    // actions instead of a placeholder line. The click map is rebuilt on
    // every render so clicks resolve against exactly what is on screen.
    surface_selected: usize,
    surface_tick: usize,
    surface_click_map: BTreeMap<usize, SurfaceClickTarget>,
    surface_hover_row: Option<usize>,
    surface_notice: Option<String>,
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
    // Agent Workspaces canvas and the pinned host section of the session rail.
    live_runs_feed_degraded: bool,
    // Host Home resident (`home true` on the Agent Workspaces canvas). It is
    // the only instance that moves a client: once, when that client attaches.
    home_resident: bool,
    agent_scope: AgentPanelScope,
    selected_agent: usize,
    home_notice: Option<String>,
    // Rail truth guard: set when the host filter would have emptied the
    // session list and the rail fell back to the unfiltered set. The filter
    // must never render "SESSIONS 0" while sessions exist.
    session_list_degraded: bool,
    // False until the first session-list payload arrives (either path).
    // Before it the rail header shows "?" — "0" would be a claim, not a fact.
    session_list_seen: bool,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.own_plugin_id = Some(get_plugin_ids().plugin_id);
        self.workspace_surface =
            configuration.get("workspace_surface").map(String::as_str) == Some("true");
        if self.workspace_surface {
            // The empty-host overview is live data, not a placeholder: it
            // consumes the same server-owned projections as the rail and the
            // Agent Workspaces canvas, and takes keyboard/mouse selection
            // while no guest is projected into this pane.
            subscribe(&[
                EventType::ModeUpdate,
                EventType::Key,
                EventType::Mouse,
                EventType::SessionUpdate,
                EventType::CustomMessage,
                EventType::RunCommandResult,
                EventType::Timer,
            ]);
            // Quick actions are permission-gated commands: both opening an
            // existing workspace and creating a new one ride the guarded CLI
            // pipe via run_command (RunCommands). Requested once at load,
            // granted once per plugin location, then cached — the same
            // contract the status-bar holds. Without this the actions would
            // silently no-op.
            request_permission(&[PermissionType::RunCommands]);
            self.refresh_session_list();
            self.arm_refresh_timer();
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
        self.home_resident = self.workspace_dashboard
            && configuration.get("home").map(String::as_str) == Some("true");
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
        // This instance belongs to exactly one client. Screen registered that
        // client before its plugin instances load, and a non-mirrored
        // go_to_tab moves only it: a new attach starts on Home while clients
        // already attached keep their own view.
        if home_claims_client_on_load(self.home_resident) {
            go_to_tab(VC_HOME_TAB_POSITION);
        }
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
        if let (PipeSource::Cli(pipe_id), Some((session, tab))) = (
            &pipe_message.source,
            host_cli_guest_surface_visit(
                self.frame_host,
                &pipe_message.name,
                pipe_message.payload.as_deref(),
            ),
        ) {
            let ids = get_plugin_ids();
            // Compact-bar clicks and `pipe --name vc.guest-surface.v1` often
            // omit request_id. Mint one so Screen can correlate the receipt
            // and unblock this exact CLI pipe. Do not fall through to
            // handle_guest_surface_message: that path calls activate_session
            // with pipe_id=None and leaves the CLI blocked.
            let request_id = pipe_message
                .args
                .get("request_id")
                .cloned()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| Uuid::new_v4().to_string());
            block_cli_pipe_input(pipe_id);
            let pane_id = self.activate_session_request(
                &session,
                tab,
                &request_id,
                Some(pipe_id),
                pipe_message.args.get("pipe_client_id").map(String::as_str),
            );
            if pane_id.is_some() {
                // Screen owns the final acknowledgment after the visitor
                // receives guest output. Keep this exact pipe pending.
                return true;
            }
            let receipt = WorkspaceProjectionReceipt {
                request_id,
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
            return self.update_workspace_surface(event);
        }
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
            Event::CustomMessage(message, payload)
                if (self.is_rail || self.workspace_dashboard)
                    && message == VC_LIVE_RUNS_MESSAGE =>
            {
                should_render = self.apply_live_runs_payload(&payload);
            },
            Event::CustomMessage(message, payload) if message == VC_GUEST_SURFACE_MESSAGE => {
                should_render = self.handle_guest_surface_message(&payload);
            },
            Event::RunCommandResult(exit_code, stdout, _stderr, context)
                if context.contains_key(VC_OPEN_PROJECT_CONTEXT_KEY) =>
            {
                should_render = self.finish_open_project(exit_code, &stdout);
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
            self.render_workspace_surface_overview(rows, cols);
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

fn rail_header_with_truth(
    mode: RailWidthMode,
    session_count: usize,
    current_session_name: Option<&str>,
    session_list_seen: bool,
    session_list_degraded: bool,
) -> String {
    if !session_list_seen {
        // No payload from either path yet: "0" would be a claim, not a fact.
        return match mode {
            RailWidthMode::Wide | RailWidthMode::Normal => "SESSIONS ?".to_owned(),
            RailWidthMode::Dense => "S?".to_owned(),
        };
    }
    let base = rail_header_text(mode, session_count, current_session_name);
    if session_list_degraded {
        // Host-filter fallback is on screen: mark it instead of lying by omission.
        match mode {
            RailWidthMode::Dense => format!("S{}~", session_count),
            _ => format!("{} ~", base),
        }
    } else {
        base
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

/// The five pinned host section rows above workspaces in the session rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostRow {
    Dashboard,
    ActiveRuns,
    Config,
    Doctor,
    Projects,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionRailRowKind {
    HostTitle,
    Host(HostRow),
    Separator,
    Session(usize),
    LiveProcess {
        session_index: usize,
        /// 0-based tab position — handed straight to `switch_session_with_focus`
        /// / `go_to_tab` (both expect 0-based and bump internally).
        tab_position: usize,
    },
}

impl SessionRailRowKind {
    /// The pinned host section: title, host rows and the separator. These rows
    /// are reserved at the top of the rail and never scroll.
    fn is_host_section(&self) -> bool {
        matches!(
            self,
            SessionRailRowKind::HostTitle
                | SessionRailRowKind::Host(_)
                | SessionRailRowKind::Separator
        )
    }
}

/// What a left-click on a rail row should do. Derived from the rendered row
/// kind so hit-testing stays pure and independent of keyboard selection state.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RailClickTarget {
    Host(HostRow),
    Session(usize),
    LiveProcess {
        session_index: usize,
        tab_position: usize,
    },
    None,
}

/// The status-bar's plugin alias for the guest-surface pipe. Sibling of
/// `VC_COMPACT_BAR_PLUGIN_ALIAS` (zellij-utils/src/workspace.rs, C6's file) —
/// defined locally so W1-5 does not edit outside its fence; C6 should hoist it.
#[cfg(target_family = "wasm")]
const VC_STATUS_BAR_PLUGIN_ALIAS: &str = "status-bar";

/// What the host publishes about the visited guest. Pure so the
/// publisher→pipe seam is testable on the host (the pipe itself is
/// wasm-only).
///
/// Outcomes:
/// - guest present → active payload;
/// - guest gone → `status: "gone"` tombstone, republished on every
///   SessionUpdate while the guest stays absent;
/// - no visited guest / not a frame host → None.
///
/// The tombstone is replayable last-state truth, not a one-shot message:
/// delivery has no ACK and the server-side fail-closed selector
/// (`unique_guest_surface_pipe_targets`) drops the pipe while interactive
/// clients are ambiguous. Only replay lets a surviving bar converge once the
/// ambiguity clears — without ever selecting an arbitrary client. Replays
/// are idempotent for the receiver and ride the existing SessionUpdate
/// cadence, the same cadence that already republishes a live guest.
fn plan_guest_surface_publication(
    frame_host: bool,
    visited_guest_name: Option<&str>,
    session_infos: &[SessionInfo],
    host_plugin_id: Option<u32>,
) -> Option<String> {
    if !frame_host {
        return None;
    }
    let guest_name = visited_guest_name?;
    match session_infos
        .iter()
        .find(|session| session.name == guest_name)
    {
        Some(guest) => Some(
            serde_json::json!({
                "session": guest.name,
                "status": "active",
                "host_plugin_id": host_plugin_id,
                "tabs": guest.tabs.iter().map(|tab| {
                    serde_json::json!({
                        "name": tab.name,
                        "active": tab.active,
                        "position": tab.position,
                    })
                }).collect::<Vec<_>>(),
            })
            .to_string(),
        ),
        None => Some(
            serde_json::json!({
                "session": guest_name,
                "status": "gone",
                "host_plugin_id": host_plugin_id,
                "tabs": Vec::<serde_json::Value>::new(),
            })
            .to_string(),
        ),
    }
}

fn rail_row_click_target(kind: &SessionRailRowKind) -> RailClickTarget {
    match *kind {
        SessionRailRowKind::HostTitle | SessionRailRowKind::Separator => RailClickTarget::None,
        SessionRailRowKind::Host(host_row) => RailClickTarget::Host(host_row),
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

    #[cfg(test)]
    fn is_host(&self) -> bool {
        self.kind.is_host_section()
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
    frame_host: bool,
    active_runs: Option<usize>,
    live_runs_feed_degraded: bool,
) -> Vec<SessionRailRow> {
    let mut rows = vec![];
    if frame_host {
        match mode {
            RailWidthMode::Wide | RailWidthMode::Normal => {
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::HostTitle,
                    text: "Operator Frame".to_owned(),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::Dashboard),
                    text: "⌂ Dashboard".to_owned(),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::ActiveRuns),
                    text: active_runs_row_text(active_runs, live_runs_feed_degraded),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::Config),
                    text: "⚙︎ Config".to_owned(),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::Doctor),
                    text: "· Doctor".to_owned(),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::Projects),
                    text: "✧ Projects".to_owned(),
                });
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Separator,
                    text: "························".to_owned(),
                });
            },
            RailWidthMode::Dense => {
                rows.push(SessionRailRow {
                    kind: SessionRailRowKind::Host(HostRow::Dashboard),
                    text: format!(
                        "⌂❖{}✧",
                        active_runs_dense_marker(active_runs, live_runs_feed_degraded)
                    ),
                });
            },
        }
    }
    rows.extend(session_rail_session_rows(sessions, mode));
    rows
}

/// The feed truth is binary: a healthy payload projects its exact count;
/// an unseen or explicitly unavailable feed projects `?`. Cached rows may
/// remain available to the workspace canvas, but never leak a stale count
/// into the rail.
fn active_runs_status(count: Option<usize>, degraded: bool) -> String {
    match (count, degraded) {
        (Some(count), false) => count.to_string(),
        _ => "?".to_owned(),
    }
}

fn active_runs_row_text(count: Option<usize>, degraded: bool) -> String {
    format!("❖ Active runs · {}", active_runs_status(count, degraded))
}

fn active_runs_dense_marker(count: Option<usize>, degraded: bool) -> &'static str {
    match (count, degraded) {
        (Some(_), false) => "",
        _ => "?",
    }
}

/// Fit the Active runs row budgeting count and status FIRST: when the full
/// label does not fit, the row collapses to `❖ N` rather than truncating
/// the count away. The count and its truth marker must survive at every
/// width the rail can take.
fn fit_active_runs_row(count: Option<usize>, degraded: bool, cols: usize) -> String {
    let full = active_runs_row_text(count, degraded);
    let text = if full.width() <= cols {
        full
    } else {
        format!("❖ {}", active_runs_status(count, degraded))
    };
    fit_rail_line(&text, cols)
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
        // Same order as the guest tab strip (`project_tab_indices` /
        // `project_guest_organs`). Idle and plugin tabs stay; a live process
        // is a label, not a membership test. That is what kept "Start here"
        // on the rail after the projection had moved on.
        rows.extend(
            project_tab_indices(session.tabs.len(), |index| {
                session.tabs[index].name.as_str()
            })
            .into_iter()
            .map(|index| {
                let tab = &session.tabs[index];
                SessionRailRow {
                    kind: SessionRailRowKind::LiveProcess {
                        session_index,
                        tab_position: tab.position,
                    },
                    text: format_process_tab_rail_entry(tab, mode),
                }
            }),
        );
    }
    rows
}

#[cfg(test)]
fn session_rail_rows(sessions: &[SessionUiInfo]) -> Vec<SessionRailRow> {
    session_rail_rows_with_truth(sessions, RailWidthMode::Wide, false, None, false)
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

/// Render plan for the two-zone rail: the host section is PINNED — it is
/// reserved first and never scrolls; only workspace rows move through the
/// remaining window, and the footer counts hidden workspace rows only.
struct RailRenderPlan {
    host_visible: usize,
    workspace_start: usize,
    workspace_end: usize,
    footer: Option<String>,
}

fn rail_render_plan(
    list_rows: usize,
    host_count: usize,
    workspace_count: usize,
    selected_workspace_row: Option<usize>,
) -> RailRenderPlan {
    let host_visible = host_count.min(list_rows);
    let remaining = list_rows - host_visible;
    if remaining == 0 || workspace_count == 0 {
        return RailRenderPlan {
            host_visible,
            workspace_start: 0,
            workspace_end: 0,
            footer: None,
        };
    }
    let footer_rows = usize::from(workspace_count > remaining && remaining > 1);
    let entry_rows = remaining.saturating_sub(footer_rows);
    let (start, end) = rail_range_to_render(entry_rows, workspace_count, selected_workspace_row);
    let footer = if footer_rows == 1 {
        let hidden_above = start;
        let hidden_below = workspace_count.saturating_sub(end);
        Some(match (hidden_above, hidden_below) {
            (0, below) => format!("+{} more", below),
            (above, 0) => format!("+{} above", above),
            (above, below) => format!("+{} above +{} more", above, below),
        })
    } else {
        None
    };
    RailRenderPlan {
        host_visible,
        workspace_start: start,
        workspace_end: end,
        footer,
    }
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
    /// Agent Workspaces canvas and the pinned host section of the session rail.
    fn apply_live_runs_payload(&mut self, payload: &str) -> bool {
        #[derive(Deserialize)]
        struct LiveRunsFeed {
            schema: String,
            #[serde(default)]
            available: Option<bool>,
            runs: Vec<AgentRunUiInfo>,
        }
        let previous = (self.live_runs_feed_degraded, self.agent_runs.clone());
        let parsed: Option<LiveRunsFeed> = serde_json::from_str(payload)
            .ok()
            .filter(|feed: &LiveRunsFeed| {
                feed.schema == VC_LIVE_RUNS_MESSAGE && feed.available != Some(false)
            })
            // A card without an identity is not a run: an incomplete card
            // (`runs:[{}]`, empty run_id) must not replace the last good
            // census.
            .filter(|feed: &LiveRunsFeed| feed.runs.iter().all(|run| !run.run_id.is_empty()));
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
        (self.live_runs_feed_degraded, self.agent_runs.clone()) != previous
    }

    fn mark_live_runs_feed_degraded(&mut self) -> bool {
        // Degradation is a feed truth, not a data truth: a malformed FIRST
        // payload must also flip the marker, so the rail shows `?` (never
        // confirmed) instead of a healthy-looking zero.
        !std::mem::replace(&mut self.live_runs_feed_degraded, true)
    }

    fn render_agent_workspaces(&self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        let lines = if self.home_resident {
            home_agent_panel_lines(
                self.agent_runs.as_deref(),
                self.live_runs_feed_degraded,
                &self.agent_scope,
                self.selected_agent,
                self.home_notice.as_deref(),
            )
        } else {
            agent_workspace_lines(self.agent_runs.as_deref(), self.live_runs_feed_degraded)
        };
        for (row, line) in lines.into_iter().take(rows).enumerate() {
            let text = fit_rail_line(&line, cols);
            print_text_with_coordinates(Text::new(text), 0, row, None, None);
        }
    }

    fn handle_home_key(&mut self, key: KeyWithModifier) -> bool {
        match key.bare_key {
            BareKey::Tab if key.has_no_modifiers() => {
                self.toggle_agent_scope();
                true
            },
            BareKey::Down if key.has_no_modifiers() => {
                let in_scope = self
                    .agent_runs
                    .as_deref()
                    .map(|runs| home_runs_in_scope(runs, &self.agent_scope).len())
                    .unwrap_or(0);
                if self.selected_agent + 1 < in_scope {
                    self.selected_agent += 1;
                }
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.selected_agent = self.selected_agent.saturating_sub(1);
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.open_selected_agent();
                true
            },
            _ => false,
        }
    }

    /// Guests only, in stable name order: the host is never a Local scope of
    /// itself, and Local cycling must not follow rail slot churn.
    fn home_workspace_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .sessions
            .session_ui_infos
            .iter()
            .map(|session| session.name.clone())
            .filter(|name| Some(name) != self.session_name.as_ref())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn toggle_agent_scope(&mut self) {
        let workspaces = self.home_workspace_names();
        self.agent_scope = next_agent_panel_scope(&self.agent_scope, None, &workspaces);
        self.selected_agent = 0;
        self.home_notice = None;
    }

    fn plan_selected_agent_navigation(&self) -> HomeNavigationPlan {
        let destination = self.agent_runs.as_deref().and_then(|runs| {
            home_runs_in_scope(runs, &self.agent_scope)
                .get(self.selected_agent)
                .map(|run| run.operator_session.clone())
        });
        plan_home_navigation(
            destination.as_deref(),
            None,
            self.session_name.as_deref(),
            &self.live_workspace_names(),
            DestinationCard::Shared,
            &[],
        )
    }

    /// Home → the agent's existing interactive destination on the shared
    /// Workspace card, projected by the host rail. Refusal launches nothing.
    fn open_selected_agent(&mut self) {
        match self.plan_selected_agent_navigation() {
            HomeNavigationPlan::Refuse(refusal) => {
                self.home_notice = Some(refusal.to_string());
            },
            HomeNavigationPlan::ProjectShared { session, tab } => {
                self.home_notice = Some(format!(
                    "Opening `{session}` in {VC_SHARED_WORKSPACE_TAB_NAME}."
                ));
                #[cfg(target_family = "wasm")]
                {
                    pipe_message_to_plugin(
                        MessageToPlugin::new(VC_GUEST_SURFACE_MESSAGE)
                            .with_payload(project_guest_payload(&session, tab))
                            .with_plugin_url(VC_FRAME_HOST_PLUGIN_ALIAS)
                            .with_plugin_config(host_session_manager_configuration()),
                    );
                    go_to_tab_name(VC_SHARED_WORKSPACE_TAB_NAME);
                }
                #[cfg(not(target_family = "wasm"))]
                let _ = (session, tab);
            },
            HomeNavigationPlan::FocusOwnCard { .. } | HomeNavigationPlan::OpenOwnCard { .. } => {
                self.home_notice = Some(
                    "Refused: own cards are not wired in this build. Nothing was launched."
                        .to_owned(),
                );
            },
        }
    }

    /// Empty-host overview event loop: the pane is alive only while no guest
    /// is projected, so every event it consumes is overview truth.
    fn update_workspace_surface(&mut self, event: Event) -> bool {
        match event {
            Event::ModeUpdate(mode_info) => {
                self.colors = Colors::new(mode_info.style.colors);
                true
            },
            Event::Timer(_) => {
                self.refresh_timer_armed = false;
                self.surface_tick = self.surface_tick.wrapping_add(1);
                self.arm_refresh_timer();
                true
            },
            Event::SessionUpdate(session_infos, resurrectable_session_list) => {
                self.resurrectable_sessions
                    .update(resurrectable_session_list);
                self.update_session_infos(session_infos);
                self.clamp_surface_selection();
                true
            },
            Event::CustomMessage(message, payload) if message == VC_LIVE_RUNS_MESSAGE => {
                self.apply_live_runs_payload(&payload)
            },
            Event::RunCommandResult(exit_code, stdout, stderr, context)
                if context.contains_key(VC_GUEST_CREATE_CONTEXT_KEY) =>
            {
                self.handle_guest_create_result(
                    exit_code,
                    &stdout,
                    &stderr,
                    context.get(VC_GUEST_CREATE_CONTEXT_KEY).map(String::as_str),
                    context.get(VC_GUEST_CREATE_REQUEST_KEY).map(String::as_str),
                )
            },
            Event::Key(key) => self.handle_workspace_surface_key(key),
            Event::Mouse(mouse_event) => self.handle_workspace_surface_mouse(mouse_event),
            _ => false,
        }
    }

    fn handle_workspace_surface_key(&mut self, key: KeyWithModifier) -> bool {
        if self.error.is_some() {
            self.error = None;
            return true;
        }
        match key.bare_key {
            BareKey::Down if key.has_no_modifiers() => {
                if self.surface_selected + 1 < self.sessions.session_ui_infos.len() {
                    self.surface_selected += 1;
                }
                true
            },
            BareKey::Up if key.has_no_modifiers() => {
                self.surface_selected = self.surface_selected.saturating_sub(1);
                true
            },
            BareKey::Enter if key.has_no_modifiers() => {
                self.open_surface_selected_workspace();
                true
            },
            BareKey::Char('n') if key.has_no_modifiers() => {
                self.create_surface_workspace();
                true
            },
            _ => false,
        }
    }

    fn handle_workspace_surface_mouse(&mut self, mouse_event: Mouse) -> bool {
        match mouse_event {
            Mouse::LeftClick(line, _column) => {
                let Ok(row) = usize::try_from(line) else {
                    return false;
                };
                // Keep hover on the row we just activated (OS list selection).
                self.surface_hover_row = Some(row);
                // Header / footer / blank rows are absent from the map.
                let Some(target) = self.surface_click_map.get(&row).copied() else {
                    return false;
                };
                match target {
                    SurfaceClickTarget::Workspace(index) => {
                        if index >= self.sessions.session_ui_infos.len() {
                            return false;
                        }
                        self.surface_selected = index;
                        self.open_surface_selected_workspace();
                        true
                    },
                    SurfaceClickTarget::None => false,
                }
            },
            Mouse::Hover(line, _column) => {
                let next = usize::try_from(line)
                    .ok()
                    .filter(|row| self.surface_click_map.contains_key(row));
                if self.surface_hover_row != next {
                    self.surface_hover_row = next;
                    true
                } else {
                    false
                }
            },
            Mouse::ScrollUp(_) => {
                self.surface_selected = self.surface_selected.saturating_sub(1);
                true
            },
            Mouse::ScrollDown(_) => {
                if self.surface_selected + 1 < self.sessions.session_ui_infos.len() {
                    self.surface_selected += 1;
                }
                true
            },
            // Right-click / middle not mapped. Shift+click is client passthrough.
            _ => false,
        }
    }

    fn clamp_surface_selection(&mut self) {
        let max = self.sessions.session_ui_infos.len().saturating_sub(1);
        self.surface_selected = self.surface_selected.min(max);
    }

    /// Enter / click on a workspace card: project that guest into this pane.
    /// Plugin-sourced `vc.guest-surface.v1` messages from non-owner plugins
    /// are fail-closed dropped by the server (guest_surface_publisher_routes),
    /// so the request rides the guarded CLI pipe — the same route
    /// `project-workspace` and the guest-create handoff take.
    fn open_surface_selected_workspace(&mut self) {
        let Some(session) = self
            .sessions
            .session_ui_infos
            .get(self.surface_selected)
            .map(|session| session.name.clone())
        else {
            self.surface_notice = Some("No workspace to open — n creates one.".to_owned());
            return;
        };
        self.surface_notice = Some(format!("Opening `{session}` in this pane."));
        match self.plan_host_handoff() {
            HostHandoff::CliProject { host } => {
                let argv = project_workspace_argv(&host, &session, None);
                let args: Vec<&str> = argv.iter().map(String::as_str).collect();
                run_command(&args, BTreeMap::new());
            },
            // PendingOnSelf is the frame-host arm; the surface is never the
            // projection owner, so both remaining arms land here.
            HostHandoff::PendingOnSelf | HostHandoff::DetachedNotice => {
                self.surface_notice = Some(format!(
                    "No running host owns this pane — project with:\n  vc-frame --session <host> project-workspace {session}"
                ));
            },
        }
    }

    /// `n` on the empty-host overview: a new auto-named guest workspace on the
    /// product guest layout, then the host handoff projects it into this
    /// pane. Same plan/spawn path as the single-screen New Session flow.
    fn create_surface_workspace(&mut self) {
        let existing = self.live_workspace_names();
        let plan = plan_new_workspace(
            false,
            self.session_name.as_deref(),
            None,
            None,
            None,
            &existing,
        );
        self.apply_new_workspace_plan(plan);
    }

    fn render_workspace_surface_overview(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        let spinner = SURFACE_SPINNER_FRAMES[self.surface_tick % SURFACE_SPINNER_FRAMES.len()];
        let overview = SurfaceOverview {
            sessions: &self.sessions.session_ui_infos,
            session_list_seen: self.session_list_seen,
            exited_count: self.resurrectable_sessions.all_resurrectable_sessions.len(),
            runs: self.agent_runs.as_deref(),
            runs_degraded: self.live_runs_feed_degraded,
            selected: self.surface_selected,
            spinner,
            notice: self.surface_notice.as_deref().or(self.error.as_deref()),
            pending_create: self
                .pending_guest_create
                .as_ref()
                .map(|(_, pending)| pending.session.as_str()),
        };
        let lines = workspace_surface_overview_lines(&overview);
        self.surface_click_map.clear();
        for (row, line) in lines.into_iter().take(rows).enumerate() {
            if line.target != SurfaceClickTarget::None {
                self.surface_click_map.insert(row, line.target);
            }
            let fitted = fit_rail_line(&line.text, cols);
            let fitted_chars = fitted.chars().count();
            let mut text = Text::new(fitted);
            match line.tone {
                SurfaceTone::Accent => text = text.color_range(1, 0..fitted_chars),
                SurfaceTone::Dim => text = text.color_range(2, 0..fitted_chars),
                SurfaceTone::Normal => {},
            }
            if line.selected || self.surface_hover_row == Some(row) {
                text = text.selected();
            }
            print_text_with_coordinates(text, 0, row, None, None);
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
    fn session_rail_rows(&self, mode: RailWidthMode) -> Vec<SessionRailRow> {
        // None = no successful feed yet (unknown), Some(n) = confirmed count;
        // the two must never render as the same "0".
        let active_runs = self.projected_active_run_count();
        session_rail_rows_with_truth(
            &self.sessions.session_ui_infos,
            mode,
            self.frame_host,
            active_runs,
            self.live_runs_feed_degraded,
        )
    }

    fn projected_active_run_count(&self) -> Option<usize> {
        if self.live_runs_feed_degraded {
            None
        } else {
            self.agent_runs
                .as_ref()
                .map(|runs| runs.iter().filter(|run| is_current_census_run(run)).count())
        }
    }
    fn render_session_rail(&mut self, rows: usize, cols: usize) {
        if rows == 0 || cols == 0 {
            return;
        }
        self.ensure_rail_selection();
        let mode = RailWidthMode::from_cols(cols);

        let rail_rows = self.session_rail_rows(mode);

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
        let header_text = rail_header_with_truth(
            mode,
            session_count,
            current_session_name,
            self.session_list_seen,
            self.session_list_degraded,
        );
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
        // Two legal zones: the host section is pinned (reserved first, never
        // scrolled); only workspace rows move through the remaining window.
        let host_count = rail_rows
            .iter()
            .take_while(|row| row.kind.is_host_section())
            .count();
        let workspace_rows = &rail_rows[host_count..];
        let selected_index = self.sessions.selected_index.0;
        let selected_row_index = selected_index.and_then(|selected_session_index| {
            workspace_rows
                .iter()
                .position(|row| row.kind == SessionRailRowKind::Session(selected_session_index))
        });
        let plan = rail_render_plan(
            list_rows,
            host_count,
            workspace_rows.len(),
            selected_row_index,
        );
        let mut row = chrome_rows;

        let rows_to_render = rail_rows[..plan.host_visible]
            .iter()
            .chain(&workspace_rows[plan.workspace_start..plan.workspace_end]);
        for rail_row in rows_to_render {
            let fitted = match rail_row.kind {
                // The Active runs row budgets count/status first: narrow rails
                // collapse the label, never the count or its truth marker.
                SessionRailRowKind::Host(HostRow::ActiveRuns) => fit_active_runs_row(
                    self.projected_active_run_count(),
                    self.live_runs_feed_degraded,
                    cols,
                ),
                _ => fit_rail_line(&rail_row.text, cols),
            };
            let fitted_chars = fitted.chars().count();
            let mut text = Text::new(fitted.clone());
            match rail_row.kind {
                SessionRailRowKind::HostTitle => {
                    if mode != RailWidthMode::Dense {
                        let title_chars = "Operator Frame".chars().count();
                        text = text.color_range(1, 0..title_chars.min(fitted_chars));
                    }
                },
                SessionRailRowKind::Host(_host_row) => {
                    // Host rows never carry the fisheye ◉ and stay clean.
                },
                SessionRailRowKind::Separator => {
                    text = text.color_range(2, 0..fitted_chars);
                },
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
            let target = rail_row_click_target(&rail_row.kind);
            if target != RailClickTarget::None {
                self.rail_click_map.insert(row, target);
            }
            print_text_with_coordinates(text, 0, row, None, None);
            row += 1;
        }

        if let Some(footer) = plan.footer
            && row < rows
        {
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
            // Explicit return to the host's Home. Moves only this client;
            // the projected guest keeps its visitor, process and PTY.
            BareKey::Char('h') if key.has_no_modifiers() && self.frame_host => {
                go_to_tab(VC_HOME_TAB_POSITION);
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
    fn activate_host_row(&mut self, host_row: HostRow) -> bool {
        match host_row_plan(host_row) {
            HostRowPlan::FocusGuestOverview => {
                go_to_tab_name(VC_SHARED_WORKSPACE_TAB_NAME);
                true
            },
            HostRowPlan::OpenCensus => {
                go_to_tab(VC_HOME_TAB_POSITION);
                true
            },
            HostRowPlan::Doctor => {
                run_owned_argv(&doctor_pane_argv(), BTreeMap::new());
                true
            },
            HostRowPlan::ChooseProject => {
                let mut context = BTreeMap::new();
                context.insert(VC_OPEN_PROJECT_CONTEXT_KEY.to_owned(), "chooser".to_owned());
                run_owned_argv(&project_chooser_argv(), context);
                true
            },
            HostRowPlan::Inert => false,
        }
    }

    /// Folder picker finished. A cancel (non-zero, empty path) stays quiet.
    /// A chosen folder resumes through the same `vc-start resume --repo` Start Here uses.
    fn finish_open_project(&mut self, exit_code: Option<i32>, stdout: &[u8]) -> bool {
        if exit_code != Some(0) {
            return false;
        }
        let Ok(text) = std::str::from_utf8(stdout) else {
            return false;
        };
        let path = text.trim().trim_end_matches('/');
        if path.is_empty() {
            return false;
        }
        run_owned_argv(&resume_project_argv(path), BTreeMap::new());
        false
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
                    RailClickTarget::Host(host_row) => self.activate_host_row(host_row),
                    RailClickTarget::None => false,
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
            context.insert(
                "vc_workspace_pipe_client".to_owned(),
                pipe_client.to_owned(),
            );
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
        if self.home_resident {
            return self.handle_home_key(key);
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
            if should_hide_manager_after_guest_create(self.frame_host, self.workspace_surface) {
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
        if !self.workspace_surface {
            // The surface renders the error inside its own overview; floating
            // managers must be pulled back on screen instead.
            show_self(true);
        }
        true
    }

    fn publish_guest_surface(&mut self, session_infos: &[SessionInfo]) {
        let Some(payload) = plan_guest_surface_publication(
            self.frame_host,
            self.visited_guest_name.as_deref(),
            session_infos,
            self.own_plugin_id,
        ) else {
            return;
        };
        #[cfg(target_family = "wasm")]
        for alias in [VC_COMPACT_BAR_PLUGIN_ALIAS, VC_STATUS_BAR_PLUGIN_ALIAS] {
            pipe_message_to_plugin(
                MessageToPlugin::new(VC_GUEST_SURFACE_MESSAGE)
                    .with_plugin_url(alias)
                    .with_payload(payload.clone()),
            );
        }
        #[cfg(not(target_family = "wasm"))]
        let _ = payload;
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
        let previous_degraded = self.session_list_degraded;
        let first_payload = !self.session_list_seen;
        self.session_list_seen = true;
        let previous_rail_projection = self
            .is_rail
            .then(|| self.session_rail_rows(RailWidthMode::Wide));
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
        // The host filter must never empty the rail. Self-hosting layouts
        // (default_layout "host", frame_host rail) make every session an
        // internal host; filtering them all out renders "SESSIONS 0" while
        // sessions exist — a lie. Fall back to the unfiltered set and mark
        // the list degraded until a non-host session appears. Web-forbidden
        // sessions stay hidden: that filter is a permission, not a design.
        if self.is_rail && session_ui_infos.is_empty() && !session_infos.is_empty() {
            session_ui_infos = session_infos
                .iter()
                .filter_map(|s| {
                    if self.is_web_client && !s.web_clients_allowed {
                        None
                    } else {
                        let mut ui = SessionUiInfo::from_session_info(s);
                        if self.frame_host {
                            ui.is_current_session = self.visited_guest_name.as_deref()
                                == Some(ui.name.as_str())
                                || (self.visited_guest_name.is_none() && s.is_current_session);
                        }
                        Some(ui)
                    }
                })
                .collect();
            self.session_list_degraded = !session_ui_infos.is_empty();
        } else {
            self.session_list_degraded = false;
        }
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
        first_payload
            || self.session_list_degraded != previous_degraded
            || previous_rail_projection
                .is_none_or(|previous| previous != self.session_rail_rows(RailWidthMode::Wide))
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
    fn durable_slots_hold_clustered_age_order_across_repeated_session_updates() {
        let session = |name: &str, age: u64, rail_order: u64| SessionUiInfo {
            name: name.to_owned(),
            title: name.to_owned(),
            tabs: vec![],
            connected_users: 0,
            is_current_session: false,
            creation_time: Duration::from_secs(age),
            rail_order,
        };
        let mut sessions = SessionList::default();
        // 0/2/4 and then 1/3/5 model the elapsed socket ages reported by two
        // ticks. Slots, rather than those moving ages, define the rail.
        sessions.set_sessions(
            vec![
                session("third", 0, 3),
                session("first", 4, 1),
                session("second", 2, 2),
            ],
            vec![],
        );
        assert_eq!(
            sessions.all_other_sessions(),
            vec!["first", "second", "third"]
        );
        sessions.set_sessions(
            vec![
                session("third", 1, 3),
                session("first", 5, 1),
                session("second", 3, 2),
            ],
            vec![],
        );
        assert_eq!(
            sessions.all_other_sessions(),
            vec!["first", "second", "third"]
        );
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
        let wide = session_rail_rows_with_truth(&sessions, RailWidthMode::Wide, false, None, false);
        let dense =
            session_rail_rows_with_truth(&sessions, RailWidthMode::Dense, false, None, false);
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
        let mut state = State {
            frame_host: true,
            visited_guest_name: Some("workspace-a".to_owned()),
            ..Default::default()
        };
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

    #[test]
    fn rail_falls_back_to_unfiltered_list_when_every_session_is_a_host() {
        let mut state = State {
            is_rail: true,
            frame_host: true,
            ..Default::default()
        };
        let host = |name: &str, is_current: bool| SessionInfo {
            name: name.to_owned(),
            plugins: BTreeMap::from([(
                1,
                PluginInfo {
                    location: "session-manager".to_owned(),
                    configuration: BTreeMap::from([("frame_host".to_owned(), "true".to_owned())]),
                },
            )]),
            is_current_session: is_current,
            ..SessionInfo::default()
        };
        // Self-hosting layouts: every session is an internal host and the
        // filter would empty the list — the rail must show the unfiltered
        // set and mark it degraded, never "0".
        let changed = state.update_session_infos(vec![host("alpha", true), host("beta", false)]);
        assert!(changed);
        assert!(state.session_list_degraded);
        assert_eq!(state.sessions.session_ui_infos.len(), 2);
        assert_eq!(
            format_session_rail_entry(&state.sessions.session_ui_infos[0], 1, RailWidthMode::Wide),
            "01 ◉ Alpha"
        );
        assert_eq!(
            format_session_rail_entry(&state.sessions.session_ui_infos[1], 2, RailWidthMode::Wide),
            "02 ○ Beta"
        );
        // A non-host session restores the filter and clears the marker.
        let guest = SessionInfo {
            name: "workspace-c".to_owned(),
            ..SessionInfo::default()
        };
        state.update_session_infos(vec![host("alpha", true), guest]);
        assert!(!state.session_list_degraded);
        let names: Vec<&str> = state
            .sessions
            .session_ui_infos
            .iter()
            .map(|session| session.name.as_str())
            .collect();
        assert_eq!(names, vec!["workspace-c"]);
    }

    #[test]
    fn rail_header_marks_unknown_and_degraded_instead_of_lying_zero() {
        // No payload yet: never "0".
        assert_eq!(
            rail_header_with_truth(RailWidthMode::Wide, 0, None, false, false),
            "SESSIONS ?"
        );
        assert_eq!(
            rail_header_with_truth(RailWidthMode::Dense, 0, None, false, false),
            "S?"
        );
        // Filter fallback on screen: marked, with the real count.
        assert_eq!(
            rail_header_with_truth(RailWidthMode::Wide, 1, Some("alpha"), true, true),
            "SESSIONS 1 · alpha ~"
        );
        assert_eq!(
            rail_header_with_truth(RailWidthMode::Dense, 1, None, true, true),
            "S1~"
        );
        // Healthy path unchanged.
        assert_eq!(
            rail_header_with_truth(RailWidthMode::Wide, 2, Some("alpha"), true, false),
            "SESSIONS 2 · alpha"
        );
    }

    fn session(name: &str, is_current_session: bool) -> SessionUiInfo {
        SessionUiInfo {
            name: name.to_owned(),
            title: name.to_owned(),
            tabs: vec![],
            connected_users: 1,
            is_current_session,
            rail_order: 0,
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
        assert_eq!(lines[3], "Current");
        assert_eq!(lines[4], "● vc-frame · FUX");
        assert_eq!(lines[5], "  codex · implement · running");
        assert!(lines[6].contains(&run.run_id));
        assert!(lines.iter().any(|line| line == "Stalled"));
        assert!(lines.iter().any(|line| line == "Recent"));
        assert!(!lines[4].contains(&run.run_id));
    }

    fn two_workspace_runs() -> Vec<AgentRunUiInfo> {
        vec![
            agent_run(
                r#"{"run_id":"run-a1","repo":"alpha","task_title":"A1","agent":"claude","operator_session":"workspace-a"}"#,
            ),
            agent_run(
                r#"{"run_id":"run-b1","repo":"beta","task_title":"B1","agent":"codex","operator_session":"workspace-b"}"#,
            ),
            agent_run(
                r#"{"run_id":"run-a2","repo":"alpha","task_title":"A2","agent":"kimi","operator_session":"workspace-a"}"#,
            ),
            agent_run(r#"{"run_id":"run-headless","repo":"gamma","task_title":"No panel"}"#),
        ]
    }

    fn home_state(runs: Option<Vec<AgentRunUiInfo>>) -> State {
        let mut state = State {
            home_resident: true,
            workspace_dashboard: true,
            session_name: Some("frame-host".to_owned()),
            agent_runs: runs,
            ..Default::default()
        };
        state.sessions.set_sessions(
            vec![
                session("frame-host", true),
                session("workspace-a", false),
                session("workspace-b", false),
            ],
            vec![],
        );
        state
    }

    fn bare(key: BareKey) -> KeyWithModifier {
        KeyWithModifier::new(key)
    }

    #[test]
    fn command_bridge_home_global_lists_agents_from_both_workspaces() {
        let runs = two_workspace_runs();
        let lines = home_agent_panel_lines(Some(&runs), false, &AgentPanelScope::Global, 0, None);
        assert_eq!(lines[0], "⌂ Home");
        assert!(lines[1].starts_with("[Global] agent panel access"));
        assert!(lines.contains(&"4 of 4 agents in scope".to_owned()));
        for title in ["alpha · A1", "beta · B1", "alpha · A2", "gamma · No panel"] {
            assert!(
                lines.iter().any(|line| line.ends_with(title)),
                "Global must show {title}: {lines:?}"
            );
        }
        assert!(lines.contains(&"    workspace none linked".to_owned()));
    }

    #[test]
    fn command_bridge_home_local_lists_only_the_selected_workspace() {
        let runs = two_workspace_runs();
        let local_a = AgentPanelScope::Local("workspace-a".to_owned());
        let lines = home_agent_panel_lines(Some(&runs), false, &local_a, 0, None);
        assert!(lines[1].starts_with("[Local: workspace-a] agent panel access"));
        assert!(lines.contains(&"2 of 4 agents in scope".to_owned()));
        assert!(lines.iter().any(|line| line.ends_with("alpha · A1")));
        assert!(lines.iter().any(|line| line.ends_with("alpha · A2")));
        assert!(!lines.iter().any(|line| line.contains("beta · B1")));
        assert!(!lines.iter().any(|line| line.contains("No panel")));
        let local_b = AgentPanelScope::Local("workspace-b".to_owned());
        let lines = home_agent_panel_lines(Some(&runs), false, &local_b, 0, None);
        assert!(lines.contains(&"1 of 4 agents in scope".to_owned()));
        assert!(lines.iter().any(|line| line.ends_with("beta · B1")));
    }

    #[test]
    fn command_bridge_home_unknown_feed_is_not_zero_agents() {
        let lines = home_agent_panel_lines(None, false, &AgentPanelScope::Global, 0, None);
        assert!(lines.iter().any(|line| line.starts_with("UNAVAILABLE")));
        assert!(!lines.iter().any(|line| line.contains("0 of")));
        let degraded = home_agent_panel_lines(Some(&[]), true, &AgentPanelScope::Global, 0, None);
        assert!(degraded[2].starts_with("DEGRADED"));
        assert!(degraded.contains(&"0 of 0 agents in scope".to_owned()));
    }

    #[test]
    fn command_bridge_home_tab_key_toggles_global_local_over_guests_only() {
        let mut state = home_state(Some(two_workspace_runs()));
        assert_eq!(state.agent_scope, AgentPanelScope::Global);
        assert!(state.handle_key(bare(BareKey::Tab)));
        assert_eq!(
            state.agent_scope,
            AgentPanelScope::Local("workspace-a".to_owned()),
            "the host is never a Local workspace of itself"
        );
        assert!(state.handle_key(bare(BareKey::Tab)));
        assert_eq!(
            state.agent_scope,
            AgentPanelScope::Local("workspace-b".to_owned())
        );
        assert!(state.handle_key(bare(BareKey::Tab)));
        assert_eq!(state.agent_scope, AgentPanelScope::Global);
    }

    #[test]
    fn command_bridge_home_agent_without_destination_refuses_navigation() {
        let mut state = home_state(Some(two_workspace_runs()));
        for _ in 0..3 {
            state.handle_key(bare(BareKey::Down));
        }
        assert_eq!(
            state.selected_agent, 3,
            "headless run is the fourth Global row"
        );
        assert_eq!(
            state.plan_selected_agent_navigation(),
            HomeNavigationPlan::Refuse(NavigationRefusal::MissingDestination)
        );
        assert!(state.handle_key(bare(BareKey::Enter)));
        let notice = state.home_notice.clone().unwrap_or_default();
        assert!(
            notice.starts_with("Refused") && notice.contains("Nothing was launched"),
            "missing destination must refuse, not spawn a substitute shell: {notice}"
        );
        assert!(!state.handle_key(bare(BareKey::Char('x'))));
    }

    #[test]
    fn command_bridge_home_gone_workspace_refuses_and_live_one_projects_shared() {
        let runs = vec![
            agent_run(r#"{"run_id":"run-gone","repo":"old","operator_session":"workspace-gone"}"#),
            agent_run(r#"{"run_id":"run-b","repo":"beta","operator_session":"workspace-b"}"#),
        ];
        let mut state = home_state(Some(runs));
        assert_eq!(
            state.plan_selected_agent_navigation(),
            HomeNavigationPlan::Refuse(NavigationRefusal::DestinationGone {
                session: "workspace-gone".to_owned()
            })
        );
        state.handle_key(bare(BareKey::Down));
        assert_eq!(
            state.plan_selected_agent_navigation(),
            HomeNavigationPlan::ProjectShared {
                session: "workspace-b".to_owned(),
                tab: None,
            }
        );
        state.handle_key(bare(BareKey::Enter));
        assert_eq!(
            state.home_notice.as_deref(),
            Some("Opening `workspace-b` in Workspace.")
        );
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
    fn rail_lists_every_projected_tab_not_only_live_processes() {
        let mut alpha = session("alpha", true);
        alpha.tabs = vec![
            TabUiInfo::for_rail_test("Start here", false, "about", 1),
            TabUiInfo::for_rail_test("Shell", false, "zsh", 1),
            TabUiInfo::for_rail_test("Agents", true, "workshop", 0),
            TabUiInfo::for_rail_test("claude", false, "claude", 1),
        ];
        let beta = session("beta", false);

        let rows = session_rail_rows(&[alpha.clone(), beta]);
        let text: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();

        // Organs first (Agents, Shell), then the rest in source order — the
        // same order project_guest_organs gives the tab strip. "Start here"
        // is not stuck at the top, and the idle Agents tab is not dropped.
        assert_eq!(
            text,
            vec![
                "01 ◉ alpha",
                "   ◉ Agents",
                "   · Shell · zsh",
                "   · Start here · about",
                "   · claude",
                "02 ○ beta",
            ]
        );
        let names: Vec<String> = workspace_surface::project_surface_organs(&alpha.tabs)
            .into_iter()
            .map(|organ| organ.name)
            .collect();
        assert_eq!(
            names,
            ["Agents", "Shell", "Start here", "claude"].map(str::to_owned)
        );
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
    fn live_runs_feed_tombstone_marks_unknown_until_recovery() {
        let mut state = State {
            is_rail: true,
            ..Default::default()
        };
        assert!(
            state
                .apply_live_runs_payload(r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"a"}]}"#)
        );
        assert_eq!(state.projected_active_run_count(), Some(1));
        assert!(state.apply_live_runs_payload(
            r#"{"schema":"vc.live-runs.v1","available":false,"runs":[]}"#
        ));
        assert!(state.live_runs_feed_degraded);
        assert_eq!(state.projected_active_run_count(), None);
        assert!(!state.apply_live_runs_payload(
            r#"{"schema":"vc.live-runs.v1","available":false,"runs":[]}"#
        ));

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

        assert_eq!(live.len(), 3, "idle tabs stay on the projection");
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
                tab_position: 1,
            }
        );
        assert_eq!(
            live[2].kind,
            SessionRailRowKind::LiveProcess {
                session_index: 0,
                tab_position: 2,
            }
        );
    }

    #[test]
    fn host_rows_open_existing_surfaces_and_leave_config_inert() {
        assert_eq!(
            host_row_plan(HostRow::Dashboard),
            HostRowPlan::FocusGuestOverview
        );
        assert_eq!(host_row_plan(HostRow::ActiveRuns), HostRowPlan::OpenCensus);
        assert_eq!(host_row_plan(HostRow::Doctor), HostRowPlan::Doctor);
        assert_eq!(host_row_plan(HostRow::Projects), HostRowPlan::ChooseProject);
        assert_eq!(host_row_plan(HostRow::Config), HostRowPlan::Inert);
        let doctor = doctor_pane_argv();
        assert!(
            doctor
                .windows(2)
                .any(|pair| pair[0] == "--floating" && pair[1] == "--name")
        );
        assert!(doctor.iter().any(|arg| arg.contains("vibecrafted doctor")));
        let chooser = project_chooser_argv().join(" ");
        assert!(chooser.contains("osascript"));
        assert!(chooser.contains("Open a Vibecrafted project"));
        assert_eq!(
            resume_project_argv("/tmp/repo"),
            vec![
                "vc-start".to_owned(),
                "resume".to_owned(),
                "--repo".to_owned(),
                "/tmp/repo".to_owned(),
            ]
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
        let mut state = State {
            frame_host: false,
            ..Default::default()
        };
        assert!(!state.handle_guest_surface_message(&activate_guest_tab_payload("workspace-a", 1)));
        assert!(state.visited_guest_name.is_none());
        assert!(state.pending_guest_visit.is_none());
        assert_eq!(
            host_cli_guest_surface_visit(
                false,
                VC_GUEST_SURFACE_MESSAGE,
                Some(&activate_guest_tab_payload("workspace-b", 0)),
            ),
            None,
            "ordinary floating manager must not own the CLI activate_tab pipe"
        );
    }

    #[test]
    fn host_cli_activate_tab_without_request_id_is_still_an_owning_rail_visit() {
        assert_eq!(
            host_cli_guest_surface_visit(
                true,
                VC_GUEST_SURFACE_MESSAGE,
                Some(&activate_guest_tab_payload("workspace-b", 0)),
            ),
            Some(("workspace-b".to_owned(), Some(0))),
            "broadcast activate_tab must take the pipe_id projection path, not handle_guest_surface"
        );
    }

    #[test]
    fn ordinary_manager_ignores_project_and_does_not_reconnect() {
        let mut state = State {
            frame_host: false,
            ..Default::default()
        };
        assert!(!state.handle_guest_surface_message(&project_guest_payload("workspace-b", None)));
        assert!(state.visited_guest_name.is_none());
    }

    #[test]
    fn host_project_pipe_sets_pending_when_guest_pane_is_missing() {
        let mut state = State {
            frame_host: true,
            ..Default::default()
        };
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
        let mut state = State {
            frame_host: true,
            ..Default::default()
        };
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
        let mut state = State {
            pending_guest_create: Some((
                "create-new".to_owned(),
                PendingGuestRequest {
                    session: "workspace-a".to_owned(),
                    tab: Some(1),
                },
            )),
            ..Default::default()
        };
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
        let mut state = State {
            frame_host: false,
            current_session_is_host: false,
            host_session_name: Some("frame-host-a".to_owned()),
            ..Default::default()
        };
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
        let mut state = State {
            frame_host: true,
            pending_guest_create: Some((
                "create-new".to_owned(),
                PendingGuestRequest {
                    session: "workspace-b".to_owned(),
                    tab: Some(2),
                },
            )),
            ..Default::default()
        };
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
        let discovered = SessionInfo {
            name: pending.session.clone(),
            ..Default::default()
        };
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

    #[test]
    fn host_section_renders_above_workspaces_only_in_frame_host() {
        let sessions = vec![session("workspace-a", true), session("workspace-b", false)];

        // Plain session-manager rail (frame_host == false) stays flat.
        let plain =
            session_rail_rows_with_truth(&sessions, RailWidthMode::Wide, false, None, false);
        assert_eq!(plain.len(), 2);
        assert_eq!(plain[0].kind, SessionRailRowKind::Session(0));
        assert_eq!(plain[1].kind, SessionRailRowKind::Session(1));

        // When frame_host == true, the rail renders a pinned HOST section above the session list.
        let rows =
            session_rail_rows_with_truth(&sessions, RailWidthMode::Wide, true, Some(3), false);
        assert_eq!(rows.len(), 7 + 2);
        assert_eq!(rows[0].kind, SessionRailRowKind::HostTitle);
        assert_eq!(rows[0].text, "Operator Frame");
        assert_eq!(rows[1].kind, SessionRailRowKind::Host(HostRow::Dashboard));
        assert_eq!(rows[1].text, "⌂ Dashboard");
        assert_eq!(rows[2].kind, SessionRailRowKind::Host(HostRow::ActiveRuns));
        assert_eq!(rows[2].text, "❖ Active runs · 3");
        assert_eq!(rows[3].kind, SessionRailRowKind::Host(HostRow::Config));
        assert_eq!(rows[3].text, "⚙︎ Config");
        assert_eq!(rows[4].kind, SessionRailRowKind::Host(HostRow::Doctor));
        assert_eq!(rows[4].text, "· Doctor");
        assert_eq!(rows[5].kind, SessionRailRowKind::Host(HostRow::Projects));
        assert_eq!(rows[5].text, "✧ Projects");
        assert_eq!(rows[6].kind, SessionRailRowKind::Separator);
        assert_eq!(rows[7].kind, SessionRailRowKind::Session(0));
        assert_eq!(rows[8].kind, SessionRailRowKind::Session(1));

        // Host rows never carry the fisheye ◉ and never count toward SESSIONS N.
        for host_row in &rows[0..7] {
            assert!(
                !host_row.text.contains('◉'),
                "host row {:?} carried fisheye",
                host_row.text
            );
        }
        let session_count = working_session_indices(&sessions).len();
        assert_eq!(
            session_count, 2,
            "host rows must not count toward SESSIONS N"
        );
        assert_eq!(
            rail_header_with_truth(
                RailWidthMode::Wide,
                session_count,
                Some("workspace-a"),
                true,
                false
            ),
            "SESSIONS 2 · workspace-a"
        );

        // Click targets: host rows are reserved, title and separator map to None.
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::HostTitle),
            RailClickTarget::None
        );
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Separator),
            RailClickTarget::None
        );
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Host(HostRow::Dashboard)),
            RailClickTarget::Host(HostRow::Dashboard)
        );
        assert_eq!(
            rail_row_click_target(&SessionRailRowKind::Host(HostRow::ActiveRuns)),
            RailClickTarget::Host(HostRow::ActiveRuns)
        );
    }

    #[test]
    fn active_runs_row_tracks_feed_mutations_and_hides_stale_count() {
        let mut state = State {
            is_rail: true,
            frame_host: true,
            ..Default::default()
        };
        let payload_3 = r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"r1"},{"run_id":"r2"},{"run_id":"r3"}]}"#;
        assert!(state.apply_live_runs_payload(payload_3));
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(3));
        assert!(!state.live_runs_feed_degraded);

        let rows = state.session_rail_rows(RailWidthMode::Wide);
        let active_runs_row = rows
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present in host section");
        assert_eq!(active_runs_row.text, "❖ Active runs · 3");

        let payload_5 = r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"r1"},{"run_id":"r2"},{"run_id":"r3"},{"run_id":"r4"},{"run_id":"r5"}]}"#;
        assert!(state.apply_live_runs_payload(payload_5));
        let rows_5 = state.session_rail_rows(RailWidthMode::Wide);
        let active_runs_row_5 = rows_5
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present after mutation");
        assert_eq!(active_runs_row_5.text, "❖ Active runs · 5");

        assert!(state.apply_live_runs_payload(
            r#"{"schema":"vc.live-runs.v1","available":false,"runs":[]}"#
        ));
        assert!(state.live_runs_feed_degraded);
        // The canvas may retain its last rows, but the counter must not claim
        // that the stale five are still live.
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(5));

        let rows_degraded = state.session_rail_rows(RailWidthMode::Wide);
        let active_runs_row_degraded = rows_degraded
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present in degraded host section");
        assert_eq!(active_runs_row_degraded.text, "❖ Active runs · ?");
    }

    #[test]
    fn dense_host_section_is_one_iconic_row() {
        let mut alpha = session("alpha", true);
        alpha.tabs = vec![TabUiInfo::for_rail_test("build", true, "cargo", 1)];
        let sessions = [alpha, session("beta", false)];

        let rows =
            session_rail_rows_with_truth(&sessions, RailWidthMode::Dense, true, Some(3), false);
        // Dense mode renders the host section as exactly one iconic row above the workspaces.
        assert_eq!(rows[0].kind, SessionRailRowKind::Host(HostRow::Dashboard));
        assert_eq!(rows[0].text, "⌂❖✧");
        assert_eq!(rows[1].kind, SessionRailRowKind::Session(0));

        // It fits narrow columns without shredding.
        for cols in [4, 6, 8, 13] {
            assert!(fit_rail_line(&rows[0].text, cols).width() <= cols);
        }

        // 1 iconic host row + 3 workspace rows (alpha + build tab + beta) = 4 total rows.
        assert_eq!(rows.len(), 1 + 3);
    }

    #[test]
    fn active_runs_row_distinguishes_unknown_empty_and_stale() {
        let mut state = State {
            is_rail: true,
            frame_host: true,
            ..Default::default()
        };

        // Unknown: no successful feed yet — the row must not lie a healthy 0.
        let rows = state.session_rail_rows(RailWidthMode::Wide);
        let row = rows
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present");
        assert_eq!(row.text, "❖ Active runs · ?");

        // A malformed FIRST payload remains unknown, never a healthy 0.
        assert!(state.apply_live_runs_payload("not json"));
        assert!(state.live_runs_feed_degraded);
        assert!(state.agent_runs.is_none());
        let rows = state.session_rail_rows(RailWidthMode::Wide);
        let row = rows
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present");
        assert_eq!(row.text, "❖ Active runs · ?");

        // A confirmed empty feed is a real 0, distinct from unknown.
        let mut confirmed = State {
            is_rail: true,
            frame_host: true,
            ..Default::default()
        };
        assert!(confirmed.apply_live_runs_payload(r#"{"schema":"vc.live-runs.v1","runs":[]}"#));
        let rows = confirmed.session_rail_rows(RailWidthMode::Wide);
        let row = rows
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present");
        assert_eq!(row.text, "❖ Active runs · 0");

        // A stale last-good payload is retained only for workspace details;
        // the counter becomes unknown immediately.
        assert!(confirmed.apply_live_runs_payload(
            r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"r1"},{"run_id":"r2"}]}"#
        ));
        assert!(confirmed.apply_live_runs_payload("garbage"));
        let rows = confirmed.session_rail_rows(RailWidthMode::Wide);
        let row = rows
            .iter()
            .find(|r| r.kind == SessionRailRowKind::Host(HostRow::ActiveRuns))
            .expect("active runs row present");
        assert_eq!(row.text, "❖ Active runs · ?");
    }

    #[test]
    fn active_runs_row_budgets_count_and_status_first_at_narrow_widths() {
        // Full label is 17 cols healthy; Normal is 14..=23.
        assert_eq!(
            fit_active_runs_row(Some(3), false, 23),
            fit_rail_line("❖ Active runs · 3", 23)
        );
        assert_eq!(
            fit_active_runs_row(Some(3), true, 23),
            fit_rail_line("❖ Active runs · ?", 23)
        );
        assert_eq!(fit_active_runs_row(Some(3), false, 17), "❖ Active runs · 3");
        // Below the full label the row collapses to `❖ N` — the count and
        // its truth marker survive; the label is what yields.
        for cols in [14, 15, 16] {
            assert_eq!(
                fit_active_runs_row(Some(3), false, cols),
                fit_rail_line("❖ 3", cols)
            );
            assert_eq!(
                fit_active_runs_row(Some(3), true, cols),
                fit_rail_line("❖ ?", cols)
            );
            assert_eq!(
                fit_active_runs_row(None, false, cols),
                fit_rail_line("❖ ?", cols)
            );
            assert_eq!(
                fit_active_runs_row(None, true, cols),
                fit_rail_line("❖ ?", cols)
            );
        }
        // Multi-digit counts keep the same contract.
        assert_eq!(
            fit_active_runs_row(Some(12), true, 14),
            fit_rail_line("❖ ?", 14)
        );
        assert_eq!(
            fit_active_runs_row(Some(12), true, 23),
            fit_rail_line("❖ Active runs · ?", 23)
        );
        // Every emitted cell fits the budget.
        for cols in [4, 6, 14, 15, 16, 17, 23] {
            for (count, degraded) in [
                (Some(3), false),
                (Some(12), true),
                (None, false),
                (None, true),
            ] {
                assert!(fit_active_runs_row(count, degraded, cols).width() <= cols);
            }
        }
    }

    #[test]
    fn dense_host_row_preserves_unknown_and_degraded_markers() {
        let sessions = vec![session("workspace-a", true)];
        let dense = |count: Option<usize>, degraded: bool| {
            session_rail_rows_with_truth(&sessions, RailWidthMode::Dense, true, count, degraded)
                .into_iter()
                .find(|r| r.is_host())
                .expect("dense host row present")
                .text
        };
        assert_eq!(dense(Some(3), false), "⌂❖✧");
        assert_eq!(dense(Some(3), true), "⌂❖?✧");
        assert_eq!(dense(None, false), "⌂❖?✧");
        assert_eq!(dense(None, true), "⌂❖?✧");
        // Dense is 6 columns: the unknown marker fits without shredding.
        for text in ["⌂❖✧", "⌂❖?✧"] {
            assert!(fit_rail_line(text, 6).width() <= 6);
        }
    }

    #[test]
    fn host_section_is_pinned_while_workspace_rows_scroll() {
        // Wide/Normal host section is 7 rows; Dense is 1 iconic row.
        // A low selection must never push the host section off the rail.
        let plan = rail_render_plan(10, 7, 10, Some(8));
        assert_eq!(plan.host_visible, 7, "host section pinned in full");
        // remaining = 3 → 1 footer + 2 entry rows, window anchored near 8.
        assert_eq!((plan.workspace_start, plan.workspace_end), (7, 9));
        assert_eq!(plan.footer.as_deref(), Some("+7 above +1 more"));

        // Everything fits: no footer, full workspace window.
        let plan = rail_render_plan(20, 7, 3, Some(0));
        assert_eq!(plan.host_visible, 7);
        assert_eq!((plan.workspace_start, plan.workspace_end), (0, 3));
        assert_eq!(plan.footer, None);

        // Dense (1 host row) with overflow and selection at the top.
        let plan = rail_render_plan(10, 1, 30, Some(0));
        assert_eq!(plan.host_visible, 1);
        assert_eq!((plan.workspace_start, plan.workspace_end), (0, 8));
        assert_eq!(plan.footer.as_deref(), Some("+22 more"));

        // Small-height overflow: a rail shorter than the host section renders
        // the pinned host rows (truncated) and no workspace rows.
        let plan = rail_render_plan(3, 7, 10, Some(5));
        assert_eq!(plan.host_visible, 3);
        assert_eq!((plan.workspace_start, plan.workspace_end), (0, 0));
        assert_eq!(plan.footer, None);

        // One spare row beyond the host section: one workspace row, no footer
        // (a footer would consume the only entry row to say nothing new).
        let plan = rail_render_plan(8, 7, 10, Some(4));
        assert_eq!(plan.host_visible, 7);
        assert_eq!((plan.workspace_start, plan.workspace_end), (4, 5));
        assert_eq!(plan.footer, None);
    }

    #[test]
    fn guest_surface_publication_covers_active_death_and_replay() {
        let guest = || SessionInfo {
            name: "workspace-a".to_owned(),
            tabs: vec![
                TabInfo {
                    name: "Agents".to_owned(),
                    active: true,
                    position: 0,
                    ..TabInfo::default()
                },
                TabInfo {
                    name: "Shell".to_owned(),
                    active: false,
                    position: 1,
                    ..TabInfo::default()
                },
            ],
            ..SessionInfo::default()
        };

        // Active guest: the payload carries the canonical session, an explicit
        // "active" status, the host plugin id and the guest's tabs.
        let payload =
            plan_guest_surface_publication(true, Some("workspace-a"), &[guest()], Some(7))
                .expect("active guest publishes");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["session"], "workspace-a");
        assert_eq!(value["status"], "active");
        assert_eq!(value["host_plugin_id"], 7);
        assert_eq!(value["tabs"][0]["name"], "Agents");
        assert_eq!(value["tabs"][1]["position"], 1);

        // A refused/failed visit never reaches the publisher — the confirmed
        // previous guest keeps being published as active (no tombstone).
        let payload =
            plan_guest_surface_publication(true, Some("workspace-a"), &[guest()], Some(7))
                .expect("confirmed guest keeps publishing");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap()["status"],
            "active"
        );

        // Death: the guest vanished from session_infos — a tombstone.
        let payload = plan_guest_surface_publication(true, Some("workspace-a"), &[], Some(7))
            .expect("death publishes a tombstone");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["session"], "workspace-a");
        assert_eq!(value["status"], "gone");
        assert_eq!(value["tabs"].as_array().unwrap().len(), 0);

        // The tombstone is replayable last-state truth, not a one-shot
        // message: while the guest stays absent the publisher keeps emitting
        // it on the SessionUpdate cadence, so a pipe dropped under
        // multi-client ambiguity reaches the surviving bar once the ambiguity
        // clears — without ever selecting an arbitrary client.
        let replay = plan_guest_surface_publication(true, Some("workspace-a"), &[], Some(7))
            .expect("the tombstone replays while the guest is absent");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&replay).unwrap(),
            value,
            "the replay is byte-identical idempotent state"
        );

        // No visited guest, or not a frame host: nothing to say.
        assert!(plan_guest_surface_publication(true, None, &[], None).is_none());
        assert!(plan_guest_surface_publication(false, Some("workspace-a"), &[], None).is_none());
    }

    #[test]
    fn guest_death_replay_clears_a_after_ambiguity_and_refused_b_retains_a() {
        let guest = |name: &str| SessionInfo {
            name: name.to_owned(),
            ..SessionInfo::default()
        };

        // A confirmed and alive: active payloads.
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(
                &plan_guest_surface_publication(
                    true,
                    Some("workspace-a"),
                    &[guest("workspace-a")],
                    Some(7)
                )
                .unwrap()
            )
            .unwrap()["status"],
            "active"
        );

        // A dies during two-client ambiguity: the fail-closed selector may
        // drop the first tombstone pipe, but the publisher keeps replaying
        // the same tombstone on every SessionUpdate while A stays absent.
        for _ in 0..3 {
            let payload = plan_guest_surface_publication(true, Some("workspace-a"), &[], Some(7))
                .expect("tombstone replays until A is revisited or replaced");
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&payload).unwrap()["status"],
                "gone"
            );
        }

        // A refused visit to B never reaches the publisher as state:
        // `visited_guest_name` is set only by a committed visit
        // (activate_session_request), so a refusal leaves the publisher
        // speaking for A — refused B is not the death of confirmed A, and a
        // refusal cannot resurrect A either.
        let still_a =
            plan_guest_surface_publication(true, Some("workspace-a"), &[], Some(7)).unwrap();
        let still_a = serde_json::from_str::<serde_json::Value>(&still_a).unwrap();
        assert_eq!(still_a["session"], "workspace-a");
        assert_eq!(still_a["status"], "gone");

        // Once B is CONFIRMED (visited_guest_name becomes B through a
        // successful visit), B publishes active and A's tombstone replay ends
        // — the new confirmed truth replaces the replayed last-state.
        let confirmed_b = plan_guest_surface_publication(
            true,
            Some("workspace-b"),
            &[guest("workspace-b")],
            Some(7),
        )
        .unwrap();
        let value = serde_json::from_str::<serde_json::Value>(&confirmed_b).unwrap();
        assert_eq!(value["session"], "workspace-b");
        assert_eq!(value["status"], "active");
    }

    #[test]
    fn incomplete_run_card_keeps_the_last_good_census() {
        let mut state = State {
            is_rail: true,
            ..Default::default()
        };
        let good = r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"a"},{"run_id":"b"}]}"#;
        assert!(state.apply_live_runs_payload(good));
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(2));
        assert!(!state.live_runs_feed_degraded);

        // A card without an identity is not a run: the whole payload is
        // rejected and the last good census is retained as degraded.
        assert!(state.apply_live_runs_payload(r#"{"schema":"vc.live-runs.v1","runs":[{}]}"#));
        assert!(state.live_runs_feed_degraded);
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(2));
        assert_eq!(
            state.agent_runs.as_ref().unwrap()[0].run_id,
            "a",
            "last-good cards survive an incomplete-card payload"
        );
    }

    fn surface_state() -> State {
        State {
            workspace_surface: true,
            ..Default::default()
        }
    }

    #[test]
    fn workspace_surface_update_consumes_session_snapshot_and_live_runs() {
        let mut state = surface_state();
        let guest = SessionInfo {
            name: "workspace-a".to_owned(),
            ..SessionInfo::default()
        };
        let rendered = state.update(Event::SessionUpdate(
            vec![guest],
            vec![("old-one".to_owned(), Duration::ZERO)],
        ));
        assert!(rendered);
        assert!(state.session_list_seen);
        assert_eq!(state.sessions.session_ui_infos.len(), 1);
        assert_eq!(
            state
                .resurrectable_sessions
                .all_resurrectable_sessions
                .len(),
            1
        );

        let rendered = state.update(Event::CustomMessage(
            VC_LIVE_RUNS_MESSAGE.to_owned(),
            r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"r1","agent":"claude"}]}"#.to_owned(),
        ));
        assert!(rendered);
        assert_eq!(state.agent_runs.as_ref().map(Vec::len), Some(1));
        assert!(!state.live_runs_feed_degraded);

        // Garbage degrades, the same truth contract the dashboard holds.
        assert!(state.update(Event::CustomMessage(
            VC_LIVE_RUNS_MESSAGE.to_owned(),
            "garbage".to_owned()
        )));
        assert!(state.live_runs_feed_degraded);
    }

    #[test]
    fn workspace_surface_keys_move_selection_and_project_through_frame_host() {
        let mut state = surface_state();
        state.session_name = Some("live-host".to_owned());
        state.current_session_is_host = true;
        state.sessions.set_sessions(
            vec![session("alpha", false), session("beta", false)],
            vec![],
        );
        state.session_list_seen = true;

        assert_eq!(state.surface_selected, 0);
        assert!(state.update(Event::Key(bare(BareKey::Down))));
        assert_eq!(state.surface_selected, 1);
        // Clamped at the last workspace, never wraps into a lie.
        assert!(state.update(Event::Key(bare(BareKey::Down))));
        assert_eq!(state.surface_selected, 1);
        assert!(state.update(Event::Key(bare(BareKey::Up))));
        assert_eq!(state.surface_selected, 0);
        assert!(state.update(Event::Key(bare(BareKey::Down))));

        // Enter voices the projection through the guarded CLI pipe (a
        // run_command no-op natively); the notice is the observable seam.
        assert!(state.update(Event::Key(bare(BareKey::Enter))));
        assert_eq!(
            state.surface_notice.as_deref(),
            Some("Opening `beta` in this pane.")
        );
        // The CLI handoff must not leave a pending self-projection behind.
        assert!(state.pending_guest_visit.is_none());
    }

    #[test]
    fn workspace_surface_timer_advances_the_spinner() {
        let mut state = surface_state();
        assert!(state.update(Event::Timer(1.0)));
        assert_eq!(state.surface_tick, 1);
        assert!(state.refresh_timer_armed);
    }

    #[test]
    fn workspace_surface_open_without_a_host_shows_the_cli_route() {
        // No host identity known (no snapshot / foreign session): never a
        // silent no-op, and never the create-flavored handoff wording.
        let mut state = surface_state();
        state
            .sessions
            .set_sessions(vec![session("alpha", false)], vec![]);
        state.session_list_seen = true;
        assert!(state.update(Event::Key(bare(BareKey::Enter))));
        let notice = state.surface_notice.as_deref().unwrap_or("");
        assert!(notice.contains("project-workspace alpha"), "{notice}");
        assert!(!notice.contains("Created workspace"), "{notice}");
    }

    #[test]
    fn workspace_surface_click_projects_the_clicked_workspace() {
        let mut state = surface_state();
        state.session_name = Some("live-host".to_owned());
        state.current_session_is_host = true;
        state.sessions.set_sessions(
            vec![session("alpha", false), session("beta", false)],
            vec![],
        );
        state.session_list_seen = true;
        state.surface_click_map = BTreeMap::from([(5usize, SurfaceClickTarget::Workspace(1usize))]);

        assert!(state.update(Event::Mouse(Mouse::LeftClick(5, 3))));
        assert_eq!(state.surface_selected, 1);
        assert_eq!(
            state.surface_notice.as_deref(),
            Some("Opening `beta` in this pane.")
        );
        // Rows outside the click map are quiet no-ops.
        assert!(!state.update(Event::Mouse(Mouse::LeftClick(9, 3))));
    }

    #[test]
    fn workspace_surface_n_creates_a_guest_workspace_and_never_hides() {
        let mut state = surface_state();
        state.session_name = Some("vc-frame-host".to_owned());
        state.current_session_is_host = true;
        state
            .sessions
            .set_sessions(vec![session("workspace-1", false)], vec![]);

        assert!(state.update(Event::Key(bare(BareKey::Char('n')))));
        let (request_id, pending) = state
            .pending_guest_create
            .as_ref()
            .expect("n must arm a guest create request");
        // allocate_workspace_name skips the taken name.
        assert_eq!(pending.session, "workspace-2");
        let request_id = request_id.clone();

        let rendered = state.handle_guest_create_result(
            Some(0),
            b"",
            b"",
            Some("workspace-2"),
            Some(request_id.as_str()),
        );
        assert!(rendered);
        assert!(state.pending_guest_create.is_none());
    }

    #[test]
    fn guest_create_hide_guard_protects_the_tiled_surface_pane() {
        // Floating managers hide after a successful create; the host rail
        // stays; the tiled VC Guest surface must never hide itself.
        assert!(should_hide_manager_after_guest_create(false, false));
        assert!(!should_hide_manager_after_guest_create(true, false));
        assert!(!should_hide_manager_after_guest_create(false, true));
        assert!(!should_hide_manager_after_guest_create(true, true));
    }

    #[test]
    fn workspace_surface_render_builds_the_click_map() {
        let mut state = surface_state();
        state
            .sessions
            .set_sessions(vec![session("alpha", false)], vec![]);
        state.session_list_seen = true;
        state.render(30, 80);
        assert!(
            state
                .surface_click_map
                .values()
                .any(|target| *target == SurfaceClickTarget::Workspace(0)),
            "the empty-host overview must offer a clickable workspace row"
        );
    }
}
