#!/usr/bin/env python3
"""Headless host-query/resize regression; writes dumps, PTY bytes and replies.

python3 zellij-server/tests/host_query_resize_pty.py --binary /path/vc-frame --artifacts /private/path
Use --expect-regression with an unmodified donor to prove the negative control.
No provider, display, live session or installed config is used.
"""
import argparse
from pathlib import Path


def app(mode):
    import os, select, time, tty
    tty.setraw(0)
    for _ in range(200):
        cols = os.get_terminal_size(1).columns
        if cols:
            break
        time.sleep(0.02)
    assert cols == 120, cols
    os.write(1, b"READY")
    os.read(0, 1)
    query = b"\x1b]11;?\x1b\\" if mode != "control" else b""
    payload = b"\x1b[2J\x1b[HABCDEFGHIJKLMNOPQRST\x1b[1;119HX"
    suffix = b"\x1b[6n\x1b]10;?\x1b\\\x1b[16t\x1b[5n" if mode == "ordered" else b""
    # Exactly one app write; no output after the kick, even on SIGWINCH.
    os.write(1, query + payload + suffix)
    with open(os.environ["FRAME_TEST_REPLIES"], "wb", buffering=0) as replies:
        while True:
            if select.select([0], [], [], 1)[0]:
                data = os.read(0, 65536)
                if not data:
                    return
                replies.write(data)


def main():
    import json, os, subprocess, sys
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--expect-regression", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    root = args.artifacts.resolve()
    root.mkdir(parents=True, exist_ok=True)
    results = {}
    script = str(Path(__file__).resolve())
    for mode in ("control", "query", "ordered"):
        case = root / mode
        case.mkdir(exist_ok=True)
        cfg = case / "cfg"
        cfg.mkdir(exist_ok=True)
        (cfg / "config.kdl").write_text('show_release_notes false\n')
        # JSON quoting is valid for these KDL strings.
        layout = 'layout {\n    pane borderless=true command=%s {\n        args %s "--app" %s\n    }\n}\n' % tuple(map(json.dumps, (sys.executable, script, mode)))
        (case / (mode + ".kdl")).write_text(layout)
        env = dict(os.environ, FRAME_TEST_BINARY=str(binary), FRAME_TEST_CASE_DIR=str(case), FRAME_TEST_REPLIES=str(case / "replies.bin"))
        run = subprocess.run([sys.executable, script, "--case", mode, "hq-" + mode + "-" + str(os.getpid())], env=env, capture_output=True, text=True, timeout=100)
        (case / "driver.log").write_text(run.stdout + run.stderr)
        events = json.loads((case / (mode + ".events.json")).read_text())
        assert events["result"]["status"] == "ok", events["result"]
        pre = json.loads((case / (mode + ".geom-pre.json")).read_text().split("\nSTDERR:")[0])
        post = json.loads((case / (mode + ".geom-post.json")).read_text().split("\nSTDERR:")[0])
        for geometry, expected_size in ((pre, (120, 40)), (post, (100, 35))):
            terminal = next(pane for pane in geometry if not pane["is_plugin"])
            assert (terminal["pane_content_columns"], terminal["pane_content_rows"]) == expected_size, terminal
        dump = (case / (mode + ".dump.txt")).read_text().split("\nSTDERR:")[0]
        line = next(line for line in dump.splitlines() if "ABCDEFGHIJKLMNOPQRST" in line)
        xcol = line.index("X") + 1
        if mode != "control":
            assert events["result"]["forward_offset_after_kick"] >= events["result"]["kick_offset"]
        results[mode] = {"x_column": xcol, "events": events["events"], "pre": pre, "post": post}
        if mode == "ordered":
            replies = (case / "replies.bin").read_bytes()
            expected = b"\x1b]11;rgb:1111/2222/3333\x1b\\\x1b[1;120R\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\\x1b[6;16;8t\x1b[0n"
            if args.expect_regression:
                assert b"\x1b[1;101R" in replies, replies
            else:
                assert replies == expected, replies
            results[mode]["replies_hex"] = replies.hex()
    assert results["control"]["x_column"] == 119, results
    assert results["query"]["x_column"] == (100 if args.expect_regression else 119), results
    assert results["ordered"]["x_column"] == (100 if args.expect_regression else 119), results
    (root / "summary.json").write_text(json.dumps(results, indent=2))
    print(json.dumps({mode: result["x_column"] for mode, result in results.items()}))


if __name__ == "__main__":
    import sys
    if len(sys.argv) > 1 and sys.argv[1] == "--app":
        app(sys.argv[2])
        raise SystemExit
    if len(sys.argv) < 2 or sys.argv[1] != "--case":
        main()
        raise SystemExit

import os, sys, pty, fcntl, termios, struct, select, subprocess, time, json, signal, re, threading

# A private outer PTY emulates the host. Never attach to a live session.
BIN = os.environ["FRAME_TEST_BINARY"]
RD = os.environ["FRAME_TEST_CASE_DIR"]
mode, sess = sys.argv[2], sys.argv[3]
sockdir = "/tmp/r2s" + str(os.getpid())
os.makedirs(sockdir, exist_ok=True)
assert len(os.path.join(sockdir, sess)) < 100

