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

use kdl::KdlDocument;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::input::layout::Layout;

/// Canonical bucket session for runs that finished cleanly.
pub const FINALIZED_RUNS_SESSION: &str = "Finalized runs";
/// Canonical bucket session for runs that failed cleanly — the failure is
/// understood, there is nothing to investigate.
pub const FAILED_RUNS_SESSION: &str = "Failed runs";
/// Canonical bucket session for runs whose signals disagree, or are missing.
pub const NEEDS_ATTENTION_SESSION: &str = "Needs attention";
const VIEWER_CREATION_FENCE_PREFIX: &str = "vcf1:";
const MAX_VIEWER_CREATION_FENCE_BYTES: usize = 16 * 1024;
const MAX_VIEWER_CREATION_RECEIPT_BYTES: u64 = 1024 * 1024;
const MAX_VIEWER_CREATION_RECEIPT_READ_ATTEMPTS: usize = 3;

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
    /// Monotonic runtime settlement revision. Zero is the legacy/manual path.
    ///
    /// A newer non-zero revision may supersede an already completed drawer
    /// transfer for the same immutable run identity. Older or equal revisions
    /// can only resume the exact same bucket/exit classification.
    pub settlement_revision: u64,
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

    pub fn is_same_live_tab(&self, other: &Self) -> bool {
        self.is_same_durable_tab(other)
            && self.id == other.id
            && !self.session_incarnation.is_empty()
            && self.session_incarnation == other.session_incarnation
    }
}

fn matches_viewer_reservation(
    identity: &OriginTabIdentity,
    session: &str,
    tab: &str,
    token: &str,
) -> bool {
    identity.is_typed()
        && identity.session == session
        && identity.name == tab
        && identity.tab_instance_id == token
}

fn is_viewer_reservation(identity: &OriginTabIdentity) -> bool {
    !identity.session.is_empty()
        && !identity.name.is_empty()
        && identity.tab_instance_id.len() == 32
        && identity
            .tab_instance_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
}

/// Prove that a saved resurrection layout cannot recreate `viewer`.
///
/// Both the stable tab instance and the owned tab name are checked. Older
/// layouts may not contain `vc_tab_instance_id`; accepting a same-name legacy
/// tab would therefore recreate the very viewer this tombstone protects.
pub fn verify_saved_layout_excludes_viewer(
    contents: &str,
    source_name: &str,
    viewer: &OriginTabIdentity,
) -> Result<(), String> {
    if !is_viewer_reservation(viewer) {
        return Err(format!(
            "cannot verify an invalid viewer reservation for '{}'",
            viewer.name
        ));
    }
    let document = contents
        .parse::<KdlDocument>()
        .map_err(|error| format!("cannot parse saved resurrection layout: {}", error))?;
    let parsed_layout = Layout::from_kdl(contents, Some(source_name.to_owned()), None, None)
        .map_err(|error| format!("saved resurrection layout is not loadable: {}", error))?;
    if parsed_layout.tabs.iter().any(|(name, tiled, _floating)| {
        name.as_deref() == Some(viewer.name.as_str())
            || tiled.tab_instance_id.as_deref() == Some(viewer.tab_instance_id.as_str())
    }) {
        return Err(format!(
            "saved resurrection layout still contains superseded viewer '{}'",
            viewer.name
        ));
    }

    fn contains_viewer(document: &KdlDocument, viewer: &OriginTabIdentity) -> bool {
        document.nodes().iter().any(|node| {
            let is_protected_tab = node.name().value() == "tab"
                && (node
                    .get("vc_tab_instance_id")
                    .and_then(|entry| entry.value().as_string())
                    == Some(viewer.tab_instance_id.as_str())
                    || node.get("name").and_then(|entry| entry.value().as_string())
                        == Some(viewer.name.as_str()));
            is_protected_tab
                || node
                    .children()
                    .is_some_and(|children| contains_viewer(children, viewer))
        })
    }

    if contains_viewer(&document, viewer) {
        return Err(format!(
            "saved resurrection layout still contains superseded viewer '{}'",
            viewer.name
        ));
    }
    Ok(())
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
    CloseSupersededViewer,
    SaveBucketSession,
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
    #[serde(default)]
    pub settlement_revision: u64,
    pub capture: Option<CaptureEvidence>,
    pub capture_committed: bool,
    pub metadata_committed: bool,
    pub viewer_confirmed: bool,
    #[serde(default)]
    pub viewer_tab_identity: Option<OriginTabIdentity>,
    #[serde(default)]
    pub viewer_creation_pending: bool,
    #[serde(default)]
    pub viewer_creation_generation: u64,
    #[serde(default)]
    pub viewer_token: String,
    #[serde(default)]
    pub superseded_viewers: Vec<SupersededViewer>,
    pub origin_tab_state: OriginTabState,
    pub fault: Option<String>,
    pub updated_at: u64,
}

/// Durable owner fence carried by a triage viewer's one-shot layout request.
///
/// The wire representation temporarily occupies the internal
/// `vc_tab_instance_id` layout property. The server must decode and validate
/// it before allocating anything, then replace it with the bare durable token
/// so the envelope can never enter public identity or saved-layout state.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewerCreationFence {
    pub receipt_path: PathBuf,
    pub run_id: String,
    pub generation: u64,
    pub viewer_token: String,
    pub bucket: BucketKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerCreationFenceRejection {
    /// The receipt still owns the same durable tab, but this particular writer
    /// is no longer current. Its resources must be discarded without closing
    /// the stable tab that a newer same-token retry is healing.
    SupersededSameViewer,
    /// The receipt no longer owns this token/bucket/tab, or cannot be proven.
    /// A locally-current old writer must retire its exact tab incarnation.
    OwnershipLost,
}

impl ViewerCreationFenceRejection {
    pub fn should_close_exact_tab(self) -> bool {
        matches!(self, Self::OwnershipLost)
    }
}

impl std::fmt::Display for ViewerCreationFenceRejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SupersededSameViewer => {
                formatter.write_str("viewer creation fence was superseded for the same viewer")
            },
            Self::OwnershipLost => formatter.write_str("viewer creation fence lost ownership"),
        }
    }
}

impl std::fmt::Debug for ViewerCreationFence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ViewerCreationFence")
            .field("receipt_path", &"<redacted>")
            .field("run_id", &self.run_id)
            .field("generation", &self.generation)
            .field("viewer_token", &self.viewer_token)
            .field("bucket", &self.bucket)
            .finish()
    }
}

impl ViewerCreationFence {
    pub fn new(receipt_path: PathBuf, receipt: &TransferReceipt) -> Self {
        Self {
            receipt_path,
            run_id: receipt.run.clone(),
            generation: receipt.viewer_creation_generation,
            viewer_token: receipt.viewer_token.clone(),
            bucket: receipt.bucket,
        }
    }

    pub fn canonicalized(mut self) -> Result<Self, String> {
        self.receipt_path = canonicalize_bound_receipt_path(&self.receipt_path)?;
        Ok(self)
    }

