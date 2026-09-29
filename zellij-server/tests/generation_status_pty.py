"""Private PTY/dump-screen acceptance for generation rotation (requires pyte).

Run: uv run --with pyte python zellij-server/tests/generation_status_pty.py
     --binary target/debug/vc-frame --output /absolute/receipt-directory
Only fixture processes and fixture installation pointers are modified.
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
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import time

import pyte


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix="vcgen-")).resolve()
    old = "4.3.1+gf8debfd6"
    new = "4.3.1+g7a69d24d"
    runtime = scratch / "installation"
    binary = runtime / "releases" / old / "libexec/vc-frame"
    binary.parent.mkdir(parents=True)
    shutil.copy2(args.binary, binary)
    (runtime / "releases" / new).mkdir()

    def activate(generation):
        temporary = runtime / "next.json"
        temporary.write_text(json.dumps({
            "schema": "vibecrafted.active-runtime.v1",
            "runtime_root": str(runtime / "releases" / generation),
        }))
        temporary.replace(runtime / "active.json")

    activate(old)
    env = {key: os.environ[key] for key in ("PATH", "USER", "LANG") if key in os.environ}
    env.update(TERM="xterm-256color", SHELL="/bin/sh", VC_FRAME_SERVER_FOREGROUND="1")
    for key in ("HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                "XDG_RUNTIME_DIR", "XDG_STATE_HOME", "VIBECRAFTED_HOME", "VC_FRAME_SOCKET_DIR"):
        directory = scratch / key.lower()
        directory.mkdir()
        env[key] = str(directory)
    # A stale inherited root must not choose either side of the comparison.
    env["VIBECRAFTED_RUNTIME_ROOT"] = str(runtime / "releases" / new)
    config = scratch / "config.kdl"
    config.write_text('default_shell "/bin/sh"\ndefault_mode "locked"\n'
                      'show_startup_tips false\nshow_release_notes false\n'
                      'session_serialization false\nauto_lock_after_seconds 0\n')
    layout = scratch / "layout.kdl"
    layout.write_text('layout {\n pane\n pane size=1 borderless=true {\n'
                      ' plugin location="zellij:status-bar"\n }\n}\n')
    session = f"gen{os.getpid()}"
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(str(binary), [str(binary), "--config", str(config),
                  "--new-session-with-layout", str(layout), "--session", session], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R")
            else:
                super().report_device_status(mode)

    screen = Terminal(120, 30)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    raw = (args.output / "terminal.ansi").open("wb")

    def wait_for(predicate, timeout=40):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            if select.select([fd], [], [], 0.1)[0]:
                data = os.read(fd, 65536)
                raw.write(data)
                stream.feed(decoder.decode(data))
            if predicate():
                return
        raise AssertionError("timed out: " + "\n".join(screen.display))

    def cli(*command):
        return subprocess.check_output([str(binary), "--session", session, *command],
                                       env=env, text=True, timeout=15)

    try:
        wait_for(lambda: old in screen.display[-1])
        before = screen.display[-1]
        assert "HEALTH" in before and old + " !" not in before, before
        panes = json.loads(cli("action", "list-panes", "--json", "--all"))
        (args.output / "panes.json").write_text(json.dumps(panes, indent=2))
        # The fixture has exactly one plugin: the status bar.
        rows = panes if isinstance(panes, list) else panes["panes"]
        plugin = next(row for row in rows if row["is_plugin"])
        pane_id = f"plugin_{plugin['id']}"
        before_style = screen.buffer[29][before.index(old)]
        terminal_id = f"terminal_{next(row for row in rows if not row['is_plugin'])['id']}"
        os.write(fd, b"printf 'GENERATION_SESSION_SURVIVES\\n'; sleep 300\n")
        wait_for(lambda: any(line.strip() == "GENERATION_SESSION_SURVIVES" for line in screen.display[:-1]))
        terminal_before = cli("action", "dump-screen", "--pane-id", terminal_id)
        before_dump = cli("action", "dump-screen", "--pane-id", pane_id)
        (args.output / "before.txt").write_text(before_dump)
        activate(new)
        wait_for(lambda: old + " !" in screen.display[-1])
        after_style = screen.buffer[29][screen.display[-1].index(old)]
        assert (before_style.fg, before_style.bg, before_style.bold) != (after_style.fg, after_style.bg, after_style.bold)
        terminal_after = cli("action", "dump-screen", "--pane-id", terminal_id)
        assert terminal_after == terminal_before, "rotation must preserve pane content"
        after_dump = cli("action", "dump-screen", "--pane-id", pane_id)
        after_ansi = cli("action", "dump-screen", "--pane-id", pane_id, "--ansi")
        (args.output / "after.txt").write_text(after_dump)
        (args.output / "after.ansi").write_text(after_ansi)
        assert old in before_dump and old + " !" in after_dump
        assert "HEALTH" in after_dump
        assert all(label not in after_dump for label in ("VERTICAL", "HORIZONTAL", "BASE"))
        os.kill(pid, 0)
        receipt = dict(status="passed", session=session, client_pid=pid, scratch=str(scratch),
                       running=old, active=new, before=before_dump, after=after_dump,
                       binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                       build_info=json.loads(cli("--build-info")),
                       limitation="Private installation fixture; no Founder host or installed pack changed")
        (args.output / "receipt.json").write_text(json.dumps(receipt, indent=2))
        print(json.dumps(receipt, indent=2))
    finally:
        raw.close()
        # Only the session created above; never kill-all or name inference.
        subprocess.run([str(binary), "kill-session", session], env=env,
                       capture_output=True, timeout=15)
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.waitpid(pid, 0)
        os.close(fd)


if __name__ == "__main__":
    main()
