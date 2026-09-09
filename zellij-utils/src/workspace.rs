//! Shared-canvas workspace identity: distinct guest sessions under one host.
//!
//! New Session must create a real session process, not extra tabs on the
//! current client. The host rail lists those guests; `vc-frame visit` replaces
//! only the guest pane. Same-name creation refuses before mutation.

use std::path::PathBuf;

use crate::data::{LayoutInfo, PluginInfo, SessionInfo};

/// Custom plugin message: host chrome mirrors the visited guest's tabs.
pub const VC_GUEST_SURFACE_MESSAGE: &str = "vc.guest-surface.v1";

/// Context key on background `attach -b -c` so the host can visit after spawn.
pub const VC_GUEST_CREATE_CONTEXT_KEY: &str = "vc_frame_guest_create";

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
}
