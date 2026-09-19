//! Shared-canvas workspace identity: distinct guest sessions under one host.
//!
//! New Session must create a real session process, not extra tabs on the
//! current client. The host rail lists those guests; `vc-frame visit` replaces
//! only the guest pane. Same-name creation refuses before mutation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::data::{LayoutInfo, PluginInfo, SessionInfo};

/// Custom plugin message: host chrome mirrors the visited guest's tabs.
pub const VC_GUEST_SURFACE_MESSAGE: &str = "vc.guest-surface.v1";

/// Title of the replaceable host content pane in `vibecrafted-host`.
pub const VC_GUEST_PANE_TITLE: &str = "VC Guest";

/// Context key on background `attach -b -c` so the host can visit after spawn.
pub const VC_GUEST_CREATE_CONTEXT_KEY: &str = "vc_frame_guest_create";

/// Plugin alias that exclusive-matches the host rail (`frame_host true`).
/// Ordinary `session-manager` / `session-rail` must not receive tab routing.
pub const VC_FRAME_HOST_PLUGIN_ALIAS: &str = "frame-host";

/// Compact-bar alias the host uses when publishing a guest surface.
pub const VC_COMPACT_BAR_PLUGIN_ALIAS: &str = "compact-bar";

/// Canonical Quick cmd wrapper under the product config root.
pub const VC_QUICK_CMD_CANONICAL_REL: &str = ".config/vibecrafted/vc-frame/vc-quick-cmd.sh";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKind {
    CurrentName,
    ExistingSession,
}

#[derive(Debug, Clone, PartialEq)]
pub enum NewWorkspacePlan {
    SwitchSession {
        name: Option<String>,
        layout: Option<LayoutInfo>,
        cwd: Option<PathBuf>,
        quit_after: bool,
    },
    CreateGuestWorkspace {
        name: String,
        layout: LayoutInfo,
        cwd: Option<PathBuf>,
    },
    RefuseDuplicate {
        name: String,
        kind: DuplicateKind,
        supported_commands: Vec<String>,
    },
}

impl NewWorkspacePlan {
    pub fn duplicate_message(&self) -> Option<String> {
        match self {
            NewWorkspacePlan::RefuseDuplicate {
                name,
                kind,
                supported_commands,
            } => {
                let reason = match kind {
                    DuplicateKind::CurrentName => {
                        format!("Workspace `{name}` is already the current workspace.")
                    },
                    DuplicateKind::ExistingSession => {
                        format!("Workspace `{name}` already exists.")
                    },
                };
                let commands = supported_commands
                    .iter()
                    .map(|command| format!("  • {command}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!(
                    "{reason} Creation refused before mutation.\nSupported commands:\n{commands}"
                ))
            },
            _ => None,
        }
    }
}

pub fn supported_duplicate_commands(name: &str) -> Vec<String> {
    vec![
        format!("Sessions rail: select `{name}`"),
        format!("vc-frame attach {name}"),
        format!("vc-frame visit {name}"),
        "Choose a different workspace name".to_owned(),
    ]
}

/// Welcome still switches off the throwaway surface. Every later New Session
/// creates a distinct guest identity. Same name as current or any live
/// session refuses with actionable commands — never a silent no-op.
pub fn plan_new_workspace(
    is_welcome_screen: bool,
    current_session_name: Option<&str>,
    requested_name: Option<&str>,
    selected_layout: Option<LayoutInfo>,
    cwd: Option<PathBuf>,
    existing_session_names: &[String],
) -> NewWorkspacePlan {
    let layout = selected_layout
        .unwrap_or_else(|| LayoutInfo::BuiltIn("default".to_owned()))
        .resolve_product_workspace();

    if let Some(name) = requested_name.filter(|name| !name.is_empty()) {
        if current_session_name == Some(name) {
            return NewWorkspacePlan::RefuseDuplicate {
                name: name.to_owned(),
                kind: DuplicateKind::CurrentName,
                supported_commands: supported_duplicate_commands(name),
            };
        }
        if existing_session_names
            .iter()
            .any(|existing| existing == name)
        {
            return NewWorkspacePlan::RefuseDuplicate {
                name: name.to_owned(),
                kind: DuplicateKind::ExistingSession,
                supported_commands: supported_duplicate_commands(name),
            };
        }
    }

    if is_welcome_screen {
        return NewWorkspacePlan::SwitchSession {
            name: requested_name
                .filter(|name| !name.is_empty())
                .map(|name| name.to_owned()),
            layout: Some(layout),
            cwd,
            quit_after: true,
        };
    }

    let name =
        allocate_workspace_name(requested_name, current_session_name, existing_session_names);
    NewWorkspacePlan::CreateGuestWorkspace { name, layout, cwd }
}

