//! `repair servers` — name the engine each live session runs; treat drift.
//!
//! Identity is the engine's own build record read from the very image the
//! kernel says it executes, compared with the intended artifact — not process
//! age against a file's mtime.
//!
//! Treatment is where honesty matters most. Bringing a live session onto the
//! installed engine without ending its shells and agents needs an engine
//! handover: the old process passing its PTY masters, its children and its
//! screen state to the new image. No vc-frame engine has that capability — a
//! pane child closes every descriptor above stdio, the server alone holds the
//! PTY master, and its lease is close-on-exec. The only existing paths (kill,
//! save→resurrect) end or re-launch the processes, which is not a repair. So
//! a drifted engine is identified precisely and refused by name; the C2
//! decision (2026-08-04) already forbids automatic kills for the same reason.

use std::path::PathBuf;

use zellij_utils::consts::CLIENT_SERVER_CONTRACT_VERSION;
use zellij_utils::process_identity::{ProcessIdentity, current_uid};

use super::inventory::{
    Admission, Endpoint, EndpointClass, EngineIdentity, EngineImage, Intended, is_vc_frame_image,
};
use super::treatment::{Scope, Step, Surface, Treatment};
use super::{Finding, Section, Severity};

/// The capability whose absence turns every engine drift into a refusal.
pub const ENGINE_HANDOVER: &str = "engine-handover: pass a running session's PTY masters, child \
     processes and screen state to the installed engine without ending them (no vc-frame engine \
     implements it; kill and save→resurrect both end or re-launch the processes)";

/// How a running engine relates to the intended artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineVerdict {
    Current,
    /// Same contract, different build.
    Superseded {
        running: String,
        intended: String,
    },
    /// A different wire contract: this binary cannot even talk to it.
    ForeignContract(usize),
    /// The image changed on disk after start; the running build is unknowable.
    ImageReplaced(PathBuf),
    /// The image is intact but does not report a build (pre-provenance).
    Unreported,
    /// The kernel did not name the image.
    Unnamed,
}

impl EngineVerdict {
    pub fn is_drift(&self) -> bool {
        !matches!(self, EngineVerdict::Current)
    }

    pub fn sentence(&self) -> String {
        match self {
            EngineVerdict::Current => "runs the intended build".into(),
            EngineVerdict::Superseded { running, intended } => {
                format!("runs {running}, the intended build is {intended}")
            },
            EngineVerdict::ForeignContract(version) => format!(
                "speaks contract_version_{version}; this binary speaks contract_version_{CLIENT_SERVER_CONTRACT_VERSION}"
            ),
            EngineVerdict::ImageReplaced(path) => format!(
                "its image {} was replaced after it started — it runs bytes that no longer exist on disk",
                path.display()
            ),
            EngineVerdict::Unreported => {
                "its image does not report a build identity (older than --build-info)".into()
            },
            EngineVerdict::Unnamed => "the kernel cannot name its image".into(),
        }
    }
}

pub fn judge(engine: &EngineIdentity, intended: &Intended) -> EngineVerdict {
    if engine.contract_version != CLIENT_SERVER_CONTRACT_VERSION {
        return EngineVerdict::ForeignContract(engine.contract_version);
    }
    match &engine.image {
        EngineImage::Replaced { path } => return EngineVerdict::ImageReplaced(path.clone()),
        EngineImage::Unnamed => return EngineVerdict::Unnamed,
        EngineImage::Intact { .. } => {},
    }
    match &engine.build {
        None => EngineVerdict::Unreported,
        Some(build) if build.same_build(&intended.build) => EngineVerdict::Current,
        Some(build) => EngineVerdict::Superseded {
            running: describe_build(build),
            intended: describe_build(&intended.build),
        },
    }
}

fn describe_build(build: &super::inventory::EngineBuild) -> String {
    format!(
        "{} ({}, built {})",
        build.human_version, build.profile, build.build_time_utc
    )
}

/// `vc-frame --server` processes of this user that no live endpoint names as
/// its listener: running, but unreachable by any client.
pub fn undiscoverable_servers(endpoints: &[Endpoint]) -> Vec<(ProcessIdentity, String)> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output()
    else {
        return vec![];
    };
    let our_uid = current_uid();
    // Only servers started for this namespace root are ours to judge; a
    // server of another socket root is invisible here by design, not lost.
    let Some(root) = zellij_utils::consts::ZELLIJ_SOCK_DIR.parent() else {
        return vec![];
    };
    let listeners: Vec<i32> = endpoints
        .iter()
        .filter_map(|endpoint| endpoint.peer.as_ref().map(|peer| peer.pid))
        .collect();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (pid, args) = line.split_once(char::is_whitespace)?;
            let pid: i32 = pid.parse().ok()?;
            let socket = args.split(" --server ").nth(1)?.trim().to_owned();
            if !std::path::Path::new(&socket).starts_with(root) {
                return None;
            }
            // argv is only a claim; the kernel decides who the process is.
            let identity = ProcessIdentity::of(pid).ok().flatten()?;
            (identity.uid == our_uid
                && identity
                    .executable
                    .as_deref()
                    .is_some_and(is_vc_frame_image)
                && !listeners.contains(&pid))
            .then_some((identity, socket))
        })
        .collect()
}

