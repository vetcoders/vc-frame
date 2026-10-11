//! What is actually running, and what was meant to run — read-only.
//!
//! Every runtime repair starts from the same inventory: the endpoints in
//! every `contract_version_N` namespace, the engine behind each live one (as
//! the kernel names it, never as `ps` argv claims it), and the intended
//! artifact this clinic compares against. Nothing here creates, locks,
//! signals or unlinks anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use zellij_utils::asset_integrity::{
    PLUGIN_RECEIPT, PluginReceiptCheck, parse_receipt, sha256_hex,
};
use zellij_utils::build_info::build_info;
use zellij_utils::consts::CLIENT_SERVER_CONTRACT_VERSION;
use zellij_utils::process_identity::{ProcessIdentity, current_uid};
use zellij_utils::sessions::{
    EndpointProbe, EndpointReach, LeaseState, probe_endpoint, socket_namespaces,
};

/// How long one endpoint may take to accept a connection.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
/// How long an engine binary may take to answer `--build-info` or a dump.
pub const ENGINE_QUERY_TIMEOUT: Duration = Duration::from_secs(20);

/// Socket names in a namespace that are not sessions.
const NON_SESSION_ENDPOINTS: [&str; 1] = ["web_server_bus"];

/// One pathname in a session namespace and what it turned out to be.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub contract_version: usize,
    pub name: String,
    pub probe: EndpointProbe,
    /// The kernel-named listener, when something accepts.
    pub peer: Option<ProcessIdentity>,
    pub class: EndpointClass,
}

impl Endpoint {
    /// `contract_version_4/name` — unambiguous across namespaces.
    pub fn label(&self) -> String {
        format!("contract_version_{}/{}", self.contract_version, self.name)
    }

    pub fn is_current_contract(&self) -> bool {
        self.contract_version == CLIENT_SERVER_CONTRACT_VERSION
    }
}

/// The classification the repairs act on. Only `Orphan` is ever mutable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointClass {
    /// Accepting, owned by this user, served by a vc-frame engine.
    Live,
    /// Refused, not leased, owned by this user: nothing will ever answer here.
    Orphan,
    /// Accepting, but the listener is not this user's vc-frame engine.
    ForeignOwner(String),
    /// The listener started after the endpoint was created — it did not bind
    /// it (inherited listener, or a pid identity that cannot be trusted).
    OwnerMismatch(String),
    /// Refused while a lease-aware server holds the name: a rebind in flight.
    Rebinding,
    /// Did not answer in time, or could not be classified.
    Ambiguous(String),
    /// Not a socket, another user's file, or not a session endpoint.
    Foreign(String),
}

impl EndpointClass {
    pub fn id(&self) -> &'static str {
        match self {
            EndpointClass::Live => "live",
            EndpointClass::Orphan => "orphan",
            EndpointClass::ForeignOwner(_) => "foreign-owner",
            EndpointClass::OwnerMismatch(_) => "owner-mismatch",
            EndpointClass::Rebinding => "rebinding",
            EndpointClass::Ambiguous(_) => "ambiguous",
            EndpointClass::Foreign(_) => "foreign",
        }
    }

    pub fn reason(&self) -> String {
        match self {
            EndpointClass::Live => "a vc-frame engine of this user accepts on it".into(),
            EndpointClass::Orphan => {
                "connection refused, no lease held, owned by this user — nothing will answer here again"
                    .into()
            },
            EndpointClass::Rebinding => {
                "connection refused but a server holds the name's lease — a rebind in flight".into()
            },
            EndpointClass::ForeignOwner(why)
            | EndpointClass::OwnerMismatch(why)
            | EndpointClass::Ambiguous(why)
            | EndpointClass::Foreign(why) => why.clone(),
        }
    }
}

/// Slack between a server's start and its bind: the server must exist before
/// its socket. A listener that started later than the socket's birth (beyond
/// clock granularity) did not create it.
const BIND_SLACK: Duration = Duration::from_secs(1);