    pub fn wire_tab_instance_id(&self) -> Result<String, String> {
        let encoded = serde_json::to_string(self)
            .map_err(|_| "viewer creation fence cannot be encoded".to_owned())?;
        if encoded.len() > MAX_VIEWER_CREATION_FENCE_BYTES {
            return Err("viewer creation fence is oversized".to_owned());
        }
        Ok(format!("{}{}", VIEWER_CREATION_FENCE_PREFIX, encoded))
    }

    pub fn decode_tab_instance_id(
        value: &str,
    ) -> Result<(String, Option<ViewerCreationFence>), String> {
        if value.starts_with("vcf") {
            let encoded = value
                .strip_prefix(VIEWER_CREATION_FENCE_PREFIX)
                .ok_or_else(|| "unknown viewer creation fence version".to_owned())?;
            if encoded.is_empty() || encoded.len() > MAX_VIEWER_CREATION_FENCE_BYTES {
                return Err("viewer creation fence is malformed or oversized".to_owned());
            }
            let fence = serde_json::from_str::<ViewerCreationFence>(encoded)
                .map_err(|_| "viewer creation fence is malformed".to_owned())?;
            if !valid_viewer_token(&fence.viewer_token)
                || fence.run_id.is_empty()
                || fence.generation == 0
            {
                return Err("viewer creation fence has invalid binding fields".to_owned());
            }
            return Ok((fence.viewer_token.to_ascii_lowercase(), Some(fence)));
        }
        Ok((value.to_owned(), None))
    }

    /// Fail closed unless the exact canonical receipt still owns this request.
    pub fn verify_current(&self, session: &str, tab_name: &str) -> Result<(), String> {
        self.verify_for_install(session, tab_name)
            .map_err(|rejection| rejection.to_string())
    }

    pub fn verify_for_install(
        &self,
        session: &str,
        tab_name: &str,
    ) -> Result<(), ViewerCreationFenceRejection> {
        self.verify_for_install_with_reader(session, tab_name, read_bound_receipt)
    }

    fn verify_for_install_with_reader<F>(
        &self,
        session: &str,
        tab_name: &str,
        read_receipt: F,
    ) -> Result<(), ViewerCreationFenceRejection>
    where
        F: FnOnce(&Path) -> Result<Vec<u8>, String>,
    {
        let canonical = self
            .clone()
            .canonicalized()
            .map_err(|_| ViewerCreationFenceRejection::OwnershipLost)?;
        if canonical != *self {
            return Err(ViewerCreationFenceRejection::OwnershipLost);
        }
        let bytes = read_receipt(&self.receipt_path)
            .map_err(|_| ViewerCreationFenceRejection::OwnershipLost)?;
        let receipt = serde_json::from_slice::<TransferReceipt>(&bytes)
            .map_err(|_| ViewerCreationFenceRejection::OwnershipLost)?;
        let same_viewer = receipt.version == 4
            && receipt.run == self.run_id
            && receipt.bucket == self.bucket
            && receipt
                .viewer_token
                .eq_ignore_ascii_case(&self.viewer_token)
            && receipt.bucket.session_name() == session
            && receipt.viewer_tab_name() == tab_name;
        if !same_viewer {
            return Err(ViewerCreationFenceRejection::OwnershipLost);
        }
        if receipt.viewer_creation_generation != self.generation || !receipt.viewer_creation_pending
        {
            return Err(ViewerCreationFenceRejection::SupersededSameViewer);
        }
        Ok(())
    }
}

fn validate_canonical_receipt_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("viewer creation receipt path is not absolute".to_owned());
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|_| "viewer creation receipt is unreadable".to_owned())?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("viewer creation receipt is not a regular file".to_owned());
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|_| "viewer creation receipt cannot be canonicalized".to_owned())?;
    if canonical != path {
        return Err("viewer creation receipt path is not canonical".to_owned());
    }
    Ok(canonical)
}

fn open_receipt_no_follow(path: &Path) -> Result<std::fs::File, String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| "viewer creation receipt cannot be opened safely".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "viewer creation receipt metadata is unreadable".to_owned())?;
    if !metadata.is_file() || metadata.len() > MAX_VIEWER_CREATION_RECEIPT_BYTES {
        return Err("viewer creation receipt is not a bounded regular file".to_owned());
    }
    Ok(file)
}

#[cfg(unix)]
fn same_receipt_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_receipt_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok().is_some()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

#[derive(Debug)]
enum BoundReceiptReadError {
    ReplacedDuringValidation,
    Invalid(String),
}

impl std::fmt::Display for BoundReceiptReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReplacedDuringValidation => {
                formatter.write_str("viewer creation receipt changed during validation")
            },
            Self::Invalid(message) => formatter.write_str(message),
        }
    }
}

fn canonicalize_bound_receipt_path(path: &Path) -> Result<PathBuf, String> {
    for attempt in 0..MAX_VIEWER_CREATION_RECEIPT_READ_ATTEMPTS {
        let canonical =
            validate_canonical_receipt_path(path).map_err(BoundReceiptReadError::Invalid);
        let result = canonical.and_then(|canonical| {
            let file = open_receipt_no_follow(path).map_err(BoundReceiptReadError::Invalid)?;
            let opened_metadata = file.metadata().map_err(|_| {
                BoundReceiptReadError::Invalid(
                    "viewer creation receipt metadata is unreadable".to_owned(),
                )
            })?;
            ensure_path_still_refers_to_open_receipt(path, &opened_metadata)?;
            Ok(canonical)
        });
        match result {
            Err(BoundReceiptReadError::ReplacedDuringValidation)
                if attempt + 1 < MAX_VIEWER_CREATION_RECEIPT_READ_ATTEMPTS =>
            {
                continue;
            },
            result => return result.map_err(|error| error.to_string()),
        }
    }
    unreachable!("bounded viewer creation receipt canonicalization always returns")
}

fn ensure_path_still_refers_to_open_receipt(
    path: &Path,
    opened_metadata: &std::fs::Metadata,
) -> Result<(), BoundReceiptReadError> {
    let path_metadata = std::fs::symlink_metadata(path).map_err(|_| {
        BoundReceiptReadError::Invalid("viewer creation receipt path disappeared".to_owned())
    })?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err(BoundReceiptReadError::Invalid(
            "viewer creation receipt is not a regular file".to_owned(),
        ));
    }
    if !same_receipt_file(opened_metadata, &path_metadata) {
        return Err(BoundReceiptReadError::ReplacedDuringValidation);
    }
    validate_canonical_receipt_path(path).map_err(BoundReceiptReadError::Invalid)?;
    Ok(())
}

fn read_bound_receipt(path: &Path) -> Result<Vec<u8>, String> {
    read_bound_receipt_with_retries(path, |_| {})
}

