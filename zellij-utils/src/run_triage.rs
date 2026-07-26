//! Run triage — move finished runs off the working tab rail into per-status
//! bucket sessions.
//!
//! A pane's PTY belongs to the session's server process and cannot migrate
//! across sessions. "Transfer" therefore means: capture the scrollback and the
//! run metadata to durable storage, recreate a viewer/rerun tab in the target
//! bucket session, and only then close the origin tab.
//!
//! The ordering is the whole point. The origin tab is the only live copy of the
//! scrollback until the capture lands, so every failure path in this module
//! leaves the origin tab open. `transfer_finished_run` is the state machine
//! that enforces that; [`TriageIo`] is the seam that lets the tests inject a
//! fault at each step and assert the invariant still holds.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Canonical bucket session for runs that finished cleanly.
pub const FINALIZED_RUNS_SESSION: &str = "Finalized runs";
/// Canonical bucket session for runs that failed cleanly — the failure is
/// understood, there is nothing to investigate.
pub const FAILED_RUNS_SESSION: &str = "Failed runs";
/// Canonical bucket session for runs whose signals disagree, or are missing.
pub const NEEDS_ATTENTION_SESSION: &str = "Needs attention";

/// Which drawer a finished run lands in.
///
/// The verdict is a *conjunction of signals* — exit code, report presence and
/// state, log volume. Only the caller can see all of them: report and log
/// signals live in vibecrafted's control plane, not in this repo. So this
/// module transports a verdict, it does not invent one. [`for_exit_code`] is
/// the fallback for callers that have no verdict to give.
///
/// [`for_exit_code`]: BucketKind::for_exit_code
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BucketKind {
    /// Exited 0 *and* delivered a report.
    Finalized,
    /// Exited non-zero, no report, minimal log — a clean, legible failure.
    Failed,
    /// Anything in between, or with signals that contradict each other.
    NeedsAttention,
}

impl BucketKind {
    /// Fallback derivation for callers with nothing but an exit code —
    /// a manual `triage-run` invocation, mostly.
    ///
    /// Note what this deliberately never returns: `Failed`. "Clean failure"
    /// is a claim about the report and the log as much as about the exit
    /// code, and a caller that cannot see those signals cannot make it. A
    /// bare non-zero exit is contradictory *by ignorance*, so it lands in
    /// `NeedsAttention` — the fallback must not fake certainty it lacks.
    pub fn for_exit_code(exit_code: i32) -> Self {
        if exit_code == 0 {
            BucketKind::Finalized
        } else {
            BucketKind::NeedsAttention
        }
    }

    pub fn session_name(&self) -> &'static str {
        match self {
            BucketKind::Finalized => FINALIZED_RUNS_SESSION,
            BucketKind::Failed => FAILED_RUNS_SESSION,
            BucketKind::NeedsAttention => NEEDS_ATTENTION_SESSION,
        }
    }

    /// True when `name` is one of the canonical bucket sessions. Used by the
    /// rail to keep buckets out of the ordinary session listing.
    pub fn from_session_name(name: &str) -> Option<Self> {
        match name {
            FINALIZED_RUNS_SESSION => Some(BucketKind::Finalized),
            FAILED_RUNS_SESSION => Some(BucketKind::Failed),
            NEEDS_ATTENTION_SESSION => Some(BucketKind::NeedsAttention),
            _ => None,
        }
    }

    /// The `--bucket` spelling, kebab-case and stable — it is a wire contract
    /// with the caller-side classifier.
    pub fn cli_value(&self) -> &'static str {
        match self {
            BucketKind::Finalized => "finalized",
            BucketKind::Failed => "failed",
            BucketKind::NeedsAttention => "needs-attention",
        }
    }
}

/// Whether the server's idle-exit watchdog is allowed to reap a session by this
/// name.
///
/// The watchdog was armed against abandoned `--server` processes burning CPU
/// with zero clients, and it reads "no client for N seconds" as "abandoned".
/// For a triage drawer that inference is exactly backwards: the reaper
/// materializes the drawer with `attach --create-background` and *nothing ever
/// attaches* — the operator reads it off the rail without entering it. Arming
/// idle-exit on a drawer therefore guarantees it is killed N seconds after its
/// last transfer, and the rail's `f`/`x`/`n` counters — which count the
/// drawer's tabs — silently fall to zero while the runs themselves are still
/// durably captured under `finished_runs/`. A blind cockpit is a worse failure
/// than an idle server, so the drawers are exempt.
///
/// `None` (no session name in the environment) stays reapable: an unnamed
/// server is exactly the abandoned-process case the watchdog exists for.
pub fn idle_exit_may_reap(session_name: Option<&str>) -> bool {
    match session_name {
        Some(name) => BucketKind::from_session_name(name).is_none(),
        None => true,
    }
}

/// Parse an explicit `--bucket` verdict. Rejects anything it does not know
/// rather than guessing — a typo'd verdict must not silently become a bucket.
pub fn parse_bucket_verdict(value: &str) -> Result<BucketKind, String> {
    match value {
        "finalized" => Ok(BucketKind::Finalized),
        "failed" => Ok(BucketKind::Failed),
        "needs-attention" => Ok(BucketKind::NeedsAttention),
        other => Err(format!(
            "unknown bucket verdict '{}' (expected finalized, failed or needs-attention)",
            other
        )),
    }
}

/// Everything the reaper knows about a run at the moment it exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishedRun {
    /// Stable run identifier, also the capture directory and target tab name.
    pub run: String,
    pub exit_code: i32,
    /// Session the run was living in.
    pub origin_session: String,
    /// Tab within `origin_session` to close once the capture is durable.
    pub origin_tab: String,
    /// Pane to dump. `None` dumps the tab's focused pane.
    pub pane_id: Option<String>,
    /// Optional real transcript emitted by the runtime. This is only used when
    /// the exact terminal pane cannot produce scrollback; placeholders are not
    /// accepted.
    pub runtime_transcript: Option<PathBuf>,
    /// Command line, preserved so the bucket tab can offer a one-keypress rerun.
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// The caller's verdict, when it has one. `None` means "I only know the
    /// exit code" and hands the decision to [`BucketKind::for_exit_code`].
    pub bucket_verdict: Option<BucketKind>,
}