/// Classify one probe. Pure, so every rule is testable without a socket.
pub fn classify(
    name: &str,
    probe: &EndpointProbe,
    peer: Option<&ProcessIdentity>,
    our_uid: u32,
) -> EndpointClass {
    if NON_SESSION_ENDPOINTS.contains(&name) {
        return EndpointClass::Foreign("not a session endpoint".into());
    }
    if !probe.is_socket {
        return EndpointClass::Foreign("not a socket".into());
    }
    if probe.owner_uid != our_uid {
        return EndpointClass::Foreign(format!(
            "owned by uid {} (this user is {our_uid})",
            probe.owner_uid
        ));
    }
    match &probe.reach {
        EndpointReach::Accepting { peer_pid } => {
            let Some(pid) = peer_pid else {
                return EndpointClass::Ambiguous(
                    "accepting, but the kernel did not name the listener".into(),
                );
            };
            let Some(peer) = peer else {
                return EndpointClass::Ambiguous(format!(
                    "accepting, but listener pid {pid} could not be identified"
                ));
            };
            if peer.uid != our_uid {
                return EndpointClass::ForeignOwner(format!(
                    "listener pid {pid} runs as uid {}",
                    peer.uid
                ));
            }
            if !peer.executable.as_deref().is_some_and(is_vc_frame_image) {
                return EndpointClass::ForeignOwner(format!(
                    "listener pid {pid} is not a vc-frame engine ({})",
                    peer.executable
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "unnamed image".into())
                ));
            }
            if let Some(created) = probe.created_at
                && peer.started_at > created + BIND_SLACK
            {
                return EndpointClass::OwnerMismatch(format!(
                    "listener pid {pid} started after the endpoint was created — it did not bind it"
                ));
            }
            EndpointClass::Live
        },
        EndpointReach::Refused => match &probe.lease {
            LeaseState::Held => EndpointClass::Rebinding,
            LeaseState::Free | LeaseState::Absent => EndpointClass::Orphan,
            LeaseState::Unknown(why) => {
                EndpointClass::Ambiguous(format!("refused, lease unreadable: {why}"))
            },
        },
        EndpointReach::TimedOut => EndpointClass::Ambiguous(
            "did not accept within the probe deadline — busy or wedged, never treated".into(),
        ),
        EndpointReach::Failed(why) => EndpointClass::Ambiguous(why.clone()),
    }
}

/// Is this a vc-frame binary? Name-based on purpose: the identity questions
/// (which build, intact or replaced) are asked of the image afterwards.
pub fn is_vc_frame_image(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "vc-frame" || name.starts_with("vc_frame-"))
}

/// Probe every endpoint of every namespace. Read-only.
pub fn scan_endpoints() -> Vec<Endpoint> {
    let our_uid = current_uid();
    let mut endpoints = vec![];
    for (contract_version, dir) in socket_namespaces() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            let path = dir.join(&name);
            let Ok(probe) = probe_endpoint(&path, PROBE_TIMEOUT) else {
                continue;
            };
            let peer = match &probe.reach {
                EndpointReach::Accepting {
                    peer_pid: Some(pid),
                } => ProcessIdentity::of(*pid).ok().flatten(),
                _ => None,
            };
            let class = classify(&name, &probe, peer.as_ref(), our_uid);
            endpoints.push(Endpoint {
                contract_version,
                name,
                probe,
                peer,
                class,
            });
        }
    }
    endpoints
}

/// Live session names in this binary's own contract namespace — the only
/// sessions a same-contract client may talk to.
pub fn live_sessions(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints
        .iter()
        .filter(|endpoint| endpoint.is_current_contract() && endpoint.class == EndpointClass::Live)
        .map(|endpoint| endpoint.name.clone())
        .collect()
}

// ------------------------------------------------------------------ engine --

/// What the executing image of an engine is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineImage {
    /// The file at the kernel-named path is unchanged since the process
    /// started, so its bytes ARE the running bytes.
    Intact { path: PathBuf },
    /// The file at that path changed after the process started (reinstall in
    /// place): the running bytes no longer exist on disk.
    Replaced { path: PathBuf },
    /// The kernel could not name the image, or the path is gone.
    Unnamed,
}

/// The build identity an engine binary reports about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineBuild {
    pub human_version: String,
    pub git_sha: String,
    pub git_dirty: bool,
    pub build_time_utc: String,
    pub profile: String,
}

impl EngineBuild {
    pub fn of_this_binary() -> Self {
        let info = build_info();
        EngineBuild {
            human_version: info.human_version(),
            git_sha: info.git_sha.to_owned(),
            git_dirty: info.git_dirty,
            build_time_utc: info.build_time_utc.to_owned(),
            profile: info.profile.to_owned(),
        }
    }