pub fn allocate_workspace_name(
    requested_name: Option<&str>,
    current_session_name: Option<&str>,
    existing_session_names: &[String],
) -> String {
    if let Some(name) = requested_name.filter(|name| !name.is_empty()) {
        return name.to_owned();
    }
    let mut index = 1usize;
    loop {
        let candidate = format!("workspace-{index}");
        let taken = current_session_name == Some(candidate.as_str())
            || existing_session_names
                .iter()
                .any(|existing| existing == &candidate);
        if !taken {
            return candidate;
        }
        index += 1;
    }
}

pub fn is_internal_host_session(session: &SessionInfo) -> bool {
    session.plugins.values().any(plugin_is_frame_host)
}

pub fn plugin_is_frame_host(plugin: &PluginInfo) -> bool {
    plugin.configuration.get("frame_host").map(String::as_str) == Some("true")
}

/// Projection delivery owner: the exclusive host rail, never compact-bar,
/// workspace_surface, or an ordinary `session-rail` that only sets `rail`.
pub fn plugin_is_configured_projection_owner(configuration: &BTreeMap<String, String>) -> bool {
    configuration.get("frame_host").map(String::as_str) == Some("true")
        && configuration.get("rail").map(String::as_str) == Some("true")
}

/// Unique configured owner plugin plus unique interactive client.
/// Cardinality is plugin-ids × interactive clients so two attached owners
/// still report `found 2` without inferring list order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionOwnerSelection {
    Unique { plugin_id: u32, client_id: u16 },
    None,
    Ambiguous { count: usize },
}

pub fn select_configured_projection_owner(
    configured_plugin_ids: impl IntoIterator<Item = u32>,
    connected_interactive_clients: impl IntoIterator<Item = u16>,
) -> ProjectionOwnerSelection {
    let plugins: BTreeSet<u32> = configured_plugin_ids.into_iter().collect();
    let clients: BTreeSet<u16> = connected_interactive_clients.into_iter().collect();
    match (plugins.len(), clients.len()) {
        (1, 1) => ProjectionOwnerSelection::Unique {
            plugin_id: plugins.into_iter().next().expect("one configured owner"),
            client_id: clients.into_iter().next().expect("one interactive client"),
        },
        (0, _) | (_, 0) => ProjectionOwnerSelection::None,
        (plugin_count, client_count) => ProjectionOwnerSelection::Ambiguous {
            count: plugin_count.saturating_mul(client_count),
        },
    }
}

pub fn guest_create_argv(name: &str, layout: &LayoutInfo) -> Vec<String> {
    vec![
        "vc-frame:self".to_owned(),
        "--layout".to_owned(),
        layout_cli_token(layout),
        "--guest-workspace".to_owned(),
        "attach".to_owned(),
        "-b".to_owned(),
        "-c".to_owned(),
        name.to_owned(),
    ]
}

