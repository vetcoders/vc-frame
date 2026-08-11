//! Control-plane Live-runs census — the semantic source behind the rail's
//! `● Live N` row and its read surface.
//!
//! The census walks `<control-plane>/runtime_runs/*/meta.json` and keeps only
//! runs whose worker pid is still alive. Zellij tabs never enter this count:
//! a viewer tab is only an observer of a headless worker, so the run list must
//! come from the control plane, not from the screen's tab census
//! (`fleet_live_count` keeps serving the status-bar chip separately).
//!
//! The result is serialized as the `vc.live-runs.v1` payload and broadcast to
//! plugins as a `CustomMessage` by the session-metadata background loop.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// CustomMessage name AND payload schema tag — one string, one contract.
pub const VC_LIVE_RUNS_MESSAGE: &str = "vc.live-runs.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRunCard {
    pub run_id: String,
    pub agent: String,
    pub skill: String,
    /// Basename of the run's `root` workspace — enough for a compact card.
    pub repo: String,
    pub worker_pid: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveRunsSnapshot {
    pub schema: String,
    pub runs: Vec<LiveRunCard>,
}

impl LiveRunsSnapshot {
    pub fn new(runs: Vec<LiveRunCard>) -> Self {
        Self {
            schema: VC_LIVE_RUNS_MESSAGE.to_owned(),
            runs,
        }
    }
    pub fn payload(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }
}

/// Census of currently-live runs under the control plane root.
pub fn scan_live_runs(control_plane_root: &Path) -> Vec<LiveRunCard> {
    scan_live_runs_with(control_plane_root, worker_is_alive)
}

/// Liveness-injectable core so tests do not depend on real pids.
fn scan_live_runs_with(
    control_plane_root: &Path,
    is_alive: impl Fn(i32) -> bool,
) -> Vec<LiveRunCard> {
    let runs_dir = control_plane_root.join("runtime_runs");
    let Ok(entries) = std::fs::read_dir(&runs_dir) else {
        return vec![];
    };
    let mut cards: Vec<LiveRunCard> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| card_from_meta(&entry.path().join("meta.json")))
        .filter(|card| is_alive(card.worker_pid))
        .collect();
    // run_id starts with a launch timestamp, so this is chronological order.
    cards.sort_by(|a, b| a.run_id.cmp(&b.run_id));
    cards
}

/// One card from a runtime-run `meta.json`. The runtime owns that file's
/// schema; a run without `run_id` + `worker_pid` is not presentable and is
/// skipped rather than guessed at.
fn card_from_meta(meta_path: &Path) -> Option<LiveRunCard> {
    let raw = std::fs::read_to_string(meta_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let run_id = value.get("run_id")?.as_str()?.to_owned();
    let worker_pid = i32::try_from(value.get("worker_pid")?.as_i64()?).ok()?;
    let string_field = |key: &str| {
        value
            .get(key)
            .and_then(|field| field.as_str())
            .unwrap_or("")
            .to_owned()
    };
    let repo = value
        .get("root")
        .and_then(|field| field.as_str())
        .and_then(|root| Path::new(root).file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_owned();
    Some(LiveRunCard {
        run_id,
        agent: string_field("agent"),
        skill: string_field("skill"),
        repo,
        worker_pid,
    })
}

/// Signal-0 probe. `EPERM` still proves a live process; only `ESRCH` proves
/// death — same contract as `wait_for_process_exit` in os_input_output_unix.
#[cfg(unix)]
fn worker_is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    match nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None) {
        Ok(()) => true,
        Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn worker_is_alive(_pid: i32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_meta(root: &Path, run_id: &str, pid: i32) {
        let dir = root.join("runtime_runs").join(run_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("meta.json"),
            format!(
                r#"{{"run_id":"{run_id}","agent":"claude","skill":"workflow","root":"/tmp/ws/vc-frame","worker_pid":{pid}}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn census_keeps_only_runs_with_live_workers_in_chronological_order() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "work-260810-020000-2", 22);
        write_meta(tmp.path(), "work-260810-010000-1", 11);
        write_meta(tmp.path(), "work-260810-030000-3", 33);

        let cards = scan_live_runs_with(tmp.path(), |pid| pid != 22);

        assert_eq!(
            cards.iter().map(|c| c.worker_pid).collect::<Vec<_>>(),
            vec![11, 33]
        );
        assert_eq!(cards[0].repo, "vc-frame");
        assert_eq!(cards[0].agent, "claude");
        assert_eq!(cards[0].skill, "workflow");
    }

    #[test]
    fn broken_or_missing_meta_is_skipped_not_guessed() {
        let tmp = tempfile::tempdir().unwrap();
        write_meta(tmp.path(), "work-260810-010000-1", 11);
        let broken = tmp.path().join("runtime_runs").join("broken-run");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("meta.json"), "{not json").unwrap();
        std::fs::create_dir_all(tmp.path().join("runtime_runs").join("empty-run")).unwrap();

        let cards = scan_live_runs_with(tmp.path(), |_| true);

        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].run_id, "work-260810-010000-1");
    }

    #[test]
    fn missing_control_plane_yields_empty_census() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(scan_live_runs_with(&tmp.path().join("absent"), |_| true).is_empty());
    }

    #[test]
    fn snapshot_payload_carries_the_schema_tag() {
        let snapshot = LiveRunsSnapshot::new(vec![]);
        let payload = snapshot.payload().unwrap();
        assert!(payload.contains(r#""schema":"vc.live-runs.v1""#));
    }
}