    /// Same build: same commit, same dirtiness, same build instant and
    /// profile. Two builds of one commit are still two artifacts.
    pub fn same_build(&self, other: &EngineBuild) -> bool {
        self == other
    }

    fn from_json(text: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
        Some(EngineBuild {
            human_version: value["human_version"].as_str()?.to_owned(),
            git_sha: value["git_sha"].as_str()?.to_owned(),
            git_dirty: value["git_dirty"].as_bool()?,
            build_time_utc: value["build_time_utc"].as_str()?.to_owned(),
            profile: value["profile"].as_str()?.to_owned(),
        })
    }
}

/// Everything knowable about the engine behind one live endpoint.
#[derive(Debug, Clone)]
pub struct EngineIdentity {
    pub process: ProcessIdentity,
    pub image: EngineImage,
    pub build: Option<EngineBuild>,
    pub release_label: Option<String>,
    pub contract_version: usize,
}

impl EngineIdentity {
    pub fn of(endpoint: &Endpoint) -> Option<EngineIdentity> {
        let process = endpoint.peer.clone()?;
        let image = engine_image(&process);
        let build = match &image {
            EngineImage::Intact { path } => {
                run_engine(path, &["--build-info"], ENGINE_QUERY_TIMEOUT)
                    .ok()
                    .and_then(|stdout| EngineBuild::from_json(&stdout))
            },
            _ => None,
        };
        let release_label = process
            .executable
            .as_deref()
            .and_then(zellij_server::vc_generation::release_root_of)
            .as_deref()
            .and_then(zellij_server::vc_generation::generation_label);
        Some(EngineIdentity {
            process,
            image,
            build,
            release_label,
            contract_version: endpoint.contract_version,
        })
    }

    pub fn describe(&self) -> String {
        let build = self
            .build
            .as_ref()
            .map(|build| format!("{} ({})", build.human_version, build.profile))
            .unwrap_or_else(|| "build unknown".into());
        let image = match &self.image {
            EngineImage::Intact { path } => path.display().to_string(),
            EngineImage::Replaced { path } => format!("{} (replaced on disk)", path.display()),
            EngineImage::Unnamed => "unnamed image".into(),
        };
        format!(
            "pid {} · {} · contract_version_{} · {}{}",
            self.process.pid,
            build,
            self.contract_version,
            image,
            self.release_label
                .as_ref()
                .map(|label| format!(" · pack {label}"))
                .unwrap_or_default()
        )
    }

    /// The image may be executed for a read-only query only when it is the
    /// very bytes the engine runs.
    pub fn intact_image(&self) -> Option<&Path> {
        match &self.image {
            EngineImage::Intact { path } => Some(path),
            _ => None,
        }
    }
}

fn engine_image(process: &ProcessIdentity) -> EngineImage {
    let Some(path) = process.executable.clone() else {
        return EngineImage::Unnamed;
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return EngineImage::Unnamed;
    };
    use std::os::unix::fs::MetadataExt;
    let changed = std::time::SystemTime::UNIX_EPOCH
        + Duration::from_secs(metadata.ctime().max(0) as u64)
        + Duration::from_nanos(metadata.ctime_nsec().max(0) as u64);
    if changed > process.started_at {
        EngineImage::Replaced { path }
    } else {
        EngineImage::Intact { path }
    }
}

/// Run an engine binary with read-only arguments, bounded. The child gets an
/// empty stdin and is killed at the deadline — a dump never hangs a repair.
pub fn run_engine(binary: &Path, args: &[&str], timeout: Duration) -> std::io::Result<String> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let mut stdout = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                use std::io::Read;
                pipe.read_to_string(&mut stdout)?;
            }
            return if status.success() {
                Ok(stdout)
            } else {
                Err(std::io::Error::other(format!(
                    "{} {} exited with {status}",
                    binary.display(),
                    args.join(" ")
                )))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{} {} did not finish in time",
                    binary.display(),
                    args.join(" ")
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------- intended --

/// Who vouches that this binary is the intended one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// The runtime pack this binary lives in pins exactly these bytes and
    /// this source revision.
    Admitted {
        pack: String,
        revision: String,
        binary_sha256: String,
    },
    /// The pack exists but disagrees with this binary. Nothing built on the
    /// claim "this is the intended version" may proceed.
    Contradicted(String),
    /// Not installed as a pack (a cargo or worktree build): intended means
    /// "this binary", and the report says so.
    Unpacked,
}

