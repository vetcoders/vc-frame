use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const LATENCY_WINDOW: usize = 256;
const EVENT_WINDOW: usize = 128;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RouteMetric {
    pub count: u64,
    pub timeouts: u64,
    pub failures: u64,
    pub latency_p50_ms: u64,
    pub latency_p95_ms: u64,
    #[serde(skip)]
    latencies_ms: VecDeque<u64>,
}

impl Default for RouteMetric {
    fn default() -> Self {
        Self {
            count: 0,
            timeouts: 0,
            failures: 0,
            latency_p50_ms: 0,
            latency_p95_ms: 0,
            latencies_ms: VecDeque::with_capacity(LATENCY_WINDOW),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RouteEvent {
    pub sequence: u64,
    pub caller: String,
    pub action: String,
    pub latency_ms: u64,
    pub timed_out: bool,
    pub success: bool,
    pub workspace_session_id: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct RouteTelemetryState {
    pub enabled: bool,
    pub events_seen: u64,
    pub anonymous_events: u64,
    pub metrics: BTreeMap<String, RouteMetric>,
    pub recent: VecDeque<RouteEvent>,
}

impl RouteTelemetryState {
    fn record(
        &mut self,
        caller: &str,
        action: &str,
        elapsed: Duration,
        timed_out: bool,
        success: bool,
    ) {
        self.enabled = true;
        self.events_seen = self.events_seen.saturating_add(1);
        if caller == "anonymous" {
            self.anonymous_events = self.anonymous_events.saturating_add(1);
        }
        let latency_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let metric = self
            .metrics
            .entry(format!("{caller}:{action}"))
            .or_default();
        metric.count = metric.count.saturating_add(1);
        metric.timeouts = metric.timeouts.saturating_add(u64::from(timed_out));
        metric.failures = metric.failures.saturating_add(u64::from(!success));
        if metric.latencies_ms.len() == LATENCY_WINDOW {
            metric.latencies_ms.pop_front();
        }
        metric.latencies_ms.push_back(latency_ms);
        let mut sorted = metric.latencies_ms.iter().copied().collect::<Vec<_>>();
        sorted.sort_unstable();
        metric.latency_p50_ms = percentile(&sorted, 50);
        metric.latency_p95_ms = percentile(&sorted, 95);

        if self.recent.len() == EVENT_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(RouteEvent {
            sequence: self.events_seen,
            caller: caller.to_owned(),
            action: action.to_owned(),
            latency_ms,
            timed_out,
            success,
            workspace_session_id: workspace_session_id(),
        });
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let index = (sorted.len() * percentile).div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

fn diagnostics_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("VC_FRAME_ROUTE_DIAGNOSTICS")
            .map(|value| !matches!(value.trim(), "0" | "false" | "off"))
            .unwrap_or(true)
    })
}

fn workspace_session_id() -> Option<String> {
    std::env::var("VC_FRAME_WORKSPACE_SESSION_ID")
        .or_else(|_| std::env::var("VIBECRAFTED_SESSION_ID"))
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn state() -> &'static Mutex<RouteTelemetryState> {
    static STATE: OnceLock<Mutex<RouteTelemetryState>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(RouteTelemetryState {
            enabled: diagnostics_enabled(),
            ..RouteTelemetryState::default()
        })
    })
}

pub(crate) fn record(
    caller: &str,
    action: &str,
    elapsed: Duration,
    timed_out: bool,
    success: bool,
) {
    if !diagnostics_enabled() {
        return;
    }
    let caller = if caller.trim().is_empty() {
        "anonymous"
    } else {
        caller.trim()
    };
    state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .record(caller, action, elapsed, timed_out, success);
}

pub(crate) fn snapshot_json() -> String {
    let guard = state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    serde_json::to_string_pretty(&*guard)
        .unwrap_or_else(|error| format!(r#"{{"enabled":false,"error":"{error}"}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_caller_action_latency_and_timeout_receipts() {
        let mut state = RouteTelemetryState::default();
        state.record(
            "settlement",
            "SaveSession",
            Duration::from_millis(5),
            false,
            true,
        );
        state.record(
            "settlement",
            "SaveSession",
            Duration::from_millis(25),
            true,
            false,
        );
        state.record(
            "anonymous",
            "ListPanes",
            Duration::from_millis(10),
            false,
            true,
        );

        let save = &state.metrics["settlement:SaveSession"];
        assert_eq!(save.count, 2);
        assert_eq!(save.timeouts, 1);
        assert_eq!(save.failures, 1);
        assert_eq!(save.latency_p50_ms, 5);
        assert_eq!(save.latency_p95_ms, 25);
        assert_eq!(state.anonymous_events, 1);
        assert_eq!(state.recent.len(), 3);
    }

    #[test]
    fn ring_buffers_are_bounded() {
        let mut state = RouteTelemetryState::default();
        for index in 0..(EVENT_WINDOW + LATENCY_WINDOW + 10) {
            state.record(
                "operator",
                "Write",
                Duration::from_millis(index as u64),
                false,
                true,
            );
        }
        assert_eq!(state.recent.len(), EVENT_WINDOW);
        assert_eq!(
            state.metrics["operator:Write"].latencies_ms.len(),
            LATENCY_WINDOW
        );
    }
}
