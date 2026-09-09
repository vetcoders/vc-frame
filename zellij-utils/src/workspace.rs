//! Shared-canvas workspace identity: distinct guest sessions under one host.
//!
//! New Session must create a real session process, not extra tabs on the
//! current client. The host rail lists those guests; `vc-frame visit` replaces
//! only the guest pane. Same-name creation refuses before mutation.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::data::{LayoutInfo, PaneListEntry, PluginInfo, SessionInfo};

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

/// Sentinel that only the host layout's registered placeholder process carries.
/// Titles, focus, and ordinary shells must never authorize replacement.
pub const VC_GUEST_SURFACE_HOLD_SENTINEL: &str = "VC_FRAME_GUEST_SURFACE=1";

/// Shell script for the host layout placeholder. The process must stay this
/// command (no `exec zsh`) so list-panes can prove the registered identity.
pub const VC_GUEST_SURFACE_HOLD_SCRIPT: &str =
    "VC_FRAME_GUEST_SURFACE=1; while :; do sleep 86400; done";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingGuestRequest {
    pub session: String,
    pub tab: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredGuestSurface {
    pub pane_id: u32,
    pub command: String,
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

/// Exact `vc-frame visit <session>` argv — not a title or substring match.
pub fn parse_guest_visit_session(command: &str) -> Option<String> {
    let tokens = split_command_tokens(command);
    let visit_idx = tokens.iter().position(|token| token == "visit")?;
    if visit_idx == 0 {
        return None;
    }
    if !is_vc_frame_binary(&tokens[visit_idx - 1]) {
        return None;
    }
    tokens
        .get(visit_idx + 1)
        .filter(|session| !session.is_empty() && !session.starts_with('-'))
        .cloned()
}

pub fn command_is_guest_visit(command: &str) -> bool {
    parse_guest_visit_session(command).is_some()
}

pub fn is_guest_surface_hold_command(command: &str) -> bool {
    command.contains(VC_GUEST_SURFACE_HOLD_SENTINEL)
}

pub fn is_registered_guest_surface_command(command: &str) -> bool {
    is_guest_surface_hold_command(command) || command_is_guest_visit(command)
}

pub fn plugin_url_is_frame_host(plugin_url: Option<&str>) -> bool {
    let url = plugin_url.unwrap_or("").trim();
    if url.is_empty() {
        return false;
    }
    let file_name = url.rsplit(['/', ':']).next().unwrap_or(url);
    file_name == VC_FRAME_HOST_PLUGIN_ALIAS
        || file_name == format!("{VC_FRAME_HOST_PLUGIN_ALIAS}.wasm")
        || file_name == "session-manager"
        || file_name == "session-manager.wasm"
}

pub fn prove_frame_host_role(entries: &[PaneListEntry]) -> Result<u32, ProjectionRefuse> {
    let hosts: Vec<&PaneListEntry> = entries
        .iter()
        .filter(|entry| {
            entry.pane_info.is_plugin
                && !entry.pane_info.is_floating
                && plugin_url_is_frame_host(entry.pane_info.plugin_url.as_deref())
        })
        .collect();
    match hosts.as_slice() {
        [host] => Ok(host.pane_info.id),
        [] => Err(ProjectionRefuse::NotAHost),
        _ => Err(ProjectionRefuse::AmbiguousHost),
    }
}

fn pane_listed_command(entry: &PaneListEntry) -> Option<&str> {
    // Layout-invoked identity beats a truncated OS argv (`sh -c` without the
    // script). macOS process listings routinely drop `-c` operands.
    entry
        .pane_info
        .terminal_command
        .as_deref()
        .filter(|command| !command.is_empty())
        .or(entry.pane_command.as_deref())
        .filter(|command| !command.is_empty())
}

/// Unique tiled terminal whose command is the registered hold sentinel or an
/// exact visit argv. Titles, focus, and first-match fallbacks never authorize.
pub fn prove_unique_registered_guest_surface(
    entries: &[PaneListEntry],
) -> Result<RegisteredGuestSurface, ProjectionRefuse> {
    let candidates: Vec<&PaneListEntry> = entries
        .iter()
        .filter(|entry| !entry.pane_info.is_plugin && !entry.pane_info.is_floating)
        .filter(|entry| pane_listed_command(entry).is_some_and(is_registered_guest_surface_command))
        .collect();
    match candidates.as_slice() {
        [surface] => Ok(RegisteredGuestSurface {
            pane_id: surface.pane_info.id,
            command: pane_listed_command(surface).unwrap_or("").to_owned(),
        }),
        [] => Err(ProjectionRefuse::MissingGuestSurface),
        _ => Err(ProjectionRefuse::AmbiguousGuestSurface),
    }
}

/// Pick the host content pane from a `list-panes --json` snapshot.
/// `None` means refuse — never a focused or title-only fallback.
pub fn guest_surface_pane_id_from_entries(entries: &[PaneListEntry]) -> Option<u32> {
    prove_unique_registered_guest_surface(entries)
        .ok()
        .map(|surface| surface.pane_id)
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

fn is_vc_frame_binary(token: &str) -> bool {
    let name = token.rsplit('/').next().unwrap_or(token);
    name == "vc-frame" || name == "vc-frame:self" || name == "zellij"
}

fn split_command_tokens(command: &str) -> Vec<String> {
    command
        .split_whitespace()
        .map(|token| token.trim_matches(|c| c == '\'' || c == '"').to_owned())
        .filter(|token| !token.is_empty())
        .collect()
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
                assert!(supported_commands
                    .iter()
                    .any(|command| command.contains("vc-frame attach workspace-a")));
                assert!(supported_commands
                    .iter()
                    .any(|command| command.contains("vc-frame visit workspace-a")));
            },
            other => panic!("expected refuse, got {other:?}"),
        }
        assert!(plan
            .duplicate_message()
            .unwrap()
            .contains("refused before mutation"));
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

    fn pane_entry(
        id: u32,
        plugin: bool,
        title: &str,
        command: Option<&str>,
        focused: bool,
    ) -> crate::data::PaneListEntry {
        use crate::data::{PaneInfo, PaneListEntry};
        PaneListEntry {
            pane_info: PaneInfo {
                id,
                is_plugin: plugin,
                title: title.to_owned(),
                is_focused: focused,
                plugin_url: plugin.then(|| VC_FRAME_HOST_PLUGIN_ALIAS.to_owned()),
                ..PaneInfo::default()
            },
            plugin_runtime_id: None,
            tab_id: 0,
            tab_position: 0,
            tab_name: "Workspace".to_owned(),
            pane_command: command.map(ToOwned::to_owned),
            pane_cwd: None,
        }
    }

    #[test]
    fn registered_hold_surface_is_the_only_authorized_placeholder() {
        let entries = vec![
            pane_entry(3, true, VC_GUEST_PANE_TITLE, None, false),
            pane_entry(
                11,
                false,
                VC_GUEST_PANE_TITLE,
                Some(VC_GUEST_SURFACE_HOLD_SCRIPT),
                false,
            ),
        ];
        assert_eq!(guest_surface_pane_id_from_entries(&entries), Some(11));
        assert!(prove_frame_host_role(&entries).is_ok());
    }

    #[test]
    fn truncated_os_argv_does_not_hide_invoked_hold_identity() {
        use crate::data::{PaneInfo, PaneListEntry};
        let entries = vec![PaneListEntry {
            pane_info: PaneInfo {
                id: 11,
                is_plugin: false,
                title: VC_GUEST_PANE_TITLE.to_owned(),
                terminal_command: Some(format!("sh -c {}", VC_GUEST_SURFACE_HOLD_SCRIPT)),
                ..PaneInfo::default()
            },
            plugin_runtime_id: None,
            tab_id: 0,
            tab_position: 0,
            tab_name: "Workspace".to_owned(),
            pane_command: Some("sh -c".to_owned()),
            pane_cwd: None,
        }];
        assert_eq!(guest_surface_pane_id_from_entries(&entries), Some(11));
        assert!(prove_unique_registered_guest_surface(&entries)
            .unwrap()
            .command
            .contains(VC_GUEST_SURFACE_HOLD_SENTINEL));
    }

    #[test]
    fn title_focus_and_visit_substring_do_not_authorize_replacement() {
        let focused_shell = vec![pane_entry(4, false, "zsh", Some("zsh -l"), true)];
        assert_eq!(guest_surface_pane_id_from_entries(&focused_shell), None);
        assert_eq!(
            prove_unique_registered_guest_surface(&focused_shell),
            Err(ProjectionRefuse::MissingGuestSurface)
        );

        let deceptive_title = vec![pane_entry(
            5,
            false,
            VC_GUEST_PANE_TITLE,
            Some("zsh -l"),
            true,
        )];
        assert_eq!(guest_surface_pane_id_from_entries(&deceptive_title), None);

        let substring = vec![pane_entry(
            6,
            false,
            "visitor",
            Some("echo visit workspace-a"),
            true,
        )];
        assert_eq!(guest_surface_pane_id_from_entries(&substring), None);
        assert!(!command_is_guest_visit("echo visit workspace-a"));
        assert!(command_is_guest_visit("vc-frame visit workspace-a --tab 2"));
    }

    #[test]
    fn two_registered_surfaces_or_two_hosts_refuse() {
        let two_surfaces = vec![
            pane_entry(1, false, "a", Some("vc-frame visit workspace-a"), false),
            pane_entry(2, false, "b", Some("vc-frame visit workspace-b"), false),
        ];
        assert_eq!(
            prove_unique_registered_guest_surface(&two_surfaces),
            Err(ProjectionRefuse::AmbiguousGuestSurface)
        );

        let two_hosts = vec![
            pane_entry(8, true, "Sessions", None, false),
            pane_entry(9, true, "Sessions", None, false),
        ];
        assert_eq!(
            prove_frame_host_role(&two_hosts),
            Err(ProjectionRefuse::AmbiguousHost)
        );
        assert_eq!(
            prove_frame_host_role(&[pane_entry(4, false, "zsh", Some("zsh -l"), true)]),
            Err(ProjectionRefuse::NotAHost)
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
