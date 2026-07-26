//! The `vc-frame triage-run` executor.
//!
//! [`zellij_utils::run_triage`] owns the ordering — capture, then recreate,
//! then close. This module is only the hands: it turns each step into the CLI
//! actions that already exist (`dump-screen`, `attach --create-background`,
//! `new-tab`, `dump-layout`, `close-tab`) and reports back.
//!
//! Every step shells out to the running executable rather than reaching into
//! the client internals, because a bucket session is a *separate server
//! process* — there is no in-process handle to it.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use zellij_utils::run_triage::{
    BucketKind, CaptureEvidence, CaptureSource, CloseOriginError, FinishedRun, OriginTabIdentity,
    OriginTabState, RunMeta, TransferReceipt, TransferReport, TriageIo, capture_sha256,
    control_plane_root, transfer_finished_run, transfer_receipt_path,
};
use zellij_utils::sessions::session_exists;

struct RunTransferLock {
    file: std::fs::File,
}

#[cfg(unix)]
impl Drop for RunTransferLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;

        let _ = nix::fcntl::flock(self.file.as_raw_fd(), nix::fcntl::FlockArg::Unlock);
    }
}

impl RunTransferLock {
    #[cfg(unix)]
    fn acquire(path: &Path) -> Result<Self, String> {
        use std::os::fd::AsRawFd;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|error| format!("cannot open transfer lock {}: {}", path.display(), error))?;
        nix::fcntl::flock(
            file.as_raw_fd(),
            nix::fcntl::FlockArg::LockExclusiveNonblock,
        )
        .map_err(|error| {
            format!(
                "another triage process owns transfer lock {}; refusing to wait: {}",
                path.display(),
                error
            )
        })?;
        Ok(Self { file })
    }

    #[cfg(windows)]
    fn acquire(path: &Path) -> Result<Self, String> {
        use std::os::windows::fs::OpenOptionsExt;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .share_mode(0)
            .open(path)
            .map_err(|error| {
                format!("cannot acquire transfer lock {}: {}", path.display(), error)
            })?;
        Ok(Self { file })
    }

    #[cfg(not(any(unix, windows)))]
    fn acquire(path: &Path) -> Result<Self, String> {
        Err(format!(
            "run transfer locking is unsupported on this platform ({})",
            path.display()
        ))
    }
}

/// Drives the transfer through real sessions.
struct CliTriageIo {
    /// The `vc-frame` binary to re-invoke. Resolved once so a triage started by
    /// a dev build keeps talking to that same build.
    executable: PathBuf,
    /// Control-plane root, needed to point the viewer pane at the capture.
    root: PathBuf,
}

