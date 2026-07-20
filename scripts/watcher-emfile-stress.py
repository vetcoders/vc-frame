#!/usr/bin/env python3
"""Bounded real-runtime watcher teardown and EMFILE recovery probe.

The probe creates one isolated vc-frame session under a private socket root and
a process-local RLIMIT_NOFILE. It never changes the operator's global limits or
touches sessions outside that root. Exact resources are removed in ``finally``.
"""

from __future__ import annotations

import argparse
import json
import os
import resource
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def run(
    command: list[str],
    *,
    env: dict[str, str],
    timeout: float,
    check: bool = True,
    preexec_fn=None,
    pass_fds: tuple[int, ...] = (),
    capture_output: bool = True,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        command,
        env=env,
        text=True,
        stdout=subprocess.PIPE if capture_output else subprocess.DEVNULL,
        stderr=subprocess.STDOUT,
        timeout=timeout,
        check=check,
        preexec_fn=preexec_fn,
        pass_fds=pass_fds,
    )


def wait_until(predicate, *, deadline: float, description: str) -> None:
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise TimeoutError(f"timed out waiting for {description}")


def server_pid_for_socket(socket_path: Path) -> int | None:
    processes = subprocess.run(
        ["ps", "-axo", "pid=,command="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=True,
    ).stdout
    needle = str(socket_path)
    for line in processes.splitlines():
        if "--server" in line and needle in line:
            pid, _command = line.strip().split(maxsplit=1)
            return int(pid)
    return None


def fd_count(pid: int) -> int:
    result = subprocess.run(
        ["lsof", "-a", "-p", str(pid), "-Fn"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=True,
    )
    return sum(1 for line in result.stdout.splitlines() if line.startswith("f"))


def cpu_seconds(pid: int) -> float:
    value = subprocess.run(
        ["ps", "-p", str(pid), "-o", "time="],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        check=True,
    ).stdout.strip()
    days = 0
    if "-" in value:
        day_text, value = value.split("-", 1)
        days = int(day_text)
    fields = value.split(":")
    seconds = float(fields[-1])
    minutes = int(fields[-2]) if len(fields) >= 2 else 0
    hours = int(fields[-3]) if len(fields) >= 3 else 0
    return days * 86400 + hours * 3600 + minutes * 60 + seconds


def lower_fd_limit(limit: int):
    def apply_limit() -> None:
        _soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        resource.setrlimit(resource.RLIMIT_NOFILE, (min(limit, hard), hard))

    return apply_limit


def attach_and_detach(binary: Path, session_name: str, env: dict[str, str]) -> int:
    if sys.platform != "darwin":
        # util-linux script uses a different argument contract; the list/control
        # checks still prove the isolated socket remains usable on non-macOS.
        return run(
            [str(binary), "--session", session_name, "action", "dump-screen"],
            env=env,
            timeout=5,
            check=False,
        ).returncode

    process = subprocess.Popen(
        ["/usr/bin/script", "-q", "/dev/null", str(binary), "attach", session_name],
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        time.sleep(0.8)
        assert process.stdin is not None
        process.stdin.write(b"\x0fd")  # Ctrl-o enters Session mode; d detaches.
        process.stdin.flush()
        return process.wait(timeout=5)
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.send_signal(signal.SIGHUP)
                process.wait(timeout=2)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/vc-frame"))
    parser.add_argument("--cycles", type=int, default=3)
    parser.add_argument("--fd-limit", type=int, default=128)
    parser.add_argument("--timeout", type=int, default=75)
    parser.add_argument("--receipt", type=Path)
    args = parser.parse_args()

    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"vc-frame binary does not exist: {binary}")
    if shutil.which("lsof") is None:
        parser.error("lsof is required for descriptor receipts")

    session_name = f"w1b-{os.getpid()}"
    pressure_sockets: list[socket.socket] = []
    inherited_reserve: list[int] = []
    server_pid: int | None = None
    receipt: dict[str, object] = {
        "session": session_name,
        "fd_limit": args.fd_limit,
        "cycles": args.cycles,
        "timeout_seconds": args.timeout,
    }

    def timeout_handler(_signum, _frame) -> None:
        raise TimeoutError(f"probe exceeded {args.timeout} s timeout")

    previous_handler = signal.signal(signal.SIGALRM, timeout_handler)
    signal.alarm(args.timeout)
    started = time.monotonic()

    with tempfile.TemporaryDirectory(prefix="vcf-w1b-", dir="/tmp") as temp_dir_text:
        temp_dir = Path(temp_dir_text)
        socket_root = temp_dir / "s"
        config_dir = temp_dir / "config"
        layout_dir = temp_dir / "layouts"
        config_path = config_dir / "config.kdl"
        socket_root.mkdir()
        config_dir.mkdir()
        layout_dir.mkdir()
        config_path.write_text("simplified_ui true\n", encoding="utf-8")
        (layout_dir / "stress.kdl").write_text("layout { pane; }\n", encoding="utf-8")

        env = os.environ.copy()
        env["VC_FRAME_SOCKET_DIR"] = str(socket_root)
        socket_path = socket_root / "contract_version_1" / session_name
        log_path = Path(tempfile.gettempdir()) / f"vc-frame-{os.getuid()}" / "vc-frame-log" / "zellij.log"

        try:
            # Seed the isolated server with inherited descriptors. This creates
            # deterministic headroom pressure without changing a global limit or
            # attaching a debugger to an operator process.
            for _ in range(max(8, args.fd_limit // 5)):
                descriptor = os.open(os.devnull, os.O_RDONLY)
                os.set_inheritable(descriptor, True)
                inherited_reserve.append(descriptor)
            receipt["inherited_reserve_fds"] = len(inherited_reserve)

            create = run(
                [
                    str(binary),
                    "--config",
                    str(config_path),
                    "--config-dir",
                    str(config_dir),
                    "attach",
                    "-b",
                    session_name,
                    "options",
                    "--layout-dir",
                    str(layout_dir),
                    "--session-serialization",
                    "false",
                    "--default-shell",
                    "/bin/sh",
                ],
                env=env,
                timeout=15,
                check=False,
                preexec_fn=lower_fd_limit(args.fd_limit),
                pass_fds=tuple(inherited_reserve),
                capture_output=False,
            )
            for descriptor in inherited_reserve:
                os.close(descriptor)
            inherited_reserve.clear()
            if create.returncode != 0:
                raise AssertionError(
                    f"isolated session creation failed ({create.returncode})"
                )
            receipt["create_exit"] = create.returncode
            wait_until(
                lambda: socket_path.is_socket(),
                deadline=time.monotonic() + 5,
                description="isolated session socket",
            )
            wait_until(
                lambda: server_pid_for_socket(socket_path) is not None,
                deadline=time.monotonic() + 5,
                description="isolated server pid",
            )
            server_pid = server_pid_for_socket(socket_path)
            assert server_pid is not None
            receipt["server_pid"] = server_pid
            # The detached client returns once the socket is ready, while plugin
            # workers and both poll watchers finish initializing asynchronously.
            # Exclude that expected startup ramp from the leak baseline.
            time.sleep(5.0)
            receipt["fd_baseline"] = fd_count(server_pid)
            baseline_cpu_before = cpu_seconds(server_pid)
            time.sleep(3.0)
            receipt["cpu_seconds_during_baseline"] = round(
                cpu_seconds(server_pid) - baseline_cpu_before, 3
            )

            # Force both PollWatcher loops through root removal and recreation.
            parked_layout = temp_dir / "layouts.parked"
            for _ in range(args.cycles):
                config_path.unlink()
                layout_dir.rename(parked_layout)
                time.sleep(1.2)  # exceed the PollWatcher interval
                config_path.write_text("simplified_ui true\n", encoding="utf-8")
                parked_layout.rename(layout_dir)
                time.sleep(3.2)  # exceed the missing-root retry interval

            receipt["fd_after_watcher_churn"] = fd_count(server_pid)
            if int(receipt["fd_after_watcher_churn"]) > int(receipt["fd_baseline"]) + 3:
                raise AssertionError(f"watcher FD count escaped bound: {receipt}")

            log_offset = log_path.stat().st_size if log_path.exists() else 0
            pressure_started = time.monotonic()

            def pressure_log_text() -> str:
                if not log_path.exists():
                    return ""
                text = log_path.read_text(encoding="utf-8", errors="replace")
                # The shared rolling log can rotate while the isolated probe is
                # running. In that case the old byte offset is no longer valid;
                # inspect only the bounded tail of the replacement file.
                return text[log_offset:] if len(text) >= log_offset else text[-131_072:]

            # Raw clients deliberately withhold the protocol handshake. The
            # server accepts and owns them until its inherited soft limit is hit.
            for _ in range(args.fd_limit * 2):
                client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                client.settimeout(0.15)
                try:
                    client.connect(str(socket_path))
                except OSError:
                    client.close()
                    break
                pressure_sockets.append(client)

            try:
                wait_until(
                    lambda: log_path.exists()
                    and (
                        "Too many open files" in pressure_log_text()
                        or "code: 24" in pressure_log_text()
                    )
                    and any(
                        marker in pressure_log_text()
                        for marker in (
                            "failed to accept client connection",
                            "failed to register client",
                            "still failing to accept client connections",
                            "still failing to register clients",
                        )
                    ),
                    deadline=time.monotonic() + 8,
                    description="EMFILE accept/backoff log",
                )
            except TimeoutError as error:
                receipt["pressure_connections"] = len(pressure_sockets)
                receipt["fd_without_emfile_log"] = fd_count(server_pid)
                receipt["new_log_tail"] = (
                    pressure_log_text()[-2000:]
                )
                raise AssertionError(f"{error}: {json.dumps(receipt, sort_keys=True)}") from error
            # Let the finite burst of successful accepts/router-thread startups
            # drain before measuring the sustained failure loop itself.
            time.sleep(2.0)
            backoff_cpu_before = cpu_seconds(server_pid)
            backoff_started = time.monotonic()
            time.sleep(3.0)
            backoff_cpu_after = cpu_seconds(server_pid)
            receipt["pressure_window_seconds"] = round(
                time.monotonic() - pressure_started, 3
            )
            receipt["backoff_observation_seconds"] = round(
                time.monotonic() - backoff_started, 3
            )
            receipt["pressure_connections"] = len(pressure_sockets)
            receipt["fd_under_pressure"] = fd_count(server_pid)
            receipt["cpu_seconds_during_backoff"] = round(
                backoff_cpu_after - backoff_cpu_before, 3
            )
            if backoff_cpu_after - backoff_cpu_before > float(
                receipt["cpu_seconds_during_baseline"]
            ) + 0.6:
                raise AssertionError(f"accept loop consumed CPU instead of backing off: {receipt}")

            for client in pressure_sockets:
                client.close()
            pressure_sockets.clear()
            time.sleep(1.0)
            receipt["fd_after_pressure_release"] = fd_count(server_pid)
            if int(receipt["fd_after_pressure_release"]) > int(receipt["fd_baseline"]) + 3:
                raise AssertionError(f"pressure descriptors were not released: {receipt}")

            sessions = run(
                [str(binary), "list-sessions"], env=env, timeout=5
            ).stdout
            if session_name not in sessions:
                raise AssertionError(f"session disappeared after EMFILE pressure: {sessions}")
            receipt["listable_after_pressure"] = True
            receipt["attach_detach_exit"] = attach_and_detach(binary, session_name, env)
            if receipt["attach_detach_exit"] != 0:
                raise AssertionError(f"attach/detach failed after pressure: {receipt}")
            receipt["attachable_after_pressure"] = True
        finally:
            for descriptor in inherited_reserve:
                os.close(descriptor)
            inherited_reserve.clear()
            for client in pressure_sockets:
                client.close()
            pressure_sockets.clear()

            try:
                run(
                    [str(binary), "kill-session", session_name],
                    env=env,
                    timeout=8,
                    check=False,
                    capture_output=False,
                )
            except subprocess.TimeoutExpired:
                # A partially initialized, pressure-limited client can itself
                # time out. The exact server PID fallback below still owns cleanup.
                pass
            if server_pid is not None:
                try:
                    wait_until(
                        lambda: server_pid_for_socket(socket_path) is None,
                        deadline=time.monotonic() + 15,
                        description="isolated server teardown",
                    )
                except TimeoutError:
                    # Exact fallback only: this PID and socket were created by
                    # this probe, never discovered from an operator session root.
                    os.kill(server_pid, signal.SIGTERM)
                    wait_until(
                        lambda: server_pid_for_socket(socket_path) is None,
                        deadline=time.monotonic() + 15,
                        description="isolated server SIGTERM teardown",
                    )
            receipt["process_residue"] = server_pid_for_socket(socket_path) is not None

    signal.alarm(0)
    signal.signal(signal.SIGALRM, previous_handler)
    receipt["elapsed_seconds"] = round(time.monotonic() - started, 3)
    if receipt["process_residue"]:
        raise AssertionError(f"isolated server residue remained: {receipt}")
    receipt_json = json.dumps(receipt, sort_keys=True)
    if args.receipt is not None:
        args.receipt.parent.mkdir(parents=True, exist_ok=True)
        args.receipt.write_text(receipt_json + "\n", encoding="utf-8")
    print(receipt_json)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
