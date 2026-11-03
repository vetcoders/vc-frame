//! The rail declares a project; the selected Runtime Pack admits its native seat.
//! This transports that owner's receipt, never resolves or creates a workspace.
use super::*;

const REQUEST_KEY: &str = "vc_frame_project_open_request";
const OPEN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
pub(super) struct ProjectAdmission {
    schema: String,
    session_name: String,
    session_incarnation: String,
    socket_dir: String,
    workspace_root: String,
    workspace_id: String,
    workspace_instance_id: String,
    session_id: String,
    build_id: String,
}

impl ProjectAdmission {
    fn complete(&self) -> bool {
        self.schema == "vc-frame.project-admission.v1"
            && [
                &self.session_name,
                &self.session_incarnation,
                &self.socket_dir,
                &self.workspace_root,
                &self.workspace_id,
                &self.workspace_instance_id,
                &self.session_id,
                &self.build_id,
            ]
            .iter()
            .all(|value| !value.trim().is_empty())
    }
}

pub(super) struct PendingProjectOpen {
    request_id: String,
    argv: Vec<String>,
    launched: bool,
    admission: Option<ProjectAdmission>,
    deadline: Instant,
}

pub(super) fn has_product_context() -> bool {
    // An incomplete product environment must refuse through the adapter too.
    // Standalone Frame still creates ordinary native sessions without a claim.
    [
        "VIBECRAFTED_RUNTIME_ROOT",
        "VIBECRAFTED_RUNTIME_BIN",
        "VIBECRAFTED_ROOT",
        "VIBECRAFTED_CORE_DIR",
        "VIBECRAFTED_WORKSPACE_ID",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some())
}

impl State {
    pub(super) fn prepare_project(
        &mut self,
        folder: PathBuf,
        requested_name: Option<String>,
        layout: Option<LayoutInfo>,
    ) {
        if self.pending_project_open.is_some() {
            self.show_error("A project is already being opened; wait for its result.");
            return;
        }
        let mut argv = vec![
            VC_FRAME_SELF_EXECUTABLE.to_owned(),
            "prepare-project".to_owned(),
            "--root".to_owned(),
            folder.to_string_lossy().into_owned(),
        ];
        if let Some(name) = requested_name.filter(|name| !name.is_empty()) {
            argv.extend(["--requested-session".to_owned(), name]);
        }
        if let Some(mut layout) = layout {
            if let LayoutInfo::File(name, _) = &mut layout
                && !PathBuf::from(&*name).is_absolute()
            {
                // Resolve relative names against the source server's actual
                // layout directory, not the helper process's fresh config.
                let directory = PathBuf::from(get_layout_dir());
                if !directory.is_absolute() {
                    self.show_error(
                        "The source layout directory is unavailable; project preparation refused.",
                    );
                    return;
                }
                *name = directory.join(&*name).to_string_lossy().into_owned();
            }
            let Ok(json) = serde_json::to_string(&layout) else {
                self.show_error("The selected layout could not be sent to the project owner.");
                return;
            };
            argv.extend(["--layout-json".to_owned(), json]);
        }
        self.pending_project_open = Some(PendingProjectOpen {
            request_id: Uuid::new_v4().to_string(),
            argv,
            launched: false,
            admission: None,
            deadline: Instant::now() + OPEN_TIMEOUT,
        });
        self.error = None;
        request_permission(&[PermissionType::RunCommands]);
        self.arm_refresh_timer();
    }

    fn reject_project_open(&mut self, message: &str) -> bool {
        if self.pending_project_open.take().is_none() {
            return false;
        }
        self.show_error(message);
        true
    }