fn run_command_with_timeout(
    executable: &Path,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, String> {
    // Regular files avoid the classic pipe deadlock where a verbose child
    // fills stderr while the parent is waiting for stdout (or vice versa).
    let stdout = tempfile::tempfile()
        .map_err(|error| format!("cannot create temporary stdout capture: {}", error))?;
    let stderr = tempfile::tempfile()
        .map_err(|error| format!("cannot create temporary stderr capture: {}", error))?;
    let mut stdout_reader = stdout
        .try_clone()
        .map_err(|error| format!("cannot clone temporary stdout capture: {}", error))?;
    let mut stderr_reader = stderr
        .try_clone()
        .map_err(|error| format!("cannot clone temporary stderr capture: {}", error))?;
    let mut child = Command::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| {
            format!(
                "failed to invoke `{} {}`: {}",
                executable.display(),
                args.join(" "),
                error
            )
        })?;
    let deadline = Instant::now() + timeout;
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let status = child
                    .wait()
                    .map_err(|error| format!("cannot reap timed-out child: {}", error))?;
                break (status, true);
            },
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot inspect child status: {}", error));
            },
        }
    };

    stdout_reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot rewind stdout capture: {}", error))?;
    stderr_reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("cannot rewind stderr capture: {}", error))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    stdout_reader
        .read_to_end(&mut stdout)
        .map_err(|error| format!("cannot read stdout capture: {}", error))?;
    stderr_reader
        .read_to_end(&mut stderr)
        .map_err(|error| format!("cannot read stderr capture: {}", error))?;

    if timed_out {
        return Err(format!(
            "`{} {}` timed out after {:.1}s: {}",
            executable.display(),
            args.join(" "),
            timeout.as_secs_f64(),
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

impl CliTriageIo {
    fn new(root: PathBuf) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the vc-frame executable: {}", e))?;
        Ok(CliTriageIo { executable, root })
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = run_command_with_timeout(&self.executable, args, Duration::from_secs(10))?;
        if !output.status.success() {
            return Err(format!(
                "`vc-frame {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn wait_for_session_ready(&self, session: &str) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let readiness = self
                .run(&["-s", session, "action", "list-tabs", "--json"])
                .and_then(|_| {
                    self.run(&[
                        "-s",
                        session,
                        "action",
                        "list-panes",
                        "--json",
                        "--all",
                        "--tab",
                        "--state",
                    ])
                })
                .and_then(|panes| {
                    serde_json::from_str::<Vec<serde_json::Value>>(&panes)
                        .map_err(|error| format!("cannot parse pane inventory: {}", error))
                })
                .and_then(|panes| {
                    if panes.is_empty() {
                        Err("pane inventory is still empty".to_owned())
                    } else {
                        Ok(())
                    }
                });
            let error = match readiness {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            if Instant::now() >= deadline {
                return Err(format!(
                    "session '{}' did not become ready: {}",
                    session, error
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// The bucket tab: a scrollback viewer on top, a suspended rerun below.
///
/// `start_suspended true` is what makes the rerun a single keypress — the pane
/// holds the original command without running it until the operator says so.
fn bucket_tab_layout(scrollback: &Path, meta: &RunMeta) -> String {
    // Chrome mirrors `default_tab_template` in assets/layouts/vibecrafted.kdl:
    // compact-bar on top, the left Sessions rail, status-bar below. The layout
    // contract ("every tab keeps the left Sessions rail") applies to transferred
    // bucket tabs too — a fullscreen scrollback that hides the rail strands the
    // operator inside the bucket with no way back but the keyboard.
    let mut layout = String::from(
        "layout {\n\
         \x20   pane size=1 borderless=true {\n\
         \x20       plugin location=\"compact-bar\"\n\
         \x20   }\n\
         \x20   pane split_direction=\"vertical\" {\n\
         \x20       pane size=24 borderless=true {\n\
         \x20           plugin location=\"session-manager\" {\n\
         \x20               rail true\n\
         \x20               pane_title \"Sessions\"\n\
         \x20           }\n\
         \x20       }\n\
         \x20       pane {\n",
    );
    layout.push_str(&format!(
        "            pane command=\"less\" name=\"scrollback · exit {}\" {{\n                args \"-R\" \"{}\"\n            }}\n",
        meta.exit_code,
        kdl_escape(&scrollback.to_string_lossy())
    ));
    if let Some((program, args)) = meta.command.split_first() {
        layout.push_str(&format!(
            "            pane command=\"{}\" name=\"rerun\" start_suspended=true {{\n",
            kdl_escape(program)
        ));
        if !args.is_empty() {
            let rendered: Vec<String> = args
                .iter()
                .map(|arg| format!("\"{}\"", kdl_escape(arg)))
                .collect();
            layout.push_str(&format!("                args {}\n", rendered.join(" ")));
        }
        if let Some(cwd) = meta.cwd.as_ref() {
            layout.push_str(&format!(
                "                cwd \"{}\"\n",
                kdl_escape(&cwd.to_string_lossy())
            ));
        }
        layout.push_str("            }\n");
    }
    layout.push_str(
        "        }\n\
         \x20   }\n\
         \x20   pane size=1 borderless=true {\n\
         \x20       plugin location=\"status-bar\"\n\
         \x20   }\n\
         }\n",
    );
    layout
}

fn kdl_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn terminal_pane_id_for_tab(
    pane_list: &str,
    origin_tab_id: u64,
    origin_tab_name: &str,
    recorded_pane_id: Option<&str>,
) -> Result<u64, String> {
    let panes: serde_json::Value = serde_json::from_str(pane_list)
        .map_err(|e| format!("cannot parse vc-frame pane inventory: {}", e))?;
    let panes = panes
        .as_array()
        .ok_or_else(|| "vc-frame pane inventory is not an array".to_owned())?;
    let candidates: Vec<u64> = panes
        .iter()
        .filter(|pane| {
            pane.get("is_plugin").and_then(serde_json::Value::as_bool) == Some(false)
                && pane.get("tab_id").and_then(serde_json::Value::as_u64) == Some(origin_tab_id)
        })
        .filter_map(|pane| pane.get("id").and_then(serde_json::Value::as_u64))
        .collect();

    let recorded_terminal_id = recorded_pane_id.and_then(|pane_id| {
        pane_id
            .strip_prefix("terminal_")
            .unwrap_or(pane_id)
            .parse::<u64>()
            .ok()
    });
    if let Some(recorded_terminal_id) = recorded_terminal_id
        && candidates.contains(&recorded_terminal_id)
    {
        return Ok(recorded_terminal_id);
    }

    match candidates.as_slice() {
        [pane_id] => Ok(*pane_id),
        [] => Err(format!(
            "origin tab '{}' (id {}) has no terminal pane to capture",
            origin_tab_name, origin_tab_id
        )),
        _ => Err(format!(
            "origin tab '{}' (id {}) has {} terminal panes and no verified pane id",
            origin_tab_name,
            origin_tab_id,
            candidates.len()
        )),
    }
}

fn tab_ids_for_name(tab_list: &str, origin_tab: &str) -> Result<Vec<u64>, String> {
    let tabs: serde_json::Value = serde_json::from_str(tab_list)
        .map_err(|e| format!("cannot parse vc-frame tab inventory: {}", e))?;
    let tabs = tabs
        .as_array()
        .ok_or_else(|| "vc-frame tab inventory is not an array".to_owned())?;
    Ok(tabs
        .iter()
        .filter(|tab| tab.get("name").and_then(serde_json::Value::as_str) == Some(origin_tab))
        .filter_map(|tab| tab.get("tab_id").and_then(serde_json::Value::as_u64))
        .collect())
}

#[cfg(test)]
fn tab_id_for_name(tab_list: &str, origin_tab: &str) -> Result<u64, String> {
    let candidates = tab_ids_for_name(tab_list, origin_tab)?;
    match candidates.as_slice() {
        [tab_id] => Ok(*tab_id),
        [] => Err(format!("origin tab '{}' no longer exists", origin_tab)),
        _ => Err(format!(
            "origin tab name '{}' is ambiguous across {} tab ids",
            origin_tab,
            candidates.len()
        )),
    }
}

fn tab_identity_for_name(
    tab_list: &str,
    session: &str,
    tab_name: &str,
) -> Result<OriginTabIdentity, String> {
    let candidates = tab_identities_for_name(tab_list, session, tab_name)?;
    match candidates.as_slice() {
        [identity] => Ok(identity.clone()),
        [] => Err(format!("origin tab '{}' no longer exists", tab_name)),
        _ => Err(format!(
            "origin tab name '{}' is ambiguous across {} tab ids",
            tab_name,
            candidates.len()
        )),
    }
}

fn tab_identities_for_name(
    tab_list: &str,
    session: &str,
    tab_name: &str,
) -> Result<Vec<OriginTabIdentity>, String> {
    let tabs: serde_json::Value = serde_json::from_str(tab_list)
        .map_err(|error| format!("cannot parse vc-frame tab inventory: {}", error))?;
    let tabs = tabs
        .as_array()
        .ok_or_else(|| "vc-frame tab inventory is not an array".to_owned())?;
    tabs
        .iter()
        .filter(|tab| tab.get("name").and_then(serde_json::Value::as_str) == Some(tab_name))
        .map(|tab| {
            let id = tab
                .get("tab_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| format!("tab '{}' has no stable tab_id", tab_name))?;
            let session_incarnation = tab
                .get("session_incarnation")
                .and_then(serde_json::Value::as_str)
                .filter(|incarnation| !incarnation.is_empty())
                .ok_or_else(|| {
                    format!(
                        "tab '{}' has no session incarnation; the server must be upgraded before safe triage",
                        tab_name
                    )
                })?;
            let tab_instance_id = tab
                .get("tab_instance_id")
                .and_then(serde_json::Value::as_str)
                .filter(|instance_id| {
                    instance_id.len() == 32
                        && instance_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                .ok_or_else(|| {
                    format!(
                        "tab '{}' has no durable tab_instance_id; the server must be upgraded before safe triage",
                        tab_name
                    )
                })?;
            Ok(OriginTabIdentity {
                session: session.to_owned(),
                name: tab_name.to_owned(),
                id,
                session_incarnation: session_incarnation.to_owned(),
                tab_instance_id: tab_instance_id.to_ascii_lowercase(),
            })
        })
        .collect::<Result<Vec<_>, String>>()
}

fn is_owned_viewer_tab_name(tab_name: &str) -> bool {
    let Some(token) = tab_name
        .strip_suffix(']')
        .and_then(|name| name.rsplit_once(" [vc:").map(|(_, token)| token))
    else {
        return false;
    };
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validated_origin_tab_id(
    captured: &OriginTabIdentity,
    session: &str,
    expected_name: &str,
    tab_list: &str,
) -> Result<Option<u64>, String> {
    if captured.session != session || captured.name != expected_name {
        return Err(format!(
            "origin tab identity mismatch: captured {}/{} but close requested {}/{}",
            captured.session, captured.name, session, expected_name
        ));
    }
    if !captured.is_typed() {
        return Err(
            "transfer receipt lacks a complete durable tab identity; refusing to guess".to_owned(),
        );
    }

    let tabs: serde_json::Value = serde_json::from_str(tab_list)
        .map_err(|error| format!("cannot parse vc-frame tab inventory: {}", error))?;
    let tabs = tabs
        .as_array()
        .ok_or_else(|| "vc-frame tab inventory is not an array".to_owned())?;
    let by_instance = tabs
        .iter()
        .filter(|tab| {
            tab.get("tab_instance_id")
                .and_then(serde_json::Value::as_str)
                == Some(captured.tab_instance_id.as_str())
        })
        .collect::<Vec<_>>();
    if by_instance.len() > 1 {
        return Err(format!(
            "origin tab instance {} appears {} times in the current inventory",
            captured.tab_instance_id,
            by_instance.len()
        ));
    }

    if let Some(current) = by_instance.first() {
        let current_id = current
            .get("tab_id")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                format!(
                    "origin tab instance {} has no stable tab_id",
                    captured.tab_instance_id
                )
            })?;
        let current_name = current
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| format!("origin tab id {} has no name", current_id))?;
        let current_incarnation = current
            .get("session_incarnation")
            .and_then(serde_json::Value::as_str)
            .filter(|incarnation| !incarnation.is_empty())
            .ok_or_else(|| {
                "current server exposes no session incarnation; safe close is unavailable"
                    .to_owned()
            })?;
        if current_name != expected_name {
            return Err(format!(
                "origin tab id {} was renamed from {:?} to {:?}; refusing to mark it closed",
                current_id, expected_name, current_name
            ));
        }
        let same_name_ids = tab_ids_for_name(tab_list, expected_name)?;
        if same_name_ids != [current_id] {
            return Err(format!(
                "origin tab name '{}' no longer resolves uniquely to durable instance {} at current id {}",
                expected_name, captured.tab_instance_id, current_id
            ));
        }
        if current_incarnation.is_empty() {
            return Err(
                "current server exposes no session incarnation; safe close is unavailable"
                    .to_owned(),
            );
        }
        return Ok(Some(current_id));
    }

    // The durable instance disappeared. A same-name tab with a different
    // instance is a foreign successor: it must be left alone, while the
    // captured origin is correctly treated as absent.
    Ok(None)
}

fn sync_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("cannot sync directory {}: {}", parent.display(), error))
}

fn unique_staging_path(dest: &Path, label: &str) -> Result<PathBuf, String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dest.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
    let staged = tempfile::Builder::new()
        .prefix(&format!(".{}.", label))
        .suffix(".staging")
        .tempfile_in(parent)
        .map_err(|error| {
            format!(
                "cannot reserve unique {} staging file in {}: {}",
                label,
                parent.display(),
                error
            )
        })?;
    let (file, path) = staged
        .keep()
        .map_err(|error| format!("cannot retain {} staging file: {}", label, error.error))?;
    drop(file);
    Ok(path)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CaptureCommitManifest {
    version: u8,
    run_id: String,
    session: String,
    origin_tab: String,
    pane_id: Option<String>,
    runtime_transcript: Option<String>,
    staging_file: String,
    evidence: CaptureEvidence,
}

fn capture_manifest_path(dest: &Path) -> PathBuf {
    dest.with_file_name("capture.manifest.json")
}

fn capture_request_transcript(runtime_transcript: Option<&Path>) -> Option<String> {
    runtime_transcript.map(|path| path.to_string_lossy().into_owned())
}

fn file_name_only(path: &Path) -> Result<String, String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{} has no safe staging filename", path.display()))
}

fn committed_capture_matches(path: &Path, evidence: &CaptureEvidence) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.len() == evidence.bytes)
        .unwrap_or(false)
        && evidence.bytes > 0
        && !evidence.sha256.is_empty()
        && capture_sha256(path).as_deref() == Ok(evidence.sha256.as_str())
}

fn adopt_capture_commit(
    run_id: &str,
    session: &str,
    origin_tab: &str,
    pane_id: Option<&str>,
    runtime_transcript: Option<&Path>,
    dest: &Path,
) -> Result<Option<CaptureEvidence>, String> {
    let manifest_path = capture_manifest_path(dest);
    let serialized = match std::fs::read(&manifest_path) {
        Ok(serialized) => serialized,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "cannot read capture commit manifest {}: {}",
                manifest_path.display(),
                error
            ));
        },
    };
    let manifest: CaptureCommitManifest = serde_json::from_slice(&serialized).map_err(|error| {
        format!(
            "cannot parse capture commit manifest {}: {}",
            manifest_path.display(),
            error
        )
    })?;
    let request_matches = manifest.version == 1
        && manifest.run_id == run_id
        && manifest.session == session
        && manifest.origin_tab == origin_tab
        && manifest.pane_id.as_deref() == pane_id
        && manifest.runtime_transcript == capture_request_transcript(runtime_transcript);
    if !request_matches {
        return Err(format!(
            "capture commit manifest {} belongs to a different transfer; refusing to overwrite it",
            manifest_path.display()
        ));
    }
    if manifest.evidence.capture_source == CaptureSource::TerminalScrollback
        && !manifest
            .evidence
            .origin_tab_identity
            .as_ref()
            .map(OriginTabIdentity::is_typed)
            .unwrap_or(false)
    {
        return Err(format!(
            "capture commit manifest {} lacks typed terminal identity",
            manifest_path.display()
        ));
    }
    if committed_capture_matches(dest, &manifest.evidence) {
        return Ok(Some(manifest.evidence));
    }
    let parent = dest
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dest.display()))?;
    let staging = parent.join(&manifest.staging_file);
    if staging.parent() != Some(parent) || !committed_capture_matches(&staging, &manifest.evidence)
    {
        return Err(format!(
            "capture commit manifest {} has neither a matching destination nor staging file",
            manifest_path.display()
        ));
    }
    std::fs::rename(&staging, dest).map_err(|error| {
        format!(
            "cannot adopt staged capture {} -> {}: {}",
            staging.display(),
            dest.display(),
            error
        )
    })?;
    sync_parent(dest)?;
    Ok(Some(manifest.evidence))
}

fn commit_scrollback_capture(
    temporary: &Path,
    dest: &Path,
    manifest: &mut CaptureCommitManifest,
) -> Result<CaptureEvidence, String> {
    match std::fs::metadata(temporary) {
        Ok(metadata) if metadata.len() > 0 => {},
        Ok(_) => {
            let _ = std::fs::remove_file(temporary);
            return Err(format!(
                "scrollback dump at {} is empty",
                temporary.display()
            ));
        },
        Err(error) => {
            return Err(format!(
                "scrollback dump at {} is missing: {}",
                temporary.display(),
                error
            ));
        },
    }
    std::fs::File::open(temporary)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            format!(
                "cannot sync captured scrollback at {}: {}",
                temporary.display(),
                error
            )
        })?;
    let bytes = std::fs::metadata(temporary)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("cannot stat {}: {}", temporary.display(), error))?;
    manifest.evidence.bytes = bytes;
    manifest.evidence.sha256 = capture_sha256(temporary)?;
    manifest.staging_file = file_name_only(temporary)?;
    atomic_write_json(
        &capture_manifest_path(dest),
        serde_json::to_vec_pretty(manifest),
        "capture commit manifest",
    )?;
    std::fs::rename(temporary, dest).map_err(|error| {
        format!(
            "cannot atomically commit scrollback {} -> {}: {}",
            temporary.display(),
            dest.display(),
            error
        )
    })?;
    sync_parent(dest)?;
    if !committed_capture_matches(dest, &manifest.evidence) {
        return Err(format!(
            "committed scrollback {} does not match its durable manifest",
            dest.display()
        ));
    }
    Ok(manifest.evidence.clone())
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct RuntimeTranscriptManifest {
    version: u8,
    run_id: String,
    transcript: PathBuf,
    root: PathBuf,
    bytes: u64,
    sha256: String,
}