/// Read-only diagnosis of every live engine, for `doctor`.
pub fn findings(endpoints: &[Endpoint], intended: &Intended) -> (Vec<Finding>, String) {
    let mut findings = vec![];
    let mut current = 0usize;
    for endpoint in endpoints.iter().filter(|e| e.class == EndpointClass::Live) {
        let Some(engine) = EngineIdentity::of(endpoint) else {
            continue;
        };
        let verdict = judge(&engine, intended);
        if !verdict.is_drift() {
            current += 1;
            continue;
        }
        findings.push(
            Finding::new(
                Section::ServerDrift,
                Severity::Warn,
                format!("session {} {}", endpoint.name, verdict.sentence()),
                format!("vc-frame repair servers --session {}", endpoint.name),
            )
            .with_detail(vec![engine.describe()]),
        );
    }
    for (process, socket) in undiscoverable_servers(endpoints) {
        findings.push(
            Finding::new(
                Section::ServerDrift,
                Severity::Warn,
                format!(
                    "server pid {} runs with no reachable endpoint (started for {socket})",
                    process.pid
                ),
                "no client can reach it; attach is impossible — inspect it by hand",
            )
            .with_detail(vec![format!(
                "up {}s · {}",
                process.age().as_secs(),
                process
                    .executable
                    .as_deref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            )]),
        );
    }
    (
        findings,
        format!("{current} live engine(s) run the intended build"),
    )
}

/// Plan the treatment for one session (or none).
pub fn treat(
    scope: Scope,
    endpoints: &[Endpoint],
    intended: &Intended,
    dry_run: bool,
) -> Treatment {
    let mut treatment = Treatment::new(Surface::Servers, scope.clone(), dry_run);
    treatment.notes.push(intended.admission_line());
    let Scope::Session { name, .. } = &scope else {
        return treatment;
    };
    let selected: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| &e.name == name && e.class == EndpointClass::Live)
        .collect();
    for endpoint in selected {
        let target = format!("session {} ({})", endpoint.name, endpoint.label());
        let Some(engine) = EngineIdentity::of(endpoint) else {
            treatment.steps.push(Step::refused(
                target,
                "identify the engine",
                "the listener vanished while it was being identified",
            ));
            continue;
        };
        let verdict = judge(&engine, intended);
        let severity = if verdict.is_drift() {
            Severity::Warn
        } else {
            Severity::Info
        };
        treatment.findings.push(
            Finding::new(
                Section::ServerDrift,
                severity,
                format!("engine of {} {}", endpoint.name, verdict.sentence()),
                if verdict.is_drift() {
                    "keep working in the session; it moves to the installed engine only when its \
                     work allows a deliberate restart"
                } else {
                    "none"
                },
            )
            .with_detail(vec![engine.describe()]),
        );
        if !verdict.is_drift() {
            continue;
        }
        if let Admission::Contradicted(why) = &intended.admission {
            treatment.steps.push(Step::refused(
                target,
                "hand the session to the intended engine",
                format!("the intended artifact itself is contradicted by its pack: {why}"),
            ));
            continue;
        }
        let mut step = Step::refused(
            target,
            "hand the session to the intended engine, keeping every shell and agent process",
            "refused: the only available paths kill or re-launch the session's processes",
        )
        .missing(ENGINE_HANDOVER);
        // The identity the refusal is about must still be the one that runs:
        // a pid that restarted (or was reused) in between is reported, not
        // silently attributed.
        match engine.process.is_same_process() {
            Ok(true) => {
                step = step.with_evidence(format!(
                    "pid {} still the same process (start time unchanged); nothing was signalled",
                    engine.process.pid
                ))
            },
            Ok(false) => {
                step = step.with_evidence(format!(
                    "pid {} is no longer the diagnosed process — re-run to re-identify",
                    engine.process.pid
                ))
            },
            Err(error) => step = step.with_evidence(format!("pid re-check failed: {error}")),
        }
        treatment.steps.push(step);
    }
    treatment
}

#[cfg(test)]
mod tests {
    use super::super::inventory::EngineBuild;
    use super::*;
    use std::time::SystemTime;

    fn engine(contract: usize, image: EngineImage, build: Option<EngineBuild>) -> EngineIdentity {
        EngineIdentity {
            process: ProcessIdentity {
                pid: 7,
                uid: 501,
                started_at: SystemTime::UNIX_EPOCH,
                executable: None,
            },
            image,
            build,
            release_label: None,
            contract_version: contract,
        }
    }

    fn intact() -> EngineImage {
        EngineImage::Intact {
            path: PathBuf::from("/rt/releases/x/libexec/vc-frame"),
        }
    }

    #[test]
    fn the_same_build_is_current_and_any_other_build_is_superseded() {
        let intended = Intended::of_this_binary();
        let same = engine(
            CLIENT_SERVER_CONTRACT_VERSION,
            intact(),
            Some(intended.build.clone()),
        );
        assert_eq!(judge(&same, &intended), EngineVerdict::Current);
        let mut older = intended.build.clone();
        older.build_time_utc = "2026-10-10T23:05:45Z".into();
        let drifted = engine(CLIENT_SERVER_CONTRACT_VERSION, intact(), Some(older));
        assert!(matches!(
            judge(&drifted, &intended),
            EngineVerdict::Superseded { .. }
        ));
    }

    #[test]
    fn contract_replacement_and_silence_are_named_not_guessed() {
        let intended = Intended::of_this_binary();
        let build = Some(intended.build.clone());
        assert_eq!(
            judge(&engine(3, intact(), build.clone()), &intended),
            EngineVerdict::ForeignContract(3)
        );
        let replaced = EngineImage::Replaced {
            path: PathBuf::from("/x/vc-frame"),
        };
        assert!(matches!(
            judge(
                &engine(CLIENT_SERVER_CONTRACT_VERSION, replaced, build),
                &intended
            ),
            EngineVerdict::ImageReplaced(_)
        ));
        assert_eq!(
            judge(
                &engine(CLIENT_SERVER_CONTRACT_VERSION, intact(), None),
                &intended
            ),
            EngineVerdict::Unreported
        );
    }
}
