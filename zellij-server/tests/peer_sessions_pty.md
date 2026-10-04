# Native peer-session PTY acceptance

This scenario uses an explicit committed `vc-frame` binary and product-owned config/layouts. It never builds or installs a binary. Running it creates only isolated fixture sessions/processes. Do not run it until runtime verification is authorized and the candidate binary has been built through the release lane.

```sh
python3 zellij-server/tests/peer_sessions_pty.py \
  --binary /absolute/path/to/committed/vc-frame \
  --config /absolute/path/to/candidate/config.kdl \
  --operator-layout /absolute/path/to/candidate/vibecrafted-host.kdl \
  --project-layout /absolute/path/to/candidate/operator.kdl \
  --scratch /tmp/peer-proof-unique \
  --output /absolute/path/to/new/receipt-directory
```

Uses the existing `pyte` harness dependency, Python stdlib and fixture-only Git/lsof/ps. Config paths are mandatory: there is no fallback to live Founder config. Short socket paths are required on macOS. `--status-text-a` and `--status-text-b` can require additional exact footer text, with `{session}` substituted; the actual attached session identity is always required independently.

The canonical Operator layout must contain five direct tabs named Dashboard, Active runs, Config, Doctor, Projects. The private layout transformation keeps global chrome/role declarations and replaces tab bodies with harmless per-PID ACK tasks. Projects gain two distinct tabs and two tasks per tab. This verifies routing, not dashboard/config/doctor backends or packaged launch.

The proof uses two actual PTY clients of one project. Super CSI input routes sessions and tabs in Normal and LOCK, and actual SGR clicks exercise tabs/panes/rail. Per-client selected chip, runtime selected pane, workload PID and typed-input ACK must agree; global active-tab metadata is never the oracle. Returning restores independent tab/pane state. Rail 00/01/02, selected-row styling, session-local top tabs and footer identity, exact workload continuity, direct RGB cells, active bold/count/continuous close cells, plugin runtime identities and fixture process ancestry are checked.

Receipts include explicit binary/build provenance, input hashes, raw ANSI, per-client text snapshots, diagnostic command logs, workload JSONL, inventories and process tree before cleanup. Failures retain output and stay failures. Teardown only addresses exact fixture session names inside the private socket namespace.

Not certified: physical macOS key delivery, installed artifact behavior, real Operator backends, GitHub network data, opacity/blur/font, close hit behavior and narrow +N overflow. Source checks alone are not a runtime PASS.