env = {
    "HOME": RD + "/home-" + mode,
    "XDG_CONFIG_HOME": RD + "/home-" + mode + "/xdg/config",
    "XDG_CACHE_HOME": RD + "/home-" + mode + "/xdg/cache",
    "XDG_DATA_HOME": RD + "/home-" + mode + "/xdg/data",
    "VC_FRAME_SOCKET_DIR": sockdir,
    "VC_SERVER_URL": "fixture-disabled",  # reject URL before any HTTP polling
    "VC_FRAME_CONFIG_DIR": RD + "/cfg",
    "VIBECRAFTED_HOME": RD + "/home-" + mode + "/vch",
    "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
    "TERM": "xterm-256color",
    "LANG": "en_US.UTF-8",
    "SHELL": "/bin/zsh",
    "USER": os.environ.get("USER", "unknown"),
    "LOGNAME": os.environ.get("USER", "unknown"),
    "FRAME_TEST_REPLIES": os.environ["FRAME_TEST_REPLIES"],
    "__CF_USER_TEXT_ENCODING": os.environ.get("__CF_USER_TEXT_ENCODING", ""),
}
for k in (env["HOME"], env["XDG_CONFIG_HOME"], env["XDG_CACHE_HOME"], env["XDG_DATA_HOME"]):
    os.makedirs(k, exist_ok=True)
seen_rel = env["HOME"] + "/Library/Caches/io.vetcoders.vc-frame/4.4.0"
os.makedirs(seen_rel, exist_ok=True)
open(seen_rel + "/seen_release_notes", "w").close()

state = {"stop": False, "chunks": [], "buf": bytearray(), "answered_da_until": 0}
t0 = time.monotonic()
events = []
def note(ev):
    events.append((round(time.monotonic() - t0, 3), ev))

def _ctty():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)

def set_winsize(fd, rows, cols):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

master_fd = slave_fd = None
client = None
reader_started = threading.Event()

