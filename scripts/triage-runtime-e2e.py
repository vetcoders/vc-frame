#!/usr/bin/env python3
"""Isolated command-boundary proof for truthful run triage.

The harness accepts only an exact, clean, profile-matched ``vc-frame`` build,
constructs two empty runtime namespaces below one short ``mkdtemp`` root, and
never discovers or mutates the operator's normal socket tree. Durable evidence
and the isolated control plane live below the requested artifact directory;
only the Unix-socket runtime uses the short root required by macOS.

Every run leaves an atomic ``evidence.json`` receipt behind. A passing receipt
contains the fixture namespaces, binary provenance, per-negative before/after
state, artifact digests, transfer transitions, restart evidence, and exact
cleanup/process-residue proof.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import pathlib
import shlex
import signal
import shutil
import stat
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from typing import Any, Callable, Literal


DRAWER_BY_BUCKET = {
    "Finalized": "Finalized runs",
    "Failed": "Failed runs",
    "NeedsAttention": "Needs attention",
}
ENV_ALLOWLIST = (
    "COLORTERM",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LOGNAME",
    "NO_COLOR",
    "PATH",
    "RUST_BACKTRACE",
    "RUST_LOG",
    "SHELL",
    "TERM",
    "USER",
)
ISOLATION_PATH_KEYS = (
    "HOME",
    "TMPDIR",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "VIBECRAFTED_HOME",
)
RECEIPT_FIELDS = (
    "version",
    "run",
    "exit_code",
    "bucket",
    "capture",
    "capture_committed",
    "metadata_committed",
    "viewer_confirmed",
    "viewer_creation_pending",
    "viewer_creation_generation",
    "origin_tab_state",
    "viewer_token",
    "viewer_tab_identity",
    "pane_id",
    "fault",
)
UNIX_SOCKET_PATH_LIMIT = 103
SOCKET_CONTRACT_DIRECTORY = "contract_version_1"
SHORT_RUNTIME_PARENT = pathlib.Path("/tmp")


@dataclass(frozen=True)
class SessionQuery:
    """A proven live/absent result; command ambiguity is raised, never encoded."""

    state: Literal["live", "absent"]
    tabs: list[dict[str, object]] | None
    list_tabs_exit: int
    list_tabs_stderr: str
    inventory_state: Literal["live", "exited", "missing"]


class AmbiguousSessionError(AssertionError):
    """The exact session query cannot yet prove a valid live/absent state."""


@dataclass(frozen=True)
class InterruptedProcess:
    pid: int
    slices: int
    signal: str
    observed_state: dict[str, object]
    stdout_path: str
    stderr_path: str
    stdout: str
    stderr: str
    returncode: int


class EvidenceRecorder:
    """Atomic, append-friendly receipt persisted throughout the runtime proof."""

    def __init__(self, path: pathlib.Path, initial: dict[str, Any]) -> None:
        self.path = path
        self.data = initial
        self.flush()

    def flush(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = self.path.with_name(f".{self.path.name}.tmp")
        with temporary.open("w", encoding="utf-8") as handle:
            json.dump(self.data, handle, indent=2, sort_keys=True)
            handle.write("\n")
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, self.path)

    def set(self, key: str, value: Any) -> None:
        self.data[key] = value
        self.flush()

    def append(self, key: str, value: Any) -> None:
        collection = self.data.setdefault(key, [])
        require(isinstance(collection, list), f"evidence field {key!r} is not a list")
        collection.append(value)
        self.flush()


def require(condition: bool, message: str) -> None:
    """An assertion that remains active under ``python -O``."""
    if not condition:
        raise AssertionError(message)


def utc_now() -> str:
    return datetime.datetime.now(datetime.UTC).isoformat().replace("+00:00", "Z")


def sha256_bytes(contents: bytes) -> str:
    return hashlib.sha256(contents).hexdigest()


def validate_sha(value: str, label: str) -> str:
    normalized = value.strip().lower()
    require(
        len(normalized) == 40
        and all(character in "0123456789abcdef" for character in normalized),
        f"{label} must be a full 40-character hexadecimal commit SHA: {value!r}",
    )
    return normalized


def current_checkout_sha(checkout: pathlib.Path) -> str:
    """Resolve HEAD only when the checkout can truthfully produce a clean build."""
    root = subprocess.run(
        ["git", "-C", str(checkout), "rev-parse", "--show-toplevel"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(
        root.returncode == 0,
        f"cannot resolve checkout root: exit={root.returncode}, stderr={root.stderr!r}",
    )
    repo_root = pathlib.Path(root.stdout.strip()).resolve()
    status = subprocess.run(
        [
            "git",
            "-C",
            str(repo_root),
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(
        status.returncode == 0,
        f"cannot inspect checkout cleanliness: {status.stderr!r}",
    )
    require(
        not status.stdout.strip(),
        "cannot expect current-checkout provenance from a dirty tree; "
        "commit or explicitly pass --expected-sha for a previously built artifact",
    )
    head = subprocess.run(
        ["git", "-C", str(repo_root), "rev-parse", "HEAD"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(head.returncode == 0, f"cannot resolve checkout HEAD: {head.stderr!r}")
    return validate_sha(head.stdout, "checkout HEAD")


def validate_build_info(
    build_info: dict[str, object],
    *,
    expected_sha: str,
    expected_profile: str,
) -> None:
    require(
        build_info.get("product") == "vc-frame",
        f"refusing foreign binary: build-info={build_info!r}",
    )
    require(
        build_info.get("git_sha") == expected_sha,
        "binary provenance mismatch: "
        f"expected git_sha={expected_sha}, got {build_info.get('git_sha')!r}",
    )
    require(
        build_info.get("git_dirty") is False,
        f"binary was built from a dirty tree: {build_info.get('git_dirty')!r}",
    )
    require(
        build_info.get("profile") == expected_profile,
        "binary profile mismatch: "
        f"expected {expected_profile!r}, got {build_info.get('profile')!r}",
    )


def command(
    binary: pathlib.Path,
    env: dict[str, str],
    *args: str,
    expect_success: bool | None = True,
) -> subprocess.CompletedProcess[str]:
    try:
        result = subprocess.run(
            [str(binary), *args],
            env=env,
            stdin=subprocess.DEVNULL,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=30,
        )
    except subprocess.TimeoutExpired as error:
        raise AssertionError(f"command timed out: {binary} {' '.join(args)}") from error
    if expect_success is not None and expect_success != (result.returncode == 0):
        raise AssertionError(
            f"unexpected exit {result.returncode}: {binary} {' '.join(args)}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def parse_json_array(raw: str, label: str) -> list[dict[str, object]]:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError as error:
        raise AssertionError(f"{label} is not valid JSON: {error}") from error
    require(isinstance(value, list), f"{label} is not a JSON array")
    require(
        all(isinstance(item, dict) for item in value),
        f"{label} contains a non-object entry",
    )
    return value


def canonical_json(value: object) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def artifact_tree_snapshot(root: pathlib.Path) -> dict[str, object]:
    files: list[dict[str, object]] = []
    if root.exists():
        for path in sorted(
            candidate for candidate in root.rglob("*") if candidate.is_file()
        ):
            contents = path.read_bytes()
            files.append(
                {
                    "path": str(path.relative_to(root)),
                    "bytes": len(contents),
                    "sha256": sha256_bytes(contents),
                }
            )
    digest = sha256_bytes(canonical_json(files).encode())
    return {
        "root": str(root.resolve()),
        "files": files,
        "digest": digest,
    }


def guarded_tree_snapshot(
    roots: list[pathlib.Path],
    *,
    volatile_files: set[pathlib.Path] | None = None,
) -> dict[str, object]:
    """Content-address operator paths without following symlinks or sockets."""
    volatile = {path.expanduser().resolve() for path in (volatile_files or set())}
    entries: list[dict[str, object]] = []
    for root in sorted({path.expanduser().resolve() for path in roots}):
        if not root.exists():
            entries.append({"root": str(root), "state": "absent"})
            continue
        candidates = [root, *sorted(root.rglob("*"))]
        for path in candidates:
            try:
                metadata = path.lstat()
                relative = "." if path == root else str(path.relative_to(root))
                entry: dict[str, object] = {
                    "root": str(root),
                    "path": relative,
                    "mode": stat.S_IFMT(metadata.st_mode),
                    "device": metadata.st_dev,
                    "inode": metadata.st_ino,
                }
                if stat.S_ISREG(metadata.st_mode):
                    if path.resolve() in volatile:
                        # Active operator servers may append this log while the
                        # isolated fixture runs. Preserve its exact identity in
                        # the guard, but do not turn unrelated appends into a
                        # false claim that the fixture touched operator state.
                        entry["kind"] = "volatile_file_identity"
                    else:
                        contents = path.read_bytes()
                        entry.update(
                            {
                                "kind": "file",
                                "bytes": len(contents),
                                "sha256": sha256_bytes(contents),
                            }
                        )
                elif stat.S_ISDIR(metadata.st_mode):
                    entry["kind"] = "directory"
                elif stat.S_ISLNK(metadata.st_mode):
                    entry.update({"kind": "symlink", "target": os.readlink(path)})
                elif stat.S_ISSOCK(metadata.st_mode):
                    entry["kind"] = "socket"
                else:
                    entry["kind"] = "other"
                entries.append(entry)
            except (OSError, ValueError) as error:
                entries.append(
                    {
                        "root": str(root),
                        "path": str(path),
                        "kind": "unreadable",
                        "error": str(error),
                    }
                )
    return {
        "roots": [str(path.expanduser().resolve()) for path in roots],
        "entries": entries,
        "digest": sha256_bytes(canonical_json(entries).encode()),
    }


def guarded_snapshot_summary(snapshot: dict[str, object]) -> dict[str, object]:
    entries = snapshot.get("entries")
    require(isinstance(entries, list), "guard snapshot omitted entries")
    roots = snapshot.get("roots")
    digest = snapshot.get("digest")
    require(isinstance(roots, list), "guard snapshot omitted roots")
    require(isinstance(digest, str), "guard snapshot omitted digest")
    return {
        "roots": roots,
        "digest": digest,
        "entry_count": len(entries),
    }


def guarded_snapshot_diff(
    before: dict[str, object],
    after: dict[str, object],
    *,
    sample_limit: int = 50,
) -> dict[str, object]:
    """Return a bounded path-level difference without embedding full snapshots."""

    changes = guarded_snapshot_changes(before, after)

    def labels(kind: str) -> list[dict[str, str]]:
        return [
            {"root": change["root"], "path": change["path"]}
            for change in changes
            if change["kind"] == kind
        ][:sample_limit]

    return {
        "added_count": sum(change["kind"] == "added" for change in changes),
        "removed_count": sum(change["kind"] == "removed" for change in changes),
        "changed_count": sum(change["kind"] == "changed" for change in changes),
        "sample_limit": sample_limit,
        "added": labels("added"),
        "removed": labels("removed"),
        "changed": labels("changed"),
    }


def guarded_snapshot_changes(
    before: dict[str, object],
    after: dict[str, object],
) -> list[dict[str, str]]:
    """Return every changed path so attribution is never based on a sample."""

    def indexed(
        snapshot: dict[str, object],
    ) -> dict[tuple[str, str], dict[str, object]]:
        raw_entries = snapshot.get("entries")
        require(isinstance(raw_entries, list), "guard snapshot omitted entries")
        result: dict[tuple[str, str], dict[str, object]] = {}
        for raw in raw_entries:
            require(isinstance(raw, dict), "guard snapshot entry is not an object")
            root = raw.get("root")
            path = raw.get("path", ".")
            require(
                isinstance(root, str) and isinstance(path, str),
                "guard snapshot entry omitted path identity",
            )
            result[(root, path)] = raw
        return result

    before_entries = indexed(before)
    after_entries = indexed(after)
    before_keys = set(before_entries)
    after_keys = set(after_entries)
    added = sorted(after_keys - before_keys)
    removed = sorted(before_keys - after_keys)
    changed = sorted(
        key
        for key in before_keys & after_keys
        if before_entries[key] != after_entries[key]
    )
    return [
        *[
            {"kind": "added", "root": root, "path": path}
            for root, path in added
        ],
        *[
            {"kind": "removed", "root": root, "path": path}
            for root, path in removed
        ],
        *[
            {"kind": "changed", "root": root, "path": path}
            for root, path in changed
        ],
    ]


def operator_guard_fixture_markers(
    fixture_id: str,
    evidence: dict[str, object],
) -> set[str]:
    """Collect durable fixture identities that would expose namespace leakage."""
    markers = {fixture_id}
    identity_keys = {"session_incarnation", "tab_instance_id", "viewer_token"}

    def visit(value: object, key: str | None = None) -> None:
        if isinstance(value, dict):
            for nested_key, nested_value in value.items():
                visit(nested_value, str(nested_key))
        elif isinstance(value, list):
            for nested_value in value:
                visit(nested_value, key)
        elif key in identity_keys and isinstance(value, str) and value:
            markers.add(value)

    visit(evidence)
    return markers


def operator_guard_allows_concurrent_runtime_drift(root: str) -> bool:
    """Runtime cache/socket roots are shared; config and durable data are not."""
    normalized = pathlib.Path(root).as_posix()
    sensitive_suffixes = (
        "/.config/vc-frame",
        "/.local/share/vc-frame",
        "/Library/Application Support/io.vetcoders.vc-frame",
    )
    return not normalized.endswith(sensitive_suffixes)


def attribute_operator_guard_changes(
    before: dict[str, object],
    after: dict[str, object],
    fixture_markers: set[str],
    *,
    sample_limit: int = 50,
) -> dict[str, object]:
    """Separate fixture leakage from unrelated activity on a shared runner."""
    fixture_attributed: list[dict[str, str]] = []
    concurrent_runtime: list[dict[str, str]] = []
    unattributed_sensitive: list[dict[str, str]] = []
    for change in guarded_snapshot_changes(before, after):
        identity = f"{change['root']}/{change['path']}"
        if any(marker in identity for marker in fixture_markers):
            fixture_attributed.append(change)
        elif operator_guard_allows_concurrent_runtime_drift(change["root"]):
            concurrent_runtime.append(change)
        else:
            unattributed_sensitive.append(change)

    def summary(changes: list[dict[str, str]]) -> dict[str, object]:
        return {
            "count": len(changes),
            "sample_limit": sample_limit,
            "paths": changes[:sample_limit],
        }

    return {
        "safe": not fixture_attributed and not unattributed_sensitive,
        "fixture_marker_count": len(fixture_markers),
        "fixture_attributed": summary(fixture_attributed),
        "concurrent_runtime_drift": summary(concurrent_runtime),
        "unattributed_sensitive": summary(unattributed_sensitive),
    }


def operator_guard_paths() -> list[pathlib.Path]:
    """Exact non-isolated vc-frame socket/state roots from the operator env."""
    paths: set[pathlib.Path] = set()
    home_value = os.environ.get("HOME")
    if home_value:
        home = pathlib.Path(home_value).expanduser()
        paths.update(
            {
                home / ".cache" / "vc-frame",
                home / ".config" / "vc-frame",
                home / ".local" / "share" / "vc-frame",
                home / "Library" / "Caches" / "io.vetcoders.vc-frame",
                home / "Library" / "Application Support" / "io.vetcoders.vc-frame",
            }
        )
    temporary_value = os.environ.get("TMPDIR", "/tmp")
    paths.add(pathlib.Path(temporary_value) / f"vc-frame-{os.getuid()}")
    for key in (
        "VC_FRAME_SOCKET_DIR",
        "ZELLIJ_SOCKET_DIR",
    ):
        value = os.environ.get(key)
        if value:
            paths.add(pathlib.Path(value).expanduser())
    return sorted(paths)


def server_argument_paths(command_line: str) -> list[pathlib.Path]:
    """Extract exact ``--server`` arguments from one process command line."""
    try:
        arguments = shlex.split(command_line)
    except ValueError:
        return []
    paths: list[pathlib.Path] = []
    for index, argument in enumerate(arguments):
        if argument == "--server" and index + 1 < len(arguments):
            paths.append(pathlib.Path(arguments[index + 1]))
        elif argument.startswith("--server="):
            paths.append(pathlib.Path(argument.partition("=")[2]))
    return paths


def operator_guard_volatile_paths() -> set[pathlib.Path]:
    """Operator-owned heartbeat files whose identity, not bytes, is guarded."""
    temporary_value = os.environ.get("TMPDIR", "/tmp")
    runtime_root = (pathlib.Path(temporary_value) / f"vc-frame-{os.getuid()}").resolve()
    volatile = {runtime_root / "vc-frame-log" / "zellij.log"}

    home_value = os.environ.get("HOME")
    if not home_value:
        return volatile
    metadata_root = (
        pathlib.Path(home_value).expanduser()
        / "Library"
        / "Caches"
        / "io.vetcoders.vc-frame"
        / SOCKET_CONTRACT_DIRECTORY
        / "session_info"
    )
    # Any pre-existing session can be started or resurrected concurrently while
    # this long fixture runs, at which point vc-frame rewrites its metadata on a
    # heartbeat. Guard the exact pre-existing inode instead of attributing those
    # bytes to the fixture. New sessions, deleted files, replaced inodes, and
    # every other operator-state mutation still fail the guard.
    if metadata_root.is_dir():
        volatile.update(
            path
            for path in metadata_root.glob("*/session-metadata.kdl")
            if path.is_file()
        )
    return volatile


def socket_path_budget(
    socket_root: pathlib.Path,
    sessions: set[str],
    *,
    limit: int = UNIX_SOCKET_PATH_LIMIT,
) -> dict[str, object]:
    """Fail before mutation if any owned server socket exceeds sockaddr_un."""
    socket_root = socket_root.resolve()
    paths = [
        socket_root / SOCKET_CONTRACT_DIRECTORY / session
        for session in sorted(sessions)
    ]
    entries = [
        {
            "session": path.name,
            "path": str(path),
            "bytes": len(os.fsencode(path)),
        }
        for path in paths
    ]
    longest = max(entries, key=lambda entry: int(entry["bytes"]), default=None)
    remaining = limit - int(longest["bytes"]) if longest is not None else limit
    require(
        longest is None or (int(longest["bytes"]) <= limit and remaining > 0),
        "fixture Unix socket path exceeds portable limit: "
        f"limit={limit}, longest={longest!r}",
    )
    return {
        "socket_root": str(socket_root),
        "limit_bytes": limit,
        "paths": entries,
        "longest": longest,
        "remaining_bytes": remaining,
    }


def remove_runtime_root(runtime_root: pathlib.Path) -> dict[str, object]:
    """Remove only the exact short mkdtemp root created by this harness."""
    requested_root = runtime_root.expanduser().absolute()
    require(
        not requested_root.is_symlink(),
        f"refusing to remove symlink runtime root: {requested_root}",
    )
    runtime_root = requested_root.resolve()
    temporary_root = SHORT_RUNTIME_PARENT.resolve()
    require(
        runtime_root.parent == temporary_root
        and runtime_root.name.startswith("vcf-e2e-"),
        f"refusing to remove non-fixture runtime root: {runtime_root}",
    )
    existed = runtime_root.exists()
    if existed:
        shutil.rmtree(runtime_root)
    require(
        not runtime_root.exists(),
        f"fixture runtime root remained after removal: {runtime_root}",
    )
    return {
        "runtime_root": str(runtime_root),
        "existed_before_removal": existed,
        "absent_after_removal": True,
    }


def receipt_projection(receipt: dict[str, object]) -> dict[str, object]:
    return {field: receipt.get(field) for field in RECEIPT_FIELDS}


def transfer_evidence(
    control_plane: pathlib.Path,
    run: str,
    stage: str,
) -> dict[str, object]:
    scrollback, metadata, receipt = transfer_files(control_plane, run)
    contents = scrollback.read_bytes()
    manifest_path = scrollback.with_name("capture.manifest.json")
    manifest_contents = manifest_path.read_bytes()
    manifest = json.loads(manifest_contents)
    require(isinstance(manifest, dict), f"{run} capture manifest is not an object")
    return {
        "stage": stage,
        "run": run,
        "capture_path": str(scrollback.resolve()),
        "capture_bytes": len(contents),
        "capture_sha256": sha256_bytes(contents),
        "capture_manifest_path": str(manifest_path.resolve()),
        "capture_manifest_bytes": len(manifest_contents),
        "capture_manifest_sha256": sha256_bytes(manifest_contents),
        "capture_manifest": manifest,
        "metadata": metadata,
        "receipt": receipt_projection(receipt),
    }


def isolated_env(
    namespace_root: pathlib.Path, control_plane: pathlib.Path
) -> dict[str, str]:
    """Build a minimal environment; never inherit runtime/config selectors."""
    env = {key: os.environ[key] for key in ENV_ALLOWLIST if key in os.environ}
    paths = {
        "HOME": namespace_root / "home",
        "TMPDIR": namespace_root / "tmp",
        "XDG_CACHE_HOME": namespace_root / "cache",
        "XDG_CONFIG_HOME": namespace_root / "config",
        "XDG_DATA_HOME": namespace_root / "data",
        "XDG_RUNTIME_DIR": namespace_root / "runtime",
        "XDG_STATE_HOME": namespace_root / "state",
        "VIBECRAFTED_HOME": namespace_root / "vibecrafted",
    }
    socket_root = namespace_root / "sockets"
    for path in [*paths.values(), socket_root, control_plane]:
        path.mkdir(parents=True, exist_ok=True)
    (namespace_root / "runtime").chmod(0o700)
    env.update({key: str(path) for key, path in paths.items()})
    env.update(
        {
            # Set both spellings so even a compatibility path cannot fall back
            # to an inherited operator socket.
            "VC_FRAME_SOCKET_DIR": str(socket_root),
            "ZELLIJ_SOCKET_DIR": str(socket_root),
            "VIBECRAFTED_CONTROL_PLANE": str(control_plane),
        }
    )
    return env


def session_inventory(
    binary: pathlib.Path, env: dict[str, str]
) -> dict[str, Literal["live", "exited"]]:
    result = command(
        binary,
        env,
        "list-sessions",
        "--no-formatting",
        expect_success=None,
    )
    if result.returncode != 0:
        require(
            not result.stdout.strip()
            and "No active vc-frame sessions found." in result.stderr,
            "cannot prove the isolated session namespace is empty: "
            f"exit={result.returncode}, stdout={result.stdout!r}, "
            f"stderr={result.stderr!r}",
        )
        return {}
    inventory: dict[str, Literal["live", "exited"]] = {}
    for line in result.stdout.splitlines():
        name, separator, _rest = line.partition(" [Created ")
        require(bool(separator), f"unparseable session inventory line: {line!r}")
        name = name.strip()
        require(name not in inventory, f"duplicate session inventory entry: {name!r}")
        inventory[name] = (
            "exited" if "(EXITED - attach to resurrect)" in line else "live"
        )
    return inventory


def active_session_names(binary: pathlib.Path, env: dict[str, str]) -> set[str]:
    return {
        name
        for name, state in session_inventory(binary, env).items()
        if state == "live"
    }


def server_processes_from_ps(
    raw: str, socket_root: pathlib.Path
) -> list[dict[str, object]]:
    """Return only vc-frame server processes bound to this exact socket root."""
    root = socket_root.resolve()
    processes: list[dict[str, object]] = []
    for line in raw.splitlines():
        stripped = line.strip()
        pid_text, separator, command_line = stripped.partition(" ")
        if not separator or not pid_text.isdigit():
            continue
        if any(
            path.resolve().is_relative_to(root)
            for path in server_argument_paths(command_line)
        ):
            processes.append({"pid": int(pid_text), "command": command_line.strip()})
    return processes


def server_processes_for_socket_root(
    socket_root: pathlib.Path,
) -> list[dict[str, object]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,command="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(
        result.returncode == 0,
        "cannot prove isolated process cleanup because process inventory failed: "
        f"exit={result.returncode}, stderr={result.stderr!r}",
    )
    return server_processes_from_ps(result.stdout, socket_root)


def wait_for_no_server_processes(
    socket_root: pathlib.Path,
    *,
    timeout: float = 15,
) -> list[dict[str, object]]:
    deadline = time.monotonic() + timeout
    last: list[dict[str, object]] = []
    while time.monotonic() < deadline:
        last = server_processes_for_socket_root(socket_root)
        if not last:
            return []
        time.sleep(0.1)
    raise AssertionError(
        f"isolated vc-frame server process residue remained for {socket_root}: {last!r}"
    )


def namespace_preflight(
    binary: pathlib.Path,
    env: dict[str, str],
    namespace_root: pathlib.Path,
    expected_control_plane: pathlib.Path,
    *,
    expected_sha: str,
    expected_profile: str,
) -> dict[str, object]:
    """Reject a foreign/non-isolating binary before the first mutation."""
    namespace_root = namespace_root.resolve()
    for key in ("VC_FRAME_SOCKET_DIR", "ZELLIJ_SOCKET_DIR"):
        socket_root = pathlib.Path(env[key]).resolve()
        require(
            socket_root == namespace_root / "sockets",
            f"{key} escaped the fixture namespace: {socket_root}",
        )
    for key in ISOLATION_PATH_KEYS:
        resolved = pathlib.Path(env[key]).resolve()
        require(
            resolved.is_relative_to(namespace_root),
            f"isolated {key} escaped the fixture namespace: {resolved}",
        )
    control_plane = pathlib.Path(env["VIBECRAFTED_CONTROL_PLANE"]).resolve()
    require(
        control_plane == expected_control_plane.resolve(),
        f"isolated control plane does not match the durable fixture path: "
        f"{control_plane}",
    )

    build = command(binary, env, "--build-info")
    try:
        build_info = json.loads(build.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError("binary --build-info did not return JSON") from error
    require(isinstance(build_info, dict), "binary --build-info is not an object")
    validate_build_info(
        build_info,
        expected_sha=expected_sha,
        expected_profile=expected_profile,
    )
    help_result = command(binary, env, "triage-run", "--help")
    for capability in ("--runtime-transcript", "--origin-session", "--exit-code"):
        require(
            capability in help_result.stdout,
            f"binary lacks required triage capability {capability}",
        )
    require(
        session_inventory(binary, env) == {},
        "fixture namespace was not empty before mutation",
    )
    require(
        not server_processes_for_socket_root(pathlib.Path(env["VC_FRAME_SOCKET_DIR"])),
        "fixture namespace already has vc-frame server process residue",
    )
    return build_info


def query_session(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
) -> SessionQuery:
    """Prove live/absent; a failed command against an active name is ambiguity."""
    result = command(
        binary,
        env,
        "-s",
        session,
        "action",
        "list-tabs",
        "--json",
        expect_success=None,
    )
    if result.returncode == 0:
        try:
            tabs = parse_json_array(
                result.stdout,
                f"tab inventory for {session!r}",
            )
        except AssertionError as error:
            raise AmbiguousSessionError(
                f"session {session!r} returned a successful but invalid "
                f"list-tabs inventory; refusing to treat transient empty or "
                f"malformed output as runtime truth: stdout={result.stdout!r}, "
                f"stderr={result.stderr!r}"
            ) from error
        return SessionQuery(
            state="live",
            tabs=tabs,
            list_tabs_exit=0,
            list_tabs_stderr=result.stderr,
            inventory_state="live",
        )
    inventory_state = session_inventory(binary, env).get(session, "missing")
    if inventory_state == "live":
        raise AmbiguousSessionError(
            f"session {session!r} is active but list-tabs failed; refusing to "
            f"treat command failure as absence: exit={result.returncode}, "
            f"stdout={result.stdout!r}, stderr={result.stderr!r}"
        )
    return SessionQuery(
        state="absent",
        tabs=None,
        list_tabs_exit=result.returncode,
        list_tabs_stderr=result.stderr,
        inventory_state=inventory_state,
    )


def session_tabs(
    binary: pathlib.Path, env: dict[str, str], session: str
) -> list[dict[str, object]] | None:
    result = query_session(binary, env, session)
    if result.state == "absent":
        return None
    require(result.tabs is not None, f"live session {session!r} has no tab inventory")
    return result.tabs


def wait_for_tabs(
    binary: pathlib.Path, env: dict[str, str], session: str
) -> list[dict[str, object]]:
    deadline = time.monotonic() + 15
    last_ambiguity: AmbiguousSessionError | None = None
    while time.monotonic() < deadline:
        try:
            tabs = session_tabs(binary, env, session)
            last_ambiguity = None
        except AmbiguousSessionError as error:
            last_ambiguity = error
            time.sleep(0.1)
            continue
        if tabs is not None:
            return tabs
        time.sleep(0.1)
    suffix = f"; last query ambiguity: {last_ambiguity}" if last_ambiguity else ""
    raise AssertionError(f"session {session!r} did not become ready{suffix}")


def wait_for_session_gone(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    *,
    timeout: float = 15,
) -> None:
    deadline = time.monotonic() + timeout
    last_ambiguity: BaseException | None = None
    while time.monotonic() < deadline:
        try:
            if query_session(binary, env, session).state == "absent":
                return
            last_ambiguity = None
        except AssertionError as error:
            last_ambiguity = error
        time.sleep(0.1)
    suffix = f"; last query ambiguity: {last_ambiguity}" if last_ambiguity else ""
    raise AssertionError(
        f"session {session!r} remained active or ambiguous after exact kill{suffix}"
    )


def tab_state(tabs: list[dict[str, object]]) -> list[dict[str, object]]:
    """Canonical full tab inventory, including focus/selection and identities."""
    normalized: list[dict[str, object]] = []
    for tab in tabs:
        tab_id = tab.get("tab_id")
        name = tab.get("name")
        require(
            isinstance(tab_id, int) and isinstance(name, str),
            f"invalid tab inventory entry: {tab!r}",
        )
        require(
            isinstance(tab.get("active"), bool),
            f"tab inventory omitted focus/selection state: {tab!r}",
        )
        normalized.append(json.loads(canonical_json(tab)))
    return sorted(normalized, key=lambda tab: (int(tab["tab_id"]), str(tab["name"])))


def wait_for_stable_tab_state(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    *,
    stable_for: float = 0.5,
) -> list[dict[str, object]]:
    deadline = time.monotonic() + 15
    previous: list[dict[str, object]] | None = None
    stable_since = time.monotonic()
    while time.monotonic() < deadline:
        current = tab_state(wait_for_tabs(binary, env, session))
        if current != previous:
            previous = current
            stable_since = time.monotonic()
        elif time.monotonic() - stable_since >= stable_for:
            return current
        time.sleep(0.1)
    raise AssertionError(f"full tab state for {session!r} did not stabilize")


def runtime_state_snapshot(
    binary: pathlib.Path,
    env: dict[str, str],
    sessions: set[str],
    control_plane: pathlib.Path,
) -> dict[str, object]:
    session_state: dict[str, object] = {}
    for session in sorted(sessions):
        query = query_session(binary, env, session)
        if query.state == "absent":
            session_state[session] = {
                "state": "absent",
                "list_tabs_exit": query.list_tabs_exit,
            }
        else:
            session_state[session] = {
                "state": "live",
                "tabs": wait_for_stable_tab_state(binary, env, session),
            }
    snapshot = {
        "session_inventory": session_inventory(binary, env),
        "sessions": session_state,
        "server_processes": server_processes_for_socket_root(
            pathlib.Path(env["VC_FRAME_SOCKET_DIR"])
        ),
        "control_plane": artifact_tree_snapshot(control_plane),
    }
    return {
        "digest": sha256_bytes(canonical_json(snapshot).encode()),
        "state": snapshot,
    }


def tab_identity(
    binary: pathlib.Path, env: dict[str, str], session: str, name: str
) -> int:
    matches = [
        tab for tab in wait_for_tabs(binary, env, session) if tab.get("name") == name
    ]
    require(
        len(matches) == 1,
        f"expected one tab named {name!r} in {session!r}, got {len(matches)}",
    )
    tab_id = matches[0].get("tab_id")
    require(isinstance(tab_id, int), f"tab {session}/{name} has no integer id")
    return tab_id


def typed_tab_identity(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    name: str,
    tab_id: int,
) -> dict[str, object]:
    matches = [
        tab
        for tab in wait_for_tabs(binary, env, session)
        if tab.get("name") == name and tab.get("tab_id") == tab_id
    ]
    require(
        len(matches) == 1,
        f"expected one typed tab {session}/{name} id={tab_id}, got {matches!r}",
    )
    identity = matches[0]
    incarnation = identity.get("session_incarnation")
    instance = identity.get("tab_instance_id")
    require(
        isinstance(incarnation, str) and bool(incarnation),
        f"tab {session}/{name} has no session incarnation",
    )
    require(
        isinstance(instance, str)
        and len(instance) == 32
        and all(character in "0123456789abcdef" for character in instance),
        f"tab {session}/{name} has no typed instance id: {instance!r}",
    )
    return {
        "session": session,
        "name": name,
        "id": tab_id,
        "session_incarnation": incarnation,
        "tab_instance_id": instance,
    }


def terminal_capture_identity(
    session: str, tab_identity_value: dict[str, object], pane_id: int
) -> str:
    return (
        f"session={session};tab_id={tab_identity_value['id']};"
        f"tab_instance_id={tab_identity_value['tab_instance_id']};"
        f"pane_id=terminal_{pane_id}"
    )


def terminal_panes(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    tab_id: int,
) -> list[int]:
    result = command(
        binary,
        env,
        "-s",
        session,
        "action",
        "list-panes",
        "--json",
        "--all",
        "--tab",
        "--state",
    )
    panes = parse_json_array(result.stdout, f"pane inventory for {session!r}")
    terminal_ids = [
        pane["id"]
        for pane in panes
        if pane.get("is_plugin") is False
        and pane.get("tab_id") == tab_id
        and isinstance(pane.get("id"), int)
    ]
    return sorted(terminal_ids)


def marker_layout_for_markers(markers: list[str]) -> str:
    require(bool(markers), "marker layout needs at least one terminal pane")
    panes: list[str] = []
    for marker in markers:
        require(
            marker.replace("-", "").isalnum(),
            f"unsafe fixture marker: {marker!r}",
        )
        panes.append(
            'pane command="/bin/sh" {\n'
            f'args "-c" "printf \'{marker}\'; exec sleep 300"'
            "\n}"
        )
    if len(panes) == 1:
        return f"layout {{\n{panes[0]}\n}}"
    return 'layout {\npane split_direction="vertical" {\n' + "\n".join(panes) + "\n}\n}"


def marker_layout(marker: str, pane_count: int = 1) -> str:
    return marker_layout_for_markers([marker] * pane_count)


def create_session(binary: pathlib.Path, env: dict[str, str], session: str) -> None:
    require(
        query_session(binary, env, session).state == "absent",
        f"refusing to adopt pre-existing session {session!r}",
    )
    command(binary, env, "attach", "--create-background", session)
    # A detached/background session can truthfully expose an empty bootstrap
    # pane inventory. Session readiness is therefore proven through list-tabs;
    # the first marker tab below separately proves terminal-pane usability.
    wait_for_stable_tab_state(binary, env, session)


def create_marker_tab(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    name: str,
    marker: str,
    *,
    pane_count: int = 1,
) -> tuple[int, list[int]]:
    return create_marker_tab_with_markers(
        binary,
        env,
        session,
        name,
        [marker] * pane_count,
    )


def create_marker_tab_with_markers(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    name: str,
    markers: list[str],
) -> tuple[int, list[int]]:
    command(
        binary,
        env,
        "-s",
        session,
        "action",
        "new-tab",
        "--name",
        name,
        "--layout-string",
        marker_layout_for_markers(markers),
    )
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            tab_id = tab_identity(binary, env, session, name)
            panes = terminal_panes(binary, env, session, tab_id)
            if len(panes) == len(markers):
                return tab_id, panes
        except AssertionError:
            pass
        time.sleep(0.1)
    raise AssertionError(
        f"tab {session}/{name} did not materialize {len(markers)} terminal pane(s)"
    )


def wait_for_marker(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    pane_id: int,
    marker: str,
    probe_root: pathlib.Path,
) -> bytes:
    probe_root.mkdir(parents=True, exist_ok=True)
    safe_session = "".join(
        character if character.isalnum() else "-" for character in session
    )
    destination = probe_root / f"{safe_session}-terminal-{pane_id}.txt"
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        destination.unlink(missing_ok=True)
        result = command(
            binary,
            env,
            "-s",
            session,
            "action",
            "dump-screen",
            "--full",
            "--path",
            str(destination),
            "--pane-id",
            str(pane_id),
            expect_success=None,
        )
        if result.returncode == 0 and destination.is_file():
            contents = destination.read_bytes()
            if marker.encode() in contents:
                return contents
        time.sleep(0.1)
    raise AssertionError(
        f"marker {marker!r} never appeared in {session}/terminal_{pane_id}"
    )


def wait_for_marker_assignment(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    pane_ids: list[int],
    markers: list[str],
    probe_root: pathlib.Path,
) -> dict[str, int]:
    """Map distinct markers to panes without assuming pane-id/layout ordering."""
    require(len(pane_ids) == len(markers), "pane/marker assignment size mismatch")
    require(len(set(markers)) == len(markers), "assignment markers must be unique")
    probe_root.mkdir(parents=True, exist_ok=True)
    safe_session = "".join(
        character if character.isalnum() else "-" for character in session
    )
    deadline = time.monotonic() + 15
    last_assignment: dict[str, int] = {}
    while time.monotonic() < deadline:
        assignment: dict[str, int] = {}
        for pane_id in pane_ids:
            destination = (
                probe_root / f"{safe_session}-assignment-terminal-{pane_id}.txt"
            )
            destination.unlink(missing_ok=True)
            result = command(
                binary,
                env,
                "-s",
                session,
                "action",
                "dump-screen",
                "--full",
                "--path",
                str(destination),
                "--pane-id",
                str(pane_id),
                expect_success=None,
            )
            if result.returncode != 0 or not destination.is_file():
                continue
            contents = destination.read_bytes()
            present = [marker for marker in markers if marker.encode() in contents]
            require(
                len(present) <= 1,
                f"pane terminal_{pane_id} contains multiple fixture markers: {present!r}",
            )
            if present:
                marker = present[0]
                require(
                    marker not in assignment,
                    f"fixture marker {marker!r} appeared in multiple panes",
                )
                assignment[marker] = pane_id
        last_assignment = assignment
        if len(assignment) == len(markers):
            return assignment
        time.sleep(0.1)
    raise AssertionError(
        f"distinct marker assignment never stabilized in {session!r}: "
        f"{last_assignment!r}"
    )


def write_error_category(stderr: str) -> str | None:
    lowered = stderr.lower()
    if "is a directory" in lowered or "os error 21" in lowered:
        return "destination_is_directory"
    return None


def record_negative_probe(
    recorder: EvidenceRecorder,
    *,
    scenario: str,
    result: subprocess.CompletedProcess[str],
    before: dict[str, object],
    after: dict[str, object],
    error_category: str,
    durable_failure_audit: dict[str, object] | None = None,
) -> None:
    unchanged = before == after
    before_state = before.get("state")
    after_state = after.get("state")
    require(
        isinstance(before_state, dict) and isinstance(after_state, dict),
        f"{scenario} snapshots omitted runtime state",
    )
    before_non_durable = {
        key: value for key, value in before_state.items() if key != "control_plane"
    }
    after_non_durable = {
        key: value for key, value in after_state.items() if key != "control_plane"
    }
    non_durable_unchanged = before_non_durable == after_non_durable
    state_contract_satisfied = (
        non_durable_unchanged if durable_failure_audit is not None else unchanged
    )
    recorder.append(
        "negative_probes",
        {
            "scenario": scenario,
            "exit_code": result.returncode,
            "stdout": result.stdout,
            "stderr": result.stderr,
            "error_category": error_category,
            "before": before,
            "after": after,
            "state_unchanged": unchanged,
            "non_durable_state_unchanged": non_durable_unchanged,
            "durable_failure_audit": durable_failure_audit,
            "state_contract_satisfied": state_contract_satisfied,
        },
    )
    require(result.returncode != 0, f"{scenario} exited zero")
    require(
        state_contract_satisfied,
        f"{scenario} changed tab focus/inventory or unallowlisted durable artifacts",
    )


def failed_transfer_audit_evidence(
    control_plane: pathlib.Path,
    *,
    run: str,
    exit_code: int,
    origin_session: str,
    before: dict[str, object],
    after: dict[str, object],
) -> dict[str, object]:
    """Prove a failed triage wrote only its typed audit receipt and lock."""
    before_state = before.get("state")
    after_state = after.get("state")
    require(
        isinstance(before_state, dict) and isinstance(after_state, dict),
        f"{run} failure snapshots omitted state",
    )
    before_artifacts = before_state.get("control_plane")
    after_artifacts = after_state.get("control_plane")
    require(
        isinstance(before_artifacts, dict) and isinstance(after_artifacts, dict),
        f"{run} failure snapshots omitted control-plane state",
    )
    before_files_raw = before_artifacts.get("files")
    after_files_raw = after_artifacts.get("files")
    require(
        isinstance(before_files_raw, list) and isinstance(after_files_raw, list),
        f"{run} failure snapshots omitted artifact inventories",
    )

    def by_path(raw_files: list[object]) -> dict[str, dict[str, object]]:
        indexed: dict[str, dict[str, object]] = {}
        for raw in raw_files:
            require(isinstance(raw, dict), f"{run} artifact entry is not an object")
            path = raw.get("path")
            require(isinstance(path, str), f"{run} artifact entry omitted path")
            indexed[path] = raw
        return indexed

    before_files = by_path(before_files_raw)
    after_files = by_path(after_files_raw)
    run_prefix = f"finished_runs/{run}"
    receipt_relative = f"{run_prefix}/transfer.json"
    lock_relative = f"{run_prefix}/transfer.lock"
    allowed_delta = {receipt_relative, lock_relative}
    changed_paths = {
        path
        for path in set(before_files) | set(after_files)
        if before_files.get(path) != after_files.get(path)
    }
    require(
        changed_paths == allowed_delta,
        f"{run} failed triage changed artifacts outside its exact audit pair: "
        f"{sorted(changed_paths)!r}",
    )
    require(
        receipt_relative not in before_files and lock_relative not in before_files,
        f"{run} failure audit unexpectedly existed before the probe",
    )
    lock_entry = after_files.get(lock_relative)
    require(
        isinstance(lock_entry, dict)
        and lock_entry.get("bytes") == 0
        and lock_entry.get("sha256") == sha256_bytes(b""),
        f"{run} failure lock was not an empty durable lock file",
    )

    run_directory = control_plane / "finished_runs" / run
    receipt_path = run_directory / "transfer.json"
    receipt_contents = receipt_path.read_bytes()
    receipt = json.loads(receipt_contents)
    require(isinstance(receipt, dict), f"{run} failure receipt is not an object")
    expected_fields = {
        "version": 4,
        "run": run,
        "exit_code": exit_code,
        "origin_session": origin_session,
        "origin_tab": run,
        "capture": None,
        "capture_committed": False,
        "metadata_committed": False,
        "viewer_confirmed": False,
        "viewer_tab_identity": None,
        "viewer_creation_pending": False,
        "origin_tab_state": "preserved",
    }
    for field, expected in expected_fields.items():
        require(
            receipt.get(field) == expected,
            f"{run} failure receipt {field!r} mismatch: "
            f"expected={expected!r}, actual={receipt.get(field)!r}",
        )
    fault = receipt.get("fault")
    require(
        isinstance(fault, str) and fault.startswith("Capture:"),
        f"{run} failure receipt did not retain a Capture fault: {fault!r}",
    )
    viewer_token = receipt.get("viewer_token")
    require(
        isinstance(viewer_token, str)
        and len(viewer_token) == 32
        and all(character in "0123456789abcdef" for character in viewer_token),
        f"{run} failure receipt has invalid attempt token: {viewer_token!r}",
    )
    forbidden = {
        "scrollback.txt",
        "capture.manifest.json",
        f"{run}.meta.json",
    }
    present_forbidden = sorted(
        path.name for path in run_directory.iterdir() if path.name in forbidden
    )
    require(
        not present_forbidden,
        f"{run} failed triage committed success artifacts: {present_forbidden!r}",
    )
    return {
        "allowed_changed_paths": sorted(allowed_delta),
        "receipt_path": str(receipt_path.resolve()),
        "receipt_bytes": len(receipt_contents),
        "receipt_sha256": sha256_bytes(receipt_contents),
        "receipt": receipt_projection(receipt),
        "lock": lock_entry,
        "success_artifacts_absent": True,
        "fault_stage": "Capture",
    }


def triage_arguments(
    run: str,
    exit_code: int,
    origin_session: str,
    *,
    bucket: str | None = None,
    pane_id: int | None = None,
    transcript: pathlib.Path | None = None,
) -> list[str]:
    args = [
        "triage-run",
        "--run",
        run,
        "--exit-code",
        str(exit_code),
        "--origin-session",
        origin_session,
        "--origin-tab",
        run,
    ]
    if bucket is not None:
        args += ["--bucket", bucket]
    if pane_id is not None:
        args += ["--pane-id", f"terminal_{pane_id}"]
    if transcript is not None:
        args += ["--runtime-transcript", str(transcript)]
    return args


def triage(
    binary: pathlib.Path,
    env: dict[str, str],
    run: str,
    exit_code: int,
    origin_session: str,
    *,
    bucket: str | None = None,
    pane_id: int | None = None,
    transcript: pathlib.Path | None = None,
    expect_success: bool = True,
) -> subprocess.CompletedProcess[str]:
    args = triage_arguments(
        run,
        exit_code,
        origin_session,
        bucket=bucket,
        pane_id=pane_id,
        transcript=transcript,
    )
    return command(binary, env, *args, expect_success=expect_success)


def write_runtime_transcript_manifest(
    transcript: pathlib.Path,
    *,
    run: str,
    ownership_root: pathlib.Path,
) -> pathlib.Path:
    transcript = transcript.resolve()
    ownership_root = ownership_root.resolve()
    require(
        transcript.is_relative_to(ownership_root),
        f"runtime transcript escaped its ownership root: {transcript}",
    )
    contents = transcript.read_bytes()
    manifest = pathlib.Path(f"{transcript}.manifest.json")
    manifest.write_text(
        json.dumps(
            {
                "version": 1,
                "run_id": run,
                "transcript": str(transcript),
                "root": str(ownership_root),
                "bytes": len(contents),
                "sha256": sha256_bytes(contents),
            },
            indent=2,
            sort_keys=True,
        )
        + "\n",
        encoding="utf-8",
    )
    return manifest


def read_json_object_if_ready(path: pathlib.Path) -> dict[str, object] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def process_state(pid: int) -> str | None:
    result = subprocess.run(
        ["ps", "-o", "state=", "-p", str(pid)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    state = result.stdout.strip()
    return state or None


def process_group_members(group_id: int) -> list[dict[str, object]]:
    result = subprocess.run(
        ["ps", "-axo", "pid=,pgid=,state=,command="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    require(
        result.returncode == 0,
        f"cannot inspect owned process group {group_id}: {result.stderr.strip()}",
    )
    members: list[dict[str, object]] = []
    for raw_line in result.stdout.splitlines():
        fields = raw_line.strip().split(maxsplit=3)
        if len(fields) < 3:
            continue
        try:
            pid = int(fields[0])
            pgid = int(fields[1])
        except ValueError:
            continue
        if pgid == group_id:
            members.append(
                {
                    "pid": pid,
                    "pgid": pgid,
                    "state": fields[2],
                    "command": fields[3] if len(fields) == 4 else "",
                }
            )
    return members


def wait_for_process_group_gone(group_id: int, timeout: float = 5) -> None:
    deadline = time.monotonic() + timeout
    last_members: list[dict[str, object]] = []
    while time.monotonic() < deadline:
        last_members = process_group_members(group_id)
        if not last_members:
            return
        time.sleep(0.01)
    raise AssertionError(
        f"owned interrupted process group {group_id} left members: {last_members!r}"
    )


def wait_for_process_stop(
    process: subprocess.Popen[bytes],
    *,
    timeout: float = 5,
    reassert_stop: bool = False,
    signal_process_group: bool = False,
) -> None:
    deadline = time.monotonic() + timeout
    last_state: str | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(
                f"killpoint process {process.pid} exited before SIGSTOP"
            )
        if reassert_stop:
            try:
                if signal_process_group:
                    # The leader is still an owned, unreaped Popen child and
                    # start_new_session made its PID the fresh group ID.
                    os.killpg(process.pid, signal.SIGSTOP)
                else:
                    # Signal only the still-owned, unreaped Popen child. A
                    # group keyed by a dead PID can later identify unrelated
                    # work, so group signalling is explicit and opt-in.
                    process.send_signal(signal.SIGSTOP)
            except ProcessLookupError as error:
                raise AssertionError(
                    f"killpoint process {process.pid} exited before SIGSTOP"
                ) from error
        state = process_state(process.pid)
        if state is None:
            raise AssertionError(
                f"killpoint process {process.pid} exited before SIGSTOP"
            )
        last_state = state
        if "T" in state:
            return
        time.sleep(0.001)
    raise AssertionError(
        f"killpoint process {process.pid} did not enter stopped state "
        f"(last state: {last_state!r})"
    )


def interrupt_process_at_state(
    binary: pathlib.Path,
    env: dict[str, str],
    args: list[str],
    *,
    scenario: str,
    artifact_root: pathlib.Path,
    observe: Callable[[], dict[str, object] | None],
    before_interrupt: Callable[[dict[str, object]], dict[str, object]] | None = None,
    signal_process_group: bool = False,
    slice_seconds: float = 0.0005,
    max_slices: int = 20_000,
) -> InterruptedProcess:
    """Time-slice one owned child and interrupt only after durable state is seen.

    Group signalling is opt-in and safe only because ``start_new_session``
    creates a fresh group whose still-live leader is the owned ``Popen`` child.
    """
    artifact_root.mkdir(parents=True, exist_ok=True)
    stdout_path = artifact_root / f"{scenario}.stdout.log"
    stderr_path = artifact_root / f"{scenario}.stderr.log"
    process: subprocess.Popen[bytes] | None = None
    observed: dict[str, object] | None = None
    slices = 0
    with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            [
                "/bin/sh",
                "-c",
                'kill -STOP "$$"; exec "$@"',
                f"vc-frame-{scenario}",
                str(binary),
                *args,
            ],
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            start_new_session=True,
        )
        try:
            wait_for_process_stop(process)
            for slices in range(1, max_slices + 1):
                if signal_process_group:
                    os.killpg(process.pid, signal.SIGCONT)
                else:
                    process.send_signal(signal.SIGCONT)
                time.sleep(slice_seconds)
                if process.poll() is not None:
                    break
                wait_for_process_stop(
                    process,
                    reassert_stop=True,
                    signal_process_group=signal_process_group,
                )
                observed = observe()
                if observed is not None:
                    if process.poll() is not None:
                        break
                    if before_interrupt is not None:
                        observed = before_interrupt(observed)
                    if signal_process_group:
                        os.killpg(process.pid, signal.SIGKILL)
                    else:
                        process.kill()
                    break
            else:
                raise AssertionError(
                    f"{scenario} did not reach its killpoint after {max_slices} slices"
                )
            if observed is None:
                returncode = process.wait(timeout=5)
                raise AssertionError(
                    f"{scenario} exited before its killpoint: exit={returncode}"
                )
            returncode = process.wait(timeout=5)
        finally:
            if process.poll() is None:
                try:
                    if signal_process_group:
                        os.killpg(process.pid, signal.SIGKILL)
                    else:
                        process.kill()
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
    if signal_process_group:
        wait_for_process_group_gone(process.pid)
    stdout_text = stdout_path.read_text(encoding="utf-8", errors="replace")
    stderr_text = stderr_path.read_text(encoding="utf-8", errors="replace")
    require(
        returncode == -signal.SIGKILL,
        f"{scenario} was not terminated at its controlled killpoint: {returncode}",
    )
    return InterruptedProcess(
        pid=process.pid,
        slices=slices,
        signal="SIGKILL",
        observed_state=observed,
        stdout_path=str(stdout_path.resolve()),
        stderr_path=str(stderr_path.resolve()),
        stdout=stdout_text,
        stderr=stderr_text,
        returncode=returncode,
    )


def transfer_files(
    control_plane: pathlib.Path, run: str
) -> tuple[pathlib.Path, dict[str, object], dict[str, object]]:
    capture_dir = control_plane / "finished_runs" / run
    scrollback = capture_dir / "scrollback.txt"
    metadata = json.loads((capture_dir / "meta.json").read_text())
    receipt = json.loads((capture_dir / "transfer.json").read_text())
    require(isinstance(metadata, dict), f"{run} metadata is not an object")
    require(isinstance(receipt, dict), f"{run} receipt is not an object")
    return scrollback, metadata, receipt


def capture_commit_snapshot(
    control_plane: pathlib.Path, run: str
) -> dict[str, object] | None:
    capture_dir = control_plane / "finished_runs" / run
    scrollback = capture_dir / "scrollback.txt"
    manifest_path = capture_dir / "capture.manifest.json"
    if not scrollback.is_file() or not manifest_path.is_file():
        return None
    contents = scrollback.read_bytes()
    manifest_contents = manifest_path.read_bytes()
    try:
        manifest = json.loads(manifest_contents)
    except json.JSONDecodeError:
        return None
    if not contents or not isinstance(manifest, dict):
        return None
    receipt = read_json_object_if_ready(capture_dir / "transfer.json")
    metadata = read_json_object_if_ready(capture_dir / "meta.json")
    return {
        "capture_path": str(scrollback.resolve()),
        "capture_bytes": len(contents),
        "capture_sha256": sha256_bytes(contents),
        "manifest_path": str(manifest_path.resolve()),
        "manifest_bytes": len(manifest_contents),
        "manifest_sha256": sha256_bytes(manifest_contents),
        "manifest": manifest,
        "receipt": receipt,
        "metadata": metadata,
    }


def capture_receipt_killpoint_state(
    control_plane: pathlib.Path, run: str
) -> dict[str, object] | None:
    snapshot = capture_commit_snapshot(control_plane, run)
    if snapshot is None:
        return None
    receipt = snapshot.get("receipt")
    if (
        isinstance(receipt, dict)
        and receipt.get("version") == 4
        and receipt.get("capture_committed") is True
        and receipt.get("viewer_confirmed") is False
        and receipt.get("viewer_creation_pending") is False
        and receipt.get("viewer_tab_identity") is None
        and receipt.get("origin_tab_state") == "preserved"
    ):
        return snapshot
    return None


def pending_viewer_reservation_killpoint_state(
    control_plane: pathlib.Path, run: str
) -> dict[str, object] | None:
    snapshot = capture_commit_snapshot(control_plane, run)
    if snapshot is None:
        return None
    receipt = snapshot.get("receipt")
    if not isinstance(receipt, dict):
        return None
    token = receipt.get("viewer_token")
    if (
        receipt.get("version") == 4
        and receipt.get("capture_committed") is True
        and receipt.get("metadata_committed") is True
        and receipt.get("viewer_confirmed") is False
        and receipt.get("viewer_creation_pending") is True
        and receipt.get("viewer_creation_generation") == 1
        and receipt.get("viewer_tab_identity") is None
        and receipt.get("origin_tab_state") == "preserved"
        and receipt.get("fault") is None
        and isinstance(token, str)
        and len(token) == 32
        and all(character in "0123456789abcdef" for character in token)
    ):
        return snapshot
    return None


def pending_empty_viewer_killpoint_state(
    binary: pathlib.Path,
    env: dict[str, str],
    control_plane: pathlib.Path,
    *,
    run: str,
    drawer: str,
) -> dict[str, object] | None:
    snapshot = pending_viewer_reservation_killpoint_state(control_plane, run)
    if snapshot is None:
        return None
    receipt = snapshot.get("receipt")
    require(isinstance(receipt, dict), f"{run} pending receipt disappeared")
    token = receipt.get("viewer_token")
    require(isinstance(token, str), f"{run} pending receipt lost its token")
    viewer_name = f"{run} [vc:{token}]"
    matches = [
        tab
        for tab in wait_for_tabs(binary, env, drawer)
        if tab.get("name") == viewer_name
    ]
    require(
        len(matches) <= 1,
        f"{run} pending reservation already has duplicate viewers: {matches!r}",
    )
    if matches:
        return None
    # The triage group is stopped while this observer runs. Give any NewTab
    # already delivered to the independent server time to become visible; the
    # synthetic empty reservation below is valid only when no writer is in
    # flight from the interrupted client.
    time.sleep(0.05)
    settled_matches = [
        tab
        for tab in wait_for_tabs(binary, env, drawer)
        if tab.get("name") == viewer_name
    ]
    require(
        len(settled_matches) <= 1,
        f"{run} pending reservation settled into duplicate viewers: "
        f"{settled_matches!r}",
    )
    return snapshot if not settled_matches else None


def viewer_confirmation_killpoint_state(
    control_plane: pathlib.Path, run: str
) -> dict[str, object] | None:
    snapshot = capture_commit_snapshot(control_plane, run)
    if snapshot is None:
        return None
    receipt = snapshot.get("receipt")
    if (
        isinstance(receipt, dict)
        and receipt.get("version") == 4
        and receipt.get("capture_committed") is True
        and receipt.get("metadata_committed") is True
        and receipt.get("viewer_confirmed") is True
        and receipt.get("origin_tab_state") == "preserved"
    ):
        return snapshot
    return None


def materialize_empty_reserved_viewer(
    binary: pathlib.Path,
    env: dict[str, str],
    snapshot: dict[str, object],
    *,
    run: str,
    drawer: str,
) -> dict[str, object]:
    """Create the exact durable reservation with plugins but no terminal pane."""
    receipt = snapshot.get("receipt")
    require(isinstance(receipt, dict), f"{run} killpoint has no receipt")
    token = receipt.get("viewer_token")
    require(
        isinstance(token, str)
        and len(token) == 32
        and all(character in "0123456789abcdef" for character in token),
        f"{run} killpoint has an invalid viewer token: {token!r}",
    )
    viewer_name = f"{run} [vc:{token}]"
    matches = [
        tab
        for tab in wait_for_tabs(binary, env, drawer)
        if tab.get("name") == viewer_name
    ]
    require(
        not matches,
        f"{run} reached its empty-viewer materialization with an existing viewer: "
        f"{matches!r}",
    )
    layout = (
        f'layout vc_tab_instance_id="{token}" {{\n'
        "    pane {\n"
        '        plugin location="compact-bar"\n'
        "    }\n"
        "}\n"
    )
    command(
        binary,
        env,
        "-s",
        drawer,
        "action",
        "new-tab",
        "--name",
        viewer_name,
        "--layout-string",
        layout,
    )
    viewer_id = tab_identity(binary, env, drawer, viewer_name)
    identity = typed_tab_identity(binary, env, drawer, viewer_name, viewer_id)
    require(
        identity.get("tab_instance_id") == token,
        f"{run} empty viewer did not retain its reservation token: {identity!r}",
    )
    panes = terminal_panes(binary, env, drawer, viewer_id)
    require(
        panes == [],
        f"{run} empty viewer unexpectedly has terminal panes: {panes!r}",
    )
    enriched = dict(snapshot)
    enriched["empty_viewer_identity"] = identity
    enriched["empty_viewer_terminal_panes"] = panes
    return enriched


def interrupted_process_evidence(result: InterruptedProcess) -> dict[str, object]:
    return {
        "pid": result.pid,
        "slices": result.slices,
        "signal": result.signal,
        "observed_state": result.observed_state,
        "stdout_path": result.stdout_path,
        "stderr_path": result.stderr_path,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "returncode": result.returncode,
    }


def assert_viewer_inventory(
    binary: pathlib.Path,
    env: dict[str, str],
    receipt: dict[str, object],
    *,
    run: str,
    drawer: str,
) -> dict[str, object]:
    require(receipt.get("version") == 4, f"{run} receipt is not the v4 contract")
    token = receipt.get("viewer_token")
    require(
        isinstance(token, str)
        and len(token) == 32
        and all(character in "0123456789abcdef" for character in token),
        f"{run} has an invalid viewer ownership token: {token!r}",
    )
    identity = receipt.get("viewer_tab_identity")
    require(isinstance(identity, dict), f"{run} has no typed viewer identity")
    expected_name = f"{run} [vc:{token}]"
    require(
        identity.get("session") == drawer,
        f"{run} viewer identity points at the wrong drawer",
    )
    require(
        identity.get("name") == expected_name,
        f"{run} viewer identity has the wrong owned name",
    )
    viewer_id = identity.get("id")
    require(
        isinstance(viewer_id, int) and not isinstance(viewer_id, bool),
        f"{run} viewer identity has no stable integer id",
    )
    incarnation = identity.get("session_incarnation")
    require(
        isinstance(incarnation, str) and bool(incarnation),
        f"{run} viewer identity has no session incarnation",
    )
    instance = identity.get("tab_instance_id")
    require(
        isinstance(instance, str)
        and len(instance) == 32
        and all(character in "0123456789abcdef" for character in instance),
        f"{run} viewer identity has no typed instance id",
    )

    drawer_tabs = wait_for_tabs(binary, env, drawer)
    matches = [
        tab
        for tab in drawer_tabs
        if tab.get("tab_id") == viewer_id
        and tab.get("name") == expected_name
        and tab.get("session_incarnation") == incarnation
        and tab.get("tab_instance_id") == instance
    ]
    require(
        len(matches) == 1
        and sum(tab.get("name") == expected_name for tab in drawer_tabs) == 1,
        f"{run} does not have exactly one owned viewer in {drawer!r}: "
        f"identity={identity!r}, tabs={drawer_tabs!r}",
    )
    return identity


def verify_transfer(
    binary: pathlib.Path,
    env: dict[str, str],
    control_plane: pathlib.Path,
    *,
    run: str,
    exit_code: int,
    expected_bucket: str,
    expected_source: str,
    expected_identity: str,
    expected_bytes: bytes | None = None,
    marker: str | None = None,
    forbidden_markers: tuple[str, ...] = (),
) -> tuple[bytes, dict[str, object]]:
    scrollback, metadata, receipt = transfer_files(control_plane, run)
    contents = scrollback.read_bytes()
    manifest_path = scrollback.with_name("capture.manifest.json")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    require(isinstance(manifest, dict), f"{run} capture manifest is not an object")
    manifest_evidence = manifest.get("evidence")
    require(
        isinstance(manifest_evidence, dict),
        f"{run} capture manifest lacks evidence",
    )
    require(contents, f"{run} committed an empty scrollback")
    if marker is not None:
        require(marker.encode() in contents, f"{run} captured a foreign pane")
    for forbidden in forbidden_markers:
        require(
            forbidden.encode() not in contents,
            f"{run} captured sibling/foreign marker {forbidden!r}",
        )
    if expected_bytes is not None:
        require(contents == expected_bytes, f"{run} did not copy the exact transcript")
    digest = sha256_bytes(contents)
    for document_name, document in (("metadata", metadata), ("receipt", receipt)):
        require(document.get("run") == run, f"{run} {document_name} has wrong run")
        require(
            document.get("exit_code") == exit_code,
            f"{run} {document_name} has wrong exit code",
        )
        require(
            document.get("bucket") == expected_bucket,
            f"{run} {document_name} has wrong bucket: {document.get('bucket')!r}",
        )
    require(
        metadata.get("capture_source") == expected_source,
        f"{run} has wrong capture source",
    )
    require(
        metadata.get("capture_source_identity") == expected_identity,
        f"{run} has wrong capture identity: {metadata.get('capture_source_identity')!r}",
    )
    require(
        metadata.get("capture_bytes") == len(contents), f"{run} has wrong byte count"
    )
    require(metadata.get("capture_sha256") == digest, f"{run} has wrong capture digest")
    require(
        manifest.get("version") == 1
        and manifest.get("run_id") == run
        and manifest.get("session") == receipt.get("origin_session")
        and manifest.get("origin_tab") == receipt.get("origin_tab"),
        f"{run} capture manifest has the wrong transfer identity: {manifest!r}",
    )
    require(
        manifest_evidence.get("capture_source") == expected_source
        and manifest_evidence.get("source_identity") == expected_identity
        and manifest_evidence.get("bytes") == len(contents)
        and manifest_evidence.get("sha256") == digest,
        f"{run} capture manifest does not bind the committed bytes",
    )
    require(
        receipt.get("capture") == manifest_evidence,
        f"{run} receipt capture evidence diverges from capture.manifest.json",
    )
    if expected_source == "terminal_scrollback":
        origin_identity = manifest_evidence.get("origin_tab_identity")
        require(
            isinstance(origin_identity, dict)
            and origin_identity.get("session") == receipt.get("origin_session")
            and origin_identity.get("name") == receipt.get("origin_tab")
            and isinstance(origin_identity.get("session_incarnation"), str)
            and bool(origin_identity.get("session_incarnation"))
            and isinstance(origin_identity.get("tab_instance_id"), str)
            and len(origin_identity.get("tab_instance_id", "")) == 32,
            f"{run} terminal capture lacks typed origin identity",
        )
    require(receipt.get("capture_committed") is True, f"{run} capture is not committed")
    require(
        receipt.get("metadata_committed") is True, f"{run} metadata is not committed"
    )
    require(receipt.get("viewer_confirmed") is True, f"{run} viewer is not confirmed")
    require(
        receipt.get("origin_tab_state") == "closed",
        f"{run} origin is not durably closed",
    )
    drawer = DRAWER_BY_BUCKET[expected_bucket]
    assert_viewer_inventory(binary, env, receipt, run=run, drawer=drawer)
    return contents, receipt


def kill_confirmed_session(
    binary: pathlib.Path, env: dict[str, str], session: str
) -> None:
    query = query_session(binary, env, session)
    require(query.state == "live", f"refusing to kill absent session {session!r}")
    command(binary, env, "kill-session", session)
    wait_for_session_gone(binary, env, session)


def cleanup_namespace(
    binary: pathlib.Path,
    env: dict[str, str],
    owned_targets: set[str],
    *,
    timeout: float = 15,
    stable_empty_for: float = 0.5,
) -> dict[str, object]:
    """Kill exact owned targets, quiesce servers, then delete durable metadata."""
    socket_root = pathlib.Path(env["VC_FRAME_SOCKET_DIR"]).resolve()
    initial = session_inventory(binary, env)
    unexpected = set(initial) - owned_targets
    require(
        not unexpected,
        "refusing broad cleanup of unexpected isolated sessions: "
        + ", ".join(sorted(unexpected)),
    )
    live_process_sessions = {
        path.resolve().name
        for process in server_processes_for_socket_root(socket_root)
        for path in server_argument_paths(str(process["command"]))
        if path.resolve().is_relative_to(socket_root)
    }
    killed: list[str] = []
    for session, state in sorted(initial.items()):
        if state == "exited" and session not in live_process_sessions:
            continue
        if state == "live":
            query = query_session(binary, env, session)
            require(
                query.state == "live",
                f"cleanup inventory said {session!r} was active but exact query said absent",
            )
        command(binary, env, "kill-session", session)
        wait_for_session_gone(binary, env, session, timeout=timeout)
        killed.append(session)

    shutdown_process_residue = wait_for_no_server_processes(
        socket_root, timeout=timeout
    )
    require(
        not shutdown_process_residue,
        "isolated namespace retained server processes before metadata cleanup",
    )

    deleted: list[str] = []
    final_inventory: dict[str, Literal["live", "exited"]] = {}
    deadline = time.monotonic() + timeout
    empty_since: float | None = None
    stable_empty = False
    while time.monotonic() < deadline:
        final_inventory = session_inventory(binary, env)
        unexpected_after_kill = set(final_inventory) - owned_targets
        require(
            not unexpected_after_kill,
            "unexpected session appeared during isolated cleanup: "
            + ", ".join(sorted(unexpected_after_kill)),
        )
        live_sessions = sorted(
            name for name, state in final_inventory.items() if state == "live"
        )
        exited_sessions = sorted(
            name for name, state in final_inventory.items() if state == "exited"
        )
        if exited_sessions:
            empty_since = None
            for session in exited_sessions:
                command(binary, env, "delete-session", session, "--force")
                if session not in deleted:
                    deleted.append(session)
        elif live_sessions:
            empty_since = None
        else:
            now = time.monotonic()
            if empty_since is None:
                empty_since = now
            elif now - empty_since >= stable_empty_for:
                stable_empty = True
                break
        if stable_empty_for > 0:
            time.sleep(0.1)
    require(
        stable_empty,
        "isolated namespace retained session inventory after cleanup: "
        f"{final_inventory!r}",
    )
    process_residue = wait_for_no_server_processes(socket_root, timeout=timeout)
    require(not process_residue, "isolated namespace retained server processes")
    socket_entries = (
        [
            {
                "path": str(path.relative_to(socket_root)),
                "kind": "directory" if path.is_dir() else "residue",
            }
            for path in sorted(socket_root.rglob("*"))
        ]
        if socket_root.exists()
        else []
    )
    socket_residue = [entry for entry in socket_entries if entry["kind"] != "directory"]
    require(
        not socket_residue,
        f"isolated socket root retained socket/file residue: {socket_residue!r}",
    )
    return {
        "initial_session_inventory": initial,
        "killed_sessions": killed,
        "deleted_sessions": deleted,
        "final_session_inventory": {},
        "process_residue": process_residue,
        "socket_entries_after_cleanup": socket_entries,
        "socket_residue_after_cleanup": socket_residue,
    }


def cleanup_failure_snapshot(
    binary: pathlib.Path, env: dict[str, str]
) -> dict[str, object]:
    """Best-effort read-only residue evidence without masking the cleanup error."""
    snapshot: dict[str, object] = {}
    try:
        snapshot["session_inventory"] = session_inventory(binary, env)
    except BaseException as error:
        snapshot["session_inventory_error"] = {
            "type": type(error).__name__,
            "message": str(error),
        }
    socket_root = pathlib.Path(env["VC_FRAME_SOCKET_DIR"]).resolve()
    try:
        snapshot["server_processes"] = server_processes_for_socket_root(socket_root)
    except BaseException as error:
        snapshot["server_process_error"] = {
            "type": type(error).__name__,
            "message": str(error),
        }
    snapshot["socket_root"] = str(socket_root)
    snapshot["socket_entries"] = (
        sorted(str(path.relative_to(socket_root)) for path in socket_root.rglob("*"))
        if socket_root.exists()
        else []
    )
    return snapshot


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    provenance = parser.add_mutually_exclusive_group(required=True)
    provenance.add_argument("--expected-sha")
    provenance.add_argument(
        "--expect-current-checkout-sha",
        action="store_true",
        help="require a clean checkout and use its HEAD as the expected build SHA",
    )
    parser.add_argument("--expected-profile", default="debug")
    parser.add_argument(
        "--artifact-root",
        type=pathlib.Path,
        default=pathlib.Path(
            os.environ.get(
                "VC_FRAME_E2E_ARTIFACT_ROOT",
                "/tmp/vc-frame-triage-runtime-e2e",
            )
        ),
    )
    options = parser.parse_args()
    binary = options.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"binary does not exist: {binary}")

    unique = f"e{os.getpid():x}{time.time_ns() & 0xFFFFFFFF:08x}"
    stamp = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%S.%fZ")
    root = options.artifact_root.expanduser().resolve() / f"{stamp}-{unique}"
    root.mkdir(parents=True, exist_ok=False)
    control_plane = root / "control-plane"
    runtime_root = pathlib.Path(
        tempfile.mkdtemp(prefix=f"vcf-e2e-{unique}-", dir=SHORT_RUNTIME_PARENT)
    ).resolve()
    primary_root = runtime_root / "p"
    restart_root = runtime_root / "r"
    env = isolated_env(primary_root, control_plane)
    restart_env = isolated_env(restart_root, control_plane)
    receipt_path = root / "evidence.json"
    recorder = EvidenceRecorder(
        receipt_path,
        {
            "schema_version": 2,
            "status": "initializing",
            "started_at": utc_now(),
            "fixture_id": unique,
            "binary": str(binary),
            "expected_profile": options.expected_profile,
            "provenance_mode": (
                "current_checkout"
                if options.expect_current_checkout_sha
                else "explicit_sha"
            ),
            "artifact_root": str(root),
            "runtime_root": str(runtime_root),
            "evidence_path": str(receipt_path),
            "namespaces": {
                "primary": {
                    "root": str(primary_root),
                    "socket_root": env["VC_FRAME_SOCKET_DIR"],
                },
                "restart": {
                    "root": str(restart_root),
                    "socket_root": restart_env["VC_FRAME_SOCKET_DIR"],
                },
                "control_plane": str(control_plane),
            },
            "socket_path_budget": {},
            "negative_probes": [],
            "interruption_probes": [],
            "transfers": [],
            "restart": {},
            "cleanup": {},
            "operator_guard": {},
        },
    )
    # Print before the first mutation so a hard interruption still leaves an
    # operator-discoverable receipt path in the command log.
    print(f"triage-runtime-e2e evidence: {receipt_path}", file=sys.stderr, flush=True)

    origin = f"{unique}-origin"
    peer = f"{unique}-peer"
    headless_origin = f"{unique}-headless"
    missing_name = f"{unique}-missing"
    missing_origin_session = f"{unique}-miss"
    empty_origin = f"{unique}-empty"
    drawers = set(DRAWER_BY_BUCKET.values())
    primary_targets = {origin, peer, headless_origin, *drawers}
    restart_targets = {"Finalized runs"}
    primary_session_selectors = primary_targets | {
        missing_name,
        missing_origin_session,
        empty_origin,
    }
    recorder.set(
        "socket_path_budget",
        {
            "primary": socket_path_budget(
                pathlib.Path(env["VC_FRAME_SOCKET_DIR"]),
                primary_session_selectors,
            ),
            "restart": socket_path_budget(
                pathlib.Path(restart_env["VC_FRAME_SOCKET_DIR"]), restart_targets
            ),
        },
    )
    probe_root = root / "probes"
    recorder.set(
        "fixtures",
        {
            "primary_owned_sessions": sorted(primary_targets),
            "restart_owned_sessions": sorted(restart_targets),
            "probe_root": str(probe_root),
            "control_plane": str(control_plane),
        },
    )
    guarded_operator_paths = operator_guard_paths()
    guarded_volatile_paths = operator_guard_volatile_paths()
    operator_guard_before = guarded_tree_snapshot(
        guarded_operator_paths,
        volatile_files=guarded_volatile_paths,
    )
    recorder.set(
        "operator_guard",
        {
            "paths": [str(path) for path in guarded_operator_paths],
            "volatile_identity_only": [
                str(path) for path in sorted(guarded_volatile_paths)
            ],
            "before": guarded_snapshot_summary(operator_guard_before),
        },
    )
    caught: BaseException | None = None
    cleanup_errors: list[str] = []
    cleanup_receipts: dict[str, object] = {}
    mutation_started = False

    try:
        if options.expect_current_checkout_sha:
            expected_sha = current_checkout_sha(
                pathlib.Path(__file__).resolve().parents[1]
            )
        else:
            require(
                isinstance(options.expected_sha, str),
                "explicit provenance mode requires --expected-sha",
            )
            expected_sha = validate_sha(options.expected_sha, "expected SHA")
        recorder.set("expected_sha", expected_sha)

        # Both namespaces are proven empty and the exact binary provenance is
        # proven before any server can be created.
        build_info = namespace_preflight(
            binary,
            env,
            primary_root,
            control_plane,
            expected_sha=expected_sha,
            expected_profile=options.expected_profile,
        )
        restart_build_info = namespace_preflight(
            binary,
            restart_env,
            restart_root,
            control_plane,
            expected_sha=expected_sha,
            expected_profile=options.expected_profile,
        )
        require(
            build_info == restart_build_info, "build provenance changed by namespace"
        )
        recorder.set("build_info", build_info)
        recorder.set("status", "running")
        mutation_started = True

        create_session(binary, env, origin)
        create_session(binary, env, peer)
        origin_canary = f"{unique}-origin-canary"
        peer_canary = f"{unique}-peer-canary"
        origin_marker = f"ORIGIN-{unique}"
        peer_marker = f"PEER-{unique}"
        _origin_tab_id, origin_panes = create_marker_tab(
            binary, env, origin, origin_canary, origin_marker
        )
        peer_canary_id, peer_panes = create_marker_tab(
            binary, env, peer, peer_canary, peer_marker
        )
        origin_pane = origin_panes[0]
        peer_pane = peer_panes[0]
        wait_for_marker(binary, env, origin, origin_pane, origin_marker, probe_root)
        wait_for_marker(binary, env, peer, peer_pane, peer_marker, probe_root)
        # The bootstrap shell can rename its first tab shortly after the marker
        # tab appears. Quiesce that owned initialization before state-invariance
        # negatives so a cosmetic startup rename is not blamed on triage.
        wait_for_stable_tab_state(binary, env, origin, stable_for=2.0)
        wait_for_stable_tab_state(binary, env, peer, stable_for=2.0)
        guard_sessions = {origin, peer}

        before = runtime_state_snapshot(
            binary, env, guard_sessions | {missing_name}, control_plane
        )
        missing_session = command(
            binary,
            env,
            "-s",
            missing_name,
            "action",
            "list-tabs",
            "--json",
            expect_success=False,
        )
        after = runtime_state_snapshot(
            binary, env, guard_sessions | {missing_name}, control_plane
        )
        record_negative_probe(
            recorder,
            scenario="missing_session",
            result=missing_session,
            before=before,
            after=after,
            error_category="missing_session",
        )
        require(
            query_session(binary, env, missing_name).state == "absent",
            "missing-session probe created a session",
        )

        missing_tab_name = f"{unique}-missing-tab"
        before = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        missing_tab = command(
            binary,
            env,
            "-s",
            origin,
            "action",
            "go-to-tab-name",
            missing_tab_name,
            expect_success=False,
        )
        after = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        record_negative_probe(
            recorder,
            scenario="missing_tab",
            result=missing_tab,
            before=before,
            after=after,
            error_category="missing_tab",
        )

        missing_dump = root / "missing-pane.txt"
        before = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        missing_pane = command(
            binary,
            env,
            "-s",
            origin,
            "action",
            "dump-screen",
            "--full",
            "--path",
            str(missing_dump),
            "--pane-id",
            "999999",
            expect_success=False,
        )
        after = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        record_negative_probe(
            recorder,
            scenario="missing_pane",
            result=missing_pane,
            before=before,
            after=after,
            error_category="missing_pane",
        )
        require(not missing_dump.exists(), "missing pane created a trusted dump")

        # Use an inventory-proven live pane and a directory destination so this
        # cannot pass as another missing-pane failure.
        before = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        write_failure = command(
            binary,
            env,
            "-s",
            origin,
            "action",
            "dump-screen",
            "--full",
            "--path",
            str(root),
            "--pane-id",
            str(origin_pane),
            expect_success=False,
        )
        after = runtime_state_snapshot(binary, env, guard_sessions, control_plane)
        category = write_error_category(write_failure.stderr)
        record_negative_probe(
            recorder,
            scenario="dump_write_failure",
            result=write_failure,
            before=before,
            after=after,
            error_category=category or "unclassified",
        )
        require(
            category == "destination_is_directory",
            f"dump failed for the wrong reason: {write_failure.stderr!r}",
        )

        missing_origin_run = f"{unique}-missing-origin"
        before = runtime_state_snapshot(
            binary,
            env,
            guard_sessions | {missing_origin_session},
            control_plane,
        )
        missing_origin = triage(
            binary,
            env,
            missing_origin_run,
            2,
            missing_origin_session,
            expect_success=False,
        )
        after = runtime_state_snapshot(
            binary,
            env,
            guard_sessions | {missing_origin_session},
            control_plane,
        )
        record_negative_probe(
            recorder,
            scenario="missing_origin_without_transcript",
            result=missing_origin,
            before=before,
            after=after,
            error_category="capture_target_missing",
            durable_failure_audit=failed_transfer_audit_evidence(
                control_plane,
                run=missing_origin_run,
                exit_code=2,
                origin_session=missing_origin_session,
                before=before,
                after=after,
            ),
        )
        require(
            "Capture" in missing_origin.stderr,
            f"missing origin failed outside the capture step: {missing_origin.stderr!r}",
        )
        require(
            query_session(binary, env, missing_origin_session).state == "absent",
            "missing-origin/no-transcript probe created or resurrected its origin",
        )

        empty_transcript = root / "empty-runtime-transcript.log"
        empty_transcript.write_bytes(b"")
        empty_run = f"{unique}-empty-transcript"
        empty_transcript_manifest = write_runtime_transcript_manifest(
            empty_transcript,
            run=empty_run,
            ownership_root=root,
        )
        before = runtime_state_snapshot(
            binary, env, guard_sessions | {empty_origin}, control_plane
        )
        empty_fallback = triage(
            binary,
            env,
            empty_run,
            -9,
            empty_origin,
            pane_id=999999,
            transcript=empty_transcript,
            expect_success=False,
        )
        after = runtime_state_snapshot(
            binary, env, guard_sessions | {empty_origin}, control_plane
        )
        record_negative_probe(
            recorder,
            scenario="empty_runtime_transcript",
            result=empty_fallback,
            before=before,
            after=after,
            error_category="empty_runtime_transcript",
            durable_failure_audit=failed_transfer_audit_evidence(
                control_plane,
                run=empty_run,
                exit_code=-9,
                origin_session=empty_origin,
                before=before,
                after=after,
            ),
        )
        require(
            "empty" in empty_fallback.stderr.lower(),
            f"empty transcript failed without an explicit empty-source error: "
            f"{empty_fallback.stderr!r}",
        )
        require(
            empty_transcript_manifest.is_file(),
            "empty transcript probe lost its ownership manifest",
        )

        require(
            tab_identity(binary, env, peer, peer_canary) == peer_canary_id,
            "negative command-boundary probes changed the peer canary identity",
        )
        wait_for_marker(binary, env, origin, origin_pane, origin_marker, probe_root)
        wait_for_marker(binary, env, peer, peer_pane, peer_marker, probe_root)

        capture_interrupt_run = f"{unique}-kill-after-capture"
        capture_interrupt_marker = f"KILLCAPTURE-{unique}"
        capture_interrupt_tab, capture_interrupt_panes = create_marker_tab(
            binary,
            env,
            origin,
            capture_interrupt_run,
            capture_interrupt_marker,
        )
        capture_interrupt_pane = capture_interrupt_panes[0]
        wait_for_marker(
            binary,
            env,
            origin,
            capture_interrupt_pane,
            capture_interrupt_marker,
            probe_root,
        )
        capture_interrupt_identity = typed_tab_identity(
            binary,
            env,
            origin,
            capture_interrupt_run,
            capture_interrupt_tab,
        )
        capture_interrupt_source = terminal_capture_identity(
            origin,
            capture_interrupt_identity,
            capture_interrupt_pane,
        )
        capture_interrupted = interrupt_process_at_state(
            binary,
            env,
            triage_arguments(
                capture_interrupt_run,
                0,
                origin,
                pane_id=capture_interrupt_pane,
            ),
            scenario="after_capture_receipt",
            artifact_root=root / "interruptions",
            observe=lambda: capture_receipt_killpoint_state(
                control_plane, capture_interrupt_run
            ),
        )
        require(
            typed_tab_identity(
                binary,
                env,
                origin,
                capture_interrupt_run,
                capture_interrupt_tab,
            )
            == capture_interrupt_identity,
            "capture killpoint changed or closed the durable origin identity",
        )
        capture_before_recovery = capture_interrupted.observed_state
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_capture_receipt",
                "phase": "interrupted",
                **interrupted_process_evidence(capture_interrupted),
            },
        )
        capture_recovery = triage(
            binary,
            env,
            capture_interrupt_run,
            0,
            origin,
            pane_id=capture_interrupt_pane,
        )
        capture_recovered_bytes, _capture_recovered_receipt = verify_transfer(
            binary,
            env,
            control_plane,
            run=capture_interrupt_run,
            exit_code=0,
            expected_bucket="Finalized",
            expected_source="terminal_scrollback",
            expected_identity=capture_interrupt_source,
            marker=capture_interrupt_marker,
        )
        require(
            sha256_bytes(capture_recovered_bytes)
            == capture_before_recovery.get("capture_sha256"),
            "capture-receipt recovery rewrote durable scrollback",
        )
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_capture_receipt",
                "phase": "recovered",
                "triage_exit": capture_recovery.returncode,
                "transfer": transfer_evidence(
                    control_plane,
                    capture_interrupt_run,
                    "killpoint_recovery",
                ),
            },
        )

        empty_viewer_run = f"{unique}-kill-after-empty-viewer"
        empty_viewer_marker = f"KILLEMPTY-{unique}"
        empty_viewer_origin_tab, empty_viewer_origin_panes = create_marker_tab(
            binary,
            env,
            origin,
            empty_viewer_run,
            empty_viewer_marker,
        )
        empty_viewer_origin_pane = empty_viewer_origin_panes[0]
        wait_for_marker(
            binary,
            env,
            origin,
            empty_viewer_origin_pane,
            empty_viewer_marker,
            probe_root,
        )
        empty_viewer_origin_identity = typed_tab_identity(
            binary,
            env,
            origin,
            empty_viewer_run,
            empty_viewer_origin_tab,
        )
        empty_viewer_source = terminal_capture_identity(
            origin,
            empty_viewer_origin_identity,
            empty_viewer_origin_pane,
        )
        empty_viewer_interrupted = interrupt_process_at_state(
            binary,
            env,
            triage_arguments(
                empty_viewer_run,
                -9,
                origin,
                pane_id=empty_viewer_origin_pane,
            ),
            scenario="after_empty_viewer_reservation",
            artifact_root=root / "interruptions",
            observe=lambda: pending_empty_viewer_killpoint_state(
                binary,
                env,
                control_plane,
                run=empty_viewer_run,
                drawer="Needs attention",
            ),
            before_interrupt=lambda snapshot: materialize_empty_reserved_viewer(
                binary,
                env,
                snapshot,
                run=empty_viewer_run,
                drawer="Needs attention",
            ),
            signal_process_group=True,
            slice_seconds=0.00025,
        )
        require(
            process_state(empty_viewer_interrupted.pid) is None,
            "empty-viewer interruption left its owned triage process alive",
        )
        require(
            typed_tab_identity(
                binary,
                env,
                origin,
                empty_viewer_run,
                empty_viewer_origin_tab,
            )
            == empty_viewer_origin_identity,
            "empty-viewer killpoint changed or closed the durable origin identity",
        )
        empty_viewer_receipt_before = empty_viewer_interrupted.observed_state.get(
            "receipt"
        )
        empty_viewer_identity_before = empty_viewer_interrupted.observed_state.get(
            "empty_viewer_identity"
        )
        require(
            isinstance(empty_viewer_receipt_before, dict)
            and isinstance(empty_viewer_identity_before, dict),
            "empty-viewer killpoint did not preserve its receipt and live identity",
        )
        require(
            empty_viewer_receipt_before.get("viewer_creation_generation") == 1
            and empty_viewer_interrupted.observed_state.get(
                "empty_viewer_terminal_panes"
            )
            == [],
            "empty-viewer killpoint was not the first unready reservation",
        )
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_empty_viewer_reservation",
                "phase": "interrupted",
                **interrupted_process_evidence(empty_viewer_interrupted),
            },
        )
        empty_viewer_recovery = triage(
            binary,
            env,
            empty_viewer_run,
            -9,
            origin,
            pane_id=empty_viewer_origin_pane,
        )
        empty_viewer_recovered_bytes, empty_viewer_receipt_after = verify_transfer(
            binary,
            env,
            control_plane,
            run=empty_viewer_run,
            exit_code=-9,
            expected_bucket="NeedsAttention",
            expected_source="terminal_scrollback",
            expected_identity=empty_viewer_source,
            marker=empty_viewer_marker,
        )
        require(
            sha256_bytes(empty_viewer_recovered_bytes)
            == empty_viewer_interrupted.observed_state.get("capture_sha256"),
            "empty-viewer recovery rewrote durable scrollback",
        )
        require(
            empty_viewer_receipt_after.get("viewer_token")
            == empty_viewer_receipt_before.get("viewer_token")
            and empty_viewer_receipt_after.get("viewer_tab_identity")
            == empty_viewer_identity_before
            and empty_viewer_receipt_after.get("viewer_creation_generation") == 2
            and empty_viewer_receipt_after.get("viewer_creation_pending") is False,
            "empty-viewer recovery changed ownership or skipped generation two",
        )
        empty_viewer_final_id = empty_viewer_identity_before.get("id")
        require(
            isinstance(empty_viewer_final_id, int)
            and terminal_panes(
                binary,
                env,
                "Needs attention",
                empty_viewer_final_id,
            ),
            "empty-viewer recovery did not install a terminal on the same stable tab",
        )
        require(
            all(
                tab.get("name") != empty_viewer_run
                for tab in wait_for_tabs(binary, env, origin)
            ),
            "empty-viewer recovery left the origin tab open",
        )
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_empty_viewer_reservation",
                "phase": "recovered",
                "triage_exit": empty_viewer_recovery.returncode,
                "same_viewer_identity": empty_viewer_identity_before,
                "transfer": transfer_evidence(
                    control_plane,
                    empty_viewer_run,
                    "killpoint_recovery",
                ),
            },
        )

        viewer_interrupt_run = f"{unique}-kill-after-viewer"
        viewer_interrupt_marker = f"KILLVIEWER-{unique}"
        viewer_interrupt_tab, viewer_interrupt_panes = create_marker_tab(
            binary,
            env,
            origin,
            viewer_interrupt_run,
            viewer_interrupt_marker,
        )
        viewer_interrupt_pane = viewer_interrupt_panes[0]
        wait_for_marker(
            binary,
            env,
            origin,
            viewer_interrupt_pane,
            viewer_interrupt_marker,
            probe_root,
        )
        viewer_interrupt_identity = typed_tab_identity(
            binary,
            env,
            origin,
            viewer_interrupt_run,
            viewer_interrupt_tab,
        )
        viewer_interrupt_source = terminal_capture_identity(
            origin,
            viewer_interrupt_identity,
            viewer_interrupt_pane,
        )
        viewer_interrupted = interrupt_process_at_state(
            binary,
            env,
            triage_arguments(
                viewer_interrupt_run,
                -9,
                origin,
                pane_id=viewer_interrupt_pane,
            ),
            scenario="after_viewer_confirmation",
            artifact_root=root / "interruptions",
            observe=lambda: viewer_confirmation_killpoint_state(
                control_plane, viewer_interrupt_run
            ),
            slice_seconds=0.00025,
        )
        require(
            typed_tab_identity(
                binary,
                env,
                origin,
                viewer_interrupt_run,
                viewer_interrupt_tab,
            )
            == viewer_interrupt_identity,
            "viewer-confirmation killpoint closed the durable origin before interruption",
        )
        viewer_receipt_before = viewer_interrupted.observed_state.get("receipt")
        require(
            isinstance(viewer_receipt_before, dict),
            "viewer killpoint did not preserve its receipt",
        )
        viewer_identity_before = assert_viewer_inventory(
            binary,
            env,
            viewer_receipt_before,
            run=viewer_interrupt_run,
            drawer="Needs attention",
        )
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_viewer_confirmation",
                "phase": "interrupted",
                **interrupted_process_evidence(viewer_interrupted),
            },
        )
        viewer_recovery = triage(
            binary,
            env,
            viewer_interrupt_run,
            -9,
            origin,
            pane_id=viewer_interrupt_pane,
        )
        viewer_recovered_bytes, viewer_receipt_after = verify_transfer(
            binary,
            env,
            control_plane,
            run=viewer_interrupt_run,
            exit_code=-9,
            expected_bucket="NeedsAttention",
            expected_source="terminal_scrollback",
            expected_identity=viewer_interrupt_source,
            marker=viewer_interrupt_marker,
        )
        require(
            sha256_bytes(viewer_recovered_bytes)
            == viewer_interrupted.observed_state.get("capture_sha256"),
            "viewer-confirmation recovery rewrote durable scrollback",
        )
        require(
            viewer_receipt_after.get("viewer_token")
            == viewer_receipt_before.get("viewer_token")
            and viewer_receipt_after.get("viewer_tab_identity")
            == viewer_identity_before,
            "viewer-confirmation recovery changed confirmed viewer ownership",
        )
        recorder.append(
            "interruption_probes",
            {
                "scenario": "after_viewer_confirmation",
                "phase": "recovered",
                "triage_exit": viewer_recovery.returncode,
                "transfer": transfer_evidence(
                    control_plane,
                    viewer_interrupt_run,
                    "killpoint_recovery",
                ),
            },
        )

        multi_run = f"{unique}-multi-pane"
        target_marker = f"TARGET-{unique}"
        sibling_marker = f"SIBLING-{unique}"
        multi_tab_id, multi_panes = create_marker_tab_with_markers(
            binary,
            env,
            origin,
            multi_run,
            [target_marker, sibling_marker],
        )
        marker_panes = wait_for_marker_assignment(
            binary,
            env,
            origin,
            multi_panes,
            [target_marker, sibling_marker],
            probe_root,
        )
        target_pane = marker_panes[target_marker]
        sibling_pane = marker_panes[sibling_marker]
        multi_tab_identity = typed_tab_identity(
            binary, env, origin, multi_run, multi_tab_id
        )
        multi_identity = terminal_capture_identity(
            origin, multi_tab_identity, target_pane
        )
        multi_result = triage(
            binary,
            env,
            multi_run,
            0,
            origin,
            pane_id=target_pane,
        )
        verify_transfer(
            binary,
            env,
            control_plane,
            run=multi_run,
            exit_code=0,
            expected_bucket="Finalized",
            expected_source="terminal_scrollback",
            expected_identity=multi_identity,
            marker=target_marker,
            forbidden_markers=(sibling_marker,),
        )
        multi_evidence = transfer_evidence(control_plane, multi_run, "multi_pane")
        multi_evidence["triage_exit"] = multi_result.returncode
        multi_evidence["target_pane"] = f"terminal_{target_pane}"
        multi_evidence["excluded_sibling_pane"] = f"terminal_{sibling_pane}"
        recorder.append("transfers", multi_evidence)

        cases = [
            (
                f"{unique}-exit-0",
                0,
                None,
                "Finalized",
                f"EXIT0-{unique}",
            ),
            (
                f"{unique}-exit-2",
                2,
                "failed",
                "Failed",
                f"EXIT2-{unique}",
            ),
            (
                f"{unique}-signal-15",
                -15,
                None,
                "NeedsAttention",
                f"SIGNAL15-{unique}",
            ),
        ]
        finalized_run = cases[0][0]
        for run, exit_code, bucket, expected_bucket, marker in cases:
            tab_id, panes = create_marker_tab(binary, env, origin, run, marker)
            pane_id = panes[0]
            wait_for_marker(binary, env, origin, pane_id, marker, probe_root)
            origin_tab_identity = typed_tab_identity(binary, env, origin, run, tab_id)
            expected_identity = terminal_capture_identity(
                origin, origin_tab_identity, pane_id
            )
            initial_result = triage(
                binary,
                env,
                run,
                exit_code,
                origin,
                bucket=bucket,
                pane_id=pane_id,
            )
            require(
                all(
                    tab.get("name") != run for tab in wait_for_tabs(binary, env, origin)
                ),
                f"{run} origin tab survived a successful transfer",
            )
            capture_before, receipt_before = verify_transfer(
                binary,
                env,
                control_plane,
                run=run,
                exit_code=exit_code,
                expected_bucket=expected_bucket,
                expected_source="terminal_scrollback",
                expected_identity=expected_identity,
                marker=marker,
            )
            initial_evidence = transfer_evidence(control_plane, run, "initial")
            initial_evidence["triage_exit"] = initial_result.returncode
            recorder.append("transfers", initial_evidence)

            # A clean replay is fully idempotent: no recapture, no second viewer
            # and no identity churn.
            replay_result = triage(
                binary,
                env,
                run,
                exit_code,
                origin,
                bucket=bucket,
                pane_id=pane_id,
            )
            capture_replayed, receipt_replayed = verify_transfer(
                binary,
                env,
                control_plane,
                run=run,
                exit_code=exit_code,
                expected_bucket=expected_bucket,
                expected_source="terminal_scrollback",
                expected_identity=expected_identity,
                marker=marker,
            )
            require(capture_replayed == capture_before, f"{run} replay rewrote capture")
            require(
                receipt_replayed.get("viewer_tab_identity")
                == receipt_before.get("viewer_tab_identity")
                and receipt_replayed.get("viewer_token")
                == receipt_before.get("viewer_token"),
                f"{run} clean replay changed the confirmed viewer identity",
            )
            replay_evidence = transfer_evidence(control_plane, run, "clean_replay")
            replay_evidence["triage_exit"] = replay_result.returncode
            recorder.append("transfers", replay_evidence)
            _capture_path, metadata_before_foreign, receipt_before_foreign = (
                transfer_files(control_plane, run)
            )
            completed_run_directory = control_plane / "finished_runs" / run
            completed_artifacts_before_foreign = artifact_tree_snapshot(
                completed_run_directory
            )
            viewer_identity_before_foreign = receipt_before_foreign.get(
                "viewer_tab_identity"
            )

            # A replacement with the same name is foreign to the durable receipt.
            # A fully committed v4 replay must return success without re-entering
            # CloseOriginTab or touching that successor.
            replacement_marker = f"REPLACEMENT-{marker}"
            replacement_id, replacement_panes = create_marker_tab(
                binary, env, origin, run, replacement_marker
            )
            require(
                replacement_id != tab_id,
                f"{run} replacement reused the captured stable tab id",
            )
            replacement_pane = replacement_panes[0]
            replacement_identity = typed_tab_identity(
                binary,
                env,
                origin,
                run,
                replacement_id,
            )
            wait_for_marker(
                binary,
                env,
                origin,
                replacement_pane,
                replacement_marker,
                probe_root,
            )
            foreign_replay = triage(
                binary,
                env,
                run,
                exit_code,
                origin,
                bucket=bucket,
                pane_id=pane_id,
            )
            capture_after_path, metadata_after_foreign, receipt_after_foreign = (
                transfer_files(control_plane, run)
            )
            completed_artifacts_after_foreign = artifact_tree_snapshot(
                completed_run_directory
            )
            require(
                capture_after_path.read_bytes() == capture_before,
                f"{run} foreign replay rewrote capture",
            )
            require(
                metadata_after_foreign == metadata_before_foreign,
                f"{run} foreign replay rewrote metadata",
            )
            require(
                completed_artifacts_after_foreign == completed_artifacts_before_foreign,
                f"{run} foreign replay changed committed transfer/capture/meta bytes",
            )
            require(
                receipt_after_foreign == receipt_before_foreign
                and receipt_after_foreign.get("capture_committed") is True
                and receipt_after_foreign.get("metadata_committed") is True
                and receipt_after_foreign.get("viewer_confirmed") is True
                and receipt_after_foreign.get("origin_tab_state") == "closed"
                and receipt_after_foreign.get("fault") is None,
                f"{run} fully committed replay changed its v4 receipt",
            )
            require(
                receipt_after_foreign.get("viewer_tab_identity")
                == viewer_identity_before_foreign
                and receipt_after_foreign.get("viewer_token")
                == receipt_before_foreign.get("viewer_token"),
                f"{run} foreign replay changed the confirmed viewer identity",
            )
            assert_viewer_inventory(
                binary,
                env,
                receipt_after_foreign,
                run=run,
                drawer=DRAWER_BY_BUCKET[expected_bucket],
            )
            require(
                typed_tab_identity(
                    binary,
                    env,
                    origin,
                    run,
                    replacement_id,
                )
                == replacement_identity,
                f"{run} replay closed or changed a foreign successor identity",
            )
            require(
                "CloseOriginTab" not in foreign_replay.stderr,
                f"{run} fully committed replay re-entered CloseOriginTab: "
                f"{foreign_replay.stderr!r}",
            )
            wait_for_marker(
                binary,
                env,
                origin,
                replacement_pane,
                replacement_marker,
                probe_root,
            )
            foreign_evidence = transfer_evidence(
                control_plane, run, "foreign_successor_idempotent_replay"
            )
            foreign_evidence["triage_exit"] = foreign_replay.returncode
            foreign_evidence["triage_stderr"] = foreign_replay.stderr
            foreign_evidence["committed_artifacts_digest_before"] = (
                completed_artifacts_before_foreign["digest"]
            )
            foreign_evidence["committed_artifacts_digest_after"] = (
                completed_artifacts_after_foreign["digest"]
            )
            foreign_evidence["foreign_successor_identity"] = replacement_identity
            recorder.append("transfers", foreign_evidence)

        # A real headless fallback means the entire origin server is gone, not
        # merely that a recorded pane id is stale.
        fallback_run = f"{unique}-transcript"
        fallback_marker = f"HEADLESS-{unique}"
        create_session(binary, env, headless_origin)
        _fallback_tab_id, fallback_panes = create_marker_tab(
            binary,
            env,
            headless_origin,
            fallback_run,
            fallback_marker,
            pane_count=2,
        )
        wait_for_marker(
            binary,
            env,
            headless_origin,
            fallback_panes[0],
            fallback_marker,
            probe_root,
        )
        transcript = root / "runtime-transcript.log"
        transcript.write_text(
            "real runtime transcript\nworker exited by signal\n",
            encoding="utf-8",
        )
        transcript_manifest = write_runtime_transcript_manifest(
            transcript,
            run=fallback_run,
            ownership_root=root,
        )
        transcript_bytes = transcript.read_bytes()
        kill_confirmed_session(binary, env, headless_origin)
        fallback_result = triage(
            binary,
            env,
            fallback_run,
            -9,
            headless_origin,
            pane_id=fallback_panes[0],
            transcript=transcript,
        )
        fallback_capture, _fallback_receipt = verify_transfer(
            binary,
            env,
            control_plane,
            run=fallback_run,
            exit_code=-9,
            expected_bucket="NeedsAttention",
            expected_source="runtime_transcript",
            expected_identity=str(transcript.resolve()),
            expected_bytes=transcript_bytes,
        )
        require(
            query_session(binary, env, headless_origin).state == "absent",
            "transcript fallback resurrected the dead origin",
        )
        require(
            transcript_manifest.is_file(),
            "runtime transcript ownership manifest disappeared",
        )
        fallback_evidence = transfer_evidence(
            control_plane, fallback_run, "runtime_transcript"
        )
        fallback_evidence["triage_exit"] = fallback_result.returncode
        recorder.append("transfers", fallback_evidence)
        fallback_replay = triage(
            binary,
            env,
            fallback_run,
            -9,
            headless_origin,
            pane_id=fallback_panes[0],
            transcript=transcript,
        )
        fallback_after, _fallback_receipt = verify_transfer(
            binary,
            env,
            control_plane,
            run=fallback_run,
            exit_code=-9,
            expected_bucket="NeedsAttention",
            expected_source="runtime_transcript",
            expected_identity=str(transcript.resolve()),
            expected_bytes=transcript_bytes,
        )
        require(
            fallback_after == fallback_capture,
            "headless transcript replay rewrote durable evidence",
        )
        fallback_replay_evidence = transfer_evidence(
            control_plane, fallback_run, "runtime_transcript_replay"
        )
        fallback_replay_evidence["triage_exit"] = fallback_replay.returncode
        recorder.append("transfers", fallback_replay_evidence)

        # Restart into a second empty HOME/socket namespace while retaining only
        # the durable control plane.
        finalized_capture, _metadata, receipt_before_restart = transfer_files(
            control_plane, finalized_run
        )
        capture_before_restart = finalized_capture.read_bytes()
        stored_pane_id = receipt_before_restart.get("pane_id")
        require(
            isinstance(stored_pane_id, str)
            and stored_pane_id.startswith("terminal_")
            and stored_pane_id.removeprefix("terminal_").isdigit(),
            "pre-restart receipt lost its typed terminal pane id",
        )
        finalized_pane_id = int(stored_pane_id.removeprefix("terminal_"))
        kill_confirmed_session(binary, env, "Finalized runs")
        require(
            session_inventory(binary, restart_env) == {},
            "restart namespace stopped being empty before replay",
        )
        restart_result = triage(
            binary,
            restart_env,
            finalized_run,
            0,
            origin,
            pane_id=finalized_pane_id,
        )
        require(
            finalized_capture.read_bytes() == capture_before_restart,
            "drawer restart rewrote durable capture",
        )
        _capture, _metadata, receipt_after_restart = transfer_files(
            control_plane, finalized_run
        )
        require(
            receipt_after_restart.get("viewer_token")
            == receipt_before_restart.get("viewer_token"),
            "drawer restart changed the durable viewer ownership token",
        )
        restarted_identity = assert_viewer_inventory(
            binary,
            restart_env,
            receipt_after_restart,
            run=finalized_run,
            drawer="Finalized runs",
        )
        previous_identity = receipt_before_restart.get("viewer_tab_identity")
        require(
            isinstance(previous_identity, dict),
            "pre-restart receipt lost its typed viewer identity",
        )
        require(
            restarted_identity != previous_identity
            and restarted_identity.get("session_incarnation")
            != previous_identity.get("session_incarnation"),
            "drawer restart reused the dead server's viewer identity",
        )
        recorder.set(
            "restart",
            {
                "run": finalized_run,
                "triage_exit": restart_result.returncode,
                "capture_path": str(finalized_capture.resolve()),
                "capture_bytes": len(capture_before_restart),
                "capture_sha256": sha256_bytes(capture_before_restart),
                "viewer_token_before": receipt_before_restart.get("viewer_token"),
                "viewer_token_after": receipt_after_restart.get("viewer_token"),
                "viewer_identity_before": previous_identity,
                "viewer_identity_after": restarted_identity,
                "restart_namespace": str(restart_root),
            },
        )
        recorder.append(
            "transfers",
            transfer_evidence(control_plane, finalized_run, "drawer_restart"),
        )

        peer_after_id = tab_identity(binary, env, peer, peer_canary)
        require(
            peer_after_id == peer_canary_id,
            "terminal transfers replaced the peer canary tab: "
            f"before={peer_canary_id}, after={peer_after_id}",
        )
        wait_for_marker(binary, env, peer, peer_pane, peer_marker, probe_root)
        wait_for_marker(binary, env, origin, origin_pane, origin_marker, probe_root)
    except BaseException as error:
        caught = error
    finally:
        if mutation_started:
            for label, cleanup_env, targets in (
                ("primary", env, primary_targets),
                ("restart", restart_env, restart_targets),
            ):
                try:
                    cleanup_receipts[label] = {
                        "status": "passed",
                        **cleanup_namespace(binary, cleanup_env, targets),
                    }
                except BaseException as error:
                    cleanup_errors.append(f"{label}: {error}")
                    cleanup_receipts[label] = {
                        "status": "failed",
                        "error_type": type(error).__name__,
                        "error": str(error),
                        "residue": cleanup_failure_snapshot(binary, cleanup_env),
                    }
        else:
            cleanup_receipts["status"] = "not_needed_preflight_failed"
        if not cleanup_errors:
            try:
                cleanup_receipts["runtime_root"] = {
                    "status": "passed",
                    **remove_runtime_root(runtime_root),
                }
            except BaseException as error:
                cleanup_errors.append(f"runtime_root: {error}")
                cleanup_receipts["runtime_root"] = {
                    "status": "failed",
                    "error_type": type(error).__name__,
                    "error": str(error),
                    "retained": runtime_root.exists(),
                }
        else:
            cleanup_receipts["runtime_root"] = {
                "status": "retained_after_namespace_cleanup_failure",
                "runtime_root": str(runtime_root),
            }
        recorder.set("cleanup", cleanup_receipts)

    operator_guard_after = guarded_tree_snapshot(
        guarded_operator_paths,
        volatile_files=guarded_volatile_paths,
    )
    operator_guard_unchanged = operator_guard_after == operator_guard_before
    operator_guard_attribution = attribute_operator_guard_changes(
        operator_guard_before,
        operator_guard_after,
        operator_guard_fixture_markers(unique, recorder.data),
    )
    operator_guard_safe = operator_guard_unchanged or bool(
        operator_guard_attribution["safe"]
    )
    operator_guard_receipt: dict[str, object] = {
        "paths": [str(path) for path in guarded_operator_paths],
        "volatile_identity_only": [
            str(path) for path in sorted(guarded_volatile_paths)
        ],
        "before": guarded_snapshot_summary(operator_guard_before),
        "after": guarded_snapshot_summary(operator_guard_after),
        "unchanged": operator_guard_unchanged,
        "safe": operator_guard_safe,
    }
    if not operator_guard_unchanged:
        operator_guard_receipt["diff"] = guarded_snapshot_diff(
            operator_guard_before,
            operator_guard_after,
        )
        operator_guard_receipt["attribution"] = operator_guard_attribution
    recorder.set(
        "operator_guard",
        operator_guard_receipt,
    )
    if caught is None and not operator_guard_safe:
        caught = AssertionError(
            "isolated E2E touched operator vc-frame roots or durable operator state changed"
        )

    if caught is None and cleanup_errors:
        caught = AssertionError("isolated cleanup failed: " + "; ".join(cleanup_errors))

    if caught is not None:
        recorder.set(
            "error",
            {
                "type": type(caught).__name__,
                "message": str(caught),
                "cleanup_errors": cleanup_errors,
            },
        )
        recorder.set("completed_at", utc_now())
        recorder.set("status", "failed")
        if cleanup_errors:
            caught.add_note("cleanup errors: " + "; ".join(cleanup_errors))
        print(f"triage-runtime-e2e evidence: {receipt_path}", file=sys.stderr)
        raise caught
    recorder.set("completed_at", utc_now())
    recorder.set("status", "passed")
    print("triage-runtime-e2e: all isolated command-boundary checks passed")
    print(f"triage-runtime-e2e evidence: {receipt_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
