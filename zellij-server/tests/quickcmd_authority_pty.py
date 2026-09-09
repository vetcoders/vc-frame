"""Isolated shortcut acceptance; requires pyte 0.8.2 and a committed vc-frame.

Run with --binary PATH --output NEW_DIRECTORY --scratch NEW_DRAGON_DIRECTORY.
Never attaches to an existing user session. Retains ANSI, grids and inventories.
Tabs A/B exercise workspace views, not the separately owned host/guest protocol.
"""

import argparse
import codecs
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import struct
import subprocess
import sys
import termios
import time

import pyte
import pyte.screens


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--modes", nargs="+", choices=("tab", "normal", "locked", "normal-after-B-A"),
                        default=("tab", "normal", "locked", "normal-after-B-A"))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    args.scratch.mkdir(parents=True, exist_ok=False)
    binary = str(args.binary.resolve())
    session = f"qca{os.getpid()}"
    env = {k: os.environ[k] for k in ("PATH", "USER", "LANG") if k in os.environ}
    env.update(TERM="xterm-256color", SHELL="/bin/sh", VC_FRAME_SERVER_FOREGROUND="1")
    for key, directory in {
        # macOS ProjectDirs ignores XDG paths. A subprocess-only fixture home
        # keeps even startup migration and --build-info away from user state.
        "HOME": "home", "TMPDIR": "tmp", "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache",
        "XDG_DATA_HOME": "data", "XDG_RUNTIME_DIR": "runtime", "XDG_STATE_HOME": "state",
        "VIBECRAFTED_HOME": "vc", "VC_FRAME_SOCKET_DIR": "s",
    }.items():
        path = args.scratch / directory
        path.mkdir()
        env[key] = str(path)
    env["VIBECRAFTED_CONTROL_PLANE"] = str(args.scratch / "no-control-plane.json")
    workload = args.scratch / "workload.py"
    events = args.output / "workloads.jsonl"
    workload.write_text(
        f"#!{sys.executable}\nimport os,sys,json\n"
        f"f=os.open({str(events)!r},os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)\n"
        "def event(kind,value):\n"
        " os.write(f,(json.dumps(dict(pid=os.getpid(),kind=kind,value=value))+'\\n').encode())\n"
        "event('start',os.environ.get('ZELLIJ_PANE_ID'))\n"
        "print('WORKLOAD_PID='+str(os.getpid()),flush=True)\n"
        "for line in sys.stdin:\n"
        " event('input',line.rstrip());print('RECEIVED:'+line.rstrip(),flush=True)\n"
    )
    workload.chmod(0o700)
    config = args.scratch / "config.kdl"
    config.write_text(
        f'default_shell "{workload}"\nshow_startup_tips false\n'
        'show_release_notes false\nsession_serialization false\ndefault_mode "normal"\n'
        'plugins { compact-bar location="zellij:compact-bar"; }\n'
        'keybinds { shared { bind "Super Shift ." { '
        'MessagePlugin "compact-bar" { name "vc_quick_cmd"; }; }; }; }\n'
    )
    repo = Path(__file__).resolve().parents[2]
    layout = args.scratch / "layout.kdl"
    template = (repo / "zellij-utils/assets/layouts/default.kdl").read_text()
    layout.write_text(template[:template.rfind("}")] + '\n tab name="A" { pane; }\n tab name="B" { pane; }\n}\n')
    (args.output / "config.kdl").write_text(config.read_text())
    (args.output / "layout.kdl").write_text(layout.read_text())
    receipt = {
        "binary": binary, "sha256": hashlib.sha256(args.binary.read_bytes()).hexdigest(),
        "session": session, "scratch": str(args.scratch), "steps": [],
        "shortcut_hex": "1b5b34363b313075",  # Kitty CSI-u: '.' + Shift + Super
        "requested_modes": args.modes,
        "limits": ["No physical macOS key event", "A/B are tabs, not host/guest sessions"],
    }
    receipt["build_info"] = json.loads(subprocess.check_output([binary, "--build-info"], env=env, timeout=20))
    assert not receipt["build_info"]["git_dirty"], "requires a committed binary"
    children = []

    # Match Frame's documented width convention; never patch rendered text.
    original_width = pyte.screens.wcwidth
    pyte.screens.wcwidth = lambda char: 1 if char == "𝌁" else original_width(char)

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(self.fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R")
            else:
                super().report_device_status(mode)

    def launch(create):
        pid, fd = pty.fork()
        if pid == 0:
            argv = [binary, "--config", str(config)]
            if create:
                argv += ["--new-session-with-layout", str(layout), "--session", session]
            else:
                argv += ["attach", session]
            os.execve(binary, argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        screen = Terminal(120, 30)
        screen.fd = fd
        child = dict(pid=pid, fd=fd, screen=screen, stream=pyte.Stream(screen),
                     decoder=codecs.getincrementaldecoder("utf-8")("replace"),
                     raw=(args.output / f"client-{len(children)}.ansi").open("wb"))
        children.append(child)
        return child

    def drain(seconds=0.2):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([c["fd"] for c in children], [], [], 0.05)
            for child in children:
                if child["fd"] not in ready:
                    continue
                try:
                    data = os.read(child["fd"], 65536)
                except OSError:
                    continue
                child["raw"].write(data)
                child["raw"].flush()
                child["stream"].feed(child["decoder"].decode(data))

    def wait(predicate, name, timeout=30):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
        snapshot(name + "-timeout")
        raise AssertionError(name)

    def snapshot(name):
        for i, child in enumerate(children):
            (args.output / f"{name}-client-{i}.txt").write_text("\n".join(child["screen"].display))

    def cli(*command):
        result = subprocess.run([binary, "--session", session, *command], env=env,
                                capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, (command, result.stderr)
        return result.stdout

    def inventory(name):
        data = json.loads(cli("action", "list-panes", "--all", "--json"))
        (args.output / f"{name}-panes.json").write_text(json.dumps(data, indent=2))
        return data

    def chrome(panes):
        rows = [p for p in panes if p["is_plugin"] and
                (p.get("plugin_url") or "").split(":")[-1] in
                ("compact-bar", "session-manager", "status-bar")]
        for kind in ("compact-bar", "session-manager", "status-bar"):
            instances = [p for p in rows if p["plugin_url"].split(":")[-1] == kind]
            assert len(instances) == 2, (kind, instances)
            assert len({p["plugin_runtime_id"] for p in instances}) == 1
            assert all(not p["exited"] and not p["is_suppressed"] for p in instances)
            if kind != "session-manager":
                assert all(p["pane_rows"] == 1 for p in instances)
        return sorted((p["id"], p["plugin_runtime_id"], p["tab_id"]) for p in rows)

    try:
        first = launch(True)
        wait(lambda: "WORKLOAD_PID=" in "\n".join(first["screen"].display)
             and "PANE" in first["screen"].display[-1], "bootstrap", 60)
        # This substrate starts one default tab even with an explicit startup
        # layout. Materialize the two-tab fixture through the real session API.
        initial_tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
        receipt["startup_tabs"] = initial_tabs
        if len(initial_tabs) == 1:
            cli("action", "rename-tab", "A")
            cli("action", "new-tab", "--name", "B")
            cli("action", "go-to-tab", "1")
            wait(lambda: "◉ A" in first["screen"].display[0], "two-tab-fixture")
        active = launch(False)
        wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
             and "PANE" in active["screen"].display[-1], "second-client", 60)
        before_clients = cli("action", "list-clients")
        receipt["clients_before"] = before_clients
        receipt["client_pids"] = [child["pid"] for child in children]
        receipt["panes_before"] = inventory("before")
        chrome_before = chrome(receipt["panes_before"])
        terminal_ids = {p["id"] for p in receipt["panes_before"] if not p["is_plugin"]}
        receipt["tabs_before"] = json.loads(cli("action", "list-tabs", "--all", "--json"))
        agent_pid = int(re.search(r"WORKLOAD_PID=(\d+)", "\n".join(active["screen"].display))[1])
        receipt["agent_pid"] = agent_pid
        snapshot("before")

        for index, mode in enumerate(args.modes):
            if mode == "normal-after-B-A":
                for position in (2, 1):
                    os.write(active["fd"], b"\x14")
                    drain(1)
                    os.write(active["fd"], str(position).encode())
                    current_tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
                    target = next(tab for tab in current_tabs if tab["position"] == position - 1)
                    baseline_tab = next(tab for tab in receipt["tabs_before"] if tab["tab_id"] == target["tab_id"])
                    assert target["tab_instance_id"] == baseline_tab["tab_instance_id"]
                    expected_tab = target["name"]
                    wait(lambda: "PANE" in active["screen"].display[-1]
                         and f"◉ {expected_tab}" in active["screen"].display[0], f"switch-{position}")
                    snapshot(f"switch-{position}")
            elif mode == "tab":
                os.write(active["fd"], b"\x14")
                wait(lambda: "New" in active["screen"].display[-1], "tab-mode")
            elif mode == "locked":
                os.write(active["fd"], b"\x07")
                wait(lambda: "LOCK" in active["screen"].display[-1]
                     and "PANE" not in active["screen"].display[-1], "locked-mode")
            snapshot(f"{index}-{mode}-before")
            os.write(active["fd"], bytes.fromhex(receipt["shortcut_hex"]))
            deadline = time.monotonic() + 30
            while True:
                drain(0.5)
                opened = inventory(f"{index}-open")
                new_panes = [p for p in opened if not p["is_plugin"] and p["id"] not in terminal_ids]
                if new_panes or time.monotonic() >= deadline:
                    break
            snapshot(f"{index}-{mode}-opened")
            assert chrome(opened) == chrome_before
            assert len(new_panes) == 1 and new_panes[0]["is_floating"], new_panes
            receipt["steps"].append({"mode": mode, "panes_open": opened})
            wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display),
                 f"{mode}-rendered-command")
            snapshot(f"{index}-{mode}-rendered")
            marker = args.output / f"{index}-command-executed"
            command = f"printf QC_EXECUTED_{index}; printf ok > {shlex.quote(str(marker))}\r"
            os.write(active["fd"], command.encode())
            wait(lambda: marker.exists(), f"{mode}-usable-command")
            assert marker.read_text() == "ok"
            snapshot(f"{index}-{mode}-executed")
            # Close this command shell's floating pane via the standard pane keys.
            if mode == "locked":
                os.write(active["fd"], b"\x07")
                wait(lambda: "PANE" in active["screen"].display[-1], "unlock-for-dismissal")
            os.write(active["fd"], b"\x10")
            wait(lambda: "Close" in active["screen"].display[-1], "pane-mode-for-dismissal")
            os.write(active["fd"], b"x")
            wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
                 and "PANE" in active["screen"].display[-1], f"{mode}-dismissed")
            snapshot(f"{index}-{mode}-dismissed")
            closed = inventory(f"{index}-closed")
            assert chrome(closed) == chrome_before
            assert {p["id"] for p in closed if not p["is_plugin"]} == terminal_ids
        receipt["clients_after"] = cli("action", "list-clients")
        assert receipt["clients_after"] == before_clients
        for child in children:
            assert os.waitpid(child["pid"], os.WNOHANG) == (0, 0), "client exited during shortcut sequence"
        receipt["panes_after"] = inventory("after")
        assert chrome(receipt["panes_after"]) == chrome_before
        receipt["tabs_after"] = json.loads(cli("action", "list-tabs", "--all", "--json"))
        identity = lambda tabs: sorted((t["tab_id"], t["tab_instance_id"], t["session_incarnation"]) for t in tabs)
        assert identity(receipt["tabs_after"]) == identity(receipt["tabs_before"])
        os.write(active["fd"], b"AGENT_ALIVE\r")
        wait(lambda: "RECEIVED:AGENT_ALIVE" in "\n".join(active["screen"].display), "agent-still-responsive")
        snapshot("agent-still-responsive")
        recorded_events = list(map(json.loads, events.read_text().splitlines()))
        assert [e for e in recorded_events if e["kind"] == "input"] == [
            {"pid": agent_pid, "kind": "input", "value": "AGENT_ALIVE"}
        ], "command input leaked or the original agent process was replaced"
        for event in recorded_events:
            os.kill(event["pid"], 0)
        receipt["status"] = "passed_bounded_pty_scenario"
    except Exception as error:
        receipt.update(status="failed", error=repr(error))
        snapshot("failure")
        raise
    finally:
        receipt_path = args.output / "receipt.json"
        receipt_path.write_text(json.dumps(receipt, indent=2))
        if children:
            try:
                result = subprocess.run([binary, "kill-session", session, "--yes"], env=env,
                                        capture_output=True, text=True, timeout=10)
                receipt["cleanup"] = {"code": result.returncode, "stderr": result.stderr}
            except subprocess.TimeoutExpired:
                receipt["cleanup"] = {"error": "own session shutdown timed out"}
        for child in children:
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    if os.waitpid(child["pid"], os.WNOHANG)[0]:
                        break
                except ChildProcessError:
                    break
                time.sleep(0.1)
            else:
                os.kill(child["pid"], signal.SIGKILL)
                # A macOS process in kernel exit can outlive SIGKILL. Never
                # lose the scenario receipt to an unbounded blocking waitpid.
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if os.waitpid(child["pid"], os.WNOHANG)[0]:
                        break
                    time.sleep(0.1)
                else:
                    receipt.setdefault("unreaped_owned_children", []).append(child["pid"])
            os.close(child["fd"])
            child["raw"].close()
        receipt_path.write_text(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    main()
