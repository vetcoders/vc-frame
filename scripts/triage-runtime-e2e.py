#!/usr/bin/env python3
"""Isolated command-boundary proof for truthful run triage.

The harness accepts only an exact, clean, profile-matched ``vc-frame`` build,
constructs two empty runtime namespaces below one durable artifact directory,
and never discovers or mutates the operator's normal socket tree. Canonical
drawer names are safe here only because both socket aliases, HOME/XDG state and
the control plane are bound below that artifact directory before the first
session is created.

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
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Any, Literal


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
    "capture_committed",
    "metadata_committed",
    "viewer_confirmed",
    "origin_tab_state",
    "viewer_token",
    "viewer_tab_identity",
    "pane_id",
    "fault",
)


@dataclass(frozen=True)
class SessionQuery:
    """A proven live/absent result; command ambiguity is raised, never encoded."""

    state: Literal["live", "absent"]
    tabs: list[dict[str, object]] | None
    list_tabs_exit: int
    list_tabs_stderr: str
    inventory_state: Literal["live", "exited", "missing"]


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


def receipt_projection(receipt: dict[str, object]) -> dict[str, object]:
    return {field: receipt.get(field) for field in RECEIPT_FIELDS}


def transfer_evidence(
    control_plane: pathlib.Path,
    run: str,
    stage: str,
) -> dict[str, object]:
    scrollback, metadata, receipt = transfer_files(control_plane, run)
    contents = scrollback.read_bytes()
    return {
        "stage": stage,
        "run": run,
        "capture_path": str(scrollback.resolve()),
        "capture_bytes": len(contents),
        "capture_sha256": sha256_bytes(contents),
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
        try:
            arguments = shlex.split(command_line)
        except ValueError:
            continue
        server_paths: list[str] = []
        for index, argument in enumerate(arguments):
            if argument == "--server" and index + 1 < len(arguments):
                server_paths.append(arguments[index + 1])
            elif argument.startswith("--server="):
                server_paths.append(argument.partition("=")[2])
        if any(
            pathlib.Path(path).resolve().is_relative_to(root) for path in server_paths
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
        control_plane == namespace_root.parent / "control-plane",
        f"isolated control plane escaped the artifact root: {control_plane}",
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
        return SessionQuery(
            state="live",
            tabs=parse_json_array(result.stdout, f"tab inventory for {session!r}"),
            list_tabs_exit=0,
            list_tabs_stderr=result.stderr,
            inventory_state="live",
        )
    inventory_state = session_inventory(binary, env).get(session, "missing")
    if inventory_state == "live":
        raise AssertionError(
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
    while time.monotonic() < deadline:
        tabs = session_tabs(binary, env, session)
        if tabs is not None:
            return tabs
        time.sleep(0.1)
    raise AssertionError(f"session {session!r} did not become ready")


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
    binary: pathlib.Path, env: dict[str, str], session: str
) -> list[dict[str, object]]:
    deadline = time.monotonic() + 15
    previous: list[dict[str, object]] | None = None
    stable_since = time.monotonic()
    while time.monotonic() < deadline:
        current = tab_state(wait_for_tabs(binary, env, session))
        if current != previous:
            previous = current
            stable_since = time.monotonic()
        elif time.monotonic() - stable_since >= 0.5:
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
) -> None:
    unchanged = before == after
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
        },
    )
    require(result.returncode != 0, f"{scenario} exited zero")
    require(unchanged, f"{scenario} changed tab focus/inventory or durable artifacts")


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
    return command(binary, env, *args, expect_success=expect_success)


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


def assert_viewer_inventory(
    binary: pathlib.Path,
    env: dict[str, str],
    receipt: dict[str, object],
    *,
    run: str,
    drawer: str,
) -> dict[str, object]:
    require(receipt.get("version") == 3, f"{run} receipt is not the v3 contract")
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

    drawer_tabs = wait_for_tabs(binary, env, drawer)
    matches = [
        tab
        for tab in drawer_tabs
        if tab.get("tab_id") == viewer_id
        and tab.get("name") == expected_name
        and tab.get("session_incarnation") == incarnation
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
) -> dict[str, object]:
    """Kill exact owned targets, then prove empty inventory and process table."""
    initial = session_inventory(binary, env)
    unexpected = set(initial) - owned_targets
    require(
        not unexpected,
        "refusing broad cleanup of unexpected isolated sessions: "
        + ", ".join(sorted(unexpected)),
    )
    killed: list[str] = []
    for session, state in sorted(initial.items()):
        if state == "exited":
            continue
        query = query_session(binary, env, session)
        require(
            query.state == "live",
            f"cleanup inventory said {session!r} was active but exact query said absent",
        )
        command(binary, env, "kill-session", session)
        wait_for_session_gone(binary, env, session, timeout=timeout)
        killed.append(session)

    deleted: list[str] = []
    deadline = time.monotonic() + timeout
    post_kill_inventory: dict[str, Literal["live", "exited"]] = {}
    while time.monotonic() < deadline:
        post_kill_inventory = session_inventory(binary, env)
        if not any(state == "live" for state in post_kill_inventory.values()):
            break
        time.sleep(0.1)
    require(
        not any(state == "live" for state in post_kill_inventory.values()),
        "isolated namespace retained live sessions after cleanup: "
        + ", ".join(
            sorted(
                name for name, state in post_kill_inventory.items() if state == "live"
            )
        ),
    )
    unexpected_after_kill = set(post_kill_inventory) - owned_targets
    require(
        not unexpected_after_kill,
        "unexpected session appeared during isolated cleanup: "
        + ", ".join(sorted(unexpected_after_kill)),
    )
    for session in sorted(post_kill_inventory):
        command(binary, env, "delete-session", session, "--force")
        deleted.append(session)

    final_inventory: dict[str, Literal["live", "exited"]] = {}
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        final_inventory = session_inventory(binary, env)
        if not final_inventory:
            break
        time.sleep(0.1)
    require(
        not final_inventory,
        f"isolated namespace retained session inventory after cleanup: "
        f"{final_inventory!r}",
    )
    socket_root = pathlib.Path(env["VC_FRAME_SOCKET_DIR"]).resolve()
    process_residue = wait_for_no_server_processes(socket_root, timeout=timeout)
    require(not process_residue, "isolated namespace retained server processes")
    socket_entries = (
        sorted(str(path.relative_to(socket_root)) for path in socket_root.rglob("*"))
        if socket_root.exists()
        else []
    )
    require(
        not socket_entries,
        f"isolated socket root retained entries after cleanup: {socket_entries!r}",
    )
    return {
        "initial_session_inventory": initial,
        "killed_sessions": killed,
        "deleted_sessions": deleted,
        "final_session_inventory": {},
        "process_residue": process_residue,
        "socket_entries_after_cleanup": socket_entries,
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

    unique = f"e{os.getpid()}-{time.time_ns()}"
    stamp = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%S.%fZ")
    root = options.artifact_root.expanduser().resolve() / f"{stamp}-{unique}"
    root.mkdir(parents=True, exist_ok=False)
    control_plane = root / "control-plane"
    primary_root = root / "primary"
    restart_root = root / "restart"
    env = isolated_env(primary_root, control_plane)
    restart_env = isolated_env(restart_root, control_plane)
    receipt_path = root / "evidence.json"
    recorder = EvidenceRecorder(
        receipt_path,
        {
            "schema_version": 1,
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
            "negative_probes": [],
            "transfers": [],
            "restart": {},
            "cleanup": {},
            # Phase 2 is intentionally blocked on explicit protocol killpoints.
            "deferred_protocol_scenarios": [
                "interruption_after_capture_commit",
                "interruption_after_viewer_confirmation",
            ],
        },
    )
    # Print before the first mutation so a hard interruption still leaves an
    # operator-discoverable receipt path in the command log.
    print(f"triage-runtime-e2e evidence: {receipt_path}", file=sys.stderr, flush=True)

    origin = f"{unique}-origin"
    peer = f"{unique}-peer"
    headless_origin = f"{unique}-headless"
    drawers = set(DRAWER_BY_BUCKET.values())
    primary_targets = {origin, peer, headless_origin, *drawers}
    restart_targets = {"Finalized runs"}
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
            expected_sha=expected_sha,
            expected_profile=options.expected_profile,
        )
        restart_build_info = namespace_preflight(
            binary,
            restart_env,
            restart_root,
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
        wait_for_stable_tab_state(binary, env, origin)
        wait_for_stable_tab_state(binary, env, peer)
        guard_sessions = {origin, peer}

        missing_name = f"{unique}-missing"
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
        missing_origin_session = f"{unique}-missing-origin-session"
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
        empty_origin = f"{unique}-empty-origin"
        empty_run = f"{unique}-empty-transcript"
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
        )
        require(
            "empty" in empty_fallback.stderr.lower(),
            f"empty transcript failed without an explicit empty-source error: "
            f"{empty_fallback.stderr!r}",
        )

        require(
            tab_identity(binary, env, peer, peer_canary) == peer_canary_id,
            "negative command-boundary probes changed the peer canary identity",
        )
        wait_for_marker(binary, env, origin, origin_pane, origin_marker, probe_root)
        wait_for_marker(binary, env, peer, peer_pane, peer_marker, probe_root)

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
        multi_identity = (
            f"session={origin};tab_id={multi_tab_id};pane_id=terminal_{target_pane}"
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
            expected_identity = (
                f"session={origin};tab_id={tab_id};pane_id=terminal_{pane_id}"
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
            _capture_path, metadata_before_foreign, _receipt = transfer_files(
                control_plane, run
            )

            # A replacement with the same name is foreign to the durable receipt.
            replacement_marker = f"REPLACEMENT-{marker}"
            replacement_id, replacement_panes = create_marker_tab(
                binary, env, origin, run, replacement_marker
            )
            require(
                replacement_id != tab_id,
                f"{run} replacement reused the captured stable tab id",
            )
            replacement_pane = replacement_panes[0]
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
                expect_success=False,
            )
            require(
                "CloseOriginTab" in foreign_replay.stderr
                and "refusing to close a successor" in foreign_replay.stderr
                and "capture durable: true" in foreign_replay.stderr,
                f"{run} foreign replay did not report a durable fail-closed "
                f"result: {foreign_replay.stderr!r}",
            )
            capture_after_path, metadata_after_foreign, receipt_after_foreign = (
                transfer_files(control_plane, run)
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
                receipt_after_foreign.get("capture_committed") is True
                and receipt_after_foreign.get("metadata_committed") is True
                and receipt_after_foreign.get("viewer_confirmed") is True
                and receipt_after_foreign.get("origin_tab_state") == "preserved",
                f"{run} foreign replay recorded dishonest transfer state",
            )
            require(
                isinstance(receipt_after_foreign.get("fault"), str)
                and "CloseOriginTab" in receipt_after_foreign["fault"],
                f"{run} foreign replay did not persist its close fault",
            )
            require(
                receipt_after_foreign.get("viewer_tab_identity")
                == receipt_before.get("viewer_tab_identity")
                and receipt_after_foreign.get("viewer_token")
                == receipt_before.get("viewer_token"),
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
                tab_identity(binary, env, origin, run) == replacement_id,
                f"{run} replay closed or replaced a foreign origin",
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
                control_plane, run, "foreign_successor_replay"
            )
            foreign_evidence["triage_exit"] = foreign_replay.returncode
            foreign_evidence["triage_stderr"] = foreign_replay.stderr
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
        recorder.set("cleanup", cleanup_receipts)

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