fn runtime_transcript_manifest_path(transcript: &Path) -> PathBuf {
    let mut manifest = transcript.as_os_str().to_os_string();
    manifest.push(".manifest.json");
    PathBuf::from(manifest)
}

fn open_read_only_no_follow(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect {}: {}", path.display(), error))?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing symlinked transcript input {}",
                path.display()
            ));
        }
    }
    options.open(path).map_err(|error| {
        format!(
            "cannot open {} without following links: {}",
            path.display(),
            error
        )
    })
}

fn load_runtime_transcript_manifest(
    run_id: &str,
    transcript: &Path,
) -> Result<(RuntimeTranscriptManifest, PathBuf, std::fs::File), String> {
    const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
    let manifest_path = runtime_transcript_manifest_path(transcript);
    let mut manifest_file = open_read_only_no_follow(&manifest_path)?;
    let mut serialized = Vec::new();
    std::io::Read::take(&mut manifest_file, MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut serialized)
        .map_err(|error| {
            format!(
                "cannot read runtime transcript manifest {}: {}",
                manifest_path.display(),
                error
            )
        })?;
    if serialized.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "runtime transcript manifest {} exceeds {} bytes",
            manifest_path.display(),
            MAX_MANIFEST_BYTES
        ));
    }
    let manifest: RuntimeTranscriptManifest =
        serde_json::from_slice(&serialized).map_err(|error| {
            format!(
                "cannot parse runtime transcript manifest {}: {}",
                manifest_path.display(),
                error
            )
        })?;
    if manifest.version != 1 || manifest.run_id != run_id {
        return Err(format!(
            "runtime transcript manifest {} does not own run '{}'",
            manifest_path.display(),
            run_id
        ));
    }
    if manifest.bytes == 0 || manifest.sha256.len() != 64 {
        return Err(format!(
            "runtime transcript manifest {} lacks non-empty digest evidence",
            manifest_path.display()
        ));
    }
    let canonical_root = std::fs::canonicalize(&manifest.root).map_err(|error| {
        format!(
            "cannot resolve transcript ownership root {}: {}",
            manifest.root.display(),
            error
        )
    })?;
    let canonical_transcript = std::fs::canonicalize(transcript).map_err(|error| {
        format!(
            "cannot resolve runtime transcript {}: {}",
            transcript.display(),
            error
        )
    })?;
    let declared_transcript = std::fs::canonicalize(&manifest.transcript).map_err(|error| {
        format!(
            "cannot resolve declared runtime transcript {}: {}",
            manifest.transcript.display(),
            error
        )
    })?;
    let canonical_manifest = std::fs::canonicalize(&manifest_path).map_err(|error| {
        format!(
            "cannot resolve runtime transcript manifest {}: {}",
            manifest_path.display(),
            error
        )
    })?;
    if canonical_transcript != declared_transcript
        || !canonical_transcript.starts_with(&canonical_root)
        || !canonical_manifest.starts_with(&canonical_root)
    {
        return Err(format!(
            "runtime transcript {} is not owned by declared run root {}",
            canonical_transcript.display(),
            canonical_root.display()
        ));
    }
    let transcript_file = open_read_only_no_follow(&canonical_transcript)?;
    let metadata = transcript_file.metadata().map_err(|error| {
        format!(
            "cannot inspect opened runtime transcript {}: {}",
            canonical_transcript.display(),
            error
        )
    })?;
    if !metadata.is_file() || metadata.len() != manifest.bytes {
        return Err(format!(
            "runtime transcript {} size does not match its manifest",
            canonical_transcript.display()
        ));
    }
    Ok((manifest, canonical_transcript, transcript_file))
}