fn read_bound_receipt_with_retries<F>(path: &Path, mut after_open: F) -> Result<Vec<u8>, String>
where
    F: FnMut(usize),
{
    for attempt in 0..MAX_VIEWER_CREATION_RECEIPT_READ_ATTEMPTS {
        match read_bound_receipt_once_with(path, || after_open(attempt)) {
            Err(BoundReceiptReadError::ReplacedDuringValidation)
                if attempt + 1 < MAX_VIEWER_CREATION_RECEIPT_READ_ATTEMPTS =>
            {
                continue;
            },
            result => return result.map_err(|error| error.to_string()),
        }
    }
    unreachable!("bounded viewer creation receipt read always returns from the attempt loop")
}

#[cfg(test)]
fn read_bound_receipt_with<F>(path: &Path, after_open: F) -> Result<Vec<u8>, String>
where
    F: FnOnce(),
{
    read_bound_receipt_once_with(path, after_open).map_err(|error| error.to_string())
}

fn read_bound_receipt_once_with<F>(
    path: &Path,
    after_open: F,
) -> Result<Vec<u8>, BoundReceiptReadError>
where
    F: FnOnce(),
{
    validate_canonical_receipt_path(path).map_err(BoundReceiptReadError::Invalid)?;
    let mut file = open_receipt_no_follow(path).map_err(BoundReceiptReadError::Invalid)?;
    let opened_metadata = file.metadata().map_err(|_| {
        BoundReceiptReadError::Invalid("viewer creation receipt metadata is unreadable".to_owned())
    })?;
    after_open();
    ensure_path_still_refers_to_open_receipt(path, &opened_metadata)?;
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    (&mut file)
        .take(MAX_VIEWER_CREATION_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            BoundReceiptReadError::Invalid("viewer creation receipt is unreadable".to_owned())
        })?;
    if bytes.len() as u64 > MAX_VIEWER_CREATION_RECEIPT_BYTES {
        return Err(BoundReceiptReadError::Invalid(
            "viewer creation receipt is oversized".to_owned(),
        ));
    }
    ensure_path_still_refers_to_open_receipt(path, &opened_metadata)?;
    Ok(bytes)
}