def reader():
    while not state["stop"]:
        try:
            r, _, _ = select.select([master_fd], [], [], 0.05)
        except Exception:
            break
        if not r:
            continue
        try:
            chunk = os.read(master_fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        off = len(state["buf"])
        state["buf"].extend(chunk)
        state["chunks"].append((round(time.monotonic() - t0, 3), off, len(chunk)))

def answer_startup_da(deadline_off):
    # pre-kick only: answer primary DA occurrences so client startup unblocks
    buf = state["buf"]
    start = state["answered_da_until"]
    i = buf.find(b"\x1b[c", start)
    while i != -1 and i < deadline_off:
        os.write(master_fd, b"\x1b[?1;2c")
        state["answered_da_until"] = i + 3
        note("answered-DA@%d" % i)
        i = buf.find(b"\x1b[c", i + 3)
    return

result = {"mode": mode, "session": sess, "socket_dir": sockdir}
try:
    master_fd, slave_fd = pty.openpty()
    set_winsize(master_fd, 40, 120)
    client = subprocess.Popen(
        [BIN, "--layout", os.path.join(RD, mode + ".kdl"), "attach", "--create", sess],
        stdin=slave_fd, stdout=slave_fd, stderr=slave_fd, env=env, cwd=RD,
        close_fds=True, preexec_fn=_ctty,
    )
    os.close(slave_fd)
    slave_fd = -1
    rt = threading.Thread(target=reader, daemon=True)
    rt.start()
    reader_started.set()
    note("client-spawned pid=%d" % client.pid)

    # wait READY (scanning continuously; answer startup DA meanwhile)
    end = time.monotonic() + 25
    while time.monotonic() < end and b"READY" not in state["buf"]:
        answer_startup_da(len(state["buf"]))
        if client.poll() is not None:
            raise RuntimeError("client exited before READY; see outer.bin")
        time.sleep(0.05)
    if b"READY" not in state["buf"]:
        result["status"] = "fail-no-ready"; raise SystemExit
    note("ready")

    g0 = subprocess.run([BIN, "--session", sess, "action", "list-panes", "--all", "--json"],
                        env=env, cwd=RD, capture_output=True, text=True, timeout=20)
    open(os.path.join(RD, mode + ".geom-pre.json"), "w").write(g0.stdout + "\nSTDERR:\n" + g0.stderr)

    # dismiss floating chrome
    for attempt in range(6):
        gchk = subprocess.run([BIN, "--session", sess, "action", "list-panes", "--all", "--json"],
                              env=env, cwd=RD, capture_output=True, text=True, timeout=20)
        try:
            panes = json.loads(gchk.stdout)
        except Exception:
            break
        blockers = [p for p in panes if p.get("is_plugin") and p.get("is_floating") and not p.get("is_suppressed")]
        if not blockers:
            note("no-floating-blockers attempt=%d" % attempt); break
        note("dismissing: %s" % ",".join(str(p.get("title")) for p in blockers))
        os.write(master_fd, b"\x1b"); time.sleep(0.5)

    kick_off = len(state["buf"])
    note("kick at offset %d" % kick_off)
    os.write(master_fd, b"g")

    if mode in ("query", "ordered"):
        # forwarded query must appear strictly AFTER kick offset
        end = time.monotonic() + 12
        fwd_off = -1
        while time.monotonic() < end:
            i = state["buf"].find(b"\x1b]11;?\x1b\\", kick_off)
            if i != -1:
                fwd_off = i; note("forwarded-query-after-kick @%d" % i); break
            time.sleep(0.05)
        if fwd_off == -1:
            result["status"] = "fail-no-forward-after-kick"; raise SystemExit
        result["payload_visible_before_shrink"] = state["buf"].find(b"ABCDEFGHIJKLMNOPQRST", kick_off) != -1
    else:
        end = time.monotonic() + 12
        po = -1
        while time.monotonic() < end:
            po = state["buf"].find(b"ABCDEFGHIJKLMNOPQRST", kick_off)
            if po != -1:
                note("payload-visible-control @%d" % po); break
            time.sleep(0.05)
        if po == -1:
            result["status"] = "fail-no-payload-control"; raise SystemExit
        result["payload_visible_before_shrink"] = True

    shrink_off = len(state["buf"])
    note("shrink at offset %d" % shrink_off)
    set_winsize(master_fd, 35, 100)
    if mode == "ordered":
        # Keep the first reply blocked across resize. Then exercise successive
        # forwarding through the actual client matcher, not a handler mock.
        time.sleep(0.10)
        os.write(master_fd, b"\x1b]11;rgb:1111/2222/3333\x1b\\")
        note("answered-background-after-shrink")
        end = time.monotonic() + 2
        while time.monotonic() < end:
            if state["buf"].find(b"\x1b]10;?\x1b\\", kick_off) != -1:
                os.write(master_fd, b"\x1b]10;rgb:aaaa/bbbb/cccc\x1b\\")
                note("answered-foreground-after-background")
                break
            time.sleep(0.005)
        else:
            raise RuntimeError("second forward missing")
        end = time.monotonic() + 2
        while time.monotonic() < end:
            if state["buf"].find(b"\x1b[16t", kick_off) != -1:
                os.write(master_fd, b"\x1b[6;16;8t")
                note("answered-cell-size-after-foreground")
                break
            time.sleep(0.005)
        else:
            raise RuntimeError("third forward missing")
    time.sleep(1.8)  # >1000ms server fallback; reader thread keeps draining

    g1 = subprocess.run([BIN, "--session", sess, "action", "list-panes", "--all", "--json"],
                        env=env, cwd=RD, capture_output=True, text=True, timeout=20)
    open(os.path.join(RD, mode + ".geom-post.json"), "w").write(g1.stdout + "\nSTDERR:\n" + g1.stderr)
    m = re.search(r'terminal_(\d+)', g1.stdout) or re.search(r'"id":\s*(\d+)[^}]*"is_plugin": false', g1.stdout)
    tid = m.group(1) if m else "0"
    d = subprocess.run([BIN, "--session", sess, "action", "dump-screen", "--pane-id", "terminal_" + tid, "--full"],
                       env=env, cwd=RD, capture_output=True, text=True, timeout=30)
    open(os.path.join(RD, mode + ".dump.txt"), "w").write(d.stdout + "\nSTDERR:\n" + d.stderr)
    note("dumped rc=%d tid=%s bytes=%d" % (d.returncode, tid, len(d.stdout)))
    result["terminal_id"] = tid
    result["payload_in_outer_final"] = b"ABCDEFGHIJKLMNOPQRST" in state["buf"]
    result["forward_offset_after_kick"] = fwd_off if mode != "control" else None
    result["kick_offset"] = kick_off
    result["shrink_offset"] = shrink_off
    result["status"] = "ok"
except Exception as e:
    import traceback
    result["status"] = "fail-exc"; result["exc"] = traceback.format_exc()
finally:
    open(os.path.join(RD, mode + ".outer.bin"), "wb").write(bytes(state["buf"]))
    open(os.path.join(RD, mode + ".events.json"), "w").write(json.dumps(
        {"events": events, "chunks": state["chunks"], "result": result}, indent=1))
    try:
        subprocess.run([BIN, "kill-session", sess], env=env, cwd=RD,
                       capture_output=True, timeout=15)
    except Exception:
        pass
    if client is not None:
        try:
            client.wait(timeout=5)
        except subprocess.TimeoutExpired:
            client.terminate()
            try:
                client.wait(timeout=5)
            except subprocess.TimeoutExpired:
                client.kill()
                client.wait(timeout=5)
    state["stop"] = True
    if reader_started.is_set():
        rt.join(timeout=1)
    if master_fd is not None:
        os.close(master_fd)
    note("cleanup-done")
    open(os.path.join(RD, mode + ".events.json"), "w").write(json.dumps(
        {"events": events, "chunks": state["chunks"], "result": result}, indent=1))
print(json.dumps({"result": result, "events": events}, indent=1))
