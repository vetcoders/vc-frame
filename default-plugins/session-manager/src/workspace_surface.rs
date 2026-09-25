//! Empty-host overview for the `VC Guest` pane (`workspace_surface true`).
//!
//! While no guest is projected, the surface renders a live overview from the
//! same server-owned projections the rail and the Agent Workspaces canvas
//! consume: sessions with liveness, tab/organ chips, the `vc.live-runs.v1`
//! census, and quick actions. It never duplicates the rail's Sessions list
//! and never invents data — unknown feeds render as `?`, degraded as `~`.

use zellij_tile::prelude::*;

use crate::AgentRunUiInfo;
use crate::ui::{SessionUiInfo, TabUiInfo};

/// Process spinner frames, same braille cycle as the Operator Frame
/// playground (`site/public/playground`, BRAILLE_FRAMES).
pub const SURFACE_SPINNER_FRAMES: [char; 9] = ['⣸', '⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'];

/// Runs preview cap: the full census lives on the Agent Workspaces canvas.
pub const SURFACE_MAX_RUN_ROWS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceTone {
    Normal,
    Accent,
    Dim,
}

/// Row-level click truth, rebuilt on every render like the rail click map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceClickTarget {
    None,
    Workspace(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceLine {
    pub text: String,
    pub tone: SurfaceTone,
    pub target: SurfaceClickTarget,
    pub selected: bool,
}

impl SurfaceLine {
    fn new(text: String, tone: SurfaceTone) -> Self {
        SurfaceLine {
            text,
            tone,
            target: SurfaceClickTarget::None,
            selected: false,
        }
    }
    fn workspace(text: String, tone: SurfaceTone, index: usize, selected: bool) -> Self {
        SurfaceLine {
            text,
            tone,
            target: SurfaceClickTarget::Workspace(index),
            selected,
        }
    }
}

/// One tab chip on a workspace card: canonical organs first, then the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceOrgan {
    pub name: String,
    pub active: bool,
    pub canonical_organ: bool,
}

/// Canonical organ order (`Overview`, `Agents`, `Shell`) over a workspace's
/// tabs, then every remaining tab unchanged. Exact and case-sensitive: a tab
/// named `agents` is not an organ. Missing organs are absent — never invented.
pub fn project_surface_organs(tabs: &[TabUiInfo]) -> Vec<SurfaceOrgan> {
    let mut claimed = vec![false; tabs.len()];
    let mut projected = Vec::with_capacity(tabs.len());
    for organ in GUEST_ORGAN_NAMES {
        if let Some(index) = tabs.iter().position(|tab| tab.name == organ)
            && !claimed[index]
        {
            claimed[index] = true;
            projected.push(SurfaceOrgan {
                name: tabs[index].name.clone(),
                active: tabs[index].is_active,
                canonical_organ: true,
            });
        }
    }
    for (index, tab) in tabs.iter().enumerate() {
        if !claimed[index] {
            projected.push(SurfaceOrgan {
                name: tab.name.clone(),
                active: tab.is_active,
                canonical_organ: false,
            });
        }
    }
    projected
}

pub fn workspace_live_process_count(session: &SessionUiInfo) -> usize {
    session
        .tabs
        .iter()
        .map(|tab| tab.live_process_count())
        .sum()
}

/// Live-runs count truth: `?` unknown, `~` degraded — never a lying zero.
pub fn surface_runs_count_truth(runs: Option<&[AgentRunUiInfo]>, degraded: bool) -> String {
    let base = runs
        .map(|runs| runs.len().to_string())
        .unwrap_or_else(|| "?".to_owned());
    if degraded { format!("{base} ~") } else { base }
}

pub struct SurfaceOverview<'a> {
    pub sessions: &'a [SessionUiInfo],
    pub session_list_seen: bool,
    pub exited_count: usize,
    pub runs: Option<&'a [AgentRunUiInfo]>,
    pub runs_degraded: bool,
    pub selected: usize,
    pub spinner: char,
    pub notice: Option<&'a str>,
    pub pending_create: Option<&'a str>,
}

fn workspace_detail_line(session: &SessionUiInfo, spinner: char) -> String {
    let organs = project_surface_organs(&session.tabs);
    let mut segments: Vec<String> = Vec::new();
    if !organs.is_empty() {
        let label = if organs.iter().any(|organ| organ.canonical_organ) {
            "organs"
        } else {
            "tabs"
        };
        let chips = organs
            .iter()
            .map(|organ| {
                let marker = if organ.active { "◉" } else { "○" };
                format!("{} {}", organ.name, marker)
            })
            .collect::<Vec<_>>()
            .join(" ");
        segments.push(format!("{label}: {chips}"));
    }
    let live = workspace_live_process_count(session);
    if live > 0 {
        segments.push(format!("{spinner} {live} live"));
    } else {
        segments.push("idle".to_owned());
    }
    if session.connected_users > 0 {
        let clients = if session.connected_users == 1 {
            "1 client".to_owned()
        } else {
            format!("{} clients", session.connected_users)
        };
        segments.push(clients);
    }
    segments.join(" · ")
}