fn valid_viewer_token(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Exact old viewer retained while a newer settlement is being materialized.
///
/// The new drawer is confirmed first. Only then may this typed incarnation be
/// closed, which keeps every crash point recoverable without double-counting
/// forever or guessing at a same-name replacement tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupersededViewer {
    pub bucket: BucketKind,
    pub tab: String,
    pub identity: OriginTabIdentity,
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
            settlement_revision: run.settlement_revision,
            capture: None,
            capture_committed: false,
            metadata_committed: false,
            viewer_confirmed: false,
            viewer_tab_identity: None,
            viewer_creation_pending: false,
            viewer_creation_generation: 0,
            viewer_token: Uuid::new_v4().simple().to_string(),
            superseded_viewers: Vec::new(),
            origin_tab_state: OriginTabState::Preserved,
            fault: None,
            updated_at: captured_at,
        }
    }

    pub fn viewer_tab_name(&self) -> String {
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
    /// Flush the current bucket layout and prove the retired viewer cannot be
    /// resurrected from the saved cache.
    fn save_bucket_session(
        &mut self,
        session: &str,
        retired_viewer: &OriginTabIdentity,
    ) -> Result<(), String>;
    /// Open a tab named `tab` in `session`, showing the dump and offering a
    /// suspended rerun of the original command.
    fn open_bucket_tab(
        &mut self,
        session: &str,
        tab: &str,
        tab_instance_id: &str,
        fence: &ViewerCreationFence,
        meta: &RunMeta,
    ) -> Result<(), String>;
    /// Re-apply the viewer layout to the exact pending tab reservation. The
    /// generation is durably incremented before dispatch so a retry can fence
    /// off every older in-flight layout writer.
    fn recover_bucket_tab(
        &mut self,
        session: &str,
        tab: &str,
        identity: &OriginTabIdentity,
        fence: &ViewerCreationFence,
        meta: &RunMeta,
    ) -> Result<(), String>;
    /// Read back the target session and return the unique durable identity of
    /// `tab`, including the server incarnation that owns its stable ID.
    fn bucket_tab_identity(
        &mut self,
        session: &str,
        tab: &str,
        expected: Option<&OriginTabIdentity>,
    ) -> Result<Option<OriginTabIdentity>, String>;
    /// Prove the reserved viewer has materialized its runtime layout, not only
    /// its empty preallocated tab identity.
    fn bucket_tab_ready(
        &mut self,
        session: &str,
        identity: &OriginTabIdentity,
    ) -> Result<bool, String>;
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

fn advance_viewer_creation_generation(receipt: &mut TransferReceipt) -> Result<(), TransferError> {
    receipt.viewer_creation_generation = receipt
        .viewer_creation_generation
        .checked_add(1)
        .ok_or_else(|| TransferError {
            step: TransferStep::WriteReceipt,
            message: "viewer creation generation overflow".to_owned(),
            origin_tab_state: receipt.origin_tab_state,
            capture_is_durable: receipt.capture_committed && receipt.metadata_committed,
        })?;
    Ok(())
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

    let immutable_identity_matches = receipt.version == 4
        && receipt.run == run.run
        && receipt.origin_session == run.origin_session
        && receipt.origin_tab == run.origin_tab
        && receipt.command == run.command
        && receipt.cwd == run.cwd
        && receipt.pane_id == run.pane_id
        && receipt.runtime_transcript == run.runtime_transcript
        && receipt.viewer_token.len() == 32
        && receipt
            .viewer_token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit());
    let classification_changed = receipt.bucket != bucket || receipt.exit_code != run.exit_code;
    let stale_revision = receipt.settlement_revision > run.settlement_revision;
    let unversioned_reclassification = classification_changed
        && (run.settlement_revision == 0 || run.settlement_revision <= receipt.settlement_revision);
    if !immutable_identity_matches || stale_revision || unversioned_reclassification {
        let message = format!(
            "receipt identity mismatch: stored v{} run '{}' revision {} in {:?} at {}/{}, requested v4 '{}' revision {} in {:?} at {}/{}",
            receipt.version,
            receipt.run,
            receipt.settlement_revision,
            receipt.bucket,
            receipt.origin_session,
            receipt.origin_tab,
            run.run,
            run.settlement_revision,
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

    if run.settlement_revision > receipt.settlement_revision {
        if classification_changed {
            let old_viewer_tab = receipt.viewer_tab_name();
            let old_viewer_identity = match receipt.viewer_tab_identity.clone() {
                Some(identity) => Some(identity),
                None if receipt.viewer_creation_pending || receipt.viewer_confirmed => {
                    Some(OriginTabIdentity {
                        session: receipt.bucket.session_name().to_owned(),
                        name: old_viewer_tab.clone(),
                        id: 0,
                        session_incarnation: String::new(),
                        tab_instance_id: receipt.viewer_token.clone(),
                    })
                },
                None => None,
            };
            if let Some(identity) = old_viewer_identity {
                if !is_viewer_reservation(&identity)
                    || identity.session != receipt.bucket.session_name()
                    || identity.name != old_viewer_tab
                    || identity.tab_instance_id != receipt.viewer_token
                {
                    return Err(TransferError {
                        step: TransferStep::ConfirmBucketTab,
                        message: format!(
                            "cannot supersede viewer '{}' because its durable instance does not match the receipt reservation",
                            old_viewer_tab,
                        ),
                        origin_tab_state: receipt.origin_tab_state,
                        capture_is_durable: receipt.capture_committed && receipt.metadata_committed,
                    });
                }
                if !receipt
                    .superseded_viewers
                    .iter()
                    .any(|viewer| viewer.identity.is_same_durable_tab(&identity))
                {
                    receipt.superseded_viewers.push(SupersededViewer {
                        bucket: receipt.bucket,
                        tab: old_viewer_tab,
                        identity,
                    });
                }
            }
            receipt.bucket = bucket;
            receipt.exit_code = run.exit_code;
            receipt.metadata_committed = false;
            receipt.viewer_confirmed = false;
            receipt.viewer_tab_identity = None;
            receipt.viewer_creation_pending = false;
            // Persist a new generation and the old token's tombstone before
            // touching either drawer. This closes both possible orderings:
            // an old request sees the tombstone before install, or it installs
            // first and the durable superseded-viewer retirement finds it.
            advance_viewer_creation_generation(&mut receipt)?;
            receipt.viewer_token = Uuid::new_v4().simple().to_string();
        }
        receipt.settlement_revision = run.settlement_revision;
        persist_receipt(io, &receipt_dest, &mut receipt)?;
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
    let reserved_viewer_identity = OriginTabIdentity {
        session: bucket.session_name().to_owned(),
        name: viewer_tab_name.clone(),
        id: 0,
        session_incarnation: String::new(),
        tab_instance_id: receipt.viewer_token.clone(),
    };
    let expected_viewer_identity = receipt.viewer_tab_identity.as_ref().or_else(|| {
        receipt
            .viewer_creation_pending
            .then_some(&reserved_viewer_identity)
    });
    let current_viewer_identity = match io.bucket_tab_identity(
        bucket.session_name(),
        &viewer_tab_name,
        expected_viewer_identity,
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
    let confirmed_viewer_identity = match (
        current_viewer_identity,
        receipt.viewer_tab_identity.clone(),
    ) {
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
        (Some(current), None)
            if receipt.viewer_creation_pending
                && matches_viewer_reservation(
                    &current,
                    bucket.session_name(),
                    &viewer_tab_name,
                    &receipt.viewer_token,
                ) =>
        {
            current
        },
        (Some(current), None) if receipt.viewer_creation_pending => {
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::ConfirmBucketTab,
                format!(
                    "pending viewer tab '{}' does not match reserved durable instance '{}': {:?}",
                    viewer_tab_name, reserved_viewer_identity.tab_instance_id, current
                ),
                current_origin_tab_state,
            ));
        },
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
            advance_viewer_creation_generation(&mut receipt)?;
            persist_receipt(io, &receipt_dest, &mut receipt)?;
            if let Err(message) = io.open_bucket_tab(
                bucket.session_name(),
                &viewer_tab_name,
                &receipt.viewer_token,
                &ViewerCreationFence::new(receipt_dest.clone(), &receipt),
                &meta,
            ) {
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
                Some(&reserved_viewer_identity),
            ) {
                Ok(Some(viewer_identity))
                    if matches_viewer_reservation(
                        &viewer_identity,
                        bucket.session_name(),
                        &viewer_tab_name,
                        &receipt.viewer_token,
                    ) =>
                {
                    viewer_identity
                },
                Ok(Some(viewer_identity)) => {
                    return Err(fail_transfer(
                        io,
                        &receipt_dest,
                        &mut receipt,
                        TransferStep::ConfirmBucketTab,
                        format!(
                            "created viewer tab '{}' does not carry reserved durable instance '{}': {:?}",
                            viewer_tab_name,
                            reserved_viewer_identity.tab_instance_id,
                            viewer_identity
                        ),
                        current_origin_tab_state,
                    ));
                },
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
    let mut confirmed_viewer_identity = confirmed_viewer_identity;
    let viewer_is_ready =
        match io.bucket_tab_ready(bucket.session_name(), &confirmed_viewer_identity) {
            Ok(is_ready) => is_ready,
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
    if !viewer_is_ready {
        // The exact durable tab exists but NewTab died between reserving it and
        // applying its panes. Persist both the concrete live identity and the
        // next generation before asking the server to re-drive only this tab.
        receipt.viewer_tab_identity = Some(confirmed_viewer_identity.clone());
        receipt.viewer_creation_pending = true;
        advance_viewer_creation_generation(&mut receipt)?;
        persist_receipt(io, &receipt_dest, &mut receipt)?;
        if let Err(message) = io.recover_bucket_tab(
            bucket.session_name(),
            &viewer_tab_name,
            &confirmed_viewer_identity,
            &ViewerCreationFence::new(receipt_dest.clone(), &receipt),
            &meta,
        ) {
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::OpenBucketTab,
                message,
                current_origin_tab_state,
            ));
        }
        let recovered_identity = match io.bucket_tab_identity(
            bucket.session_name(),
            &viewer_tab_name,
            Some(&confirmed_viewer_identity),
        ) {
            Ok(Some(identity)) if identity.is_same_live_tab(&confirmed_viewer_identity) => identity,
            Ok(Some(identity)) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::ConfirmBucketTab,
                    format!(
                        "viewer recovery changed the exact pending identity: expected {:?}, current {:?}",
                        confirmed_viewer_identity, identity
                    ),
                    current_origin_tab_state,
                ));
            },
            Ok(None) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::ConfirmBucketTab,
                    format!(
                        "viewer tab '{}' disappeared during exact-layout recovery",
                        viewer_tab_name
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
        };
        match io.bucket_tab_ready(bucket.session_name(), &recovered_identity) {
            Ok(true) => confirmed_viewer_identity = recovered_identity,
            Ok(false) => {
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::ConfirmBucketTab,
                    format!(
                        "viewer tab '{}' recovery completed without a runtime layout",
                        viewer_tab_name
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
    }
    receipt.viewer_tab_identity = Some(confirmed_viewer_identity);
    receipt.viewer_creation_pending = false;
    receipt.viewer_confirmed = true;
    persist_receipt(io, &receipt_dest, &mut receipt)?;

    // A newer settlement may have changed the destination after an earlier
    // viewer was already durable. Keep every old typed incarnation until the
    // replacement is confirmed, then retire them one by one with a receipt
    // commit after each close. A crash anywhere resumes this exact list.
    while let Some(superseded) = receipt.superseded_viewers.first().cloned() {
        let old_session = superseded.bucket.session_name();
        if let Err(message) = io.ensure_bucket_session(old_session) {
            let origin_tab_state = receipt.origin_tab_state;
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::CloseSupersededViewer,
                format!(
                    "cannot resurrect superseded drawer '{}' before close: {}",
                    old_session, message
                ),
                origin_tab_state,
            ));
        }
        let current = match io.bucket_tab_identity(
            old_session,
            &superseded.tab,
            Some(&superseded.identity),
        ) {
            Ok(current) => current,
            Err(message) => {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    message,
                    origin_tab_state,
                ));
            },
        };
        if let Some(identity) = current.as_ref() {
            if !identity.is_same_durable_tab(&superseded.identity) {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    format!(
                        "superseded viewer '{}' changed durable identity",
                        superseded.tab
                    ),
                    origin_tab_state,
                ));
            }
            if let Err(error) = io.close_origin_tab(old_session, &superseded.tab, Some(identity)) {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    error.message,
                    origin_tab_state,
                ));
            }
        }

        // close-tab updates live state, while resurrection layouts normally
        // serialize on a timer. Flush synchronously before deleting the only
        // durable pointer to the old viewer, then prove its exact name absent
        // from that saved live state.
        if let Err(message) = io.save_bucket_session(old_session, &superseded.identity) {
            let origin_tab_state = receipt.origin_tab_state;
            return Err(fail_transfer(
                io,
                &receipt_dest,
                &mut receipt,
                TransferStep::SaveBucketSession,
                format!(
                    "cannot save superseded drawer '{}' after close: {}",
                    old_session, message
                ),
                origin_tab_state,
            ));
        }
        match io.bucket_tab_identity(old_session, &superseded.tab, Some(&superseded.identity)) {
            Ok(None) => {},
            Ok(Some(identity)) if identity.is_same_durable_tab(&superseded.identity) => {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    format!(
                        "superseded viewer '{}' remained after saved close",
                        superseded.tab
                    ),
                    origin_tab_state,
                ));
            },
            Ok(Some(_)) => {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    format!(
                        "superseded viewer '{}' was replaced during close proof",
                        superseded.tab
                    ),
                    origin_tab_state,
                ));
            },
            Err(message) => {
                let origin_tab_state = receipt.origin_tab_state;
                return Err(fail_transfer(
                    io,
                    &receipt_dest,
                    &mut receipt,
                    TransferStep::CloseSupersededViewer,
                    message,
                    origin_tab_state,
                ));
            },
        }
        receipt.superseded_viewers.remove(0);
        persist_receipt(io, &receipt_dest, &mut receipt)?;
    }

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
    use std::collections::HashSet;

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
            settlement_revision: 0,
        }
    }

    fn with_verdict(exit_code: i32, verdict: BucketKind) -> FinishedRun {
        FinishedRun {
            bucket_verdict: Some(verdict),
            ..finished(exit_code)
        }
    }

    fn typed_viewer(name: &str) -> OriginTabIdentity {
        OriginTabIdentity {
            session: FINALIZED_RUNS_SESSION.to_owned(),
            name: name.to_owned(),
            id: 7,
            session_incarnation: "drawer-incarnation".to_owned(),
            tab_instance_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        }
    }

    fn current_fence_fixture() -> (tempfile::TempDir, TransferReceipt, ViewerCreationFence) {
        let directory = tempfile::tempdir().unwrap();
        let receipt_path = directory.path().join("transfer.json");
        let mut receipt = TransferReceipt::new(&finished(0), 1);
        receipt.viewer_creation_generation = 7;
        receipt.viewer_creation_pending = true;
        std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        let canonical_path = std::fs::canonicalize(receipt_path).unwrap();
        let fence = ViewerCreationFence::new(canonical_path, &receipt);
        (directory, receipt, fence)
    }

    #[test]
    fn viewer_creation_fence_wire_roundtrip_preserves_binding_and_raw_tokens() {
        let (_directory, _receipt, fence) = current_fence_fixture();
        let encoded = fence.wire_tab_instance_id().unwrap();
        let (token, decoded) = ViewerCreationFence::decode_tab_instance_id(&encoded).unwrap();

        assert_eq!(token, fence.viewer_token);
        assert_eq!(decoded.as_ref(), Some(&fence));
        assert_eq!(
            ViewerCreationFence::decode_tab_instance_id("0123456789abcdef0123456789abcdef")
                .unwrap(),
            ("0123456789abcdef0123456789abcdef".to_owned(), None)
        );
    }

    #[test]
    fn viewer_creation_fence_rejects_malformed_unknown_and_oversized_envelopes() {
        assert!(ViewerCreationFence::decode_tab_instance_id("vcf2:{}").is_err());
        assert!(ViewerCreationFence::decode_tab_instance_id("vcf1:not-json").is_err());
        let oversized = format!("vcf1:{}", "x".repeat(MAX_VIEWER_CREATION_FENCE_BYTES + 1));
        assert!(ViewerCreationFence::decode_tab_instance_id(&oversized).is_err());
    }

    #[test]
    fn viewer_creation_fence_verifies_only_the_current_pending_receipt() {
        let (_directory, receipt, fence) = current_fence_fixture();
        let tab_name = receipt.viewer_tab_name();
        fence
            .verify_current(receipt.bucket.session_name(), &tab_name)
            .unwrap();

        let mut stale_generation = fence.clone();
        stale_generation.generation += 1;
        assert!(
            stale_generation
                .verify_current(receipt.bucket.session_name(), &tab_name)
                .is_err()
        );

        let mut stale_token = fence.clone();
        stale_token.viewer_token = "ffffffffffffffffffffffffffffffff".to_owned();
        assert!(
            stale_token
                .verify_current(receipt.bucket.session_name(), &tab_name)
                .is_err()
        );
        assert!(
            fence
                .verify_current(FAILED_RUNS_SESSION, &tab_name)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn viewer_creation_fence_rejects_symlink_and_noncanonical_receipt_paths() {
        use std::os::unix::fs::symlink;

        let (directory, receipt, fence) = current_fence_fixture();
        let symlink_path = directory.path().join("receipt-link.json");
        symlink(&fence.receipt_path, &symlink_path).unwrap();
        let symlink_fence = ViewerCreationFence::new(symlink_path, &receipt);
        assert!(symlink_fence.canonicalized().is_err());
        assert!(read_bound_receipt(&directory.path().join("receipt-link.json")).is_err());

        let noncanonical_path = directory.path().join(".").join("transfer.json");
        let noncanonical_fence = ViewerCreationFence::new(noncanonical_path, &receipt);
        assert!(noncanonical_fence.canonicalized().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn viewer_creation_receipt_read_rejects_an_atomic_inode_swap() {
        let (directory, receipt, fence) = current_fence_fixture();
        let replacement = directory.path().join("replacement.json");
        std::fs::write(&replacement, serde_json::to_vec(&receipt).unwrap()).unwrap();

        let error = read_bound_receipt_with(&fence.receipt_path, || {
            std::fs::rename(&replacement, &fence.receipt_path).unwrap();
        })
        .expect_err("a rename after open must not let the old inode authorize the request");

        assert!(error.contains("changed during validation"));
    }

    #[cfg(unix)]
    #[test]
    fn viewer_creation_fence_rereads_atomic_same_viewer_replacement_before_classifying() {
        let (directory, mut replacement_receipt, fence) = current_fence_fixture();
        let tab_name = replacement_receipt.viewer_tab_name();
        replacement_receipt.viewer_creation_generation += 1;
        let replacement = directory.path().join("replacement.json");
        std::fs::write(
            &replacement,
            serde_json::to_vec(&replacement_receipt).unwrap(),
        )
        .unwrap();
        let mut replacement = Some(replacement);

        let rejection = fence
            .verify_for_install_with_reader(
                replacement_receipt.bucket.session_name(),
                &tab_name,
                |receipt_path| {
                    read_bound_receipt_with_retries(receipt_path, |attempt| {
                        if attempt == 0 {
                            std::fs::rename(
                                replacement.take().expect("replacement is consumed once"),
                                receipt_path,
                            )
                            .unwrap();
                        }
                    })
                },
            )
            .expect_err("a newer generation cannot authorize the old writer");

        assert_eq!(
            rejection,
            ViewerCreationFenceRejection::SupersededSameViewer
        );
    }

    #[test]
    fn viewer_creation_fence_keeps_malformed_or_unreadable_receipts_fail_closed() {
        let (_directory, receipt, fence) = current_fence_fixture();
        let tab_name = receipt.viewer_tab_name();
        std::fs::write(&fence.receipt_path, b"{not-json").unwrap();
        assert_eq!(
            fence.verify_for_install(receipt.bucket.session_name(), &tab_name),
            Err(ViewerCreationFenceRejection::OwnershipLost)
        );

        std::fs::remove_file(&fence.receipt_path).unwrap();
        assert_eq!(
            fence.verify_for_install(receipt.bucket.session_name(), &tab_name),
            Err(ViewerCreationFenceRejection::OwnershipLost)
        );
    }

    #[test]
    fn saved_layout_proof_rejects_exact_or_legacy_same_name_viewers() {
        let viewer = typed_viewer("run--rev-1");
        let exact = format!(
            r#"layout {{
    tab name="renamed" vc_tab_instance_id="{}" {{}}
}}"#,
            viewer.tab_instance_id
        );
        assert!(verify_saved_layout_excludes_viewer(&exact, "test-layout.kdl", &viewer).is_err());

        let legacy_same_name = format!(
            r#"layout {{
    tab name="{}" {{}}
}}"#,
            viewer.name
        );
        assert!(
            verify_saved_layout_excludes_viewer(&legacy_same_name, "test-layout.kdl", &viewer)
                .is_err()
        );
    }

    #[test]
    fn saved_layout_proof_accepts_only_a_parseable_layout_without_the_viewer() {
        let viewer = typed_viewer("run--rev-1");
        let clean = r#"layout {
    tab name="another-run" vc_tab_instance_id="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb" {}
}"#;
        verify_saved_layout_excludes_viewer(clean, "test-layout.kdl", &viewer).unwrap();
        assert!(
            verify_saved_layout_excludes_viewer("layout {", "test-layout.kdl", &viewer).is_err()
        );
        assert!(verify_saved_layout_excludes_viewer("foo {}", "test-layout.kdl", &viewer).is_err());
        let root_named_viewer = format!(
            r#"layout name="{}" {{
    pane
}}"#,
            viewer.name
        );
        assert!(
            verify_saved_layout_excludes_viewer(&root_named_viewer, "test-layout.kdl", &viewer)
                .is_err()
        );
    }

    #[test]
    fn saved_layout_proof_accepts_a_preallocated_viewer_reservation() {
        let reservation = OriginTabIdentity {
            session: FINALIZED_RUNS_SESSION.to_owned(),
            name: "run-1 [vc:0123456789abcdef0123456789abcdef]".to_owned(),
            id: 0,
            session_incarnation: String::new(),
            tab_instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
        };
        let clean = r#"layout {
    tab name="operator" {
        pane
    }
}"#;
        verify_saved_layout_excludes_viewer(clean, "test-layout.kdl", &reservation).unwrap();

        let stale = format!(
            r#"layout {{
    tab name="{}" vc_tab_instance_id="{}" {{
        pane
    }}
}}"#,
            reservation.name, reservation.tab_instance_id
        );
        assert!(
            verify_saved_layout_excludes_viewer(&stale, "test-layout.kdl", &reservation).is_err()
        );
    }

    #[derive(Default)]
    struct FakeIo {
        calls: Vec<&'static str>,
        fail_at: Option<TransferStep>,
        offline_sessions: HashSet<String>,
        opened_tab_instance_id: Option<String>,
        tab_appears: bool,
        visible_tab_name: Option<String>,
        open_materializes_tab: bool,
        captured_tab: Option<String>,
        close_error_state: OriginTabState,
        closed_identity: Option<OriginTabIdentity>,
        closed_tab_instances: HashSet<String>,
        resurrect_on_ensure: Option<OriginTabIdentity>,
        receipt: Option<TransferReceipt>,
        viewer_ready: Option<bool>,
        recovered_generation: Option<u64>,
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
        fn ensure_bucket_session(&mut self, session: &str) -> Result<(), String> {
            self.guard(TransferStep::EnsureBucketSession, "ensure")?;
            self.offline_sessions.remove(session);
            if let Some(identity) = self.resurrect_on_ensure.as_ref()
                && identity.session == session
                && !self
                    .closed_tab_instances
                    .contains(&identity.tab_instance_id)
            {
                self.tab_appears = true;
            }
            Ok(())
        }
        fn save_bucket_session(
            &mut self,
            _session: &str,
            _retired_viewer: &OriginTabIdentity,
        ) -> Result<(), String> {
            self.guard(TransferStep::SaveBucketSession, "save")
        }
        fn open_bucket_tab(
            &mut self,
            _session: &str,
            tab: &str,
            tab_instance_id: &str,
            _fence: &ViewerCreationFence,
            _meta: &RunMeta,
        ) -> Result<(), String> {
            self.guard(TransferStep::OpenBucketTab, "open")?;
            if self.open_materializes_tab {
                self.tab_appears = true;
                self.visible_tab_name = Some(tab.to_owned());
                self.opened_tab_instance_id = Some(tab_instance_id.to_owned());
            }
            Ok(())
        }
        fn recover_bucket_tab(
            &mut self,
            _session: &str,
            tab: &str,
            identity: &OriginTabIdentity,
            fence: &ViewerCreationFence,
            _meta: &RunMeta,
        ) -> Result<(), String> {
            self.guard(TransferStep::OpenBucketTab, "recover")?;
            let current = self
                .bucket_tab_identity(&identity.session, tab, Some(identity))?
                .ok_or_else(|| "pending viewer disappeared before recovery".to_owned())?;
            if !current.is_same_live_tab(identity) {
                return Err("pending viewer changed identity before recovery".to_owned());
            }
            self.recovered_generation = Some(fence.generation);
            self.viewer_ready = Some(true);
            Ok(())
        }
        fn bucket_tab_identity(
            &mut self,
            session: &str,
            tab: &str,
            expected: Option<&OriginTabIdentity>,
        ) -> Result<Option<OriginTabIdentity>, String> {
            self.guard(TransferStep::ConfirmBucketTab, "confirm")?;
            if self.offline_sessions.contains(session) {
                return Err(format!("session '{}' is offline", session));
            }
            let candidate = if let Some(identity) = self.resurrect_on_ensure.as_ref()
                && identity.session == session
                && identity.name == tab
                && !self
                    .closed_tab_instances
                    .contains(&identity.tab_instance_id)
            {
                Some(identity.clone())
            } else {
                let name_matches = self
                    .visible_tab_name
                    .as_deref()
                    .map(|visible| visible == tab)
                    .unwrap_or(true);
                (self.tab_appears && name_matches).then(|| OriginTabIdentity {
                    session: session.to_owned(),
                    name: tab.to_owned(),
                    id: 11,
                    session_incarnation: "viewer-incarnation".to_owned(),
                    tab_instance_id: self
                        .opened_tab_instance_id
                        .clone()
                        .unwrap_or_else(|| "22222222222222222222222222222222".to_owned()),
                })
            };
            if let (Some(candidate), Some(expected)) = (candidate.as_ref(), expected)
                && !candidate.is_same_durable_tab(expected)
            {
                return Err(format!(
                    "viewer tab '{}' does not match expected durable instance",
                    tab
                ));
            }
            Ok(candidate)
        }
        fn bucket_tab_ready(
            &mut self,
            _session: &str,
            _identity: &OriginTabIdentity,
        ) -> Result<bool, String> {
            Ok(self.viewer_ready.unwrap_or(self.tab_appears))
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
            session: &str,
            _tab: &str,
            identity: Option<&OriginTabIdentity>,
        ) -> Result<(), CloseOriginError> {
            self.calls.push("close");
            self.closed_identity = identity.cloned();
            if self.fail_at == Some(TransferStep::CloseSupersededViewer)
                && BucketKind::from_session_name(session).is_some()
            {
                return Err(CloseOriginError {
                    message: "injected fault at CloseSupersededViewer".to_owned(),
                    origin_tab_state: OriginTabState::Preserved,
                });
            }
            if self.fail_at == Some(TransferStep::CloseOriginTab) {
                return Err(CloseOriginError {
                    message: "injected fault at CloseOriginTab".to_owned(),
                    origin_tab_state: self.close_error_state,
                });
            }
            if let Some(identity) = identity {
                self.closed_tab_instances
                    .insert(identity.tab_instance_id.clone());
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
        let viewer_token = first_process.receipt.as_ref().unwrap().viewer_token.clone();

        let mut second_process = FakeIo {
            receipt: first_process.receipt,
            tab_appears: true,
            visible_tab_name: Some(viewer_tab_name.clone()),
            opened_tab_instance_id: Some(viewer_token.clone()),
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
                tab_instance_id: viewer_token,
            })
        );
        assert!(!receipt.viewer_creation_pending);
        assert!(receipt.viewer_confirmed);
        assert!(!second_process.calls.contains(&"open"));
    }

    #[test]
    fn a_pending_receipt_recovers_the_exact_empty_preallocated_viewer() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        let receipt = first_process.receipt.unwrap();
        let viewer_tab_name = receipt.viewer_tab_name();
        let viewer_token = receipt.viewer_token.clone();

        let mut second_process = FakeIo {
            receipt: Some(receipt),
            tab_appears: true,
            visible_tab_name: Some(viewer_tab_name),
            opened_tab_instance_id: Some(viewer_token),
            viewer_ready: Some(false),
            ..Default::default()
        };
        transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap();

        assert!(second_process.calls.contains(&"recover"));
        assert!(second_process.calls.contains(&"close"));
        let retained = second_process.receipt.unwrap();
        assert_eq!(retained.viewer_creation_generation, 2);
        assert_eq!(second_process.recovered_generation, Some(2));
        assert!(!retained.viewer_creation_pending);
        assert!(retained.viewer_confirmed);
    }

    #[test]
    fn a_pending_receipt_rejects_a_same_name_foreign_tab_instance() {
        let root = tempfile::tempdir().unwrap();
        let run = finished(0);
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &run, root.path(), 1).unwrap_err();
        let receipt = first_process.receipt.unwrap();
        let viewer_tab_name = receipt.viewer_tab_name();

        let mut second_process = FakeIo {
            receipt: Some(receipt),
            tab_appears: true,
            visible_tab_name: Some(viewer_tab_name),
            opened_tab_instance_id: Some("ffffffffffffffffffffffffffffffff".to_owned()),
            open_materializes_tab: true,
            ..Default::default()
        };
        let error = transfer_finished_run(&mut second_process, &run, root.path(), 2).unwrap_err();

        assert_eq!(error.step, TransferStep::ConfirmBucketTab);
        assert_eq!(error.origin_tab_state, OriginTabState::Preserved);
        assert!(!second_process.calls.contains(&"close"));
        assert!(!second_process.calls.contains(&"open"));
        assert!(
            second_process
                .receipt
                .as_ref()
                .unwrap()
                .viewer_creation_pending
        );
    }

    #[test]
    fn reclassification_persists_the_old_reservation_before_cache_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 1;
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &first_run, root.path(), 1).unwrap_err();
        let pending_receipt = first_process.receipt.unwrap();
        let original_token = pending_receipt.viewer_token.clone();
        assert!(pending_receipt.viewer_creation_pending);

        let mut revised_run = with_verdict(9, BucketKind::Failed);
        revised_run.settlement_revision = 2;
        let mut second_process = FakeIo {
            receipt: Some(pending_receipt),
            fail_at: Some(TransferStep::SaveBucketSession),
            ..FakeIo::healthy()
        };
        let error =
            transfer_finished_run(&mut second_process, &revised_run, root.path(), 2).unwrap_err();

        assert_eq!(error.step, TransferStep::SaveBucketSession);
        let retained = second_process.receipt.unwrap();
        assert_ne!(retained.viewer_token, original_token);
        assert_eq!(retained.bucket, BucketKind::Failed);
        assert_eq!(retained.settlement_revision, 2);
        assert!(retained.viewer_confirmed);
        assert_eq!(retained.superseded_viewers.len(), 1);
        assert_eq!(
            retained.superseded_viewers[0].identity.tab_instance_id,
            original_token
        );
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
    fn a_newer_settlement_rebuckets_and_retires_the_exact_old_viewer() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 1;
        let mut io = FakeIo::healthy();
        transfer_finished_run(&mut io, &first_run, root.path(), 1).unwrap();
        let old_viewer = io
            .receipt
            .as_ref()
            .unwrap()
            .viewer_tab_identity
            .clone()
            .unwrap();
        io.resurrect_on_ensure = Some(old_viewer.clone());

        let mut revised_run = with_verdict(9, BucketKind::Failed);
        revised_run.settlement_revision = 2;
        transfer_finished_run(&mut io, &revised_run, root.path(), 2).unwrap();

        let receipt = io.receipt.unwrap();
        assert_eq!(receipt.bucket, BucketKind::Failed);
        assert_eq!(receipt.exit_code, 9);
        assert_eq!(receipt.settlement_revision, 2);
        assert!(receipt.viewer_confirmed);
        assert!(receipt.superseded_viewers.is_empty());
        assert_eq!(
            receipt.viewer_tab_identity.as_ref().unwrap().session,
            FAILED_RUNS_SESSION
        );
        assert_ne!(
            receipt.viewer_tab_identity.as_ref().unwrap().name,
            old_viewer.name,
            "each revision gets a distinct viewer ownership token"
        );
        assert!(
            io.calls.iter().filter(|call| **call == "close").count() >= 3,
            "old origin, superseded viewer, and idempotent origin close all run"
        );
        assert!(
            io.closed_tab_instances
                .contains(&old_viewer.tab_instance_id),
            "a serialized old drawer must be resurrected and closed exactly"
        );
        assert!(io.calls.contains(&"save"));
    }

    #[test]
    fn reclassification_resurrects_an_offline_pending_drawer_before_identity_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 1;
        let mut first_process = FakeIo::failing_at(TransferStep::OpenBucketTab);
        transfer_finished_run(&mut first_process, &first_run, root.path(), 1).unwrap_err();
        let pending_receipt = first_process.receipt.unwrap();
        assert!(pending_receipt.viewer_creation_pending);
        assert!(pending_receipt.viewer_tab_identity.is_none());
        let reserved_instance_id = pending_receipt.viewer_token.clone();
        let old_viewer = OriginTabIdentity {
            session: FINALIZED_RUNS_SESSION.to_owned(),
            name: pending_receipt.viewer_tab_name(),
            id: 11,
            session_incarnation: "resurrected-drawer".to_owned(),
            tab_instance_id: reserved_instance_id,
        };

        let mut revised_run = with_verdict(9, BucketKind::Failed);
        revised_run.settlement_revision = 2;
        let mut retry = FakeIo {
            receipt: Some(pending_receipt),
            offline_sessions: HashSet::from([FINALIZED_RUNS_SESSION.to_owned()]),
            visible_tab_name: Some(old_viewer.name.clone()),
            resurrect_on_ensure: Some(old_viewer.clone()),
            open_materializes_tab: true,
            ..Default::default()
        };

        transfer_finished_run(&mut retry, &revised_run, root.path(), 2).unwrap();

        let reclassification_receipt = retry
            .calls
            .iter()
            .position(|call| *call == "receipt")
            .unwrap();
        let first_ensure = retry
            .calls
            .iter()
            .position(|call| *call == "ensure")
            .unwrap();
        let first_confirm = retry
            .calls
            .iter()
            .position(|call| *call == "confirm")
            .unwrap();
        assert!(
            reclassification_receipt < first_ensure && first_ensure < first_confirm,
            "the old reservation tombstone must be durable before the offline drawer is queried"
        );
        let receipt = retry.receipt.unwrap();
        assert_eq!(receipt.bucket, BucketKind::Failed);
        assert_eq!(receipt.settlement_revision, 2);
        assert!(receipt.superseded_viewers.is_empty());
        assert!(
            retry
                .closed_tab_instances
                .contains(&old_viewer.tab_instance_id)
        );
    }

    #[test]
    fn an_equal_or_older_settlement_cannot_reclassify_a_receipt() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 4;
        let mut first_process = FakeIo::healthy();
        transfer_finished_run(&mut first_process, &first_run, root.path(), 1).unwrap();
        let original_receipt = first_process.receipt.clone().unwrap();

        for revision in [3, 4] {
            let mut stale_run = with_verdict(9, BucketKind::Failed);
            stale_run.settlement_revision = revision;
            let mut stale_process = FakeIo {
                receipt: Some(original_receipt.clone()),
                ..FakeIo::healthy()
            };
            let error =
                transfer_finished_run(&mut stale_process, &stale_run, root.path(), 2).unwrap_err();
            assert_eq!(error.step, TransferStep::LoadReceipt);
            assert!(error.message.contains("receipt identity mismatch"));
            assert_eq!(stale_process.calls, vec!["load"]);
            assert_eq!(stale_process.receipt, Some(original_receipt.clone()));
        }
    }

    #[test]
    fn a_failed_old_viewer_close_resumes_without_losing_the_new_viewer() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 1;
        let mut io = FakeIo::healthy();
        transfer_finished_run(&mut io, &first_run, root.path(), 1).unwrap();
        io.resurrect_on_ensure = io
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.viewer_tab_identity.clone());

        let mut revised_run = with_verdict(9, BucketKind::Failed);
        revised_run.settlement_revision = 2;
        io.fail_at = Some(TransferStep::CloseSupersededViewer);
        let error = transfer_finished_run(&mut io, &revised_run, root.path(), 2).unwrap_err();
        assert_eq!(error.step, TransferStep::CloseSupersededViewer);
        let interrupted = io.receipt.as_ref().unwrap();
        assert_eq!(interrupted.bucket, BucketKind::Failed);
        assert_eq!(interrupted.settlement_revision, 2);
        assert!(interrupted.viewer_confirmed);
        assert_eq!(interrupted.superseded_viewers.len(), 1);

        io.fail_at = None;
        transfer_finished_run(&mut io, &revised_run, root.path(), 3).unwrap();
        let completed = io.receipt.unwrap();
        assert_eq!(completed.bucket, BucketKind::Failed);
        assert!(completed.viewer_confirmed);
        assert!(completed.superseded_viewers.is_empty());
        assert!(completed.fault.is_none());
    }

    #[test]
    fn a_failed_resurrection_save_keeps_the_old_viewer_tombstone() {
        let root = tempfile::tempdir().unwrap();
        let mut first_run = with_verdict(0, BucketKind::Finalized);
        first_run.settlement_revision = 1;
        let mut io = FakeIo::healthy();
        transfer_finished_run(&mut io, &first_run, root.path(), 1).unwrap();
        io.resurrect_on_ensure = io
            .receipt
            .as_ref()
            .and_then(|receipt| receipt.viewer_tab_identity.clone());

        let mut revised_run = with_verdict(9, BucketKind::Failed);
        revised_run.settlement_revision = 2;
        io.fail_at = Some(TransferStep::SaveBucketSession);
        let error = transfer_finished_run(&mut io, &revised_run, root.path(), 2).unwrap_err();
        assert_eq!(error.step, TransferStep::SaveBucketSession);
        assert_eq!(io.receipt.as_ref().unwrap().superseded_viewers.len(), 1);

        io.fail_at = None;
        transfer_finished_run(&mut io, &revised_run, root.path(), 3).unwrap();
        assert!(io.receipt.unwrap().superseded_viewers.is_empty());
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
