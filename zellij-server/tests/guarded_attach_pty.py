#!/usr/bin/env python3
"""Exact-incarnation native attach acceptance through real isolated PTYs.

python3 zellij-server/tests/guarded_attach_pty.py --binary /candidate/vc-frame \
    --output /new/receipt-directory

Requires pyte. Never builds, installs, launches a provider or addresses live
sessions. Refused clients use different terminal geometry and configuration;
membership, original geometry and workloads must survive their refusal.
"""
import argparse
import codecs
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import uuid

from peer_sessions_pty import parse_client_memberships, verify_build_info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=30)
    args = parser.parse_args()
    import pyte

    binary = args.binary.resolve(strict=True)
    assert os.access(binary, os.X_OK), binary
    args.output.mkdir(parents=True, exist_ok=False)
    out = args.output.resolve()
    scratch = Path(tempfile.mkdtemp(prefix="vcga-", dir="/tmp")).resolve()
    env = {key: os.environ[key] for key in ("PATH", "USER", "LOGNAME", "LANG") if key in os.environ}
    env.update(TERM="xterm-256color", COLORTERM="truecolor", SHELL="/bin/sh",
               VC_FRAME_SERVER_FOREGROUND="1", VC_FRAME_ROUTE_DIAGNOSTICS="1")
    for key in ("HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "VIBECRAFTED_HOME", "VC_FRAME_CONFIG_DIR"):
        path = scratch / key.lower()
        path.mkdir()
        env[key] = str(path)
    sockets = scratch / "s"
    sockets.mkdir()
    env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets),
               VIBECRAFTED_CONTROL_PLANE=str(scratch / "disabled-control-plane.json"))
    config = scratch / "config.kdl"
    config.write_text('default_shell "/bin/sh"\ndefault_mode "normal"\n'
                      'show_startup_tips false\nshow_release_notes false\n'
                      'session_serialization true\nauto_lock_after_seconds 0\n'
                      'pane_frames false\n')
    refused_config = scratch / "refused-config.kdl"
    refused_config.write_text(config.read_text().replace('default_mode "normal"', 'default_mode "locked"')
                              .replace('pane_frames false', 'pane_frames true'))
    workload_events = out / "workloads.jsonl"
    workload = scratch / "workload.py"
    workload.write_text('''import json,os,sys
label=sys.argv[1];pid=os.getpid()
def record(kind,value):
 with open(%r,'a') as f:f.write(json.dumps(dict(label=label,pid=pid,ppid=os.getppid(),kind=kind,value=value,tty=os.ttyname(0)))+'\\n')
record('start',os.getcwd())
print('READY:'+label+':'+str(pid),flush=True)
for line in sys.stdin:
 token=line.rstrip();record('input',token);print('ACK:'+label+':'+str(pid)+':'+token,flush=True)
''' % str(workload_events))
    receipt = {"status": "running", "binary": str(binary), "scratch": str(scratch),
               "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               "commands": [], "steps": [], "cleanup": [],
               "limits": "Private native PTYs and IPC; no desktop delivery or installed app acceptance."}
    clients = []
    sessions = {"guard-source", "guard-target"}
    base = [str(binary), "--config", str(config), "--config-dir", env["VC_FRAME_CONFIG_DIR"]]

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(self.fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R")
            else:
                super().report_device_status(mode)

    def pump(seconds=.1):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            live = [c for c in clients if c["fd"] >= 0]
            if not live:
                time.sleep(.02)
                continue
            ready, _, _ = select.select([c["fd"] for c in live], [], [], .05)
            for c in live:
                if c["fd"] in ready:
                    try:
                        data = os.read(c["fd"], 65536)
                    except OSError:
                        data = b""
                    c["raw"].write(data)
                    c["raw"].flush()
                    c["stream"].feed(c["decoder"].decode(data))
                if c.get("exit") is None:
                    pid, status = os.waitpid(c["pid"], os.WNOHANG)
                    if pid:
                        c["exit"] = os.waitstatus_to_exitcode(status)

    def save(label):
        for index, c in enumerate(clients):
            (out / f"{label}-client-{index}.txt").write_text("\n".join(c["screen"].display))
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

    def wait(predicate, label):
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            pump()
            if predicate():
                return
        save(label + "-timeout")
        raise AssertionError(label)

    def cli(*command, check=True):
        argv = base + list(map(str, command))
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(argv, env=env, cwd=scratch, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=log)
            deadline = time.monotonic() + args.timeout
            while process.poll() is None and time.monotonic() < deadline:
                pump()
            if process.poll() is None:
                process.kill()
                process.wait()
                raise AssertionError(("fixture CLI timeout", argv))
            log.seek(0)
            text = log.read().decode(errors="replace")
        receipt["commands"].append({"argv": argv, "exit": process.returncode, "output": text})
        if check:
            assert process.returncode == 0, (argv, text)
        return process.returncode, text

    def inventory(session):
        return {"members": parse_client_memberships(cli("--session", session, "action", "list-clients")[1]),
                "panes": json.loads(cli("--session", session, "action", "list-panes", "--all", "--json", "--command")[1]),
                "tabs": json.loads(cli("--session", session, "action", "list-tabs", "--all", "--json")[1])}

    def incarnation(session):
        tabs = inventory(session)["tabs"]
        tokens = {tab["session_incarnation"] for tab in tabs}
        assert len(tokens) == 1 and next(iter(tokens)), tabs
        return tokens.pop()

    def events():
        return [json.loads(line) for line in workload_events.read_text().splitlines()] if workload_events.exists() else []

    def create(session, label):
        layout = scratch / (label + ".kdl")
        layout.write_text('layout { pane command=%s { args %s %s; }; }\n' %
                          (json.dumps(sys.executable), json.dumps(str(workload)), json.dumps(label)))
        cli("--layout", layout, "attach", "--create-background", session)
        wait(lambda: any(e["kind"] == "start" and e["label"] == label for e in events()), "started-" + label)
        return next(e for e in events() if e["kind"] == "start" and e["label"] == label)

    def attach(session, token=None, *, refused=False):
        argv = base.copy()
        if refused:
            argv[2] = str(refused_config)
        if token is not None:
            argv += ["--expected-session-incarnation", token]
        argv += ["attach", session]
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(scratch)
            os.execve(binary, argv, env)
        rows, cols = (25, 71) if refused else (40, 120)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        screen = Terminal(cols, rows)
        screen.fd = fd
        c = dict(pid=pid, fd=fd, exit=None, screen=screen, stream=pyte.Stream(screen),
                 decoder=codecs.getincrementaldecoder("utf-8")("replace"),
                 raw=(out / f"client-{len(clients)}.ansi").open("wb"), argv=argv)
        clients.append(c)
        receipt["steps"].append({"attach_argv": argv, "frontend_pid": pid, "geometry": [cols, rows]})
        return c

    def ack(client, row, token):
        os.write(client["fd"], (token + "\r").encode())
        wait(lambda: any(e["kind"] == "input" and e["value"] == token for e in events()), token)
        matched = [e for e in events() if e["kind"] == "input" and e["value"] == token]
        assert len(matched) == 1 and matched[0]["pid"] == row["pid"], matched
        wait(lambda: f"ACK:{row['label']}:{row['pid']}:{token}" in "\n".join(client["screen"].display), "rendered-" + token)

    def refusal(label, token, expected_inventory=None):
        before_starts = [e for e in events() if e["kind"] == "start"]
        source_before = inventory("guard-source")
        c = attach("guard-target", token, refused=True)
        wait(lambda: c["exit"] is not None, "refused-exit-" + label)
        assert c["exit"] != 0, ("guard silently succeeded", label, c["exit"])
        if expected_inventory is not None:
            assert inventory("guard-target") == expected_inventory, label
        assert [e for e in events() if e["kind"] == "start"] == before_starts, label
        assert inventory("guard-source") == source_before, label
        os.kill(source["pid"], 0)
        os.kill(source["ppid"], 0)
        ack(source_client, source, "source-survives-" + label)
        receipt["steps"].append({"case": label, "exit": c["exit"], "workloads_unchanged": True,
                                  "target_inventory_unchanged": expected_inventory is not None})
        save(label)

    def target_cache():
        return {str(p.relative_to(scratch)): hashlib.sha256(p.read_bytes()).hexdigest()
                for p in scratch.rglob("session-layout.kdl") if p.parent.name == "guard-target"}

    try:
        receipt["build_info"] = verify_build_info(cli("--build-info")[1])
        source = create("guard-source", "source")
        original = create("guard-target", "original-target")
        source_client = attach("guard-source")
        wait(lambda: len(inventory("guard-source")["members"]) == 1, "source-attached")
        ack(source_client, source, "source-before")
        token = incarnation("guard-target")
        correct = attach("guard-target", token)
        wait(lambda: len(inventory("guard-target")["members"]) == 1, "correct-token-attached")
        ack(correct, original, "correct-token-before-refusal")
        before = inventory("guard-target")
        receipt["steps"].append({"case": "correct-token", "incarnation": token, "inventory": before})
        wrong = str(uuid.uuid4())
        assert wrong != token
        refusal("wrong-live-token", wrong, before)
        ack(correct, original, "correct-client-after-refusal")
        os.kill(original["pid"], 0)
        os.kill(original["ppid"], 0)

        cli("--session", "guard-target", "action", "save-session")
        wait(lambda: bool(target_cache()), "serialized-cache-exists")
        cli("kill-session", "guard-target")
        wait(lambda: "EXITED" in cli("list-sessions", "--no-formatting")[1]
             and "guard-target" in cli("list-sessions", "--no-formatting")[1], "target-serialized-dead")
        saved = target_cache()
        assert saved, "resurrection cache was not retained"
        refusal("absent-saved-target", token)
        assert target_cache() == saved, "guarded attach changed the saved target"
        assert cli("--session", "guard-target", "action", "list-clients", check=False)[0] != 0, "guard resurrected target"
        assert "EXITED" in cli("list-sessions", "--no-formatting")[1]

        # Deleting only this fixture cache enables a truly new same-name seat.
        # Its observed server incarnation, never its requested name, is truth.
        cli("delete-session", "guard-target")
        replacement = create("guard-target", "replacement-target")
        replacement_token = incarnation("guard-target")
        assert replacement_token != token
        replacement_before = inventory("guard-target")
        refusal("old-token-new-incarnation", token, replacement_before)
        admitted = attach("guard-target", replacement_token)
        wait(lambda: len(inventory("guard-target")["members"]) == 1, "replacement-correct-token")
        ack(admitted, replacement, "replacement-admitted")
        os.kill(source["pid"], 0)
        os.kill(replacement["pid"], 0)
        receipt.update(status="passed", original_incarnation=token, replacement_incarnation=replacement_token,
                       saved_target_cache=saved, workloads=events())
        save("final")
    except BaseException as exc:
        receipt.update(status="failed", error=f"{type(exc).__name__}: {exc}")
        save("failure")
        raise
    finally:
        for session in sorted(sessions):
            try:
                code, text = cli("kill-session", session, check=False)
                receipt["cleanup"].append({"session": session, "exit": code, "output": text})
            except (AssertionError, OSError) as exc:
                receipt["cleanup"].append({"session": session, "error": str(exc)})
        for c in clients:
            if c["exit"] is None:
                try:
                    os.kill(c["pid"], signal.SIGTERM)
                    os.waitpid(c["pid"], 0)
                except (ProcessLookupError, ChildProcessError):
                    pass
            os.close(c["fd"])
            c["fd"] = -1
            c["raw"].close()
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(json.dumps({"status": receipt["status"], "receipt": str(out / "receipt.json")}))


if __name__ == "__main__":
    main()