fn capture_runtime_transcript(
    run_id: &str,
    session: &str,
    origin_tab: &str,
    pane_id: Option<&str>,
    transcript: &Path,
    dest: &Path,
    terminal_error: &str,
    origin_tab_identity: Option<OriginTabIdentity>,
) -> Result<CaptureEvidence, String> {
    let (transcript_manifest, canonical_transcript, mut transcript_file) =
        load_runtime_transcript_manifest(run_id, transcript)
            .map_err(|error| format!("{}; {}", terminal_error, error))?;
    let temporary = unique_staging_path(dest, "runtime-transcript")?;
    let mut temporary_file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| format!("cannot open {}: {}", temporary.display(), error))?;
    std::io::copy(&mut transcript_file, &mut temporary_file).map_err(|error| {
        format!(
            "{}; cannot copy runtime transcript {} to {}: {}",
            terminal_error,
            canonical_transcript.display(),
            temporary.display(),
            error
        )
    })?;
    temporary_file
        .sync_all()
        .map_err(|error| format!("cannot sync {}: {}", temporary.display(), error))?;
    let copied_bytes = temporary_file
        .metadata()
        .map_err(|error| format!("cannot inspect {}: {}", temporary.display(), error))?
        .len();
    if copied_bytes != transcript_manifest.bytes
        || capture_sha256(&temporary)? != transcript_manifest.sha256
    {
        return Err(format!(
            "{}; runtime transcript {} changed or failed digest verification while being copied",
            terminal_error,
            canonical_transcript.display()
        ));
    }
    let after = transcript_file.metadata().map_err(|error| {
        format!(
            "cannot re-inspect opened runtime transcript {}: {}",
            canonical_transcript.display(),
            error
        )
    })?;
    if after.len() != transcript_manifest.bytes {
        return Err(format!(
            "{}; runtime transcript {} changed during capture",
            terminal_error,
            canonical_transcript.display()
        ));
    }
    let source_identity = canonical_transcript.display().to_string();
    let mut manifest = CaptureCommitManifest {
        version: 1,
        run_id: run_id.to_owned(),
        session: session.to_owned(),
        origin_tab: origin_tab.to_owned(),
        pane_id: pane_id.map(str::to_owned),
        runtime_transcript: capture_request_transcript(Some(transcript)),
        staging_file: String::new(),
        evidence: CaptureEvidence {
            capture_source: CaptureSource::RuntimeTranscript,
            source_identity,
            bytes: 0,
            sha256: String::new(),
            origin_tab_identity,
        },
    };
    commit_scrollback_capture(&temporary, dest, &mut manifest)
}

fn atomic_write_json(
    dest: &Path,
    serialized: Result<Vec<u8>, serde_json::Error>,
    label: &str,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
    }
    let serialized =
        serialized.map_err(|error| format!("cannot serialize {}: {}", label, error))?;
    let temporary = unique_staging_path(dest, "json")?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&temporary)
        .map_err(|error| format!("cannot open {}: {}", temporary.display(), error))?;
    file.write_all(&serialized)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("cannot write {}: {}", temporary.display(), error))?;
    std::fs::rename(&temporary, dest)
        .map_err(|error| format!("cannot commit {}: {}", dest.display(), error))?;
    sync_parent(dest)
}

fn verified_tab_list_before_close(
    tab_list_result: Result<String, String>,
    session_exists_result: Result<bool, String>,
) -> Result<Option<String>, CloseOriginError> {
    match tab_list_result {
        Ok(tab_list) => Ok(Some(tab_list)),
        Err(list_error) => match session_exists_result {
            // A crash after closing the last tab can leave a receipt that still
            // says Preserved. An absent session proves there is nothing left
            // to close, so retry may safely finish.
            Ok(false) => Ok(None),
            Ok(true) => Err(CloseOriginError {
                message: list_error,
                origin_tab_state: OriginTabState::Unknown,
            }),
            Err(session_error) => Err(CloseOriginError {
                message: format!(
                    "{}; cannot verify whether origin session survived: {}",
                    list_error, session_error
                ),
                origin_tab_state: OriginTabState::Unknown,
            }),
        },
    }
}

