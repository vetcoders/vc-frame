#!/usr/bin/env python3
"""Exercise creation shortcuts through an isolated real Frame client PTY.

Use --binary /path/to/native/vc-frame --config /path/to/config.kdl --artifacts
/private/path. --hot-reload replaces the config atomically after startup.
--negative proves that a baseline config forwards the unbound key to the pane.
No live session, provider, display, or installed configuration is changed.
"""

import argparse
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

parser = argparse.ArgumentParser()
parser.add_argument("--config", type=Path, required=True)
parser.add_argument("--output", "--artifacts", type=Path, required=True)
parser.add_argument("--negative", action="store_true")
parser.add_argument("--hot-reload", action="store_true")
parser.add_argument("--binary", type=Path)
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
if args.binary:
    binary = str(args.binary.resolve(strict=True))
else:
    active = json.loads(
        (Path.home() / ".local/share/vibecrafted/active.json").read_text()
    )
    binary = str(Path(active["runtime_root"]) / "libexec/vc-frame")
assert Path(binary).is_file()
session = "keys-" + str(os.getpid())
sock = tempfile.mkdtemp(prefix="vck-", dir="/tmp")
home = root / "home"
home.mkdir(exist_ok=True)
app = root / "pane.py"
app.write_text(
    'import os,tty\nfrom pathlib import Path\ntty.setraw(0)\np=Path(os.environ["KEY_INPUT_DIR"])/("input-"+str(os.getpid())+".bin")\nwith p.open("ab",buffering=0) as f:\n while True:\n  b=os.read(0,65536)\n  if not b: break\n  f.write(b)\n'
)
shell = root / "shell"
shell.write_text(
    "#!/bin/sh\nexec " + shlex.join([sys.executable, "-I", "-B", str(app)]) + "\n"
)
shell.chmod(0o700)
layout = root / "plain.kdl"
layout.write_text("layout { pane; }\n")
config = args.config.read_text()
config, replaced = re.subn(
    r"^default_shell .*$", "default_shell " + json.dumps(str(shell)), config, flags=re.M
)
if not replaced:
    config += "\ndefault_shell " + json.dumps(str(shell)) + "\n"
config = re.sub(r"^default_layout .*$", "", config, flags=re.M)
config += (
    "\ndefault_layout "
    + json.dumps(str(layout))
    + "\nshow_release_notes false\nshow_startup_tips false\nsupport_kitty_keyboard_protocol true\n"
)
cfg = root / "config.kdl"
baseline = re.sub(
    r"^ *bind \"Ctrl (?:Shift )?=\" \"Ctrl (?:Shift )?\+\".*\n", "", config, flags=re.M
)
cfg.write_text(baseline if args.hot_reload else config)
env = {
    "HOME": str(home),
    "XDG_CONFIG_HOME": str(home / "config"),
    "XDG_CACHE_HOME": str(home / "cache"),
    "XDG_DATA_HOME": str(home / "data"),
    "VIBECRAFTED_HOME": str(home / "vch"),
    "VC_FRAME_SOCKET_DIR": sock,
    "VC_FRAME_CONFIG_DIR": str(root),
    "VC_SERVER_URL": "fixture-disabled",
    "TERM": "xterm-256color",
    "LANG": "en_US.UTF-8",
    "PATH": os.environ["PATH"],
    "USER": os.environ.get("USER", "fixture"),
    "LOGNAME": os.environ.get("USER", "fixture"),
    "SHELL": str(shell),
    "KEY_INPUT_DIR": str(root),
}
for name in ["XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "VIBECRAFTED_HOME"]:
    Path(env[name]).mkdir(parents=True, exist_ok=True)
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))


def ctty():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


# This CLI deliberately runs the selected local Frame executable. All arguments
# remain separate argv entries; no shell interprets executable or artifact paths.
client = subprocess.Popen(
    [  # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
        binary,
        "--config",
        str(cfg),
        "--layout",
        str(layout),
        "attach",
        "--create",
        session,
    ],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    env=env,
    cwd=root,
    preexec_fn=ctty,
)
os.close(slave)
stop = threading.Event()
raw = bytearray()


def reader():
    scanned = 0
    while not stop.is_set():
        try:
            if not select.select([master], [], [], 0.05)[0]:
                continue
            chunk = os.read(master, 65536)
        except OSError:
            return
        if not chunk:
            return
        raw.extend(chunk)
        while True:
            at = raw.find(b"\x1b[c", scanned)
            if at < 0:
                break
            os.write(master, b"\x1b[?1;2c")
            scanned = at + 3


