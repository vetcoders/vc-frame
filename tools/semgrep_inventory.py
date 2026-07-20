#!/usr/bin/env python3
"""Fail-closed Semgrep inventory gate for vc-frame."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parents[1]
SECURITY_DIR = ROOT / "security" / "semgrep"
BASELINE_PATH = SECURITY_DIR / "baseline.json"
INVENTORY_PATH = SECURITY_DIR / "findings.jsonl"
IGNORE_POLICY_PATH = SECURITY_DIR / "ignore-policy.json"
SEMGREPIGNORE_PATH = ROOT / ".semgrepignore"
VERDICTS = {"fixed_defect", "scoped_false_positive", "accepted_unsafe_boundary"}
REQUIRED_FIELDS = {
    "id", "fingerprint", "rule", "path", "line", "column", "verdict",
    "reason", "invariant", "owner", "evidence",
}


class InventoryError(RuntimeError):
    pass


def read_json(path: Path) -> Any:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise InventoryError(f"cannot read {path}: {error}") from error


def read_inventory(path: Path = INVENTORY_PATH) -> list[dict[str, Any]]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise InventoryError(f"cannot read {path}: {error}") from error
    rows = []
    for line_number, line in enumerate(lines, 1):
        if not line.strip():
            raise InventoryError(f"blank inventory row at line {line_number}")
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise InventoryError(f"invalid JSON at inventory line {line_number}: {error}") from error
        if not isinstance(row, dict):
            raise InventoryError(f"inventory line {line_number} is not an object")
        rows.append(row)
    return rows


def evidence_path(reference: str) -> Path:
    return ROOT / reference.split("#", 1)[0]


def validate_ignore_policy(policy_path: Path = IGNORE_POLICY_PATH) -> None:
    entries = read_json(policy_path).get("allowed_ignores", [])
    if not isinstance(entries, list) or not entries:
        raise InventoryError("ignore policy needs explicit allowed ignores")
    allowed: dict[str, dict[str, Any]] = {}
    for entry in entries:
        pattern = entry.get("pattern", "")
        if not pattern or pattern in allowed:
            raise InventoryError("ignore policy has an empty or duplicate pattern")
        if any(token in pattern for token in ("*", "?", "[", "]")):
            raise InventoryError(f"broad ignore patterns are forbidden: {pattern}")
        if not entry.get("owner") or not entry.get("invariant") or not entry.get("evidence"):
            raise InventoryError(f"ignore policy lacks owner/invariant/evidence: {pattern}")
        for reference in entry["evidence"]:
            if not evidence_path(reference).exists():
                raise InventoryError(f"ignore evidence does not exist: {reference}")
        allowed[pattern] = entry
    configured = [
        line.strip()
        for line in SEMGREPIGNORE_PATH.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    if configured != list(allowed):
        raise InventoryError(
            f".semgrepignore differs from policy: configured={configured!r}, allowed={list(allowed)!r}"
        )


def validate_inventory(rows: list[dict[str, Any]], baseline: dict[str, Any]) -> None:
    if len(rows) != baseline["finding_count"]:
        raise InventoryError(
            f"inventory has {len(rows)} rows; baseline requires {baseline['finding_count']}"
        )
    ids: set[str] = set()
    fingerprints: set[str] = set()
    rule_counts: Counter[str] = Counter()
    for index, row in enumerate(rows, 1):
        missing = REQUIRED_FIELDS - row.keys()
        if missing:
            raise InventoryError(f"row {index} lacks fields: {sorted(missing)}")
        expected_id = f"SG-{index:04d}"
        if row["id"] != expected_id:
            raise InventoryError(f"row {index} id is {row['id']!r}; expected {expected_id}")
        if row["id"] in ids or row["fingerprint"] in fingerprints:
            raise InventoryError(f"duplicate id or fingerprint at row {index}")
        ids.add(row["id"])
        fingerprints.add(row["fingerprint"])
        if row["verdict"] not in VERDICTS:
            raise InventoryError(f"invalid verdict at {row['id']}: {row['verdict']}")
        for field in ("rule", "path", "reason", "invariant", "owner"):
            if not isinstance(row[field], str) or not row[field].strip():
                raise InventoryError(f"empty {field} at {row['id']}")
        if not isinstance(row["line"], int) or row["line"] < 1:
            raise InventoryError(f"invalid line at {row['id']}")
        if not isinstance(row["column"], int) or row["column"] < 1:
            raise InventoryError(f"invalid column at {row['id']}")
        if not isinstance(row["evidence"], list) or not row["evidence"]:
            raise InventoryError(f"missing evidence at {row['id']}")
        for reference in row["evidence"]:
            if not isinstance(reference, str) or not evidence_path(reference).exists():
                raise InventoryError(f"evidence does not exist at {row['id']}: {reference}")
        if not (ROOT / row["path"]).is_file():
            raise InventoryError(f"finding path does not exist at {row['id']}: {row['path']}")
        rule_counts[row["rule"]] += 1
    if dict(sorted(rule_counts.items())) != baseline["finding_counts_by_rule"]:
        raise InventoryError("inventory rule counts differ from baseline")


def validate_results(
    results_path: Path, rows: list[dict[str, Any]], baseline: dict[str, Any]
) -> None:
    payload = read_json(results_path)
    if payload.get("errors"):
        raise InventoryError(f"Semgrep returned scanner errors: {payload['errors']!r}")
    if payload.get("version") != baseline["scanner_version"]:
        raise InventoryError(
            f"Semgrep version drift: {payload.get('version')} != {baseline['scanner_version']}"
        )
    resolved_rules = sorted(payload.get("time", {}).get("rules", []))
    if resolved_rules != baseline["resolved_rule_ids"]:
        raise InventoryError("resolved p/rust rules drifted; refresh and re-adjudicate")
    results = payload.get("results", [])
    current = {result["extra"]["fingerprint"]: result for result in results}
    if len(current) != len(results):
        raise InventoryError("Semgrep produced duplicate result fingerprints")
    active = {row["fingerprint"]: row for row in rows if row["verdict"] != "fixed_defect"}
    unexplained = sorted(set(current) - set(active))
    missing = sorted(set(active) - set(current))
    if unexplained or missing:
        details = []
        for fingerprint in unexplained[:10]:
            result = current[fingerprint]
            details.append(
                f"unexplained {result['check_id']} {result['path']}:{result['start']['line']}"
            )
        for fingerprint in missing[:10]:
            row = active[fingerprint]
            details.append(f"missing {row['id']} {row['path']}:{row['line']}")
        raise InventoryError("finding drift: " + "; ".join(details))
    target_count = len(payload.get("paths", {}).get("scanned", []))
    if target_count < baseline["target_count"]:
        raise InventoryError(f"target coverage regressed: {target_count} < {baseline['target_count']}")
    print(
        f"semgrep inventory PASS: {len(results)} explained blockers, 0 unexplained, "
        f"{len(resolved_rules)} resolved rules, {target_count} targets"
    )


def validate_resolved_config(config: str, baseline: dict[str, Any]) -> None:
    completed = subprocess.run(
        ["semgrep", "show", "dump-config", config],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode:
        raise InventoryError(f"cannot resolve Semgrep config {config!r}")
    normalized = re.sub(
        r"osemgrep[0-9A-Za-z-]+\.yaml", "osemgrep-RULESET.yaml", completed.stdout
    )
    digest = hashlib.sha256(normalized.encode("utf-8")).hexdigest()
    if digest != baseline["resolved_config_sha256"]:
        raise InventoryError(
            f"resolved Semgrep rule content drifted: {digest} != "
            f"{baseline['resolved_config_sha256']}"
        )


def unsafe_policy(path: str) -> tuple[str, str, list[str]]:
    policies = {
        "zellij-tile/src/shim.rs": (
            "Plugin API FFI",
            "Unsafe calls are the wasm host-call boundary; values are serialized before the single host trampoline and no raw host pointer crosses the API.",
            ["security/semgrep/EVIDENCE.md#plugin-api-ffi", "zellij-server/src/plugins/unit/plugin_tests.rs"],
        ),
        "zellij-utils/src/vendored/termwiz/input.rs": (
            "Upstream termwiz boundary",
            "Unsafe accesses decode Windows tagged unions or validated UTF-8 inside the vendored module; callers receive owned safe Rust values.",
            ["security/semgrep/EVIDENCE.md#vendored-termwiz", "zellij-utils/src/vendored/termwiz/mod.rs"],
        ),
        "zellij-utils/src/consts.rs": (
            "IPC transport",
            "OwnedFd owns the socket, sockaddr length is checked, connect is poll-bounded, and flags are restored before return.",
            ["security/semgrep/EVIDENCE.md#ipc-libc", "zellij-utils/src/ipc/tests/socket_tests.rs"],
        ),
        "zellij-utils/src/sessions.rs": (
            "Session discovery",
            "The libc process probe is read-only and accepts only a PID parsed from a locally owned socket name.",
            ["security/semgrep/EVIDENCE.md#process-probes", "zellij-utils/src/sessions.rs"],
        ),
        "zellij-utils/src/envs.rs": (
            "Process environment",
            "Environment mutation is serialized during process initialization or isolated test cleanup.",
            ["security/semgrep/EVIDENCE.md#process-environment", "zellij-utils/src/envs.rs"],
        ),
        "zellij-server/src/plugins/zellij_exports.rs": (
            "Plugin host API",
            "Environment mutation is confined to synchronous host-command handling and the explicitly requested variable.",
            ["security/semgrep/EVIDENCE.md#process-environment", "zellij-server/src/plugins/unit/plugin_tests.rs"],
        ),
        "zellij-server/src/lib.rs": (
            "Server process lifecycle",
            "Unix daemonization is isolated to startup before threaded work and every fork outcome is handled.",
            ["security/semgrep/EVIDENCE.md#process-probes", "zellij-server/src/lib.rs"],
        ),
    }
    if path in policies:
        return policies[path]
    if "/unit/" in path or path.endswith("_tests.rs") or "/tests/" in path:
        return (
            "Rust test harness",
            "Unsafe environment mutation is test-only, restores prior state, and does not ship in production binaries.",
            ["security/semgrep/EVIDENCE.md#test-only-unsafe", path],
        )
    if "windows" in path:
        return (
            "Windows platform I/O",
            "Unsafe blocks are narrow Win32/ConPTY adapters; handles are checked and wrapped before safe code observes them.",
            ["security/semgrep/EVIDENCE.md#windows-platform-ffi", path],
        )
    if "unix" in path:
        return (
            "Unix platform I/O",
            "Unsafe blocks are narrow libc/terminal adapters; descriptors and pointers are validated and exposed as safe types.",
            ["security/semgrep/EVIDENCE.md#unix-platform-ffi", path],
        )
    raise InventoryError(f"unsafe finding has no policy: {path}")


def adjudicate(result: dict[str, Any]) -> tuple[str, str, str, str, list[str]]:
    rule, path = result["check_id"], result["path"]
    if rule == "rust.lang.security.unsafe-usage.unsafe-usage":
        owner, invariant, evidence = unsafe_policy(path)
        return (
            "accepted_unsafe_boundary",
            "Required FFI, process-global, or test-only unsafe boundary reviewed in its owner module.",
            invariant, owner, evidence,
        )
    if rule == "rust.lang.security.temp-dir.temp-dir":
        if path == "zellij-server/src/tab/mod.rs":
            return (
                "scoped_false_positive",
                "The rule flags temp_dir itself; each scrollback file appends a fresh UUID v4.",
                "Every editor dump has a new UUID v4 and carries only current-user terminal contents.",
                "Terminal scrollback",
                ["security/semgrep/EVIDENCE.md#temporary-paths", "zellij-server/src/tab/mod.rs#edit_scrollback"],
            )
        return (
            "scoped_false_positive",
            "The hit is inside cfg(test), not a production security boundary.",
            "Test data is process-local, cleaned by the test, and never trusted by production code.",
            "Rust test harness", ["security/semgrep/EVIDENCE.md#temporary-paths", path],
        )
    if rule == "rust.lang.security.current-exe.current-exe":
        return (
            "scoped_false_positive",
            "current_exe only spawns another mode of the running vc-frame binary; it establishes no trust.",
            "The resolved current binary receives fixed internal flags plus already-validated configuration paths.",
            "Client process lifecycle", ["security/semgrep/EVIDENCE.md#current-executable", path],
        )
    if rule == "rust.lang.security.args-os.args-os":
        return (
            "scoped_false_positive",
            "args_os is the CLI entrypoint preserving platform arguments for typed clap parsing.",
            "All arguments flow into typed clap parsing and command validation before execution.",
            "CLI parsing", ["security/semgrep/EVIDENCE.md#cli-arguments", "zellij-utils/src/cli.rs#CliArgs::parse"],
        )
    if rule == "rust.actix.path-traversal.tainted-path.tainted-path":
        policies = {
            "zellij-server/src/plugins/plugin_loader.rs": ("Plugin filesystem capability", "The path is a host-selected WASI preopen, not an HTTP parameter."),
            "zellij-server/src/plugins/watch_filesystem.rs": ("Plugin filesystem capability", "The hit only converts an authorized host cwd before a local watcher is registered."),
            "zellij-utils/src/consts.rs": ("Runtime directories", "Source and target are fixed legacy/current ProjectDirs owned by the local OS user."),
            "zellij-utils/src/input/plugins.rs": ("Plugin loading", "Reading an operator-selected plugin is the explicit plugin capability; builtins resolve from embedded assets first."),
            "zellij-utils/src/ipc/protobuf_conversion.rs": ("IPC data model", "The hit is data-only PathBuf construction; no filesystem operation occurs."),
            "zellij-utils/src/vibecrafted_install.rs": ("Vibecrafted layout installer", "The source is enumerated from a validated framework root and destination is the current-user layouts directory."),
            "zellij-utils/src/web_server_commands.rs": ("Local webserver IPC", "Socket paths are discovered below the current-user runtime directory and connect probing is bounded."),
        }
        if path not in policies:
            raise InventoryError(f"traversal finding has no policy: {path}")
        owner, invariant = policies[path]
        return (
            "scoped_false_positive",
            "The Actix taint rule matched a generic path operation outside an Actix request flow.",
            invariant, owner, ["security/semgrep/EVIDENCE.md#path-traversal", path],
        )
    raise InventoryError(f"finding has no policy: {rule} {path}")


def generate_inventory(results_path: Path, output_path: Path) -> None:
    rows = []
    for index, result in enumerate(read_json(results_path).get("results", []), 1):
        verdict, reason, invariant, owner, evidence = adjudicate(result)
        rows.append({
            "id": f"SG-{index:04d}",
            "fingerprint": result["extra"]["fingerprint"],
            "rule": result["check_id"], "path": result["path"],
            "line": result["start"]["line"], "column": result["start"]["col"],
            "verdict": verdict, "reason": reason, "invariant": invariant,
            "owner": owner, "evidence": evidence,
        })
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        "".join(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows),
        encoding="utf-8",
    )
    print(f"generated {len(rows)} inventory rows at {output_path}")


def run_scan(config: str) -> None:
    with tempfile.TemporaryDirectory(prefix="vc-frame-semgrep-") as temp_dir:
        results_path = Path(temp_dir) / "results.json"
        baseline, rows = read_json(BASELINE_PATH), read_inventory()
        validate_resolved_config(config, baseline)
        command = [
            "semgrep", "scan", "--config", config, "--metrics", "off", "--time",
            "--timeout", "0", "--json", "--output", str(results_path), ".",
        ]
        completed = subprocess.run(command, cwd=ROOT, check=False)
        if completed.returncode:
            raise InventoryError(f"Semgrep scanner failed with exit code {completed.returncode}")
        validate_ignore_policy()
        validate_inventory(rows, baseline)
        validate_results(results_path, rows, baseline)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    validate = commands.add_parser("validate")
    validate.add_argument("--results", type=Path)
    scan = commands.add_parser("scan")
    scan.add_argument("--config", default="p/rust")
    generate = commands.add_parser("generate")
    generate.add_argument("results", type=Path)
    generate.add_argument("--output", type=Path, default=INVENTORY_PATH)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "generate":
            generate_inventory(args.results, args.output)
            return 0
        baseline, rows = read_json(BASELINE_PATH), read_inventory()
        validate_ignore_policy()
        validate_inventory(rows, baseline)
        if args.command == "scan":
            run_scan(args.config)
        elif args.results:
            validate_results(args.results, rows, baseline)
        else:
            print(f"semgrep inventory schema PASS: {len(rows)} adjudicated rows")
        return 0
    except InventoryError as error:
        print(f"semgrep inventory FAIL: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