impl FinishedRun {
    /// The caller's verdict wins whenever there is one — it saw the report and
    /// the log; we only ever saw the exit code.
    pub fn bucket(&self) -> BucketKind {
        self.bucket_verdict
            .unwrap_or_else(|| BucketKind::for_exit_code(self.exit_code))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    TerminalScrollback,
    RuntimeTranscript,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginTabIdentity {
    pub session: String,
    pub name: String,
    pub id: u64,
    #[serde(default)]
    pub session_incarnation: String,
    #[serde(default)]
    pub tab_instance_id: String,
}

impl OriginTabIdentity {
    pub fn is_typed(&self) -> bool {
        !self.session.is_empty()
            && !self.name.is_empty()
            && !self.session_incarnation.is_empty()
            && self.tab_instance_id.len() == 32
            && self
                .tab_instance_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }

    pub fn is_same_durable_tab(&self, other: &Self) -> bool {
        self.session == other.session
            && self.name == other.name
            && !self.tab_instance_id.is_empty()
            && self.tab_instance_id == other.tab_instance_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureEvidence {
    pub capture_source: CaptureSource,
    pub source_identity: String,
    pub bytes: u64,
    #[serde(default)]
    pub sha256: String,
    #[serde(default)]
    pub origin_tab_identity: Option<OriginTabIdentity>,
}

/// What lands next to the scrollback, so a run stays rerunnable after the
/// origin tab is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMeta {
    pub run: String,
    pub exit_code: i32,
    pub bucket: BucketKind,
    pub origin_session: String,
    pub origin_tab: String,
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Unix seconds. Supplied by the caller — this module stays clock-free so
    /// its tests are deterministic.
    pub captured_at: u64,
    pub capture_source: CaptureSource,
    pub capture_source_identity: String,
    pub capture_bytes: u64,
    pub capture_sha256: String,
}

impl RunMeta {
    pub fn new(run: &FinishedRun, captured_at: u64, capture: &CaptureEvidence) -> Self {
        RunMeta {
            run: run.run.clone(),
            exit_code: run.exit_code,
            bucket: run.bucket(),
            origin_session: run.origin_session.clone(),
            origin_tab: run.origin_tab.clone(),
            command: run.command.clone(),
            cwd: run.cwd.clone(),
            captured_at,
            capture_source: capture.capture_source,
            capture_source_identity: capture.source_identity.clone(),
            capture_bytes: capture.bytes,
            capture_sha256: capture.sha256.clone(),
        }
    }
}

/// `<root>/finished_runs/<run>` — the durable home of one transferred run.
pub fn capture_dir(root: &Path, run: &str) -> PathBuf {
    root.join("finished_runs").join(run)
}

pub fn scrollback_path(root: &Path, run: &str) -> PathBuf {
    capture_dir(root, run).join("scrollback.txt")
}

pub fn meta_path(root: &Path, run: &str) -> PathBuf {
    capture_dir(root, run).join("meta.json")
}

pub fn transfer_receipt_path(root: &Path, run: &str) -> PathBuf {
    capture_dir(root, run).join("transfer.json")
}

pub fn capture_sha256(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("cannot hash {}: {}", path.display(), error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {}: {}", path.display(), error))?;
        if bytes == 0 {
            break;
        }
        hasher.update(&buffer[..bytes]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Control-plane root, `~/.vibecrafted/control_plane` by convention.
pub fn control_plane_root() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("VIBECRAFTED_CONTROL_PLANE") {
        return Some(PathBuf::from(explicit));
    }
    let home = std::env::var("VIBECRAFTED_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .map(|h| PathBuf::from(h).join(".vibecrafted"))
                .ok()
        })?;
    Some(home.join("control_plane"))
}

/// The step a transfer was on when it failed. Carried in [`TransferError`] so
/// the caller can tell "nothing happened" from "captured but not yet moved".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStep {
    LoadReceipt,
    Capture,
    WriteReceipt,
    WriteMeta,
    EnsureBucketSession,
    OpenBucketTab,
    ConfirmBucketTab,
    CloseOriginTab,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferError {
    pub step: TransferStep,
    pub message: String,
    /// Proven state of the origin after the failing step. A close can remove a
    /// tab and still fail during downstream cleanup, so a boolean "preserved"
    /// claim is not honest enough for this boundary.
    pub origin_tab_state: OriginTabState,
    /// True once the scrollback + meta are durable on disk, even if the move
    /// itself failed. The operator can still recover the run by hand.
    pub capture_is_durable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginTabState {
    #[default]
    Preserved,
    Closed,
    Unknown,
}

impl std::fmt::Display for OriginTabState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            OriginTabState::Preserved => "preserved",
            OriginTabState::Closed => "closed",
            OriginTabState::Unknown => "unknown",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOriginError {
    pub message: String,
    pub origin_tab_state: OriginTabState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    pub run: String,
    pub bucket: BucketKind,
    pub scrollback: PathBuf,
    pub meta: PathBuf,
    pub origin_tab_closed: bool,
    pub receipt: PathBuf,
}

/// Durable transfer evidence. This is deliberately independent from the live
/// drawer inventory: it lets a reconciler resume after interruption without
/// guessing whether capture, viewer creation or origin close already happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferReceipt {
    pub version: u8,
    pub run: String,
    pub bucket: BucketKind,
    #[serde(default)]
    pub exit_code: i32,
    #[serde(default)]
    pub origin_session: String,
    #[serde(default)]
    pub origin_tab: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub pane_id: Option<String>,
    #[serde(default)]
    pub runtime_transcript: Option<PathBuf>,
    pub capture: Option<CaptureEvidence>,
    pub capture_committed: bool,
    pub metadata_committed: bool,
    pub viewer_confirmed: bool,
    #[serde(default)]
    pub viewer_tab_identity: Option<OriginTabIdentity>,
    #[serde(default)]
    pub viewer_creation_pending: bool,
    #[serde(default)]
    pub viewer_token: String,
    pub origin_tab_state: OriginTabState,
    pub fault: Option<String>,
    pub updated_at: u64,
}

impl TransferReceipt {
    fn new(run: &FinishedRun, captured_at: u64) -> Self {
        Self {
            version: 4,
            run: run.run.clone(),
            bucket: run.bucket(),
            exit_code: run.exit_code,
            origin_session: run.origin_session.clone(),
            origin_tab: run.origin_tab.clone(),
            command: run.command.clone(),
            cwd: run.cwd.clone(),
            pane_id: run.pane_id.clone(),
            runtime_transcript: run.runtime_transcript.clone(),
            capture: None,
            capture_committed: false,
            metadata_committed: false,
            viewer_confirmed: false,
            viewer_tab_identity: None,
            viewer_creation_pending: false,
            viewer_token: Uuid::new_v4().simple().to_string(),
            origin_tab_state: OriginTabState::Preserved,
            fault: None,
            updated_at: captured_at,
        }
    }

    fn viewer_tab_name(&self) -> String {
        format!("{} [vc:{}]", self.run, self.viewer_token)
    }
}

/// The side-effecting surface of a transfer. Split out so the ordering
/// invariant can be tested with a fault injected at any step.
pub trait TriageIo {
    /// Dump the run's scrollback to `dest`. `origin_tab` is the run's own tab
    /// — the durable address of the scrollback, and the fallback target when
    /// `pane_id` is absent or no longer resolves to a live pane.
    fn capture_scrollback(
        &mut self,
        run_id: &str,
        session: &str,
        origin_tab: &str,
        pane_id: Option<&str>,
        runtime_transcript: Option<&Path>,
        dest: &Path,
    ) -> Result<CaptureEvidence, String>;
    fn load_receipt(&mut self, path: &Path) -> Result<Option<TransferReceipt>, String>;
    fn write_receipt(&mut self, path: &Path, receipt: &TransferReceipt) -> Result<(), String>;
    fn write_meta(&mut self, dest: &Path, meta: &RunMeta) -> Result<(), String>;
    /// Create the bucket session if it does not exist yet. Idempotent.
    fn ensure_bucket_session(&mut self, session: &str) -> Result<(), String>;
    /// Open a tab named `tab` in `session`, showing the dump and offering a
    /// suspended rerun of the original command.
    fn open_bucket_tab(&mut self, session: &str, tab: &str, meta: &RunMeta) -> Result<(), String>;
    /// Read back the target session and return the unique durable identity of
    /// `tab`, including the server incarnation that owns its stable ID.
    fn bucket_tab_identity(
        &mut self,
        session: &str,
        tab: &str,
        expected: Option<&OriginTabIdentity>,
    ) -> Result<Option<OriginTabIdentity>, String>;
    /// Re-resolve a captured tab through its durable tab incarnation. A server
    /// restart may change both the server incarnation and numeric tab ID.
    fn rebind_origin_tab_identity(
        &mut self,
        session: &str,
        tab: &str,
        captured: &OriginTabIdentity,
    ) -> Result<Option<OriginTabIdentity>, CloseOriginError>;
    fn close_origin_tab(
        &mut self,
        session: &str,
        tab: &str,
        identity: Option<&OriginTabIdentity>,
    ) -> Result<(), CloseOriginError>;
}

fn file_matches_capture(path: &Path, capture: &CaptureEvidence) -> bool {
    std::fs::metadata(path)
        .map(|metadata| {
            metadata.is_file()
                && capture.bytes > 0
                && metadata.len() == capture.bytes
                && !capture.sha256.is_empty()
                && capture_sha256(path).as_deref() == Ok(capture.sha256.as_str())
        })
        .unwrap_or(false)
}

fn file_matches_meta(path: &Path, expected: &RunMeta) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<RunMeta>(&bytes).ok())
        .as_ref()
        == Some(expected)
}

fn fail_transfer<Io: TriageIo>(
    io: &mut Io,
    receipt_path: &Path,
    receipt: &mut TransferReceipt,
    step: TransferStep,
    message: String,
    origin_tab_state: OriginTabState,
) -> TransferError {
    receipt.origin_tab_state = origin_tab_state;
    receipt.fault = Some(sanitize_persisted_fault(&format!(
        "{:?}: {}",
        step, message
    )));
    let message = match io.write_receipt(receipt_path, receipt) {
        Ok(()) => message,
        Err(receipt_error) => format!(
            "{}; additionally failed to persist transfer fault at {}: {}",
            message,
            receipt_path.display(),
            receipt_error
        ),
    };
    TransferError {
        step,
        message,
        origin_tab_state,
        capture_is_durable: receipt.capture_committed && receipt.metadata_committed,
    }
}

fn sanitize_persisted_fault(message: &str) -> String {
    const MAX_FAULT_CHARS: usize = 2048;
    let mut sanitized = message
        .chars()
        .map(|character| {
            if character.is_control() && character != '\n' && character != '\t' {
                '�'
            } else {
                character
            }
        })
        .take(MAX_FAULT_CHARS + 1)
        .collect::<String>();
    if sanitized.chars().count() > MAX_FAULT_CHARS {
        sanitized = sanitized.chars().take(MAX_FAULT_CHARS).collect();
        sanitized.push_str("…[truncated]");
    }
    sanitized
}

fn persist_receipt<Io: TriageIo>(
    io: &mut Io,
    receipt_path: &Path,
    receipt: &mut TransferReceipt,
) -> Result<(), TransferError> {
    receipt.fault = None;
    io.write_receipt(receipt_path, receipt)
        .map_err(|message| TransferError {
            step: TransferStep::WriteReceipt,
            message,
            origin_tab_state: receipt.origin_tab_state,
            capture_is_durable: receipt.capture_committed && receipt.metadata_committed,
        })
}

/// Transfer one finished run into its status bucket.
///
/// Capture happens before anything is torn down, and the origin tab is closed
/// only after the target tab has been read back and confirmed. If any step
/// fails the origin tab stays open and the error says how far we got.
pub fn transfer_finished_run<Io: TriageIo>(
    io: &mut Io,
    run: &FinishedRun,
    root: &Path,
    captured_at: u64,
) -> Result<TransferReport, TransferError> {
    let bucket = run.bucket();
    let scrollback = scrollback_path(root, &run.run);
    let meta_dest = meta_path(root, &run.run);
    let receipt_dest = transfer_receipt_path(root, &run.run);
    let mut receipt = io
        .load_receipt(&receipt_dest)
        .map_err(|message| TransferError {
            step: TransferStep::LoadReceipt,
            message,
            origin_tab_state: OriginTabState::Unknown,
            capture_is_durable: false,
        })?
        .unwrap_or_else(|| TransferReceipt::new(run, captured_at));

    if receipt.version != 4
        || receipt.run != run.run
        || receipt.bucket != bucket
        || receipt.exit_code != run.exit_code
        || receipt.origin_session != run.origin_session
        || receipt.origin_tab != run.origin_tab
        || receipt.command != run.command
        || receipt.cwd != run.cwd
        || receipt.pane_id != run.pane_id
        || receipt.runtime_transcript != run.runtime_transcript
        || receipt.viewer_token.is_empty()
    {
        let message = format!(
            "receipt identity mismatch: stored v{} run '{}' in {:?} at {}/{}, requested v4 '{}' in {:?} at {}/{}",
            receipt.version,
            receipt.run,
            receipt.bucket,
            receipt.origin_session,
            receipt.origin_tab,
            run.run,
            bucket,
            run.origin_session,
            run.origin_tab
        );
        return Err(TransferError {
            step: TransferStep::LoadReceipt,
            message,
            origin_tab_state: OriginTabState::Unknown,
            capture_is_durable: receipt.capture_committed && receipt.metadata_committed,
        });
    }

    if receipt.capture_committed
        && !receipt
            .capture
            .as_ref()
            .map(|capture| file_matches_capture(&scrollback, capture))
            .unwrap_or(false)
    {
        receipt.capture_committed = false;
        receipt.metadata_committed = false;
        receipt.capture = None;
    }

    if !receipt.capture_committed {
        let capture = match io.capture_scrollback(
            &run.run,
            &run.origin_session,
            &run.origin_tab,
            run.pane_id.as_deref(),
            run.runtime_transcript.as_deref(),
            &scrollback,
        ) {
            Ok(capture) => capture,
            Err(message) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::Capture,
                    message,
                    OriginTabState::Preserved,
                ));
            },
        };
        receipt.capture = Some(capture);
        receipt.capture_committed = true;
        receipt.metadata_committed = false;
        persist_receipt(io, &receipt_dest, &mut receipt)?;
    }

    let capture = receipt
        .capture
        .clone()
        .expect("capture evidence exists after committed capture");
    // A retry is a continuation of the same transfer, not a new capture
    // epoch. Keeping the original timestamp makes metadata verification
    // stable across process restarts.
    let meta = RunMeta::new(run, receipt.updated_at, &capture);
    if receipt.metadata_committed && !file_matches_meta(&meta_dest, &meta) {
        receipt.metadata_committed = false;
    }
    if !receipt.metadata_committed {
        if let Err(message) = io.write_meta(&meta_dest, &meta) {
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::WriteMeta,
                message,
                OriginTabState::Preserved,
            ));
        }
        receipt.metadata_committed = true;
        persist_receipt(io, &receipt_dest, &mut receipt)?;
    }

    // Always revalidate the viewer. A drawer server can restart after the
    // receipt was committed; the durable capture remains truth and the viewer
    // can be resurrected without touching the origin.
    receipt.viewer_confirmed = false;
    let current_origin_tab_state = receipt.origin_tab_state;
    if let Err(message) = io.ensure_bucket_session(bucket.session_name()) {
        return Err(fail_transfer(
            io,
            &receipt_dest,
            &mut receipt,
            TransferStep::EnsureBucketSession,
            message,
            current_origin_tab_state,
        ));
    }
    let viewer_tab_name = receipt.viewer_tab_name();
    let current_viewer_identity = match io.bucket_tab_identity(
        bucket.session_name(),
        &viewer_tab_name,
        receipt.viewer_tab_identity.as_ref(),
    ) {
        Ok(current_viewer_identity) => current_viewer_identity,
        Err(message) => {
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::ConfirmBucketTab,
                message,
                current_origin_tab_state,
            ));
        },
    };
    let confirmed_viewer_identity =
        match (current_viewer_identity, receipt.viewer_tab_identity.clone()) {
            (Some(current), Some(expected)) if current.is_same_durable_tab(&expected) => current,
            (Some(current), Some(expected)) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::ConfirmBucketTab,
                    format!(
                        "viewer tab '{}' changed identity: expected {:?}, current {:?}",
                        viewer_tab_name, expected, current
                    ),
                    current_origin_tab_state,
                ));
            },
            (Some(current), None) if receipt.viewer_creation_pending => current,
            (Some(current), None) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::ConfirmBucketTab,
                    format!(
                        "unowned viewer tab '{}' ({:?}) already exists in session '{}'",
                        viewer_tab_name,
                        current,
                        bucket.session_name()
                    ),
                    current_origin_tab_state,
                ));
            },
            (None, _) => {
                // Persist the creation reservation before opening. If the process
                // dies after `new-tab`, a retry may adopt exactly one matching tab;
                // an unrelated pre-existing tab is never accepted without this
                // durable pending marker.
                receipt.viewer_tab_identity = None;
                receipt.viewer_creation_pending = true;
                persist_receipt(io, &receipt_dest, &mut receipt)?;
                if let Err(message) =
                    io.open_bucket_tab(bucket.session_name(), &viewer_tab_name, &meta)
                {
                    return Err(fail_transfer(
                        io,
                        &receipt_dest,
                        &mut receipt,
                        TransferStep::OpenBucketTab,
                        message,
                        current_origin_tab_state,
                    ));
                }
                match io.bucket_tab_identity(
                    bucket.session_name(),
                    &viewer_tab_name,
                    receipt.viewer_tab_identity.as_ref(),
                ) {
                    Ok(Some(viewer_identity)) => viewer_identity,
                    Ok(None) => {
                        return Err(fail_transfer(
                            io,
                            &receipt_dest,
                            &mut receipt,
                            TransferStep::ConfirmBucketTab,
                            format!(
                                "tab '{}' did not appear in session '{}'",
                                viewer_tab_name,
                                bucket.session_name()
                            ),
                            current_origin_tab_state,
                        ));
                    },
                    Err(message) => {
                        return Err(fail_transfer(
                            io,
                            &receipt_dest,
                            &mut receipt,
                            TransferStep::ConfirmBucketTab,
                            message,
                            current_origin_tab_state,
                        ));
                    },
                }
            },
        };
    receipt.viewer_tab_identity = Some(confirmed_viewer_identity);
    receipt.viewer_creation_pending = false;
    receipt.viewer_confirmed = true;
    persist_receipt(io, &receipt_dest, &mut receipt)?;

    // Revalidate even after a prior receipt said Closed. A server can restore
    // serialized tabs after a crash; a stale Closed bit must never turn that
    // resurrection into a false-success.
    let captured_identity = receipt
        .capture
        .as_ref()
        .and_then(|capture| capture.origin_tab_identity.clone());
    let rebound_identity = match captured_identity.as_ref() {
        Some(captured) => {
            match io.rebind_origin_tab_identity(&run.origin_session, &run.origin_tab, captured) {
                Ok(identity) => identity,
                Err(error) => {
                    return Err(fail_transfer(
                        io,
                        &receipt_dest,
                        &mut receipt,
                        TransferStep::CloseOriginTab,
                        error.message,
                        error.origin_tab_state,
                    ));
                },
            }
        },
        None => None,
    };
    if let Some(rebound_identity) = rebound_identity.as_ref()
        && captured_identity.as_ref() != Some(rebound_identity)
    {
        if let Some(capture) = receipt.capture.as_mut() {
            capture.origin_tab_identity = Some(rebound_identity.clone());
        }
        // Commit the rebind before close. A crash after the close must never
        // make a retry target a same-name successor through stale numeric IDs.
        persist_receipt(io, &receipt_dest, &mut receipt)?;
    }
    let identity = rebound_identity.as_ref().or(captured_identity.as_ref());
    match io.close_origin_tab(&run.origin_session, &run.origin_tab, identity) {
        Ok(()) => receipt.origin_tab_state = OriginTabState::Closed,
        Err(error) => {
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::CloseOriginTab,
                error.message,
                error.origin_tab_state,
            ));
        },
    }
    persist_receipt(io, &receipt_dest, &mut receipt)?;

    Ok(TransferReport {
        run: run.run.clone(),
        bucket,
        scrollback,
        meta: meta_dest,
        origin_tab_closed: true,
        receipt: receipt_dest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(exit_code: i32) -> FinishedRun {
        FinishedRun {
            run: "impl-260720-120000-01000".to_owned(),
            exit_code,
            origin_session: "Operator".to_owned(),
            origin_tab: "impl-260720-120000-01000".to_owned(),
            pane_id: Some("terminal_3".to_owned()),
            runtime_transcript: None,
            command: vec!["claude".to_owned(), "--resume".to_owned()],
            cwd: Some(PathBuf::from("/repo")),
            bucket_verdict: None,
        }
    }

    fn with_verdict(exit_code: i32, verdict: BucketKind) -> FinishedRun {
        FinishedRun {
            bucket_verdict: Some(verdict),
            ..finished(exit_code)
        }
    }

    #[derive(Default)]
    struct FakeIo {
        calls: Vec<&'static str>,
        fail_at: Option<TransferStep>,
        tab_appears: bool,
        visible_tab_name: Option<String>,
        open_materializes_tab: bool,
        captured_tab: Option<String>,
        close_error_state: OriginTabState,
        closed_identity: Option<OriginTabIdentity>,
        receipt: Option<TransferReceipt>,
    }

    impl FakeIo {
        fn healthy() -> Self {
            FakeIo {
                open_materializes_tab: true,
                ..Default::default()
            }
        }
        fn failing_at(step: TransferStep) -> Self {
            FakeIo {
                fail_at: Some(step),
                open_materializes_tab: true,
                ..Default::default()
            }
        }
        fn failing_close_with_state(origin_tab_state: OriginTabState) -> Self {
            FakeIo {
                fail_at: Some(TransferStep::CloseOriginTab),
                open_materializes_tab: true,
                close_error_state: origin_tab_state,
                ..Default::default()
            }
        }
        fn guard(&mut self, step: TransferStep, name: &'static str) -> Result<(), String> {
            self.calls.push(name);
            if self.fail_at == Some(step) {
                return Err(format!("injected fault at {:?}", step));
            }
            Ok(())
        }
    }

    impl TriageIo for FakeIo {
        fn capture_scrollback(
            &mut self,
            _run_id: &str,
            _session: &str,
            origin_tab: &str,
            _pane_id: Option<&str>,
            _runtime_transcript: Option<&Path>,
            _dest: &Path,
        ) -> Result<CaptureEvidence, String> {
            self.captured_tab = Some(origin_tab.to_owned());
            self.guard(TransferStep::Capture, "capture")?;
            Ok(CaptureEvidence {
                capture_source: CaptureSource::TerminalScrollback,
                source_identity: "Operator/tab:7/pane:3".to_owned(),
                bytes: 42,
                sha256: "fake-sha256".to_owned(),
                origin_tab_identity: Some(OriginTabIdentity {
                    session: "Operator".to_owned(),
                    name: origin_tab.to_owned(),
                    id: 7,
                    session_incarnation: "origin-incarnation".to_owned(),
                    tab_instance_id: "11111111111111111111111111111111".to_owned(),
                }),
            })
        }
        fn load_receipt(&mut self, _path: &Path) -> Result<Option<TransferReceipt>, String> {
            self.calls.push("load");
            if self.fail_at == Some(TransferStep::LoadReceipt) {
                return Err("injected fault at LoadReceipt".to_owned());
            }
            Ok(self.receipt.clone())
        }
        fn write_receipt(&mut self, _path: &Path, receipt: &TransferReceipt) -> Result<(), String> {
            self.calls.push("receipt");
            if self.fail_at == Some(TransferStep::WriteReceipt) {
                return Err("injected fault at WriteReceipt".to_owned());
            }
            self.receipt = Some(receipt.clone());
            Ok(())
        }
        fn write_meta(&mut self, _dest: &Path, _meta: &RunMeta) -> Result<(), String> {
            self.guard(TransferStep::WriteMeta, "meta")
        }
        fn ensure_bucket_session(&mut self, _session: &str) -> Result<(), String> {
            self.guard(TransferStep::EnsureBucketSession, "ensure")
        }
        fn open_bucket_tab(
            &mut self,
            _session: &str,
            tab: &str,
            _meta: &RunMeta,
        ) -> Result<(), String> {
            self.guard(TransferStep::OpenBucketTab, "open")?;
            if self.open_materializes_tab {
                self.tab_appears = true;
                self.visible_tab_name = Some(tab.to_owned());
            }
            Ok(())
        }
        fn bucket_tab_identity(
            &mut self,
            session: &str,
            tab: &str,
            _expected: Option<&OriginTabIdentity>,
        ) -> Result<Option<OriginTabIdentity>, String> {
            self.guard(TransferStep::ConfirmBucketTab, "confirm")?;
            let name_matches = self
                .visible_tab_name
                .as_deref()
                .map(|visible| visible == tab)
                .unwrap_or(true);
            Ok(
                (self.tab_appears && name_matches).then(|| OriginTabIdentity {
                    session: session.to_owned(),
                    name: tab.to_owned(),
                    id: 11,
                    session_incarnation: "viewer-incarnation".to_owned(),
                    tab_instance_id: "22222222222222222222222222222222".to_owned(),
                }),
            )
        }
        fn rebind_origin_tab_identity(
            &mut self,
            _session: &str,
            _tab: &str,
            captured: &OriginTabIdentity,
        ) -> Result<Option<OriginTabIdentity>, CloseOriginError> {
            Ok(Some(captured.clone()))
        }
        fn close_origin_tab(
            &mut self,
            _session: &str,
            _tab: &str,
            identity: Option<&OriginTabIdentity>,
        ) -> Result<(), CloseOriginError> {
            self.calls.push("close");
            self.closed_identity = identity.cloned();
            if self.fail_at == Some(TransferStep::CloseOriginTab) {
                return Err(CloseOriginError {
                    message: "injected fault at CloseOriginTab".to_owned(),
                    origin_tab_state: self.close_error_state,
                });
            }
            Ok(())
        }
    }

    #[test]
    fn the_exit_code_fallback_never_claims_a_clean_failure() {
        assert_eq!(BucketKind::for_exit_code(0), BucketKind::Finalized);
        // Not `Failed`: without report and log signals a non-zero exit is
        // contradictory by ignorance, and the fallback must not fake certainty.
        assert_eq!(BucketKind::for_exit_code(1), BucketKind::NeedsAttention);
        assert_eq!(BucketKind::for_exit_code(137), BucketKind::NeedsAttention);
        assert_eq!(
            BucketKind::for_exit_code(0).session_name(),
            "Finalized runs"
        );
        assert_eq!(
            BucketKind::for_exit_code(2).session_name(),
            "Needs attention"
        );
    }

    #[test]
    fn an_explicit_verdict_overrides_the_exit_code_derivation() {
        // The caller saw the report and the log; we only saw the exit code.
        assert_eq!(
            with_verdict(1, BucketKind::Failed).bucket(),
            BucketKind::Failed
        );
        // Exit 0 with a missing report is still not a finalized run.
        assert_eq!(
            with_verdict(0, BucketKind::NeedsAttention).bucket(),
            BucketKind::NeedsAttention
        );
        assert_eq!(
            with_verdict(3, BucketKind::Finalized).bucket(),
            BucketKind::Finalized
        );
    }

    #[test]
    fn a_verdictless_run_still_falls_back_to_the_exit_code() {
        // Backward compatibility: the flag-less W2-B-2 caller keeps working.
        assert_eq!(finished(0).bucket(), BucketKind::Finalized);
        assert_eq!(finished(1).bucket(), BucketKind::NeedsAttention);
    }

    #[test]
    fn bucket_verdicts_round_trip_through_their_cli_spelling() {
        for bucket in [
            BucketKind::Finalized,
            BucketKind::Failed,
            BucketKind::NeedsAttention,
        ] {
            assert_eq!(parse_bucket_verdict(bucket.cli_value()), Ok(bucket));
        }
        // A typo must not silently become a bucket.
        assert!(parse_bucket_verdict("needs_attention").is_err());
        assert!(parse_bucket_verdict("").is_err());
    }

    #[test]
    fn bucket_sessions_are_recognised_by_name() {
        assert_eq!(
            BucketKind::from_session_name("Finalized runs"),
            Some(BucketKind::Finalized)
        );
        assert_eq!(
            BucketKind::from_session_name("Failed runs"),
            Some(BucketKind::Failed)
        );
        assert_eq!(
            BucketKind::from_session_name("Needs attention"),
            Some(BucketKind::NeedsAttention)
        );
        assert_eq!(BucketKind::from_session_name("Operator"), None);
    }

    /// Regression, 2026-07-25: the rail's f/x/n counters went blind during the
    /// day while `finished_runs/` kept filling up. Cause: the idle-exit
    /// watchdog reaped every triage drawer 900s after its last transfer,
    /// because a drawer has zero clients by construction. Once the drawer's
    /// server is gone it leaves the live `SessionUpdate` list, the rail finds
    /// no session, and the count renders 0.
    #[test]
    fn the_idle_watchdog_never_reaps_a_triage_drawer() {
        for bucket in [
            BucketKind::Finalized,
            BucketKind::Failed,
            BucketKind::NeedsAttention,
        ] {
            assert!(
                !idle_exit_may_reap(Some(bucket.session_name())),
                "drawer '{}' must survive idle-exit — it holds the rail's counter",
                bucket.session_name()
            );
        }
    }

    #[test]
    fn ordinary_and_unnamed_sessions_stay_reapable() {
        // The watchdog exists for abandoned servers; exempting the drawers must
        // not quietly disarm it everywhere else.
        assert!(idle_exit_may_reap(Some("Operator")));
        assert!(idle_exit_may_reap(Some("vc-frame")));
        assert!(idle_exit_may_reap(Some("Finalized")));
        assert!(idle_exit_may_reap(Some("needs attention")));
        assert!(idle_exit_may_reap(None));
    }

    #[test]
    fn a_failed_verdict_transfers_into_its_own_drawer() {
        let mut io = FakeIo::healthy();
        let report = transfer_finished_run(
            &mut io,
            &with_verdict(1, BucketKind::Failed),
            Path::new("/cp"),
            1_753_000_000,
        )
        .unwrap();

        assert_eq!(report.bucket, BucketKind::Failed);
        assert_eq!(report.bucket.session_name(), "Failed runs");
    }

    #[test]
    fn capture_paths_are_namespaced_per_run() {
        let root = PathBuf::from("/cp");
        assert_eq!(
            scrollback_path(&root, "run-1"),
            PathBuf::from("/cp/finished_runs/run-1/scrollback.txt")
        );
        assert_eq!(
            meta_path(&root, "run-1"),
            PathBuf::from("/cp/finished_runs/run-1/meta.json")
        );
    }

    #[test]
    fn happy_path_captures_before_it_closes() {
        let mut io = FakeIo::healthy();
        let report =
            transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 1_753_000_000).unwrap();

        assert_eq!(
            io.calls,
            vec![
                "load", "capture", "receipt", "meta", "receipt", "ensure", "confirm", "receipt",
                "open", "confirm", "receipt", "close", "receipt"
            ]
        );
        assert_eq!(report.bucket, BucketKind::Finalized);
        assert!(report.origin_tab_closed);
    }

    #[test]
    fn failing_run_lands_in_needs_attention() {
        let mut io = FakeIo::healthy();
        let report =
            transfer_finished_run(&mut io, &finished(1), Path::new("/cp"), 1_753_000_000).unwrap();
        assert_eq!(report.bucket, BucketKind::NeedsAttention);
    }

    #[test]
    fn a_fault_at_any_step_leaves_the_origin_tab_open() {
        for step in [
            TransferStep::Capture,
            TransferStep::WriteReceipt,
            TransferStep::WriteMeta,
            TransferStep::EnsureBucketSession,
            TransferStep::OpenBucketTab,
            TransferStep::ConfirmBucketTab,
            TransferStep::CloseOriginTab,
        ] {
            let mut io = FakeIo::failing_at(step);
            let error =
                transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();

            assert_eq!(error.step, step, "reported the wrong failing step");
            assert!(
                error.origin_tab_state == OriginTabState::Preserved,
                "origin tab must survive a fault at {:?}",
                step
            );
            assert!(
                !io.calls.contains(&"close") || step == TransferStep::CloseOriginTab,
                "closed the origin tab despite failing at {:?}",
                step
            );
        }
    }

    #[test]
    fn a_bucket_session_killed_mid_transfer_does_not_cost_the_origin_tab() {
        // open() succeeds, but the target session dies before we read it back.
        let mut io = FakeIo {
            open_materializes_tab: false,
            ..FakeIo::healthy()
        };
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();

        assert_eq!(error.step, TransferStep::ConfirmBucketTab);
        assert_eq!(error.origin_tab_state, OriginTabState::Preserved);
        assert!(error.capture_is_durable, "capture landed before the move");
        assert!(!io.calls.contains(&"close"));
    }

    #[test]
    fn an_unowned_preexisting_viewer_never_authorizes_origin_close() {
        let mut io = FakeIo {
            tab_appears: true,
            open_materializes_tab: true,
            ..Default::default()
        };
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();

        assert_eq!(error.step, TransferStep::ConfirmBucketTab);
        assert!(error.message.contains("unowned viewer tab"));
        assert!(!io.calls.contains(&"close"));
    }

    #[test]
    fn a_pending_creation_receipt_can_adopt_the_single_created_viewer() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        assert!(
            first_process
                .receipt
                .as_ref()
                .unwrap()
                .viewer_creation_pending
        );
        let viewer_tab_name = first_process.receipt.as_ref().unwrap().viewer_tab_name();

        let mut second_process = FakeIo {
            receipt: first_process.receipt,
            tab_appears: true,
            visible_tab_name: Some(viewer_tab_name.clone()),
            open_materializes_tab: true,
            ..Default::default()
        };
        transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap();

        let receipt = second_process.receipt.unwrap();
        assert_eq!(
            receipt.viewer_tab_identity,
            Some(OriginTabIdentity {
                session: BucketKind::Finalized.session_name().to_owned(),
                name: viewer_tab_name,
                id: 11,
                session_incarnation: "viewer-incarnation".to_owned(),
                tab_instance_id: "22222222222222222222222222222222".to_owned(),
            })
        );
        assert!(!receipt.viewer_creation_pending);
        assert!(receipt.viewer_confirmed);
        assert!(!second_process.calls.contains(&"open"));
    }

    #[test]
    fn a_pending_receipt_never_adopts_a_plain_foreign_run_tab() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();

        let mut second_process = FakeIo {
            receipt: first_process.receipt,
            tab_appears: true,
            visible_tab_name: Some(run.run.clone()),
            open_materializes_tab: true,
            ..Default::default()
        };
        transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap();

        assert!(
            second_process.calls.contains(&"open"),
            "the nonce-qualified viewer must be created instead of adopting the plain run tab"
        );
        assert_ne!(
            second_process
                .receipt
                .as_ref()
                .unwrap()
                .viewer_tab_identity
                .as_ref()
                .unwrap()
                .name,
            run.run
        );
    }

    #[test]
    fn a_close_cleanup_failure_reports_when_the_origin_is_already_gone() {
        let mut io = FakeIo::failing_close_with_state(OriginTabState::Closed);
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();

        assert_eq!(error.step, TransferStep::CloseOriginTab);
        assert_eq!(error.origin_tab_state, OriginTabState::Closed);
        assert!(error.capture_is_durable);
    }

    #[test]
    fn capture_is_only_durable_once_both_files_are_written() {
        let mut io = FakeIo::failing_at(TransferStep::WriteMeta);
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();
        assert!(!error.capture_is_durable);

        let mut io = FakeIo::failing_at(TransferStep::EnsureBucketSession);
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();
        assert!(error.capture_is_durable);
    }

    /// Regression, 2026-07-25: dispatched runs carried a foreign pane id ("1",
    /// the operator's pane), the dump aimed at it found nothing, and the tabs
    /// never reached their buckets. The tab name is the durable address of the
    /// run's scrollback, so capture must always receive it for the fallback.
    #[test]
    fn capture_is_aimed_at_the_runs_own_tab() {
        let mut io = FakeIo::healthy();
        transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap();
        assert_eq!(io.captured_tab.as_deref(), Some("impl-260720-120000-01000"));
    }

    #[test]
    fn a_new_process_resumes_with_the_durable_origin_identity() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::EnsureBucketSession);
        let first_error =
            transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        assert_eq!(first_error.step, TransferStep::EnsureBucketSession);

        let scrollback = scrollback_path(root.path(), &run.run);
        let metadata = meta_path(root.path(), &run.run);
        std::fs::create_dir_all(scrollback.parent().unwrap()).unwrap();
        std::fs::write(&scrollback, vec![b'x'; 42]).unwrap();
        let mut durable_receipt = first_process.receipt.clone().unwrap();
        let capture = durable_receipt.capture.as_mut().unwrap();
        capture.sha256 = capture_sha256(&scrollback).unwrap();
        let durable_meta = RunMeta::new(&run, 1, capture);
        std::fs::write(&metadata, serde_json::to_vec_pretty(&durable_meta).unwrap()).unwrap();

        let mut second_process = FakeIo {
            receipt: Some(durable_receipt),
            open_materializes_tab: true,
            ..Default::default()
        };
        transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap();

        assert!(
            !second_process.calls.contains(&"capture"),
            "a committed capture must not be repeated on resume"
        );
        assert_eq!(
            second_process.closed_identity,
            Some(OriginTabIdentity {
                session: run.origin_session,
                name: run.origin_tab,
                id: 7,
                session_incarnation: "origin-incarnation".to_owned(),
                tab_instance_id: "11111111111111111111111111111111".to_owned(),
            })
        );
    }

    #[test]
    fn a_receipt_from_another_origin_is_rejected_before_reuse() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::EnsureBucketSession);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();

        let mut foreign_receipt = first_process.receipt.unwrap();
        foreign_receipt.origin_tab = "another-run".to_owned();
        let original_foreign_receipt = foreign_receipt.clone();
        let mut second_process = FakeIo {
            receipt: Some(foreign_receipt),
            open_materializes_tab: true,
            ..Default::default()
        };
        let error = transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap_err();

        assert_eq!(error.step, TransferStep::LoadReceipt);
        assert!(error.message.contains("receipt identity mismatch"));
        assert_eq!(second_process.calls, vec!["load"]);
        assert_eq!(second_process.receipt, Some(original_foreign_receipt));
    }

    #[test]
    fn capture_selectors_are_part_of_the_receipt_identity() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::EnsureBucketSession);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        let durable_receipt = first_process.receipt.unwrap();

        let mut changed_pane = run.clone();
        changed_pane.pane_id = Some("terminal_99".to_owned());
        let mut pane_retry = FakeIo {
            receipt: Some(durable_receipt.clone()),
            ..Default::default()
        };
        let pane_error =
            transfer_finished_run(&mut pane_retry, &changed_pane, root.path(), 2).unwrap_err();
        assert_eq!(pane_error.step, TransferStep::LoadReceipt);

        let mut changed_transcript = run;
        changed_transcript.runtime_transcript = Some(PathBuf::from("/tmp/corrected.jsonl"));
        let mut transcript_retry = FakeIo {
            receipt: Some(durable_receipt),
            ..Default::default()
        };
        let transcript_error =
            transfer_finished_run(&mut transcript_retry, &changed_transcript, root.path(), 2)
                .unwrap_err();
        assert_eq!(transcript_error.step, TransferStep::LoadReceipt);
    }

    #[test]
    fn same_size_scrollback_corruption_invalidates_the_capture() {
        let root = tempfile::tempdir().unwrap();
        let scrollback = root.path().join("scrollback.txt");
        std::fs::write(&scrollback, b"good").unwrap();
        let capture = CaptureEvidence {
            capture_source: CaptureSource::RuntimeTranscript,
            source_identity: "/tmp/runtime.log".to_owned(),
            bytes: 4,
            sha256: capture_sha256(&scrollback).unwrap(),
            origin_tab_identity: None,
        };
        assert!(file_matches_capture(&scrollback, &capture));

        std::fs::write(&scrollback, b"evil").unwrap();
        assert!(!file_matches_capture(&scrollback, &capture));
    }

    #[test]
    fn stale_metadata_is_not_treated_as_committed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("meta.json");
        let capture = CaptureEvidence {
            capture_source: CaptureSource::RuntimeTranscript,
            source_identity: "/tmp/runtime.log".to_owned(),
            bytes: 10,
            sha256: "runtime-sha256".to_owned(),
            origin_tab_identity: None,
        };
        let expected = RunMeta::new(&finished(1), 1_753_000_000, &capture);
        std::fs::write(&path, serde_json::to_vec_pretty(&expected).unwrap()).unwrap();
        assert!(file_matches_meta(&path, &expected));

        let stale = RunMeta {
            origin_tab: "another-run".to_owned(),
            ..expected.clone()
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&stale).unwrap()).unwrap();
        assert!(!file_matches_meta(&path, &expected));
    }

    #[test]
    fn failed_viewer_revalidation_clears_the_old_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_close_with_state(OriginTabState::Preserved);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        assert!(first_process.receipt.as_ref().unwrap().viewer_confirmed);

        let mut second_process = FakeIo {
            receipt: first_process.receipt,
            fail_at: Some(TransferStep::EnsureBucketSession),
            open_materializes_tab: true,
            ..Default::default()
        };
        transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap_err();
        assert!(!second_process.receipt.unwrap().viewer_confirmed);
    }

    #[test]
    fn meta_preserves_the_command_for_rerun() {
        let capture = CaptureEvidence {
            capture_source: CaptureSource::RuntimeTranscript,
            source_identity: "/tmp/runtime.log".to_owned(),
            bytes: 10,
            sha256: "runtime-sha256".to_owned(),
            origin_tab_identity: None,
        };
        let meta = RunMeta::new(&finished(1), 1_753_000_000, &capture);
        assert_eq!(meta.command, vec!["claude", "--resume"]);
        assert_eq!(meta.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(meta.bucket, BucketKind::NeedsAttention);
        assert_eq!(meta.captured_at, 1_753_000_000);
        assert_eq!(meta.capture_source, CaptureSource::RuntimeTranscript);
        assert_eq!(meta.capture_source_identity, "/tmp/runtime.log");
        assert_eq!(meta.capture_sha256, "runtime-sha256");
    }
}