    pub(super) fn expire_project_open(&mut self, now: Instant) -> bool {
        if self
            .pending_project_open
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline)
        {
            self.reject_project_open(
                "Project preparation timed out. The current session was preserved; inspect the Sessions rail before retrying.",
            )
        } else {
            false
        }
    }

    pub(super) fn project_open_event(&mut self, event: &Event) -> Option<bool> {
        match event {
            Event::PermissionRequestResult(result)
                if self
                    .pending_project_open
                    .as_ref()
                    .is_some_and(|pending| !pending.launched) =>
            {
                if self.expire_project_open(Instant::now()) {
                    return Some(true);
                }
                if *result != PermissionStatus::Granted {
                    return Some(
                        self.reject_project_open("Project preparation permission was denied."),
                    );
                }
                let pending = self.pending_project_open.as_mut().unwrap();
                pending.launched = true;
                let argv: Vec<&str> = pending.argv.iter().map(String::as_str).collect();
                run_command(
                    &argv,
                    BTreeMap::from([(REQUEST_KEY.to_owned(), pending.request_id.clone())]),
                );
                Some(true)
            },
            Event::RunCommandResult(code, stdout, stderr, context)
                if context.contains_key(REQUEST_KEY) =>
            {
                if self.expire_project_open(Instant::now()) {
                    return Some(true);
                }
                let Some(pending) = self.pending_project_open.as_mut() else {
                    return Some(false);
                };
                if !pending.launched
                    || pending.admission.is_some()
                    || context.get(REQUEST_KEY) != Some(&pending.request_id)
                {
                    return Some(false);
                }
                if *code != Some(0) {
                    let detail = String::from_utf8_lossy(stderr);
                    let detail = detail.trim();
                    let message = if detail.is_empty() {
                        "The selected runtime refused project preparation. The current session was preserved.".to_owned()
                    } else {
                        format!(
                            "Project could not be opened: {}",
                            detail.chars().take(1600).collect::<String>()
                        )
                    };
                    return Some(self.reject_project_open(&message));
                }
                match serde_json::from_slice::<ProjectAdmission>(stdout) {
                    Ok(admission) if admission.complete() => pending.admission = Some(admission),
                    _ => return Some(self.reject_project_open("Project preparation returned no complete admission receipt; navigation refused.")),
                }
                // A successful owner result is still checked against fresh native
                // inventory, including incarnation, before moving this frontend.
                #[cfg(target_family = "wasm")]
                if let Ok(snapshot) = get_session_list() {
                    self.update_session_infos(snapshot.live_sessions);
                }
                Some(true)
            },
            _ => None,
        }
    }

    pub(super) fn settle_project_open(&mut self, sessions: &[SessionInfo], now: Instant) -> bool {
        if self.expire_project_open(now) {
            return true;
        }
        let Some(admission) = self
            .pending_project_open
            .as_ref()
            .and_then(|pending| pending.admission.as_ref())
        else {
            return false;
        };
        let mut matches = sessions
            .iter()
            .filter(|session| session.name == admission.session_name);
        let Some(session) = matches.next() else {
            return false;
        };
        let conflicts = matches.next().is_some() || is_internal_host_session(session);
        if !conflicts && session.session_incarnation.is_empty() {
            // A socket can precede the detached seat's first metadata write.
            // Keep the owner's admission pending; an unnamed lifetime grants
            // no navigation authority, including for a web client.
            return false;
        }
        if conflicts
            || session.session_incarnation != admission.session_incarnation
            || (self.is_web_client && !session.web_clients_allowed)
        {
            return self
                .reject_project_open("The admitted project session changed; navigation refused.");
        }
        let name = admission.session_name.clone();
        let incarnation = admission.session_incarnation.clone();
        self.pending_project_open = None;
        if self.session_name.as_deref() != Some(name.as_str()) {
            #[cfg(target_family = "wasm")]
            switch_session_with_incarnation(&name, &incarnation);
            #[cfg(test)]
            {
                self.admitted_project_switch = Some((name, incarnation));
            }
            #[cfg(all(not(target_family = "wasm"), not(test)))]
            let _ = (name, incarnation);
        }
        if should_hide_manager_after_guest_create(self.frame_host, self.workspace_surface)
            && !self.is_rail
        {
            hide_self();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting() -> State {
        State {
            is_rail: true,
            session_name: Some("source".into()),
            pending_project_open: Some(PendingProjectOpen {
                request_id: "request-1".into(),
                argv: vec![],
                launched: true,
                admission: None,
                deadline: Instant::now() + OPEN_TIMEOUT,
            }),
            ..Default::default()
        }
    }

    fn result(request: &str, incarnation: &str) -> Event {
        Event::RunCommandResult(
            Some(0),
            serde_json::to_vec(&serde_json::json!({
                "schema": "vc-frame.project-admission.v1",
                "session_name": "project-collision-resolved",
                "session_incarnation": incarnation,
                "socket_dir": "/private/socket",
                "workspace_root": "/projects/project",
                "workspace_id": "workspace-1",
                "workspace_instance_id": "instance-1",
                "session_id": "session-1",
                "build_id": "build-1"
            }))
            .unwrap(),
            vec![],
            BTreeMap::from([(REQUEST_KEY.into(), request.into())]),
        )
    }

    #[test]
    fn admission_waits_for_exact_live_incarnation_and_uses_resolved_name() {
        let mut state = waiting();
        state.update(result("another-request", "life-1"));
        assert!(
            state
                .pending_project_open
                .as_ref()
                .unwrap()
                .admission
                .is_none()
        );
        state.update(result("request-1", "life-1"));
        assert!(state.pending_session_switch.is_none());
        assert!(!state.settle_project_open(&[], Instant::now()));
        // Socket discovery can precede a detached seat's first metadata write.
        // Its fallback inventory row has no lifetime yet, not a replacement.
        for web_client in [false, true] {
            state.is_web_client = web_client;
            assert!(!state.settle_project_open(
                &[SessionInfo {
                    name: "project-collision-resolved".into(),
                    ..Default::default()
                }],
                Instant::now()
            ));
        }
        state.is_web_client = false;
        assert!(state.pending_project_open.is_some());
        assert!(state.admitted_project_switch.is_none());
        assert!(state.error.is_none());
        assert!(state.settle_project_open(
            &[SessionInfo {
                name: "project-collision-resolved".into(),
                session_incarnation: "life-1".into(),
                ..Default::default()
            }],
            Instant::now()
        ));
        assert!(state.pending_project_open.is_none());
        assert_eq!(
            state.admitted_project_switch,
            Some(("project-collision-resolved".into(), "life-1".into()))
        );
    }

    #[test]
    fn replacement_and_host_role_refuse_navigation() {
        for host in [false, true] {
            let mut state = waiting();
            state.update(result("request-1", "life-1"));
            let mut peer = SessionInfo {
                name: "project-collision-resolved".into(),
                session_incarnation: if host { "life-1" } else { "replaced" }.into(),
                ..Default::default()
            };
            if host {
                peer.plugins = BTreeMap::from([(
                    1,
                    PluginInfo {
                        location: "session-manager".into(),
                        configuration: BTreeMap::from([("frame_host".into(), "true".into())]),
                    },
                )]);
            }
            assert!(state.settle_project_open(&[peer], Instant::now()));
            assert!(state.pending_session_switch.is_none());
            assert!(state.pending_project_open.is_none());
            assert!(state.error.as_deref().unwrap().contains("changed"));
        }
    }

    #[test]
    fn incomplete_or_failed_result_and_timeout_never_switch_or_retry() {
        for event in [
            Event::RunCommandResult(
                Some(0),
                b"{}".to_vec(),
                vec![],
                BTreeMap::from([(REQUEST_KEY.into(), "request-1".into())]),
            ),
            Event::RunCommandResult(
                Some(4),
                vec![],
                b"foreign owner".to_vec(),
                BTreeMap::from([(REQUEST_KEY.into(), "request-1".into())]),
            ),
        ] {
            let mut state = waiting();
            state.update(event);
            assert!(state.pending_project_open.is_none());
            assert!(state.pending_session_switch.is_none());
            assert!(state.error.is_some());
        }
        let mut state = waiting();
        assert!(state.expire_project_open(Instant::now() + OPEN_TIMEOUT));
        state.update(result("request-1", "life-1"));
        assert!(state.pending_project_open.is_none());
        assert!(state.pending_session_switch.is_none());
    }
}
