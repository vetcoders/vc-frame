#!/usr/bin/env python3
"""Isolated command-boundary proof for truthful run triage.

The harness accepts only a provenance-bearing ``vc-frame`` binary, constructs
two empty temporary runtime namespaces, and never discovers or mutates the
operator's normal socket tree. Canonical drawer names are safe here only because
both socket aliases, HOME/XDG state and the control plane are bound below the
temporary root before the first session is created.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import subprocess
import tempfile
import time


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


def require(condition: bool, message: str) -> None:
    """An assertion that remains active under ``python -O``."""
    if not condition:
        raise AssertionError(message)


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


def active_session_names(binary: pathlib.Path, env: dict[str, str]) -> set[str]:
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
        return set()
    active: set[str] = set()
    for line in result.stdout.splitlines():
        if "(EXITED - attach to resurrect)" in line:
            continue
        name, separator, _rest = line.partition(" [Created ")
        require(bool(separator), f"unparseable session inventory line: {line!r}")
        active.add(name.strip())
    return active


def namespace_preflight(
    binary: pathlib.Path, env: dict[str, str], namespace_root: pathlib.Path
) -> None:
    """Reject a foreign/non-isolating binary before the first mutation."""
    for key in ("VC_FRAME_SOCKET_DIR", "ZELLIJ_SOCKET_DIR"):
        socket_root = pathlib.Path(env[key]).resolve()
        require(
            socket_root == namespace_root.resolve() / "sockets",
            f"{key} escaped the fixture namespace: {socket_root}",
        )
    require(
        pathlib.Path(env["HOME"]).resolve().is_relative_to(namespace_root.resolve()),
        "isolated HOME escaped the fixture namespace",
    )
    require(
        pathlib.Path(env["VIBECRAFTED_HOME"])
        .resolve()
        .is_relative_to(namespace_root.resolve()),
        "isolated VIBECRAFTED_HOME escaped the fixture namespace",
    )

    build = command(binary, env, "--build-info")
    try:
        build_info = json.loads(build.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError("binary --build-info did not return JSON") from error
    require(
        build_info.get("product") == "vc-frame",
        f"refusing foreign binary: build-info={build_info!r}",
    )
    help_result = command(binary, env, "triage-run", "--help")
    for capability in ("--runtime-transcript", "--origin-session", "--exit-code"):
        require(
            capability in help_result.stdout,
            f"binary lacks required triage capability {capability}",
        )
    require(
        active_session_names(binary, env) == set(),
        "fixture namespace was not empty before mutation",
    )


def session_tabs(
    binary: pathlib.Path, env: dict[str, str], session: str
) -> list[dict[str, object]] | None:
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
    if result.returncode != 0:
        return None
    return parse_json_array(result.stdout, f"tab inventory for {session!r}")


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
    binary: pathlib.Path, env: dict[str, str], session: str
) -> None:
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if session_tabs(binary, env, session) is None:
            return
        time.sleep(0.1)
    raise AssertionError(f"session {session!r} remained active after exact kill")


def tab_signature(tabs: list[dict[str, object]]) -> tuple[tuple[int, str], ...]:
    signature: list[tuple[int, str]] = []
    for tab in tabs:
        tab_id = tab.get("tab_id")
        name = tab.get("name")
        require(
            isinstance(tab_id, int) and isinstance(name, str),
            f"invalid tab inventory entry: {tab!r}",
        )
        signature.append((tab_id, name))
    return tuple(sorted(signature))


def wait_for_stable_tab_signature(
    binary: pathlib.Path, env: dict[str, str], session: str
) -> tuple[tuple[int, str], ...]:
    deadline = time.monotonic() + 15
    previous: tuple[tuple[int, str], ...] | None = None
    stable_since = time.monotonic()
    while time.monotonic() < deadline:
        current = tab_signature(wait_for_tabs(binary, env, session))
        if current != previous:
            previous = current
            stable_since = time.monotonic()
        elif time.monotonic() - stable_since >= 0.5:
            return current
        time.sleep(0.1)
    raise AssertionError(f"tab inventory for {session!r} did not stabilize")


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


def marker_layout(marker: str, pane_count: int = 1) -> str:
    require(
        marker.replace("-", "").isalnum(),
        f"unsafe fixture marker: {marker!r}",
    )
    pane = (
        'pane command="/bin/sh" {\n'
        f'args "-c" "printf \'{marker}\'; exec sleep 300"'
        "\n}"
    )
    if pane_count == 1:
        return f"layout {{\n{pane}\n}}"
    return (
        'layout {\npane split_direction="vertical" {\n'
        + "\n".join(pane for _ in range(pane_count))
        + "\n}\n}"
    )


def create_session(binary: pathlib.Path, env: dict[str, str], session: str) -> None:
    require(
        session_tabs(binary, env, session) is None,
        f"refusing to adopt pre-existing session {session!r}",
    )
    command(binary, env, "attach", "--create-background", session)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
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
            expect_success=None,
        )
        if result.returncode == 0 and parse_json_array(
            result.stdout, f"bootstrap pane inventory for {session!r}"
        ):
            return
        time.sleep(0.1)
    raise AssertionError(f"session {session!r} never completed layout bootstrap")


def create_marker_tab(
    binary: pathlib.Path,
    env: dict[str, str],
    session: str,
    name: str,
    marker: str,
    *,
    pane_count: int = 1,
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
        marker_layout(marker, pane_count),
    )
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            tab_id = tab_identity(binary, env, session, name)
            panes = terminal_panes(binary, env, session, tab_id)
            if len(panes) == pane_count:
                return tab_id, panes
        except AssertionError:
            pass
        time.sleep(0.1)
    raise AssertionError(
        f"tab {session}/{name} did not materialize {pane_count} terminal pane(s)"
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
) -> tuple[bytes, dict[str, object]]:
    scrollback, metadata, receipt = transfer_files(control_plane, run)
    contents = scrollback.read_bytes()
    require(contents, f"{run} committed an empty scrollback")
    if marker is not None:
        require(marker.encode() in contents, f"{run} captured a foreign pane")
    if expected_bytes is not None:
        require(contents == expected_bytes, f"{run} did not copy the exact transcript")
    digest = hashlib.sha256(contents).hexdigest()
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
    require(
        session_tabs(binary, env, session) is not None,
        f"refusing to kill unconfirmed session {session!r}",
    )
    command(binary, env, "kill-session", session)
    wait_for_session_gone(binary, env, session)


def cleanup_namespace(
    binary: pathlib.Path,
    env: dict[str, str],
    owned_targets: set[str],
) -> None:
    """Kill only exact, inventory-confirmed targets in an exclusive namespace."""
    for session in sorted(owned_targets):
        if session_tabs(binary, env, session) is not None:
            command(binary, env, "kill-session", session)
            wait_for_session_gone(binary, env, session)
    unexpected = active_session_names(binary, env) - owned_targets
    require(
        not unexpected,
        "refusing broad cleanup of unexpected isolated sessions: "
        + ", ".join(sorted(unexpected)),
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=pathlib.Path)
    options = parser.parse_args()
    binary = options.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"binary does not exist: {binary}")

    unique = f"e{os.getpid()}-{time.time_ns()}"
    with tempfile.TemporaryDirectory(prefix="vce2e-", dir="/tmp") as raw_root:
        root = pathlib.Path(raw_root)
        control_plane = root / "control-plane"
        primary_root = root / "primary"
        restart_root = root / "restart"
        env = isolated_env(primary_root, control_plane)
        restart_env = isolated_env(restart_root, control_plane)

        # Both namespaces are proven empty and the binary proven compatible
        # before any server can be created.
        namespace_preflight(binary, env, primary_root)
        namespace_preflight(binary, restart_env, restart_root)

        origin = f"{unique}-origin"
        peer = f"{unique}-peer"
        headless_origin = f"{unique}-headless"
        drawers = set(DRAWER_BY_BUCKET.values())
        primary_targets = {origin, peer, headless_origin, *drawers}
        restart_targets = {"Finalized runs"}
        probe_root = root / "probes"
        caught: BaseException | None = None
        cleanup_errors: list[str] = []

        try:
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
            origin_before = wait_for_stable_tab_signature(binary, env, origin)
            wait_for_stable_tab_signature(binary, env, peer)

            missing_name = f"{unique}-missing"
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
            require(missing_session.returncode != 0, "missing session exited zero")
            require(
                session_tabs(binary, env, missing_name) is None,
                "missing-session probe created a session",
            )

            missing_tab_name = f"{unique}-missing-tab"
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
            require(missing_tab.returncode != 0, "missing tab exited zero")
            require(
                tab_signature(wait_for_tabs(binary, env, origin)) == origin_before,
                "missing-tab probe changed origin inventory",
            )

            missing_dump = root / "missing-pane.txt"
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
            require(missing_pane.returncode != 0, "missing pane exited zero")
            require(not missing_dump.exists(), "missing pane created a trusted dump")

            # Use an inventory-proven live pane so this cannot pass as another
            # missing-pane failure.
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
            require(write_failure.returncode != 0, "dump write failure exited zero")
            require(
                tab_signature(wait_for_tabs(binary, env, origin)) == origin_before,
                "negative command-boundary probes changed origin inventory",
            )
            require(
                tab_identity(binary, env, peer, peer_canary) == peer_canary_id,
                "negative command-boundary probes changed the peer canary identity",
            )
            wait_for_marker(binary, env, origin, origin_pane, origin_marker, probe_root)
            wait_for_marker(binary, env, peer, peer_pane, peer_marker, probe_root)

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
                triage(
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
                        tab.get("name") != run
                        for tab in wait_for_tabs(binary, env, origin)
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

                # A clean replay is fully idempotent: no recapture, no second
                # viewer and no identity churn.
                triage(
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
                require(
                    capture_replayed == capture_before,
                    f"{run} clean replay rewrote capture",
                )
                require(
                    receipt_replayed.get("viewer_tab_identity")
                    == receipt_before.get("viewer_tab_identity")
                    and receipt_replayed.get("viewer_token")
                    == receipt_before.get("viewer_token"),
                    f"{run} clean replay changed the confirmed viewer identity",
                )
                _capture_path, metadata_before_foreign, _receipt = transfer_files(
                    control_plane, run
                )

                # A replacement with the same name is foreign to the durable
                # receipt. Replay must fail closed, preserve it and not
                # duplicate the already-confirmed viewer.
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

            # A real headless fallback means the entire origin server is gone,
            # not merely that a recorded pane id is stale.
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
            transcript.write_text("real runtime transcript\nworker exited by signal\n")
            transcript_bytes = transcript.read_bytes()
            kill_confirmed_session(binary, env, headless_origin)
            triage(
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
                session_tabs(binary, env, headless_origin) is None,
                "transcript fallback resurrected the dead origin",
            )
            triage(
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

            # Restart into a second empty HOME/socket namespace while retaining
            # only the isolated durable control plane. This cannot pass by
            # racing the old server's asynchronous shutdown or resurrection
            # cache.
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
                active_session_names(binary, restart_env) == set(),
                "restart namespace stopped being empty before replay",
            )
            triage(
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
            for label, cleanup_env, targets in (
                ("primary", env, primary_targets),
                ("restart", restart_env, restart_targets),
            ):
                try:
                    cleanup_namespace(binary, cleanup_env, targets)
                except BaseException as error:
                    cleanup_errors.append(f"{label}: {error}")

        if caught is not None:
            if cleanup_errors:
                caught.add_note("cleanup errors: " + "; ".join(cleanup_errors))
            raise caught
        require(
            not cleanup_errors,
            "isolated cleanup failed: " + "; ".join(cleanup_errors),
        )

    print("triage-runtime-e2e: all isolated command-boundary checks passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
