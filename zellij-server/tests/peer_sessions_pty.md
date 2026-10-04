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

## Source schema validation (c3a49fd92 baseline)

`zellij-utils/src/build_info.rs:72-94` emits `product`, full `git_sha` and boolean `git_dirty`; the harness requires exactly `git_dirty is False`, correct product and a full SHA. `src/main.rs:77-79` publishes this JSON before launching clients.

`zellij-utils/src/data.rs:2495-2556,2626-2640` defines flattened `PaneInfo` plus `tab_id`, `tab_name`, optional `plugin_runtime_id` and `pane_command`. `zellij-server/src/route.rs:3260-3321` enriches terminal commands and serializes the list. The harness explicitly requests `--command`, requires those fields and refuses an unobserved command. `session_layout_metadata.rs:85-101,658-675` maps focused client IDs to pane IDs and emits `CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND`; the parser requires that header and unique numeric client IDs.

`zellij-utils/src/cli.rs:76-84,1334-1345` accepts global `--layout` with positional `attach <name>` and boolean `-b/-c`. `src/commands.rs:764-818` uses `create_background` for detached creation; `zellij-utils/src/setup.rs:709-714` excludes explicit layout from implicit-host selection. These are source contracts, not proof that a built candidate successfully executes them.

Each frontend is bound to its admitted client ID sequentially. On a physical route or rail click, exactly one destination client ID must appear and that frontend's origin ID must disappear; returning rebinds the newly admitted ID. ACK checks require that exact ID's selected pane. A deliberate same-pane phase exercises distinct frontends sharing one pane. Project close glyph and all three continuous cells are mandatory, and a later tab's close glyph cannot satisfy the current tab's check.