/// Exact host-rail configuration. Must stay aligned with
/// `assets/layouts/vibecrafted-host.kdl` and the `frame-host` alias.
pub fn host_session_manager_configuration() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("session_canvas".to_owned(), "true".to_owned()),
        (
            "session_canvas_kind".to_owned(),
            "session-manager".to_owned(),
        ),
        ("rail".to_owned(), "true".to_owned()),
        ("frame_host".to_owned(), "true".to_owned()),
        ("pane_title".to_owned(), "Sessions".to_owned()),
    ])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestSurfaceRequest {
    Project {
        session: String,
        tab: Option<usize>,
    },
    ActivateTab {
        session: String,
        tab: usize,
    },
    Surface {
        session: String,
        host_plugin_id: Option<u32>,
        tabs: Vec<GuestSurfaceTab>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSurfaceTab {
    pub name: String,
    pub active: bool,
    pub position: usize,
}

/// Parse host↔chrome surface JSON. `project: true` is the launcher handoff;
/// `activate_tab` alone is a compact-bar click. Tabs without those fields are
/// the host→bar projection.
pub fn parse_guest_surface_payload(payload: &str) -> Option<GuestSurfaceRequest> {
    let value = serde_json::from_str::<serde_json::Value>(payload).ok()?;
    let session = value.get("session")?.as_str()?.to_owned();
    if value.get("project").and_then(|value| value.as_bool()) == Some(true) {
        let tab = value
            .get("activate_tab")
            .and_then(|value| value.as_u64())
            .map(|tab| tab as usize);
        return Some(GuestSurfaceRequest::Project { session, tab });
    }
    if let Some(tab) = value.get("activate_tab").and_then(|value| value.as_u64()) {
        return Some(GuestSurfaceRequest::ActivateTab {
            session,
            tab: tab as usize,
        });
    }
    let tabs = value.get("tabs").and_then(|value| value.as_array())?;
    let tabs = tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| GuestSurfaceTab {
            name: tab
                .get("name")
                .and_then(|value| value.as_str())
                .unwrap_or("tab")
                .to_owned(),
            active: tab
                .get("active")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
            position: tab
                .get("position")
                .and_then(|value| value.as_u64())
                .unwrap_or(index as u64) as usize,
        })
        .collect();
    let host_plugin_id = value
        .get("host_plugin_id")
        .and_then(|value| value.as_u64())
        .map(|id| id as u32);
    Some(GuestSurfaceRequest::Surface {
        session,
        host_plugin_id,
        tabs,
    })
}

/// Only the owning host manager may apply project / activate_tab.
/// An ordinary floating Session Manager must ignore those payloads so a
/// tab click cannot `switch_session_with_focus` the outer client.
pub fn host_owns_guest_surface_routing(frame_host: bool) -> bool {
    frame_host
}

/// CLI `vc.guest-surface.v1` visit the host rail must apply.
///
/// Compact-bar clicks arrive as `ActivateTab` with no `request_id`. Those are
/// still host-owned projections, not a reconnect and not a silent ignore.
/// Ordinary floating managers return `None` so they cannot steal the pipe.
pub fn host_cli_guest_surface_visit(
    frame_host: bool,
    message_name: &str,
    payload: Option<&str>,
) -> Option<(String, Option<usize>)> {
    if !host_owns_guest_surface_routing(frame_host) || message_name != VC_GUEST_SURFACE_MESSAGE {
        return None;
    }
    match payload.and_then(parse_guest_surface_payload)? {
        GuestSurfaceRequest::Project { session, tab } => Some((session, tab)),
        GuestSurfaceRequest::ActivateTab { session, tab } => Some((session, Some(tab))),
        GuestSurfaceRequest::Surface { .. } => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGuestRequest {
    pub session: String,
    pub tab: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionRefuse {
    NotAHost,
    AmbiguousHost,
    MissingGuestSurface,
    AmbiguousGuestSurface,
    MissingGuestSession,
    AmbiguousOwningClient,
    MissingOwningClient,
}

impl std::fmt::Display for ProjectionRefuse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectionRefuse::NotAHost => write!(
                f,
                "Refused: target session is not a unique frame_host. Zero process/pane mutation."
            ),
            ProjectionRefuse::AmbiguousHost => write!(
                f,
                "Refused: multiple frame_host plugins. Zero process/pane mutation."
            ),
            ProjectionRefuse::MissingGuestSurface => write!(
                f,
                "Refused: no unique registered guest surface. Zero process/pane mutation."
            ),
            ProjectionRefuse::AmbiguousGuestSurface => write!(
                f,
                "Refused: multiple registered guest surfaces. Zero process/pane mutation."
            ),
            ProjectionRefuse::MissingGuestSession => write!(
                f,
                "Refused: guest session is missing. Zero process/pane mutation."
            ),
            ProjectionRefuse::AmbiguousOwningClient => write!(
                f,
                "Refused: multiple interactive clients own this host. Zero process/pane mutation."
            ),
            ProjectionRefuse::MissingOwningClient => write!(
                f,
                "Refused: no owning interactive client. Zero process/pane mutation."
            ),
        }
    }
}

/// One-shot visitor readiness. The host reservation validates every field;
/// receiving bytes from the selected guest is distinct from installing a pane.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkspaceProjectionReady {
    pub request_id: String,
    pub host: String,
    pub client_id: u16,
    pub plugin_id: u32,
    pub guest: String,
    pub tab: Option<usize>,
    pub pane_id: u32,
}

/// Application receipt emitted by the configured projection owner, never inferred
/// from transport unblock, process argv or a matching previously visited guest.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkspaceProjectionReceipt {
    pub request_id: String,
    pub client_id: u16,
    pub plugin_id: u32,
    pub guest: String,
    pub tab: Option<usize>,
    pub pane_id: Option<u32>,
    pub status: ProjectionStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub enum ProjectionStatus {
    Handled,
    Refused,
    Unavailable,
}