/// The artifact this clinic compares running engines against.
#[derive(Debug, Clone)]
pub struct Intended {
    pub executable: Option<PathBuf>,
    pub build: EngineBuild,
    pub plugins: BTreeMap<String, String>,
    pub receipt_sha256: String,
    /// The binary's embedded plugin bytes against its own receipt.
    pub self_consistent: Result<usize, Vec<String>>,
    pub admission: Admission,
    /// The pack the installation points at, when this binary is not it.
    pub active_pack_elsewhere: Option<String>,
}

impl Intended {
    pub fn of_this_binary() -> Self {
        // Read to hash this image against its pack's provenance; never run.
        let executable = std::env::current_exe()
            .and_then(|path| path.canonicalize())
            .ok();
        let build = EngineBuild::of_this_binary();
        let mut plugins: BTreeMap<String, String> =
            parse_receipt(PLUGIN_RECEIPT).into_iter().collect();
        plugins.remove("fixture-plugin-for-tests.wasm");
        let self_consistent = match super::embedded_plugin_check().clone() {
            PluginReceiptCheck::Report {
                verified,
                mismatched,
                unreceipted,
                unembedded,
            } => {
                let mut problems = mismatched;
                problems.extend(unreceipted.into_iter().map(|n| format!("{n}: unreceipted")));
                problems.extend(unembedded.into_iter().map(|n| format!("{n}: not embedded")));
                if problems.is_empty() {
                    Ok(verified.len())
                } else {
                    Err(problems)
                }
            },
            PluginReceiptCheck::NotApplicable(reason) => Err(vec![reason.to_owned()]),
        };
        let pack_root = executable
            .as_deref()
            .and_then(zellij_server::vc_generation::release_root_of);
        let admission = match (&pack_root, &executable) {
            (Some(root), Some(exe)) => admission_of(root, exe, &build),
            _ => Admission::Unpacked,
        };
        let active_pack_elsewhere = pack_root.as_deref().and_then(|root| {
            let home = root.parent()?.parent()?;
            let active = zellij_server::vc_generation::active_release_root(home)?;
            (active != root.canonicalize().ok()?)
                .then(|| zellij_server::vc_generation::generation_label(&active))
                .flatten()
        });
        Intended {
            executable,
            build,
            plugins,
            receipt_sha256: sha256_hex(PLUGIN_RECEIPT.as_bytes()),
            self_consistent,
            admission,
            active_pack_elsewhere,
        }
    }

    pub fn admission_line(&self) -> String {
        let vouched = match &self.admission {
            Admission::Admitted {
                pack,
                revision,
                binary_sha256,
            } => format!(
                "admitted by runtime pack {pack} (vc-frame@{}, binary sha256 {})",
                &revision[..revision.len().min(12)],
                &binary_sha256[..binary_sha256.len().min(16)]
            ),
            Admission::Contradicted(why) => format!("CONTRADICTED by its runtime pack: {why}"),
            Admission::Unpacked => {
                "this binary (not installed as a runtime pack, so no external admission)".to_owned()
            },
        };
        let mut line = format!(
            "intended: {} — {vouched} · plugin receipt sha256 {} · {}",
            self.build.human_version,
            &self.receipt_sha256[..16],
            self.executable
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "own image unnamed".into())
        );
        if let Some(active) = &self.active_pack_elsewhere {
            line.push_str(&format!(
                " · NOTE: the installation points at pack {active}, not at this binary"
            ));
        }
        line
    }
}