thread = threading.Thread(target=reader, daemon=True)
thread.start()


def action(*argv):
    r = subprocess.run(
        [binary, "--session", session, "action", *argv],
        env=env,
        cwd=root,
        capture_output=True,
        text=True,
        timeout=5,
    )
    if r.returncode:
        raise RuntimeError(r.stderr)
    return r.stdout


def panes():
    data = json.loads(action("list-panes", "--all", "--json"))
    (root / "last-panes.json").write_text(json.dumps(data, indent=2))
    return [p for p in data if not p["is_plugin"]]


def tab_count():
    data = json.loads(action("list-tabs", "--json"))
    return len(data["tabs"] if isinstance(data, dict) else data)


def wait_for(condition, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            result = condition()
            if result:
                return result
        except (RuntimeError, json.JSONDecodeError):
            pass
        time.sleep(0.05)
    raise AssertionError("condition timed out")


results = []
try:
    wait_for(lambda: len(panes()) == 1 and list(root.glob("input-*.bin")), 20)
    if args.hot_reload:
        # The server starts its config watcher asynchronously after pane setup.
        time.sleep(1.2)
        replacement = cfg.with_suffix(".new")
        replacement.write_text(config)
        os.replace(replacement, cfg)
        time.sleep(2.2)
        results.append({"hot_reload_without_restart": True, "client_pid": client.pid})
    for number in [61, 43, 57413]:
        before = panes()
        old = next(
            p
            for p in before
            if p["is_focused"] and p["tab_id"] == max(x["tab_id"] for x in before)
        )
        wire = f"\x1b[{number};5u".encode()
        os.write(master, wire)
        if args.negative:
            time.sleep(0.7)
            after = panes()
            assert len(after) == len(before), after
            assert any(p.read_bytes() == b"=" for p in root.glob("input-*.bin"))
            results.append(
                {
                    "negative_control": "shortcut absent; decoded key reached legacy pane",
                    "wire": wire.hex(),
                }
            )
            break
        wait_for(lambda: len(panes()) == len(before) + 1)
        after = panes()
        new = next(p for p in after if p["id"] not in {a["id"] for a in before})
        old_after = next(p for p in after if p["id"] == old["id"])
        assert new["tab_id"] == old_after["tab_id"], (new, old_after)
        assert new["pane_x"] > old_after["pane_x"], (new, old_after)
        tabs = tab_count()
        shifted = f"\x1b[{number};6u".encode()
        os.write(master, shifted)
        wait_for(lambda: tab_count() == tabs + 1)
        assert {p["id"] for p in after}.issubset({p["id"] for p in panes()})
        results.append(
            {
                "key_code": number,
                "right_split": True,
                "shift_new_tab": True,
                "prior_panes_retained": True,
            }
        )
    if not args.negative:
        action("switch-mode", "locked")
        time.sleep(0.15)
        before = panes()
        tabs = action("list-tabs", "--json")
        wire = b"\x1b[61;5u\x1b[43;6u"
        os.write(master, wire)
        wait_for(
            lambda: any(
                p.read_bytes().endswith(b"=+") for p in root.glob("input-*.bin")
            )
        )
        assert len(panes()) == len(before) and action("list-tabs", "--json") == tabs
        results.append({"locked_passthrough": True})
    (root / "result.json").write_text(
        json.dumps(
            {"status": "pass", "negative": args.negative, "results": results}, indent=2
        )
        + "\n"
    )
    print(json.dumps({"status": "pass", "negative": args.negative, "results": results}))
finally:
    (root / "outer.bin").write_bytes(raw)
    # Same explicitly selected executable, limited to this fixture's session.
    subprocess.run(
        # nosemgrep: python.lang.security.audit.dangerous-subprocess-use-tainted-env-args.dangerous-subprocess-use-tainted-env-args
        [
            binary,
            "kill-session",
            session,
        ],
        env=env,
        cwd=root,
        capture_output=True,
        timeout=10,
    )
    if client.poll() is None:
        try:
            client.wait(timeout=5)
        except subprocess.TimeoutExpired:
            client.terminate()
            client.wait(timeout=5)
    stop.set()
    os.close(master)
    thread.join(timeout=1)