/// The empty-host overview. Selection (`selected`) marks the workspace that
/// Enter / click projects into this pane through the frame-host routing.
pub fn workspace_surface_overview_lines(overview: &SurfaceOverview) -> Vec<SurfaceLine> {
    let mut lines = vec![
        SurfaceLine::new(
            "⚒ VC Guest · no workspace projected".to_owned(),
            SurfaceTone::Accent,
        ),
        SurfaceLine::new(
            "Pick a workspace here or in the Sessions rail — it opens in this pane.".to_owned(),
            SurfaceTone::Dim,
        ),
        SurfaceLine::new("─".repeat(56), SurfaceTone::Dim),
    ];
    if let Some(notice) = overview.notice {
        lines.push(SurfaceLine::new(notice.to_owned(), SurfaceTone::Normal));
    }
    if let Some(name) = overview.pending_create {
        lines.push(SurfaceLine::new(
            format!("{} creating workspace {name}…", overview.spinner),
            SurfaceTone::Normal,
        ));
    }
    if !overview.session_list_seen {
        lines.push(SurfaceLine::new(
            "WORKSPACES ? · waiting for the first session snapshot".to_owned(),
            SurfaceTone::Dim,
        ));
    } else {
        lines.push(SurfaceLine::new(
            format!(
                "WORKSPACES {} live · {} exited",
                overview.sessions.len(),
                overview.exited_count
            ),
            SurfaceTone::Normal,
        ));
        if overview.sessions.is_empty() {
            lines.push(SurfaceLine::new(
                "No workspaces yet — n creates the first one.".to_owned(),
                SurfaceTone::Dim,
            ));
        }
        for (index, session) in overview.sessions.iter().enumerate() {
            let selected = index == overview.selected;
            let marker = if selected { "▸" } else { " " };
            lines.push(SurfaceLine::workspace(
                format!("{marker} ● {}", session.title),
                SurfaceTone::Normal,
                index,
                selected,
            ));
            lines.push(SurfaceLine::workspace(
                format!("  {}", workspace_detail_line(session, overview.spinner)),
                SurfaceTone::Dim,
                index,
                selected,
            ));
        }
    }
    lines.push(SurfaceLine::new(String::new(), SurfaceTone::Normal));
    lines.push(SurfaceLine::new(
        format!(
            "❖ ACTIVE RUNS · {}",
            surface_runs_count_truth(overview.runs, overview.runs_degraded)
        ),
        SurfaceTone::Normal,
    ));
    match overview.runs {
        None => lines.push(SurfaceLine::new(
            "UNAVAILABLE · waiting for canonical workspace data".to_owned(),
            SurfaceTone::Dim,
        )),
        Some([]) => lines.push(SurfaceLine::new(
            "no active agent runs".to_owned(),
            SurfaceTone::Dim,
        )),
        Some(runs) => {
            for run in runs.iter().take(SURFACE_MAX_RUN_ROWS) {
                lines.push(SurfaceLine::new(
                    format!(
                        "{} {} — {}",
                        overview.spinner,
                        run.status_summary(),
                        run.primary_title()
                    ),
                    SurfaceTone::Normal,
                ));
            }
            if runs.len() > SURFACE_MAX_RUN_ROWS {
                lines.push(SurfaceLine::new(
                    format!(
                        "+{} more on the Agent Workspaces canvas",
                        runs.len() - SURFACE_MAX_RUN_ROWS
                    ),
                    SurfaceTone::Dim,
                ));
            }
        },
    }
    lines.push(SurfaceLine::new(String::new(), SurfaceTone::Normal));
    let quick_actions = if overview.sessions.is_empty() {
        "n: new workspace · Voc host console: Ctrl o v"
    } else {
        "Enter: open workspace · n: new workspace · Voc host console: Ctrl o v"
    };
    lines.push(SurfaceLine::new(quick_actions.to_owned(), SurfaceTone::Dim));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tab(name: &str, active: bool, live_process_count: usize) -> TabUiInfo {
        TabUiInfo::for_rail_test(name, active, "agent", live_process_count)
    }

    fn workspace(name: &str, tabs: Vec<TabUiInfo>, connected_users: usize) -> SessionUiInfo {
        SessionUiInfo {
            name: name.to_owned(),
            title: name.to_owned(),
            tabs,
            connected_users,
            is_current_session: false,
            creation_time: Duration::ZERO,
            rail_order: 0,
        }
    }

    fn run(payload: &str) -> AgentRunUiInfo {
        serde_json::from_str(payload).unwrap()
    }

    fn texts(lines: &[SurfaceLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn organs_render_in_canonical_order_and_keep_fisheye() {
        // Guest tabs arrive in server order; organs lead in canonical order.
        let tabs = vec![
            tab("Shell", false, 1),
            tab("Agents", true, 1),
            tab("Foo", false, 0),
        ];
        let projected = project_surface_organs(&tabs);
        let names: Vec<&str> = projected.iter().map(|organ| organ.name.as_str()).collect();
        assert_eq!(names, vec!["Agents", "Shell", "Foo"]);
        assert!(projected[0].canonical_organ && projected[0].active);
        assert!(projected[1].canonical_organ && !projected[1].active);
        assert!(!projected[2].canonical_organ);
    }

    #[test]
    fn missing_organs_are_absent_and_lowercase_is_not_an_organ() {
        let tabs = vec![tab("agents", true, 1), tab("Shell", false, 1)];
        let projected = project_surface_organs(&tabs);
        assert!(projected.iter().all(|organ| organ.name != "Overview"));
        assert_eq!(
            projected
                .iter()
                .filter(|organ| organ.canonical_organ)
                .map(|organ| organ.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Shell"]
        );
        // `agents` stays an ordinary tab; it is never promoted to the organ.
        assert!(
            projected
                .iter()
                .any(|organ| organ.name == "agents" && !organ.canonical_organ)
        );
    }

    #[test]
    fn overview_header_states_the_empty_host_truth() {
        let overview = SurfaceOverview {
            sessions: &[],
            session_list_seen: true,
            exited_count: 0,
            runs: None,
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&overview);
        assert_eq!(lines[0].text, "⚒ VC Guest · no workspace projected");
        assert_eq!(lines[0].tone, SurfaceTone::Accent);
        // The old placeholder's guidance survives, but the rail's Sessions
        // list must not be duplicated in this canvas.
        assert!(
            texts(&lines)
                .iter()
                .any(|text| text.contains("Sessions rail"))
        );
        assert!(
            !texts(&lines)
                .iter()
                .any(|text| text.starts_with("SESSIONS"))
        );
    }

    #[test]
    fn unseen_session_list_is_unknown_never_zero() {
        let overview = SurfaceOverview {
            sessions: &[],
            session_list_seen: false,
            exited_count: 0,
            runs: None,
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&overview);
        assert!(
            texts(&lines)
                .iter()
                .any(|text| text.contains("WORKSPACES ?"))
        );
        assert!(
            !texts(&lines)
                .iter()
                .any(|text| text.contains("WORKSPACES 0"))
        );
    }

    #[test]
    fn empty_workspace_set_offers_first_creation() {
        let overview = SurfaceOverview {
            sessions: &[],
            session_list_seen: true,
            exited_count: 0,
            runs: None,
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&overview);
        let texts = texts(&lines);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("WORKSPACES 0 live · 0 exited"))
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("n creates the first one"))
        );
        let footer = texts.last().unwrap();
        assert!(footer.contains("n: new workspace"));
        assert!(footer.contains("Voc"));
        // No workspace to open — the Enter hint would be a lie.
        assert!(!footer.contains("Enter"));
    }

    #[test]
    fn workspace_cards_carry_organs_liveness_and_selection() {
        let sessions = vec![
            workspace(
                "alpha",
                vec![
                    tab("Overview", false, 0),
                    tab("Agents", true, 2),
                    tab("Shell", false, 1),
                ],
                1,
            ),
            workspace("beta", vec![tab("Shell", true, 0)], 0),
        ];
        let overview = SurfaceOverview {
            sessions: &sessions,
            session_list_seen: true,
            exited_count: 2,
            runs: Some(&[]),
            runs_degraded: false,
            selected: 1,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&overview);
        let texts = texts(&lines);
        assert!(
            texts
                .iter()
                .any(|text| text.contains("WORKSPACES 2 live · 2 exited"))
        );
        assert!(texts.iter().any(|text| text.contains("  ● alpha")));
        assert!(texts.iter().any(|text| text.contains("▸ ● beta")));
        assert!(texts.iter().any(|text| {
            text.contains("organs: Overview ○ Agents ◉ Shell ○")
                && text.contains("3 live")
                && text.contains("1 client")
        }));
        assert!(
            texts
                .iter()
                .any(|text| text.contains("organs: Shell ◉") && text.contains("idle"))
        );
        // Selection rides on both rows of the beta card, and both rows are
        // click targets that project the guest.
        let beta_rows: Vec<&SurfaceLine> = lines
            .iter()
            .filter(|line| line.target == SurfaceClickTarget::Workspace(1))
            .collect();
        assert_eq!(beta_rows.len(), 2);
        assert!(beta_rows.iter().all(|line| line.selected));
        assert!(
            !lines
                .iter()
                .any(|line| line.target == SurfaceClickTarget::Workspace(0) && line.selected)
        );
        // Header and footer never project anything.
        assert_eq!(lines[0].target, SurfaceClickTarget::None);
        assert_eq!(lines.last().unwrap().target, SurfaceClickTarget::None);
        let footer = texts.last().unwrap();
        assert!(footer.contains("Enter: open workspace"));
        assert!(footer.contains("n: new workspace"));
        assert!(footer.contains("Voc host console: Ctrl o v"));
    }

    #[test]
    fn runs_feed_truth_unknown_degraded_empty_and_live() {
        assert_eq!(surface_runs_count_truth(None, false), "?");
        assert_eq!(surface_runs_count_truth(None, true), "? ~");
        assert_eq!(surface_runs_count_truth(Some(&[]), false), "0");
        let runs = vec![
            run(
                r#"{"run_id":"r1","agent":"claude","skill":"implement","execution_state":"running","repo":"vc-frame","task_title":"Guest overview"}"#,
            ),
            run(
                r#"{"run_id":"r2","agent":"codex","skill":"review","execution_state":"active","repo":"vc-frame","task_title":"Two"}"#,
            ),
            run(
                r#"{"run_id":"r3","agent":"kimi","skill":"audit","execution_state":"active","repo":"vc-frame","task_title":"Three"}"#,
            ),
            run(
                r#"{"run_id":"r4","agent":"grok","skill":"ship","execution_state":"active","repo":"vc-frame","task_title":"Four"}"#,
            ),
        ];
        assert_eq!(surface_runs_count_truth(Some(&runs), true), "4 ~");

        let overview = SurfaceOverview {
            sessions: &[],
            session_list_seen: true,
            exited_count: 0,
            runs: Some(&runs),
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&overview);
        let rendered = texts(&lines);
        assert!(
            rendered
                .iter()
                .any(|text| text.contains("❖ ACTIVE RUNS · 4"))
        );
        assert!(rendered.iter().any(|text| {
            text.contains("⣾ claude · implement · running — vc-frame · Guest overview")
        }));
        // Preview is capped; the full census stays on the dashboard canvas.
        assert!(!rendered.iter().any(|text| text.contains("grok")));
        assert!(rendered.iter().any(|text| text.contains("+1 more")));

        let unknown = SurfaceOverview {
            sessions: &[],
            session_list_seen: true,
            exited_count: 0,
            runs: None,
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: None,
            pending_create: None,
        };
        let lines = workspace_surface_overview_lines(&unknown);
        let rendered = texts(&lines);
        assert!(
            rendered
                .iter()
                .any(|text| text.contains("❖ ACTIVE RUNS · ?"))
        );
        assert!(rendered.iter().any(|text| text.contains("UNAVAILABLE")));
        assert!(
            !rendered
                .iter()
                .any(|text| text.contains("no active agent runs"))
        );

        let empty = SurfaceOverview {
            runs: Some(&[]),
            ..unknown
        };
        let lines = workspace_surface_overview_lines(&empty);
        let rendered = texts(&lines);
        assert!(
            rendered
                .iter()
                .any(|text| text.contains("❖ ACTIVE RUNS · 0"))
        );
        assert!(
            rendered
                .iter()
                .any(|text| text.contains("no active agent runs"))
        );
    }

    #[test]
    fn notice_and_pending_create_render_above_the_workspace_section() {
        let overview = SurfaceOverview {
            sessions: &[],
            session_list_seen: true,
            exited_count: 0,
            runs: None,
            runs_degraded: false,
            selected: 0,
            spinner: '⣾',
            notice: Some("Opening `alpha` in this pane."),
            pending_create: Some("workspace-1"),
        };
        let lines = workspace_surface_overview_lines(&overview);
        let texts = texts(&lines);
        let notice_at = texts
            .iter()
            .position(|text| text.contains("Opening `alpha`"))
            .unwrap();
        let create_at = texts
            .iter()
            .position(|text| text.contains("⣾ creating workspace workspace-1…"))
            .unwrap();
        let header_at = texts
            .iter()
            .position(|text| text.contains("WORKSPACES"))
            .unwrap();
        assert!(notice_at < header_at && create_at < header_at);
    }
}