fn verify_origin_close(
    tab_id: u64,
    tab_name: &str,
    expected_session_incarnation: &str,
    expected_tab_instance_id: &str,
    close_result: Result<String, String>,
    tab_list_result: Result<String, String>,
    session_exists_result: Result<bool, String>,
) -> Result<(), CloseOriginError> {
    let (origin_tab_state, verification_error) = match tab_list_result {
        Ok(tab_list) => match serde_json::from_str::<serde_json::Value>(&tab_list) {
            Ok(serde_json::Value::Array(tabs)) => {
                let current_origin = tabs.iter().find(|tab| {
                    tab.get("tab_instance_id")
                        .and_then(serde_json::Value::as_str)
                        == Some(expected_tab_instance_id)
                });
                if let Some(current) = current_origin {
                    let current_id = current
                        .get("tab_id")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(tab_id);
                    let current_name = current
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("<missing>");
                    let current_incarnation = current
                        .get("session_incarnation")
                        .and_then(serde_json::Value::as_str);
                    match current_incarnation {
                        Some(incarnation) if incarnation == expected_session_incarnation => (
                            OriginTabState::Preserved,
                            (current_name != tab_name).then(|| {
                                format!(
                                    "origin tab id {} survived under renamed identity {:?}",
                                    current_id, current_name
                                )
                            }),
                        ),
                        Some(incarnation) => (
                            OriginTabState::Unknown,
                            Some(format!(
                                "tab id {} now belongs to session incarnation {} instead of {}",
                                current_id, incarnation, expected_session_incarnation
                            )),
                        ),
                        None => (
                            OriginTabState::Unknown,
                            Some("post-close inventory lacks session incarnation".to_owned()),
                        ),
                    }
                } else {
                    // The durable instance is absent. Same-name tabs are
                    // successors with different identities and are deliberately
                    // ignored.
                    (OriginTabState::Closed, None)
                }
            },
            Ok(_) => (
                OriginTabState::Unknown,
                Some("vc-frame tab inventory is not an array".to_owned()),
            ),
            Err(error) => (
                OriginTabState::Unknown,
                Some(format!("cannot parse vc-frame tab inventory: {}", error)),
            ),
        },
        Err(list_error) => match session_exists_result {
            // Closing the only tab terminates the session, so list-tabs has no
            // server to query. Absent session proves that the origin is gone.
            Ok(false) => (OriginTabState::Closed, None),
            Ok(true) => (OriginTabState::Unknown, Some(list_error)),
            Err(session_error) => (
                OriginTabState::Unknown,
                Some(format!(
                    "{}; cannot verify whether origin session survived: {}",
                    list_error, session_error
                )),
            ),
        },
    };

    // A server-side close can remove the tab and then fail during downstream
    // cleanup. Preserve the real nonzero result and attach the observed state.
    if let Err(close_error) = close_result
        && origin_tab_state != OriginTabState::Closed
    {
        let message = verification_error
            .map(|error| format!("{}; post-close verification: {}", close_error, error))
            .unwrap_or(close_error);
        return Err(CloseOriginError {
            message,
            origin_tab_state,
        });
    }

    match origin_tab_state {
        OriginTabState::Closed => Ok(()),
        OriginTabState::Preserved => Err(CloseOriginError {
            message: format!(
                "origin tab '{}' (id {}) still exists after close",
                tab_name, tab_id
            ),
            origin_tab_state,
        }),
        OriginTabState::Unknown => Err(CloseOriginError {
            message: verification_error
                .unwrap_or_else(|| "origin tab state is unknown after close".to_owned()),
            origin_tab_state,
        }),
    }
}

impl TriageIo for CliTriageIo {
    fn capture_scrollback(
        &mut self,
        run_id: &str,
        session: &str,
        origin_tab: &str,
        pane_id: Option<&str>,
        runtime_transcript: Option<&Path>,
        dest: &Path,
    ) -> Result<CaptureEvidence, String> {
        if let Some(capture) = adopt_capture_commit(
            run_id,
            session,
            origin_tab,
            pane_id,
            runtime_transcript,
            dest,
        )? {
            return Ok(capture);
        }
        // Terminal capture requires a live, uniquely named tab. A real runtime
        // transcript remains valid recovery evidence even after that session
        // has disappeared, so identity resolution is part of the terminal
        // attempt rather than a global precondition.
        let origin_tab_identity = (|| {
            let tab_list = self.run(&["-s", session, "action", "list-tabs", "--json"])?;
            tab_identity_for_name(&tab_list, session, origin_tab)
        })();

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }

        let terminal_capture =
            origin_tab_identity
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|origin_tab_identity| {
                    let temporary = unique_staging_path(dest, "terminal-scrollback")?;
                    let origin_tab_id = origin_tab_identity.id;
                    let pane_list = self.run(&[
                        "-s",
                        session,
                        "action",
                        "list-panes",
                        "--json",
                        "--all",
                        "--tab",
                        "--state",
                    ])?;
                    let terminal_pane_id =
                        terminal_pane_id_for_tab(&pane_list, origin_tab_id, origin_tab, pane_id)?
                            .to_string();
                    let temporary_arg = temporary.to_string_lossy().into_owned();
                    let origin_tab_id_arg = origin_tab_id.to_string();
                    self.run(&[
                        "-s",
                        session,
                        "action",
                        "dump-screen",
                        "--full",
                        "--path",
                        &temporary_arg,
                        "--pane-id",
                        &terminal_pane_id,
                        "--expected-tab-id",
                        &origin_tab_id_arg,
                        "--expected-tab-name",
                        origin_tab,
                        "--expected-session-incarnation",
                        &origin_tab_identity.session_incarnation,
                        "--expected-tab-instance-id",
                        &origin_tab_identity.tab_instance_id,
                    ])?;
                    let mut manifest = CaptureCommitManifest {
                        version: 1,
                        run_id: run_id.to_owned(),
                        session: session.to_owned(),
                        origin_tab: origin_tab.to_owned(),
                        pane_id: pane_id.map(str::to_owned),
                        runtime_transcript: capture_request_transcript(runtime_transcript),
                        staging_file: String::new(),
                        evidence: CaptureEvidence {
                            capture_source: CaptureSource::TerminalScrollback,
                            source_identity: format!(
                                "session={};tab_id={};tab_instance_id={};pane_id=terminal_{}",
                                session,
                                origin_tab_id,
                                origin_tab_identity.tab_instance_id,
                                terminal_pane_id
                            ),
                            bytes: 0,
                            sha256: String::new(),
                            origin_tab_identity: Some(origin_tab_identity.clone()),
                        },
                    };
                    commit_scrollback_capture(&temporary, dest, &mut manifest)
                });
        let terminal_error = match terminal_capture {
            Ok(capture) => return Ok(capture),
            Err(error) => error,
        };
        let transcript = runtime_transcript.ok_or_else(|| terminal_error.clone())?;
        capture_runtime_transcript(
            run_id,
            session,
            origin_tab,
            pane_id,
            transcript,
            dest,
            &terminal_error,
            origin_tab_identity.ok(),
        )
    }

    fn load_receipt(&mut self, path: &Path) -> Result<Option<TransferReceipt>, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| format!("cannot parse {}: {}", path.display(), error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("cannot read {}: {}", path.display(), error)),
        }
    }

    fn write_receipt(&mut self, path: &Path, receipt: &TransferReceipt) -> Result<(), String> {
        atomic_write_json(path, serde_json::to_vec_pretty(receipt), "transfer receipt")
    }

    fn write_meta(&mut self, dest: &Path, meta: &RunMeta) -> Result<(), String> {
        atomic_write_json(dest, serde_json::to_vec_pretty(meta), "run metadata")
    }

    fn ensure_bucket_session(&mut self, session: &str) -> Result<(), String> {
        match session_exists(session) {
            Ok(true) => self.wait_for_session_ready(session),
            Ok(false) => {
                self.run(&["attach", "--create-background", session])?;
                self.wait_for_session_ready(session)
            },
            Err(e) => Err(format!("cannot check for session '{}': {:?}", session, e)),
        }
    }

    fn open_bucket_tab(&mut self, session: &str, tab: &str, meta: &RunMeta) -> Result<(), String> {
        let scrollback = zellij_utils::run_triage::scrollback_path(&self.root, &meta.run);
        let layout = bucket_tab_layout(&scrollback, meta);
        self.run(&[
            "-s",
            session,
            "action",
            "new-tab",
            "--name",
            tab,
            "--layout-string",
            &layout,
        ])?;
        Ok(())
    }

    fn bucket_tab_identity(
        &mut self,
        session: &str,
        tab: &str,
        expected: Option<&OriginTabIdentity>,
    ) -> Result<Option<OriginTabIdentity>, String> {
        let mut inventory = self.run(&["-s", session, "action", "list-tabs", "--json"])?;
        let mut matches = tab_identities_for_name(&inventory, session, tab)?;
        if matches.len() > 1 {
            if !is_owned_viewer_tab_name(tab) {
                return Err(format!(
                    "bucket tab name '{}' is ambiguous across {} unowned tabs",
                    tab,
                    matches.len()
                ));
            }
            let keeper = expected
                .and_then(|expected| {
                    matches
                        .iter()
                        .find(|candidate| candidate.is_same_durable_tab(expected))
                })
                .cloned()
                .or_else(|| matches.iter().min_by_key(|identity| identity.id).cloned())
                .ok_or_else(|| format!("cannot select a keeper for viewer tab '{}'", tab))?;
            for duplicate in matches
                .iter()
                .filter(|identity| identity.tab_instance_id != keeper.tab_instance_id)
            {
                let id = duplicate.id.to_string();
                self.run(&[
                    "-s",
                    session,
                    "action",
                    "close-tab",
                    "--tab-id",
                    &id,
                    "--expected-name",
                    tab,
                    "--expected-session-incarnation",
                    &duplicate.session_incarnation,
                    "--expected-tab-instance-id",
                    &duplicate.tab_instance_id,
                ])?;
            }
            inventory = self.run(&["-s", session, "action", "list-tabs", "--json"])?;
            matches = tab_identities_for_name(&inventory, session, tab)?;
        }
        match matches.as_slice() {
            [] => Ok(None),
            [identity] => Ok(Some(identity.clone())),
            _ => Err(format!(
                "viewer tab '{}' is still ambiguous across {} tabs after reconciliation",
                tab,
                matches.len()
            )),
        }
    }

    fn rebind_origin_tab_identity(
        &mut self,
        session: &str,
        tab: &str,
        captured: &OriginTabIdentity,
    ) -> Result<Option<OriginTabIdentity>, CloseOriginError> {
        let preserved = |message: String| CloseOriginError {
            message,
            origin_tab_state: OriginTabState::Preserved,
        };
        let tab_list_result = self.run(&["-s", session, "action", "list-tabs", "--json"]);
        let session_exists_result = session_exists(session)
            .map_err(|error| format!("cannot check session '{}': {:?}", session, error));
        let Some(tab_list) =
            verified_tab_list_before_close(tab_list_result, session_exists_result)?
        else {
            return Ok(None);
        };
        let Some(current_id) =
            validated_origin_tab_id(captured, session, tab, &tab_list).map_err(preserved)?
        else {
            return Ok(None);
        };
        let current = tab_identities_for_name(&tab_list, session, tab)
            .map_err(preserved)?
            .into_iter()
            .find(|identity| identity.id == current_id)
            .ok_or_else(|| {
                preserved(format!(
                    "durable origin instance {} vanished during rebind",
                    captured.tab_instance_id
                ))
            })?;
        Ok(Some(current))
    }

    fn close_origin_tab(
        &mut self,
        session: &str,
        tab: &str,
        identity: Option<&OriginTabIdentity>,
    ) -> Result<(), CloseOriginError> {
        let preserved_error = |message: String| CloseOriginError {
            message,
            origin_tab_state: OriginTabState::Preserved,
        };
        // Resolve the unique name immediately before close, then require the
        // same ID captured before any durable side effect. A changed ID could
        // be a resurrected or replacement tab; preserving it is safer than
        // guessing which incarnation owns the name.
        let tab_list_result = self.run(&["-s", session, "action", "list-tabs", "--json"]);
        let session_exists_result = session_exists(session)
            .map_err(|error| format!("cannot check session '{}': {:?}", session, error));
        let Some(tabs) = verified_tab_list_before_close(tab_list_result, session_exists_result)?
        else {
            return Ok(());
        };
        let captured = match identity {
            Some(identity) => identity,
            None => {
                let ids = tab_ids_for_name(&tabs, tab).map_err(preserved_error)?;
                if ids.is_empty() {
                    return Ok(());
                }
                return Err(preserved_error(
                    "transfer receipt lacks typed origin identity while the origin name is still live; refusing to guess"
                        .to_owned(),
                ));
            },
        };
        let Some(current_tab_id) =
            validated_origin_tab_id(captured, session, tab, &tabs).map_err(preserved_error)?
        else {
            return Ok(());
        };
        let tab_id = current_tab_id.to_string();
        let close_result = self.run(&[
            "-s",
            session,
            "action",
            "close-tab",
            "--tab-id",
            &tab_id,
            "--expected-name",
            tab,
            "--expected-session-incarnation",
            &captured.session_incarnation,
            "--expected-tab-instance-id",
            &captured.tab_instance_id,
        ]);
        let tab_list_result = self.run(&["-s", session, "action", "list-tabs", "--json"]);
        let session_exists_result = session_exists(session)
            .map_err(|error| format!("cannot check session '{}': {:?}", session, error));
        verify_origin_close(
            current_tab_id,
            tab,
            &captured.session_incarnation,
            &captured.tab_instance_id,
            close_result,
            tab_list_result,
            session_exists_result,
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn triage_run(
    run: String,
    exit_code: i32,
    bucket_verdict: Option<BucketKind>,
    origin_session: Option<String>,
    origin_tab: Option<String>,
    pane_id: Option<String>,
    runtime_transcript: Option<PathBuf>,
    cwd: Option<PathBuf>,
    dry_run: bool,
    command: Vec<String>,
) -> Result<TransferReport, String> {
    let origin_session = origin_session
        .or_else(|| zellij_utils::envs::get_session_name().ok())
        .ok_or_else(|| {
            "no origin session: pass --origin-session or run from inside a session".to_owned()
        })?;
    let origin_tab = origin_tab.unwrap_or_else(|| run.clone());
    let cwd = cwd.or_else(|| std::env::current_dir().ok());

    let finished = FinishedRun {
        run,
        exit_code,
        origin_session,
        origin_tab,
        pane_id,
        runtime_transcript,
        command,
        cwd,
        bucket_verdict,
    };
    let root = control_plane_root()
        .ok_or_else(|| "cannot resolve the control plane root (set VIBECRAFTED_HOME)".to_owned())?;
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    if dry_run {
        let bucket = finished.bucket();
        println!(
            "would transfer '{}' (exit {}) from {}/{} into '{}'",
            finished.run,
            finished.exit_code,
            finished.origin_session,
            finished.origin_tab,
            bucket.session_name()
        );
        return Ok(TransferReport {
            run: finished.run.clone(),
            bucket,
            scrollback: zellij_utils::run_triage::scrollback_path(&root, &finished.run),
            meta: zellij_utils::run_triage::meta_path(&root, &finished.run),
            origin_tab_closed: false,
            receipt: transfer_receipt_path(&root, &finished.run),
        });
    }

    let lock_path = transfer_receipt_path(&root, &finished.run).with_file_name("transfer.lock");
    let _transfer_lock = RunTransferLock::acquire(&lock_path)?;
    let mut io = CliTriageIo::new(root.clone())?;
    transfer_finished_run(&mut io, &finished, &root, captured_at).map_err(|error| {
        format!(
            "triage failed at {:?}: {} (origin tab: {}, capture durable: {})",
            error.step, error.message, error.origin_tab_state, error.capture_is_durable
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn child_commands_are_killed_at_the_timeout_boundary() {
        let error = run_command_with_timeout(
            Path::new("/bin/sh"),
            &["-c", "sleep 2"],
            Duration::from_millis(40),
        )
        .unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn bounded_child_output_is_captured_without_pipes() {
        let output = run_command_with_timeout(
            Path::new("/bin/sh"),
            &["-c", "printf ready"],
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_transfer_lock_fails_fast_and_recovers_after_release() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("transfer.lock");
        let first = RunTransferLock::acquire(&path).unwrap();
        let error = RunTransferLock::acquire(&path).err().unwrap();
        assert!(error.contains("refusing to wait"), "{error}");
        drop(first);
        RunTransferLock::acquire(&path).unwrap();
    }

    fn meta(command: Vec<&str>) -> RunMeta {
        RunMeta {
            run: "impl-260720-120000-01000".to_owned(),
            exit_code: 1,
            bucket: BucketKind::NeedsAttention,
            origin_session: "Operator".to_owned(),
            origin_tab: "impl-260720-120000-01000".to_owned(),
            command: command.into_iter().map(str::to_owned).collect(),
            cwd: Some(PathBuf::from("/repo")),
            captured_at: 0,
            capture_source: CaptureSource::TerminalScrollback,
            capture_source_identity: "session=Operator;tab_id=7;pane_id=terminal_3".to_owned(),
            capture_bytes: 42,
            capture_sha256: "fixture-sha256".to_owned(),
        }
    }

    #[test]
    fn bucket_tab_shows_the_scrollback_and_a_suspended_rerun() {
        let layout = bucket_tab_layout(
            Path::new("/cp/finished_runs/impl-260720-120000-01000/scrollback.txt"),
            &meta(vec!["claude", "--resume"]),
        );

        assert!(layout.contains("/cp/finished_runs/impl-260720-120000-01000/scrollback.txt"));
        assert!(layout.contains("command=\"claude\""));
        assert!(layout.contains("args \"--resume\""));
        // one-keypress rerun, not an automatic one
        assert!(layout.contains("start_suspended=true"));
        assert!(layout.contains("cwd \"/repo\""));
    }

    #[test]
    fn a_run_without_a_command_still_gets_a_viewer() {
        let layout = bucket_tab_layout(Path::new("/cp/s.txt"), &meta(vec![]));
        assert!(layout.contains("/cp/s.txt"));
        assert!(!layout.contains("start_suspended"));
    }

    #[test]
    fn quotes_and_backslashes_cannot_break_out_of_the_layout() {
        let layout = bucket_tab_layout(
            Path::new("/cp/s.txt"),
            &meta(vec!["sh", "-c", "echo \"hi\""]),
        );
        assert!(layout.contains(r#"args "-c" "echo \"hi\"""#));
    }

    #[test]
    fn recorded_pane_must_belong_to_the_origin_tab() {
        let panes = r#"[
            {"id": 1, "is_plugin": false, "tab_id": 1, "tab_name": "operator"},
            {"id": 7, "is_plugin": false, "tab_id": 7, "tab_name": "run-1"}
        ]"#;

        assert_eq!(
            terminal_pane_id_for_tab(panes, 7, "run-1", Some("terminal_1")).unwrap(),
            7
        );
    }

    #[test]
    fn verified_recorded_pane_disambiguates_a_multi_pane_origin_tab() {
        let panes = r#"[
            {"id": 7, "is_plugin": false, "tab_id": 7, "tab_name": "run-1"},
            {"id": 8, "is_plugin": false, "tab_id": 7, "tab_name": "run-1"}
        ]"#;

        assert_eq!(
            terminal_pane_id_for_tab(panes, 7, "run-1", Some("terminal_8")).unwrap(),
            8
        );
    }

    #[test]
    fn ambiguous_origin_tab_fails_closed() {
        let panes = r#"[
            {"id": 7, "is_plugin": false, "tab_id": 7, "tab_name": "run-1"},
            {"id": 8, "is_plugin": false, "tab_id": 7, "tab_name": "run-1"},
            {"id": 9, "is_plugin": true, "tab_id": 7, "tab_name": "run-1"}
        ]"#;

        let error = terminal_pane_id_for_tab(panes, 7, "run-1", Some("terminal_1")).unwrap_err();
        assert!(error.contains("2 terminal panes"));
    }

    #[test]
    fn non_selectable_recorded_pane_is_still_captured_from_full_inventory() {
        let panes = r#"[
            {
                "id": 7,
                "is_plugin": false,
                "is_selectable": false,
                "is_suppressed": true,
                "tab_id": 3,
                "tab_name": "run-1"
            }
        ]"#;

        assert_eq!(
            terminal_pane_id_for_tab(panes, 3, "run-1", Some("terminal_7")).unwrap(),
            7
        );
    }

    #[test]
    fn pane_selection_uses_tab_id_not_a_duplicate_name() {
        let panes = r#"[
            {"id": 1, "is_plugin": false, "tab_id": 2, "tab_name": "run-1"},
            {"id": 7, "is_plugin": false, "tab_id": 3, "tab_name": "run-1"}
        ]"#;

        assert_eq!(
            terminal_pane_id_for_tab(panes, 3, "run-1", Some("terminal_1")).unwrap(),
            7
        );
    }

    #[test]
    fn origin_tab_name_resolves_to_its_stable_id() {
        let tabs = r#"[
            {"tab_id": 1, "name": "operator"},
            {"tab_id": 7, "name": "run-1"}
        ]"#;

        assert_eq!(tab_id_for_name(tabs, "run-1").unwrap(), 7);
    }

    #[test]
    fn duplicate_origin_tab_names_fail_closed() {
        let tabs = r#"[
            {"tab_id": 7, "name": "run-1"},
            {"tab_id": 8, "name": "run-1"}
        ]"#;

        let error = tab_id_for_name(tabs, "run-1").unwrap_err();
        assert!(error.contains("ambiguous across 2 tab ids"));
    }

    #[test]
    fn origin_identity_rebinds_across_server_lifetimes() {
        let tabs = r#"[
            {"tab_id": 0, "name": "operator", "session_incarnation": "inc-2", "tab_instance_id":"00000000000000000000000000000000"},
            {"tab_id": 9, "name": "run-1", "session_incarnation": "inc-2", "tab_instance_id":"11111111111111111111111111111111"}
        ]"#;
        let captured = OriginTabIdentity {
            session: "workers".to_owned(),
            name: "run-1".to_owned(),
            id: 7,
            session_incarnation: "inc-1".to_owned(),
            tab_instance_id: "11111111111111111111111111111111".to_owned(),
        };
        assert_eq!(
            validated_origin_tab_id(&captured, "workers", "run-1", tabs).unwrap(),
            Some(9)
        );
    }

    #[test]
    fn captured_id_renamed_is_preserved_instead_of_marked_closed() {
        let tabs = r#"[
            {"tab_id": 7, "name": "renamed", "session_incarnation": "inc-1", "tab_instance_id":"11111111111111111111111111111111"}
        ]"#;
        let captured = OriginTabIdentity {
            session: "workers".to_owned(),
            name: "run-1".to_owned(),
            id: 7,
            session_incarnation: "inc-1".to_owned(),
            tab_instance_id: "11111111111111111111111111111111".to_owned(),
        };
        let error = validated_origin_tab_id(&captured, "workers", "run-1", tabs).unwrap_err();
        assert!(error.contains("was renamed"));
    }

    #[test]
    fn same_name_successor_is_preserved_instead_of_closed() {
        let captured = OriginTabIdentity {
            session: "workers".to_owned(),
            name: "run-1".to_owned(),
            id: 7,
            session_incarnation: "inc-1".to_owned(),
            tab_instance_id: "11111111111111111111111111111111".to_owned(),
        };
        let successor = r#"[
            {"tab_id": 8, "name": "run-1", "session_incarnation": "inc-1", "tab_instance_id":"22222222222222222222222222222222"}
        ]"#;
        assert_eq!(
            validated_origin_tab_id(&captured, "workers", "run-1", successor).unwrap(),
            None
        );

        let absent = r#"[
            {"tab_id": 8, "name": "another", "session_incarnation": "inc-1"}
        ]"#;
        assert_eq!(
            validated_origin_tab_id(&captured, "workers", "run-1", absent).unwrap(),
            None
        );
    }

    #[test]
    fn real_transcript_recovers_after_the_origin_session_is_gone() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "vc-frame-triage-transcript-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let transcript = directory.join("runtime.log");
        let dest = directory.join("scrollback.txt");
        std::fs::write(&transcript, b"real runtime evidence").unwrap();
        let transcript_manifest = runtime_transcript_manifest_path(&transcript);
        std::fs::write(
            &transcript_manifest,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "run_id": "run-1",
                "transcript": transcript.clone(),
                "root": directory.clone(),
                "bytes": 21,
                "sha256": capture_sha256(&transcript).unwrap(),
            }))
            .unwrap(),
        )
        .unwrap();

        let capture = capture_runtime_transcript(
            "run-1",
            "workers",
            "run-1",
            None,
            &transcript,
            &dest,
            "origin session not found",
            None,
        )
        .unwrap();

        assert_eq!(capture.capture_source, CaptureSource::RuntimeTranscript);
        assert_eq!(capture.origin_tab_identity, None);
        assert_eq!(std::fs::read(&dest).unwrap(), b"real runtime evidence");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn failed_capture_retry_preserves_the_last_good_scrollback() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "vc-frame-triage-capture-{}-{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let dest = directory.join("scrollback.txt");
        let temporary = directory.join("scrollback.tmp");
        std::fs::write(&dest, b"last-good").unwrap();
        std::fs::write(&temporary, b"").unwrap();

        let mut manifest = CaptureCommitManifest {
            version: 1,
            run_id: "run-1".to_owned(),
            session: "workers".to_owned(),
            origin_tab: "run-1".to_owned(),
            pane_id: Some("terminal_7".to_owned()),
            runtime_transcript: None,
            staging_file: String::new(),
            evidence: CaptureEvidence {
                capture_source: CaptureSource::TerminalScrollback,
                source_identity: "typed-test".to_owned(),
                bytes: 0,
                sha256: String::new(),
                origin_tab_identity: Some(OriginTabIdentity {
                    session: "workers".to_owned(),
                    name: "run-1".to_owned(),
                    id: 7,
                    session_incarnation: "inc-1".to_owned(),
                    tab_instance_id: "11111111111111111111111111111111".to_owned(),
                }),
            },
        };
        assert!(commit_scrollback_capture(&temporary, &dest, &mut manifest).is_err());
        assert_eq!(std::fs::read(&dest).unwrap(), b"last-good");

        std::fs::write(&temporary, b"replacement").unwrap();
        commit_scrollback_capture(&temporary, &dest, &mut manifest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"replacement");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn committed_capture_is_adopted_after_crash_before_transfer_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let dest = directory.path().join("scrollback.txt");
        let temporary = unique_staging_path(&dest, "terminal-capture").unwrap();
        std::fs::write(&temporary, b"durable terminal evidence").unwrap();

        let origin_tab_identity = OriginTabIdentity {
            session: "workers".to_owned(),
            name: "run-1".to_owned(),
            id: 7,
            session_incarnation: "inc-1".to_owned(),
            tab_instance_id: "11111111111111111111111111111111".to_owned(),
        };
        let mut manifest = CaptureCommitManifest {
            version: 1,
            run_id: "run-1".to_owned(),
            session: "workers".to_owned(),
            origin_tab: "run-1".to_owned(),
            pane_id: Some("terminal_7".to_owned()),
            runtime_transcript: None,
            staging_file: String::new(),
            evidence: CaptureEvidence {
                capture_source: CaptureSource::TerminalScrollback,
                source_identity: "typed-test".to_owned(),
                bytes: 0,
                sha256: String::new(),
                origin_tab_identity: Some(origin_tab_identity),
            },
        };

        // This is the killpoint: the evidence and its manifest are durable, but
        // no transfer receipt has been written yet.
        let committed = commit_scrollback_capture(&temporary, &dest, &mut manifest).unwrap();
        assert!(dest.is_file());
        assert!(capture_manifest_path(&dest).is_file());

        let adopted =
            adopt_capture_commit("run-1", "workers", "run-1", Some("terminal_7"), None, &dest)
                .unwrap()
                .expect("restart should adopt the already committed capture");

        assert_eq!(adopted, committed);
        assert_eq!(std::fs::read(&dest).unwrap(), b"durable terminal evidence");
    }

    #[test]
    fn absent_durable_instance_wins_over_a_stale_close_error() {
        let result = verify_origin_close(
            7,
            "run-1",
            "inc-1",
            "11111111111111111111111111111111",
            Err("server cleanup failed".to_owned()),
            Ok("[]".to_owned()),
            Ok(false),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn retry_after_closing_the_last_tab_accepts_an_absent_session() {
        assert_eq!(
            verified_tab_list_before_close(Err("session not found".to_owned()), Ok(false),)
                .unwrap(),
            None
        );
    }

    #[test]
    fn preclose_inventory_failure_is_unknown_while_the_session_exists() {
        let error = verified_tab_list_before_close(Err("list-tabs timed out".to_owned()), Ok(true))
            .unwrap_err();

        assert_eq!(error.origin_tab_state, OriginTabState::Unknown);
        assert_eq!(error.message, "list-tabs timed out");
    }

    #[test]
    fn stale_id_absence_does_not_hide_a_live_origin_name() {
        let result = verify_origin_close(
            7,
            "run-1",
            "inc-1",
            "11111111111111111111111111111111",
            Err("stale id".to_owned()),
            Ok(r#"[{"tab_id":2,"name":"run-1","session_incarnation":"inc-1","tab_instance_id":"22222222222222222222222222222222"}]"#.to_owned()),
            Ok(true),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn closing_the_last_tab_is_verified_by_session_absence() {
        let result = verify_origin_close(
            7,
            "run-1",
            "inc-1",
            "11111111111111111111111111111111",
            Ok(String::new()),
            Err("session not found".to_owned()),
            Ok(false),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn failed_postverify_is_not_ignored_while_the_session_exists() {
        let result = verify_origin_close(
            7,
            "run-1",
            "inc-1",
            "11111111111111111111111111111111",
            Ok(String::new()),
            Err("list-tabs timed out".to_owned()),
            Ok(true),
        );

        let error = result.unwrap_err();
        assert_eq!(error.message, "list-tabs timed out");
        assert_eq!(error.origin_tab_state, OriginTabState::Unknown);
    }

    #[test]
    fn failed_atomic_close_never_marks_a_renamed_origin_closed() {
        let result = verify_origin_close(
            7,
            "run-1",
            "inc-1",
            "11111111111111111111111111111111",
            Err("expected name mismatch".to_owned()),
            Ok(r#"[{"tab_id":7,"name":"renamed","session_incarnation":"inc-1","tab_instance_id":"11111111111111111111111111111111"}]"#.to_owned()),
            Ok(true),
        );

        let error = result.unwrap_err();
        assert_eq!(error.origin_tab_state, OriginTabState::Preserved);
        assert!(error.message.contains("expected name mismatch"));
    }
}