/// Check the pack's own provenance record against this binary: the pinned
/// `libexec/vc-frame` digest and the pinned vc-frame source revision.
fn admission_of(pack_root: &Path, executable: &Path, build: &EngineBuild) -> Admission {
    let pack = zellij_server::vc_generation::generation_label(pack_root)
        .unwrap_or_else(|| pack_root.display().to_string());
    let provenance = pack_root.join("runtime-pack-provenance.json");
    let Ok(text) = std::fs::read_to_string(&provenance) else {
        return Admission::Contradicted(format!("{} is unreadable", provenance.display()));
    };
    let Ok(document) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Admission::Contradicted(format!("{} is not JSON", provenance.display()));
    };
    let relative = executable
        .strip_prefix(pack_root)
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let pinned_sha = document["payload"]["files"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|file| file["path"].as_str() == Some(relative.as_str()))
        .and_then(|file| file["sha256"].as_str())
        .map(str::to_owned);
    let Some(pinned_sha) = pinned_sha else {
        return Admission::Contradicted(format!("the pack does not pin {relative}"));
    };
    let revision = document["source_revisions"]["vc-frame"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    if revision != build.git_sha {
        return Admission::Contradicted(format!(
            "pack pins vc-frame@{revision}, this binary is {}",
            build.git_sha
        ));
    }
    let actual = match std::fs::read(executable) {
        Ok(bytes) => sha256_hex(&bytes),
        Err(error) => return Admission::Contradicted(format!("cannot read own image: {error}")),
    };
    if actual != pinned_sha {
        return Admission::Contradicted(format!(
            "pack pins binary sha256 {pinned_sha}, the file is {actual}"
        ));
    }
    Admission::Admitted {
        pack,
        revision,
        binary_sha256: actual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn probe(reach: EndpointReach, lease: LeaseState) -> EndpointProbe {
        EndpointProbe {
            path: PathBuf::from("/tmp/contract_version_4/s"),
            is_socket: true,
            identity: (1, 2),
            owner_uid: 501,
            created_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000)),
            reach,
            lease,
        }
    }

    fn engine(pid: i32, started: u64, uid: u32, exe: &str) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            uid,
            started_at: SystemTime::UNIX_EPOCH + Duration::from_secs(started),
            executable: Some(PathBuf::from(exe)),
        }
    }

    const EXE: &str = "/rt/releases/4.4.0+gabc/libexec/vc-frame";

    #[test]
    fn a_vc_frame_engine_that_bound_its_socket_is_live() {
        let accepting = EndpointReach::Accepting { peer_pid: Some(7) };
        let peer = engine(7, 990, 501, EXE);
        assert_eq!(
            classify("s", &probe(accepting, LeaseState::Held), Some(&peer), 501),
            EndpointClass::Live
        );
    }

    #[test]
    fn refused_is_an_orphan_only_without_a_held_lease() {
        assert_eq!(
            classify(
                "s",
                &probe(EndpointReach::Refused, LeaseState::Free),
                None,
                501
            ),
            EndpointClass::Orphan
        );
        assert_eq!(
            classify(
                "s",
                &probe(EndpointReach::Refused, LeaseState::Absent),
                None,
                501
            ),
            EndpointClass::Orphan
        );
        assert_eq!(
            classify(
                "s",
                &probe(EndpointReach::Refused, LeaseState::Held),
                None,
                501
            ),
            EndpointClass::Rebinding
        );
        assert!(matches!(
            classify(
                "s",
                &probe(EndpointReach::Refused, LeaseState::Unknown("x".into())),
                None,
                501
            ),
            EndpointClass::Ambiguous(_)
        ));
    }

    #[test]
    fn a_reused_pid_or_a_foreign_listener_is_never_live() {
        let accepting = EndpointReach::Accepting { peer_pid: Some(7) };
        // PID reuse shape: the process behind the pid started long after the
        // endpoint existed, so it cannot be the server that bound it.
        let late = engine(7, 5_000, 501, EXE);
        assert!(matches!(
            classify(
                "s",
                &probe(accepting.clone(), LeaseState::Held),
                Some(&late),
                501
            ),
            EndpointClass::OwnerMismatch(_)
        ));
        let other_user = engine(7, 990, 0, EXE);
        assert!(matches!(
            classify(
                "s",
                &probe(accepting.clone(), LeaseState::Held),
                Some(&other_user),
                501
            ),
            EndpointClass::ForeignOwner(_)
        ));
        let not_frame = engine(7, 990, 501, "/usr/bin/nc");
        assert!(matches!(
            classify(
                "s",
                &probe(accepting, LeaseState::Held),
                Some(&not_frame),
                501
            ),
            EndpointClass::ForeignOwner(_)
        ));
    }

    #[test]
    fn foreign_files_and_timeouts_are_protected() {
        let mut not_socket = probe(
            EndpointReach::Failed("not a socket".into()),
            LeaseState::Absent,
        );
        not_socket.is_socket = false;
        assert!(matches!(
            classify("s", &not_socket, None, 501),
            EndpointClass::Foreign(_)
        ));
        let mut theirs = probe(EndpointReach::Refused, LeaseState::Free);
        theirs.owner_uid = 0;
        assert!(matches!(
            classify("s", &theirs, None, 501),
            EndpointClass::Foreign(_)
        ));
        assert!(matches!(
            classify(
                "s",
                &probe(EndpointReach::TimedOut, LeaseState::Held),
                None,
                501
            ),
            EndpointClass::Ambiguous(_)
        ));
        assert!(matches!(
            classify(
                "web_server_bus",
                &probe(EndpointReach::Refused, LeaseState::Free),
                None,
                501
            ),
            EndpointClass::Foreign(_)
        ));
    }

    #[test]
    fn build_info_parses_and_two_builds_of_one_commit_differ() {
        let json = r#"{"product":"vc-frame","version":"4.4.0","human_version":"4.4.0+gbf13d4df",
            "git_sha":"bf13d4df7bd0f23adb525dafa42f5ef029f829cd","git_sha_short":"bf13d4df",
            "git_dirty":false,"build_time_utc":"2026-10-11T11:01:18Z","profile":"release"}"#;
        let release = EngineBuild::from_json(json).unwrap();
        let mut debug = release.clone();
        debug.profile = "debug".into();
        assert!(release.same_build(&release.clone()));
        assert!(!release.same_build(&debug));
        assert_eq!(EngineBuild::from_json("not json"), None);
    }

    /// A runtime pack on disk: `<home>/releases/<label>/libexec/vc-frame`
    /// plus the pack's own provenance record.
    fn pack(pinned_sha: &str, revision: &str, path: &str) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("releases").join("4.4.0+gtest");
        std::fs::create_dir_all(root.join("libexec")).unwrap();
        let exe = root.join("libexec").join("vc-frame");
        std::fs::write(&exe, b"engine bytes").unwrap();
        std::fs::write(
            root.join("runtime-pack-provenance.json"),
            serde_json::json!({
                "schema": "io.vetcoders.vibecrafted.runtime-pack-provenance.v1",
                "payload": {"files": [{"path": path, "sha256": pinned_sha}]},
                "source_revisions": {"vc-frame": revision},
            })
            .to_string(),
        )
        .unwrap();
        (home, exe)
    }

    #[test]
    fn a_pack_that_pins_these_bytes_and_this_revision_admits_the_binary() {
        let build = EngineBuild::of_this_binary();
        let digest = sha256_hex(b"engine bytes");
        let (_home, exe) = pack(&digest, &build.git_sha, "libexec/vc-frame");
        let root = exe.parent().unwrap().parent().unwrap();
        assert_eq!(
            admission_of(root, &exe, &build),
            Admission::Admitted {
                pack: "4.4.0+gtest".into(),
                revision: build.git_sha.clone(),
                binary_sha256: digest,
            }
        );
    }

    #[test]
    fn a_pack_that_disagrees_contradicts_instead_of_vouching() {
        let build = EngineBuild::of_this_binary();
        let digest = sha256_hex(b"engine bytes");
        for (pinned, revision, path, expect) in [
            (
                "0".repeat(64),
                build.git_sha.clone(),
                "libexec/vc-frame",
                "pins binary sha256",
            ),
            (
                digest.clone(),
                "f".repeat(40),
                "libexec/vc-frame",
                "pins vc-frame@",
            ),
            (
                digest.clone(),
                build.git_sha.clone(),
                "bin/vc-frame",
                "does not pin",
            ),
        ] {
            let (_home, exe) = pack(&pinned, &revision, path);
            let root = exe.parent().unwrap().parent().unwrap();
            match admission_of(root, &exe, &build) {
                Admission::Contradicted(why) => assert!(why.contains(expect), "{why}"),
                other => panic!("expected a contradiction about `{expect}`, got {other:?}"),
            }
        }
    }

    #[test]
    fn this_binary_is_self_consistent_and_names_its_admission() {
        let intended = Intended::of_this_binary();
        assert!(
            intended.self_consistent.is_ok(),
            "{:?}",
            intended.self_consistent
        );
        assert!(intended.plugins.len() >= 10);
        assert!(
            !intended
                .plugins
                .contains_key("fixture-plugin-for-tests.wasm")
        );
        // A test binary lives in a cargo target, never in a runtime pack.
        assert_eq!(intended.admission, Admission::Unpacked);
    }
}