impl WorkspaceProjectionReceipt {
    pub fn acknowledges(&self, request_id: &str, guest: &str, tab: Option<usize>) -> bool {
        self.request_id == request_id
            && self.guest == guest
            && self.tab == tab
            && (self.status != ProjectionStatus::Handled || self.pane_id.is_some())
    }
}

/// A peer `SessionInfo` discovered from a live socket often has empty `tabs`
/// until metadata lands. Empty is unknown, not "tab 0 does not exist".
/// Only a non-empty tab list may refuse a requested 0-based position.
pub fn guest_projection_tab_is_available(
    session: &SessionInfo,
    requested_tab: Option<usize>,
) -> bool {
    match requested_tab {
        None => true,
        Some(position) => {
            session.tabs.is_empty() || session.tabs.iter().any(|tab| tab.position == position)
        },
    }
}

pub fn prove_unique_owning_client<'a, C>(
    clients: impl IntoIterator<Item = &'a C>,
    is_cli: impl Fn(&C) -> bool,
) -> Result<&'a C, ProjectionRefuse> {
    let interactive: Vec<&'a C> = clients
        .into_iter()
        .filter(|client| !is_cli(client))
        .collect();
    match interactive.as_slice() {
        [owner] => Ok(*owner),
        [] => Err(ProjectionRefuse::MissingOwningClient),
        _ => Err(ProjectionRefuse::AmbiguousOwningClient),
    }
}

/// Guest-surface pipes must reach exactly one client per plugin id.
/// Multiple interactive clients are ambiguous: drop that plugin rather than
/// infer ownership from list order.
pub fn unique_guest_surface_pipe_targets<C: Copy + Eq>(
    message_name: &str,
    targets: Vec<(u32, Option<C>)>,
    prefer_not: Option<C>,
) -> Vec<(u32, Option<C>)> {
    if message_name != VC_GUEST_SURFACE_MESSAGE {
        return targets;
    }
    let mut by_plugin: BTreeMap<u32, Vec<Option<C>>> = BTreeMap::new();
    for (plugin_id, client_id) in targets {
        by_plugin.entry(plugin_id).or_default().push(client_id);
    }
    let mut unique = Vec::new();
    for (plugin_id, clients) in by_plugin {
        let interactive: Vec<Option<C>> = clients
            .iter()
            .copied()
            .filter(|client| *client != prefer_not)
            .collect();
        match interactive.as_slice() {
            [owner] => unique.push((plugin_id, *owner)),
            [] if clients.len() == 1 => unique.push((plugin_id, clients[0])),
            _ => {},
        }
    }
    unique
}

pub fn project_guest_payload(session: &str, tab: Option<usize>) -> String {
    let mut value = serde_json::json!({
        "session": session,
        "project": true,
    });
    if let Some(tab) = tab {
        value["activate_tab"] = serde_json::json!(tab);
    }
    value.to_string()
}

pub fn activate_guest_tab_payload(session: &str, tab: usize) -> String {
    serde_json::json!({
        "session": session,
        "activate_tab": tab,
    })
    .to_string()
}

/// `visit --tab` and `AttachClient.tab_position_to_focus` / `Screen::go_to_tab`
/// are 1-based. Projection identity (`activate_tab`, `WorkspaceProjectionReady.tab`)
/// is 0-based.
///
/// Do not subtract before attach. `go_to_tab` does its own `saturating_sub(1)`:
/// `--tab 2` subtracted to `1` becomes `go_to_tab(1)` and stays on the first
/// guest tab — the observed second-tab projection miss.
pub fn visit_attach_tab(tab: Option<usize>) -> Result<Option<usize>, &'static str> {
    match tab {
        None => Ok(None),
        Some(0) => Err("--tab is one-based and must be at least 1"),
        Some(tab) => Ok(Some(tab)),
    }
}

/// True when a 1-based attach/visit tab names the same tab as a 0-based
/// projection receipt. Missing on either side is not a match for a present
/// identity on the other.
pub fn attach_tab_matches_projection(
    attach_one_based: Option<usize>,
    projection_zero_based: Option<usize>,
) -> bool {
    match (attach_one_based, projection_zero_based) {
        (None, None) => true,
        (Some(attach), Some(projection)) => attach.checked_sub(1) == Some(projection),
        _ => false,
    }
}

