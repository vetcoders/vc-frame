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

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

impl RunMeta {
    pub fn new(run: &FinishedRun, captured_at: u64) -> Self {
        RunMeta {
            run: run.run.clone(),
            exit_code: run.exit_code,
            bucket: run.bucket(),
            origin_session: run.origin_session.clone(),
            origin_tab: run.origin_tab.clone(),
            command: run.command.clone(),
            cwd: run.cwd.clone(),
            captured_at,
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
    Capture,
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
    /// True whenever the origin tab is still open — i.e. no scrollback was lost.
    pub origin_tab_preserved: bool,
    /// True once the scrollback + meta are durable on disk, even if the move
    /// itself failed. The operator can still recover the run by hand.
    pub capture_is_durable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReport {
    pub run: String,
    pub bucket: BucketKind,
    pub scrollback: PathBuf,
    pub meta: PathBuf,
    pub origin_tab_closed: bool,
}

/// The side-effecting surface of a transfer. Split out so the ordering
/// invariant can be tested with a fault injected at any step.
pub trait TriageIo {
    /// Dump the pane's viewport + full scrollback to `dest`.
    fn capture_scrollback(
        &mut self,
        session: &str,
        pane_id: Option<&str>,
        dest: &Path,
    ) -> Result<(), String>;
    fn write_meta(&mut self, dest: &Path, meta: &RunMeta) -> Result<(), String>;
    /// Create the bucket session if it does not exist yet. Idempotent.
    fn ensure_bucket_session(&mut self, session: &str) -> Result<(), String>;
    /// Open a tab named `tab` in `session`, showing the dump and offering a
    /// suspended rerun of the original command.
    fn open_bucket_tab(&mut self, session: &str, tab: &str, meta: &RunMeta) -> Result<(), String>;
    /// Read back the target session and report whether `tab` is really there.
    fn bucket_tab_exists(&mut self, session: &str, tab: &str) -> Result<bool, String>;
    fn close_origin_tab(&mut self, session: &str, tab: &str) -> Result<(), String>;
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
    let meta = RunMeta::new(run, captured_at);

    let fail = |step: TransferStep, message: String, capture_is_durable: bool| TransferError {
        step,
        message,
        // Nothing in this function closes the origin tab before the final step,
        // so every early return leaves it standing.
        origin_tab_preserved: true,
        capture_is_durable,
    };

    io.capture_scrollback(&run.origin_session, run.pane_id.as_deref(), &scrollback)
        .map_err(|e| fail(TransferStep::Capture, e, false))?;
    io.write_meta(&meta_dest, &meta)
        .map_err(|e| fail(TransferStep::WriteMeta, e, false))?;

    // From here on the run is recoverable from disk no matter what breaks.
    io.ensure_bucket_session(bucket.session_name())
        .map_err(|e| fail(TransferStep::EnsureBucketSession, e, true))?;
    io.open_bucket_tab(bucket.session_name(), &run.run, &meta)
        .map_err(|e| fail(TransferStep::OpenBucketTab, e, true))?;

    // Read back rather than trust the open: a bucket session killed mid-transfer
    // must not cost us the origin tab.
    let confirmed = io
        .bucket_tab_exists(bucket.session_name(), &run.run)
        .map_err(|e| fail(TransferStep::ConfirmBucketTab, e, true))?;
    if !confirmed {
        return Err(fail(
            TransferStep::ConfirmBucketTab,
            format!(
                "tab '{}' did not appear in session '{}'",
                run.run,
                bucket.session_name()
            ),
            true,
        ));
    }

    io.close_origin_tab(&run.origin_session, &run.origin_tab)
        .map_err(|e| TransferError {
            step: TransferStep::CloseOriginTab,
            message: e,
            // The close is what failed, so the tab is still there — a duplicate,
            // not a loss.
            origin_tab_preserved: true,
            capture_is_durable: true,
        })?;

    Ok(TransferReport {
        run: run.run.clone(),
        bucket,
        scrollback,
        meta: meta_dest,
        origin_tab_closed: true,
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
    }

    impl FakeIo {
        fn healthy() -> Self {
            FakeIo {
                tab_appears: true,
                ..Default::default()
            }
        }
        fn failing_at(step: TransferStep) -> Self {
            FakeIo {
                fail_at: Some(step),
                tab_appears: true,
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
            _session: &str,
            _pane_id: Option<&str>,
            _dest: &Path,
        ) -> Result<(), String> {
            self.guard(TransferStep::Capture, "capture")
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
            _tab: &str,
            _meta: &RunMeta,
        ) -> Result<(), String> {
            self.guard(TransferStep::OpenBucketTab, "open")
        }
        fn bucket_tab_exists(&mut self, _session: &str, _tab: &str) -> Result<bool, String> {
            self.guard(TransferStep::ConfirmBucketTab, "confirm")?;
            Ok(self.tab_appears)
        }
        fn close_origin_tab(&mut self, _session: &str, _tab: &str) -> Result<(), String> {
            self.guard(TransferStep::CloseOriginTab, "close")
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
            vec!["capture", "meta", "ensure", "open", "confirm", "close"]
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
                error.origin_tab_preserved,
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
            tab_appears: false,
            ..FakeIo::healthy()
        };
        let error = transfer_finished_run(&mut io, &finished(0), Path::new("/cp"), 0).unwrap_err();

        assert_eq!(error.step, TransferStep::ConfirmBucketTab);
        assert!(error.origin_tab_preserved);
        assert!(error.capture_is_durable, "capture landed before the move");
        assert!(!io.calls.contains(&"close"));
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

    #[test]
    fn meta_preserves_the_command_for_rerun() {
        let meta = RunMeta::new(&finished(1), 1_753_000_000);
        assert_eq!(meta.command, vec!["claude", "--resume"]);
        assert_eq!(meta.cwd, Some(PathBuf::from("/repo")));
        assert_eq!(meta.bucket, BucketKind::NeedsAttention);
        assert_eq!(meta.captured_at, 1_753_000_000);
    }
}