/// Framework / floating-manager handoff: project `guest` into a running host.
/// Tab is 0-based internally; the CLI flag is 1-based like `visit --tab`.
pub fn project_workspace_argv(host: &str, guest: &str, tab: Option<usize>) -> Vec<String> {
    let mut argv = vec![
        "vc-frame:self".to_owned(),
        "--session".to_owned(),
        host.to_owned(),
        "project-workspace".to_owned(),
        guest.to_owned(),
    ];
    if let Some(tab) = tab {
        argv.push("--tab".to_owned());
        argv.push(tab.saturating_add(1).to_string());
    }
    argv
}

/// POSIX lookup order for the Quick cmd wrapper. Canonical product root
/// first; leftover frontier / bare vc-frame paths stay as fallbacks.
pub fn quick_cmd_wrapper_paths() -> [&'static str; 3] {
    [
        r#"${HOME}/.config/vibecrafted/vc-frame/vc-quick-cmd.sh"#,
        r#"${HOME}/.config/vetcoders/frontier/vc-frame/vc-quick-cmd.sh"#,
        r#"${HOME}/.config/vc-frame/vc-quick-cmd.sh"#,
    ]
}

pub fn quick_cmd_runner_script() -> String {
    let [canonical, frontier, legacy] = quick_cmd_wrapper_paths();
    format!(
        r#"if [ -x "{canonical}" ]; then exec "{canonical}"; elif [ -x "{frontier}" ]; then exec "{frontier}"; elif [ -x "{legacy}" ]; then exec "{legacy}"; else u="${{USER:-op}}"; h="$(hostname -s 2>/dev/null || echo host)"; d="$PWD"; case "${{HOME:-}}" in "") ;; *) case "$d" in "$HOME"|"$HOME"/*) d="~${{d#"$HOME"}}" ;; esac ;; esac; printf '\n  %s@%s in %s\n\n' "$u" "$h" "$d"; exec "${{SHELL:-/bin/zsh}}" -l; fi"#
    )
}

pub fn layout_cli_token(layout: &LayoutInfo) -> String {
    match layout {
        LayoutInfo::BuiltIn(name) | LayoutInfo::File(name, _) => name.clone(),
        LayoutInfo::Url(url) => url.clone(),
        LayoutInfo::Stringified(_) => "vibecrafted".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::TabInfo;
    use std::collections::BTreeMap;

    fn builtin(name: &str) -> LayoutInfo {
        LayoutInfo::BuiltIn(name.to_owned())
    }

    #[test]
    fn live_canvas_creates_distinct_guest_and_remaps_default() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-a"),
            Some("workspace-b"),
            Some(builtin("default")),
            Some(PathBuf::from("/tmp/workspace-b")),
            &["workspace-a".to_owned()],
        );
        match plan {
            NewWorkspacePlan::CreateGuestWorkspace { name, layout, cwd } => {
                assert_eq!(name, "workspace-b");
                assert_eq!(layout.name(), "vibecrafted");
                assert_eq!(cwd, Some(PathBuf::from("/tmp/workspace-b")));
            },
            other => panic!("expected guest workspace, got {other:?}"),
        }
    }

    #[test]
    fn live_canvas_does_not_add_tabs_to_the_current_session() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-a"),
            Some("workspace-b"),
            Some(builtin("vibecrafted")),
            None,
            &[],
        );
        assert!(
            !matches!(plan, NewWorkspacePlan::SwitchSession { .. }),
            "live New Session must not reconnect the outer client"
        );
        match plan {
            NewWorkspacePlan::CreateGuestWorkspace { name, .. } => {
                assert_ne!(name, "workspace-a");
            },
            other => panic!("expected distinct guest, got {other:?}"),
        }
    }

    #[test]
    fn welcome_still_switches_to_operator() {
        let plan = plan_new_workspace(
            true,
            None,
            Some("first"),
            Some(builtin("default")),
            None,
            &[],
        );
        match plan {
            NewWorkspacePlan::SwitchSession {
                name,
                layout,
                quit_after,
                ..
            } => {
                assert_eq!(name.as_deref(), Some("first"));
                assert_eq!(
                    layout.as_ref().map(|layout| layout.name()),
                    Some("vibecrafted")
                );
                assert!(quit_after);
            },
            other => panic!("expected first-session switch, got {other:?}"),
        }
    }

    #[test]
    fn leaked_host_layout_resolves_to_operator_guest() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-a"),
            Some("workspace-b"),
            Some(builtin("vibecrafted-host")),
            None,
            &[],
        );
        match plan {
            NewWorkspacePlan::CreateGuestWorkspace { layout, .. } => {
                assert_eq!(layout.name(), "vibecrafted");
            },
            other => panic!("expected remapped host, got {other:?}"),
        }
    }

    #[test]
    fn same_name_as_current_refuses_before_mutation() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-a"),
            Some("workspace-a"),
            Some(builtin("vibecrafted")),
            None,
            &["workspace-a".to_owned()],
        );
        match plan {
            NewWorkspacePlan::RefuseDuplicate {
                ref name,
                kind,
                ref supported_commands,
            } => {
                assert_eq!(name, "workspace-a");
                assert_eq!(kind, DuplicateKind::CurrentName);
                assert!(
                    supported_commands
                        .iter()
                        .any(|command| command.contains("vc-frame attach workspace-a"))
                );
                assert!(
                    supported_commands
                        .iter()
                        .any(|command| command.contains("vc-frame visit workspace-a"))
                );
            },
            other => panic!("expected refuse, got {other:?}"),
        }
        assert!(
            plan.duplicate_message()
                .unwrap()
                .contains("refused before mutation")
        );
    }

    #[test]
    fn existing_other_session_refuses_before_mutation() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-a"),
            Some("workspace-b"),
            Some(builtin("vc-workflow")),
            None,
            &["workspace-a".to_owned(), "workspace-b".to_owned()],
        );
        match plan {
            NewWorkspacePlan::RefuseDuplicate { kind, name, .. } => {
                assert_eq!(kind, DuplicateKind::ExistingSession);
                assert_eq!(name, "workspace-b");
            },
            other => panic!("expected refuse of existing B, got {other:?}"),
        }
    }

    #[test]
    fn empty_name_allocates_unused_workspace_identity() {
        let plan = plan_new_workspace(
            false,
            Some("workspace-1"),
            None,
            Some(builtin("default")),
            None,
            &["workspace-1".to_owned()],
        );
        match plan {
            NewWorkspacePlan::CreateGuestWorkspace { name, .. } => {
                assert_eq!(name, "workspace-2");
            },
            other => panic!("expected allocated guest, got {other:?}"),
        }
    }

    #[test]
    fn host_plugin_marks_internal_session() {
        let mut plugins = BTreeMap::new();
        plugins.insert(
            1,
            PluginInfo {
                location: "session-manager".to_owned(),
                configuration: BTreeMap::from([
                    ("rail".to_owned(), "true".to_owned()),
                    ("frame_host".to_owned(), "true".to_owned()),
                ]),
            },
        );
        let host = SessionInfo {
            name: "vc-frame-host".to_owned(),
            plugins,
            is_current_session: true,
            ..SessionInfo::default()
        };
        let guest = SessionInfo {
            name: "workspace-a".to_owned(),
            ..SessionInfo::default()
        };
        assert!(is_internal_host_session(&host));
        assert!(!is_internal_host_session(&guest));
    }

    #[test]
    fn guest_create_argv_uses_self_binary_and_detached_attach() {
        let argv = guest_create_argv("workspace-b", &builtin("vibecrafted"));
        assert_eq!(
            argv,
            vec![
                "vc-frame:self",
                "--layout",
                "vibecrafted",
                "--guest-workspace",
                "attach",
                "-b",
                "-c",
                "workspace-b",
            ]
        );
    }

    #[test]
    fn only_frame_host_owns_guest_tab_routing() {
        assert!(host_owns_guest_surface_routing(true));
        assert!(!host_owns_guest_surface_routing(false));
    }

    #[test]
    fn visit_attach_tab_stays_one_based_for_go_to_tab() {
        assert_eq!(visit_attach_tab(None), Ok(None));
        assert_eq!(visit_attach_tab(Some(1)), Ok(Some(1)));
        assert_eq!(visit_attach_tab(Some(2)), Ok(Some(2)));
        assert!(visit_attach_tab(Some(0)).is_err());
        assert!(
            attach_tab_matches_projection(Some(2), Some(1)),
            "--tab 2 must name 0-based projection tab 1"
        );
        assert!(
            !attach_tab_matches_projection(Some(1), Some(1)),
            "subtracting before attach would match the first tab and miss tab two"
        );
        assert!(!attach_tab_matches_projection(Some(2), Some(0)));
        assert!(attach_tab_matches_projection(None, None));
        assert!(!attach_tab_matches_projection(Some(2), None));
    }

    #[test]
    fn guest_surface_pipe_targets_one_instance_and_prefers_interactive_client() {
        let targets = vec![(7, Some(2u16)), (7, Some(9u16)), (8, Some(2u16))];
        let unique = unique_guest_surface_pipe_targets(VC_GUEST_SURFACE_MESSAGE, targets, Some(2));
        assert_eq!(unique, vec![(7, Some(9)), (8, Some(2))]);
        let passthrough = unique_guest_surface_pipe_targets("other", vec![(1, Some(1u16))], None);
        assert_eq!(passthrough, vec![(1, Some(1))]);
    }

    #[test]
    fn guest_surface_pipe_drops_ambiguous_interactive_clients() {
        let targets = vec![(7, Some(3u16)), (7, Some(9u16))];
        let unique = unique_guest_surface_pipe_targets(VC_GUEST_SURFACE_MESSAGE, targets, Some(2));
        assert!(
            unique.is_empty(),
            "two interactive clients must not infer ownership from list order"
        );
    }

    #[test]
    fn receipt_rejects_other_request_guest_tab_and_missing_mutation() {
        let receipt = WorkspaceProjectionReceipt {
            request_id: "new".into(),
            client_id: 4,
            plugin_id: 7,
            guest: "a".into(),
            tab: Some(1),
            pane_id: Some(12),
            status: ProjectionStatus::Handled,
            detail: String::new(),
        };
        assert!(receipt.acknowledges("new", "a", Some(1)));
        assert!(!receipt.acknowledges("old", "a", Some(1)));
        assert!(!receipt.acknowledges("new", "b", Some(1)));
        assert!(!receipt.acknowledges("new", "a", Some(0)));
        assert!(
            !WorkspaceProjectionReceipt {
                pane_id: None,
                ..receipt
            }
            .acknowledges("new", "a", Some(1))
        );
    }

    #[test]
    fn empty_peer_tabs_are_unknown_not_a_missing_first_tab() {
        let unknown = SessionInfo {
            name: "workspace-a".into(),
            ..SessionInfo::default()
        };
        assert!(
            guest_projection_tab_is_available(&unknown, Some(0)),
            "socket-discovered guests with empty tabs must accept --tab 1 / position 0"
        );
        assert!(guest_projection_tab_is_available(&unknown, Some(1)));
        assert!(guest_projection_tab_is_available(&unknown, None));
        let listed = SessionInfo {
            name: "workspace-a".into(),
            tabs: vec![TabInfo {
                position: 1,
                ..Default::default()
            }],
            ..SessionInfo::default()
        };
        assert!(
            !guest_projection_tab_is_available(&listed, Some(0)),
            "a materialized tab list still refuses a position it does not contain"
        );
        assert!(
            !guest_projection_tab_is_available(&listed, Some(99)),
            "a genuine invalid tab stays refused after empty-tabs became unknown"
        );
        assert!(guest_projection_tab_is_available(&listed, Some(1)));
        assert!(guest_projection_tab_is_available(&listed, None));
    }

    #[test]
    fn configured_projection_owner_rejects_unrelated_chrome() {
        assert!(plugin_is_configured_projection_owner(
            &host_session_manager_configuration()
        ));
        assert!(
            !plugin_is_configured_projection_owner(&BTreeMap::from([(
                "session_canvas".to_owned(),
                "true".to_owned()
            )])),
            "compact-bar is not a projection owner"
        );
        assert!(
            !plugin_is_configured_projection_owner(&BTreeMap::from([(
                "workspace_surface".to_owned(),
                "true".to_owned()
            )])),
            "VC Guest session-manager is not a projection owner"
        );
        assert!(
            !plugin_is_configured_projection_owner(&BTreeMap::from([(
                "rail".to_owned(),
                "true".to_owned()
            )])),
            "ordinary session-rail without frame_host is not a projection owner"
        );
        assert!(!plugin_is_configured_projection_owner(&BTreeMap::from([(
            "frame_host".to_owned(),
            "true".to_owned()
        )])));
        assert_eq!(
            select_configured_projection_owner([3u32], [1u16]),
            ProjectionOwnerSelection::Unique {
                plugin_id: 3,
                client_id: 1,
            }
        );
        assert_eq!(
            select_configured_projection_owner(std::iter::empty::<u32>(), [1u16]),
            ProjectionOwnerSelection::None
        );
        assert_eq!(
            select_configured_projection_owner([3u32], std::iter::empty::<u16>()),
            ProjectionOwnerSelection::None
        );
        assert_eq!(
            select_configured_projection_owner([3u32], [1u16, 2]),
            ProjectionOwnerSelection::Ambiguous { count: 2 }
        );
        assert_eq!(
            select_configured_projection_owner([3u32, 9], [1u16]),
            ProjectionOwnerSelection::Ambiguous { count: 2 }
        );
    }

    #[test]
    fn unique_owning_client_refuses_missing_and_ambiguous() {
        let clients = [1u16, 2];
        assert_eq!(
            prove_unique_owning_client(&clients, |_| false).err(),
            Some(ProjectionRefuse::AmbiguousOwningClient)
        );
        assert_eq!(
            prove_unique_owning_client(&[7u16], |_| true).err(),
            Some(ProjectionRefuse::MissingOwningClient)
        );
        assert_eq!(
            prove_unique_owning_client(&[3u16, 9], |id| *id == 3)
                .ok()
                .copied(),
            Some(9)
        );
    }

    #[test]
    fn project_payload_is_launcher_callable_without_pending_state() {
        let payload = project_guest_payload("workspace-a", None);
        match parse_guest_surface_payload(&payload) {
            Some(GuestSurfaceRequest::Project { session, tab }) => {
                assert_eq!(session, "workspace-a");
                assert_eq!(tab, None);
            },
            other => panic!("expected project, got {other:?}"),
        }
        let argv = project_workspace_argv("frame-host", "workspace-b", Some(2));
        assert_eq!(
            argv,
            vec![
                "vc-frame:self",
                "--session",
                "frame-host",
                "project-workspace",
                "workspace-b",
                "--tab",
                "3",
            ]
        );
    }

    #[test]
    fn activate_tab_payload_is_not_a_silent_project() {
        let payload = activate_guest_tab_payload("workspace-a", 1);
        match parse_guest_surface_payload(&payload) {
            Some(GuestSurfaceRequest::ActivateTab { session, tab }) => {
                assert_eq!(session, "workspace-a");
                assert_eq!(tab, 1);
            },
            other => panic!("expected activate, got {other:?}"),
        }
        assert_eq!(
            host_cli_guest_surface_visit(true, VC_GUEST_SURFACE_MESSAGE, Some(&payload)),
            Some(("workspace-a".to_owned(), Some(1))),
            "host CLI activate_tab is a real owning-rail visit"
        );
        assert_eq!(
            host_cli_guest_surface_visit(false, VC_GUEST_SURFACE_MESSAGE, Some(&payload)),
            None,
            "ordinary floating manager must refuse to own the CLI pipe"
        );
        assert_eq!(
            host_cli_guest_surface_visit(
                true,
                VC_GUEST_SURFACE_MESSAGE,
                Some(&project_guest_payload("workspace-b", Some(0))),
            ),
            Some(("workspace-b".to_owned(), Some(0)))
        );
        assert_eq!(
            host_cli_guest_surface_visit(
                true,
                VC_GUEST_SURFACE_MESSAGE,
                Some(r#"{"session":"workspace-b","tabs":[]}"#),
            ),
            None,
            "surface broadcasts are not CLI visits"
        );
    }

    #[test]
    fn surface_payload_carries_guest_identity_and_host_plugin_id() {
        let payload = r#"{"session":"workspace-b","host_plugin_id":7,"status":"workspace-b","tabs":[{"name":"Start here","active":true,"position":0}]}"#;
        match parse_guest_surface_payload(payload) {
            Some(GuestSurfaceRequest::Surface {
                session,
                host_plugin_id,
                tabs,
            }) => {
                assert_eq!(session, "workspace-b");
                assert_eq!(host_plugin_id, Some(7));
                assert_eq!(tabs[0].name, "Start here");
            },
            other => panic!("expected surface, got {other:?}"),
        }
    }

    #[test]
    fn host_rail_configuration_pins_frame_host() {
        let config = host_session_manager_configuration();
        assert_eq!(config.get("frame_host").map(String::as_str), Some("true"));
        assert_eq!(config.get("rail").map(String::as_str), Some("true"));
        assert_eq!(
            config.get("session_canvas_kind").map(String::as_str),
            Some("session-manager"),
            "layout identity stays session-manager; owner lookup must not fold the rail into the canvas singleton"
        );
        assert!(plugin_is_configured_projection_owner(&config));
        assert_eq!(VC_FRAME_HOST_PLUGIN_ALIAS, "frame-host");
    }

    #[test]
    fn quick_cmd_prefers_canonical_vibecrafted_config() {
        let paths = quick_cmd_wrapper_paths();
        assert!(paths[0].contains(".config/vibecrafted/vc-frame/vc-quick-cmd.sh"));
        let runner = quick_cmd_runner_script();
        assert!(runner.contains(".config/vibecrafted/vc-frame/vc-quick-cmd.sh"));
        assert!(
            runner.find(".config/vibecrafted/vc-frame").unwrap()
                < runner.find(".config/vetcoders/frontier").unwrap()
        );
    }
}
