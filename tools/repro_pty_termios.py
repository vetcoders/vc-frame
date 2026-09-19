#!/usr/bin/env python3
"""F03 repro: a vc-frame session spawned with raw stdin must still give panes
sane termios (OPOST|ONLCR), so plain `printf 'a\nb\n'` output is not stair-stepped.

Mechanism under test: ServerOsInputOutput::new() snapshots tcgetattr(0) once at
server start and clones it into every pane pty via openpty(). A client whose
stdin was already raw (vc-frame launched from inside another raw terminal /
agent pty / harness) therefore poisons every pane of the session for life.

This harness launches the client with stdin = a pty slave in RAW mode (default)
or sane mode (VCF_STDIN_MODE=sane control run), lets a single borderless pane
run `printf 'a\nb\n'` plus `stty -a` (redirected to $STTY_OUT), snapshots the
client terminal buffer through pyte, and asserts column(b) == column(a).

Usage:
    uv venv /tmp/vcf-venv && uv pip install --python /tmp/vcf-venv/bin/python pyte
    VCF_SOCK=/tmp/f03-sock VCF_SESSION=f03-A /tmp/vcf-venv/bin/python \
        tools/repro_pty_termios.py target/dev-opt/vc-frame /tmp/f03-out

Always run with VCF_SOCK pointing at a scratch socket dir so live operator
sessions are never touched.
"""
import os, sys, time, fcntl, termios, struct, signal, subprocess, threading, re, json, tty
import pyte

class RobustScreen(pyte.Screen):
    def report_device_status(self, *a, **k): pass
    def report_device_attributes(self, *a, **k): pass
    def debug(self, *a, **k): pass

VCF = sys.argv[1]
OUT = sys.argv[2]
os.makedirs(OUT, exist_ok=True)
SOCK = os.environ.get("VCF_SOCK", "/tmp/f03-sock")
os.makedirs(SOCK, exist_ok=True)
SESSION = os.environ.get("VCF_SESSION", f"f03-{int(time.time())}")
STDIN_MODE = os.environ.get("VCF_STDIN_MODE", "raw")  # raw = repro, sane = control
STTY_OUT = os.path.join(OUT, "pane-stty.txt")
LAYOUT = os.path.join(OUT, "f03-layout.kdl")

with open(LAYOUT, "w") as f:
    # Single borderless pane: no plugin bars, no frame title — the buffer shows
    # exactly what the pane's shell printed, so column positions are the truth.
    f.write(
        'layout {\n'
        '    pane borderless=true command="sh" {\n'
        '        args "-c" "stty -a > \\"$STTY_OUT\\" 2>&1; printf \'a\\\\nb\\\\n\'; sleep 600"\n'
        '    }\n'
        '}\n'
    )

class Client:
    def __init__(self, name, cols, rows, args):
        self.name, self.cols, self.rows = name, cols, rows
        self.screen = RobustScreen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.master, slave = os.openpty()
        self.set_size(cols, rows, signal_child=False)
        if STDIN_MODE == "raw":
            # The F03 poison: the client is born into a terminal that is
            # already raw (no OPOST/ONLCR), exactly like vc-frame launched from
            # inside another raw terminal. The client "restores" this same raw
            # mode before spawning the server, so the server snapshots raw.
            tty.setraw(slave)
        env = dict(os.environ, TERM="xterm-256color", ZELLIJ_SOCKET_DIR=SOCK,
                   VC_FRAME_SOCKET_DIR=SOCK, STTY_OUT=STTY_OUT)
        env.pop("ZELLIJ", None); env.pop("ZELLIJ_SESSION_NAME", None)
        env.pop("VC_FRAME", None); env.pop("VC_FRAME_SESSION_NAME", None)
        self.proc = subprocess.Popen([VCF] + args, stdin=slave, stdout=slave, stderr=slave,
                                     env=env, preexec_fn=lambda: (os.setsid(), fcntl.ioctl(0, termios.TIOCSCTTY, 0)))
        os.close(slave)
        self.log = open(f"{OUT}/{name}.raw", "wb")
        self.lock = threading.Lock(); self.errors = []
        self.t = threading.Thread(target=self.pump, daemon=True); self.t.start()
    def pump(self):
        while True:
            try: data = os.read(self.master, 65536)
            except OSError: return
            if not data: return
            self.log.write(data); self.log.flush()
            with self.lock:
                try: self.stream.feed(data)
                except Exception as e: self.errors.append(repr(e)[:120])
    def set_size(self, cols, rows, signal_child=True):
        self.cols, self.rows = cols, rows
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        if signal_child:
            with self.lock: self.screen.resize(rows, cols)
            os.killpg(os.getpgid(self.proc.pid), signal.SIGWINCH)
    def snapshot(self, label):
        with self.lock: lines = list(self.screen.display)
        text = "\n".join(l.rstrip() for l in lines)
        with open(f"{OUT}/{self.name}-{label}.txt", "w") as f: f.write(text)
        return text
    def stop(self):
        try: os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
        except Exception: pass

def find_marker_columns(text):
    """Locate the single-char rows 'a' and 'b' printed by the pane."""
    a_col = b_col = None
    a_row = b_row = None
    for i, line in enumerate(text.split("\n")):
        stripped = line.strip()
        if a_col is None and stripped == "a":
            a_col, a_row = line.index("a"), i
        elif a_col is not None and b_col is None and stripped == "b":
            b_col, b_row = line.index("b"), i
            break
    return (a_col, a_row), (b_col, b_row)

def run_client(label):
    A = Client("A", 100, 30, ["--session", SESSION, "--new-session-with-layout", LAYOUT])
    time.sleep(8)
    text = A.snapshot(label)
    return A, text

def cli(*args):
    return subprocess.run([VCF] + list(args),
                          env=dict(os.environ, ZELLIJ_SOCKET_DIR=SOCK, VC_FRAME_SOCKET_DIR=SOCK),
                          capture_output=True, timeout=10)

def clear_stale_session():
    """Drop a dead server's leftover lease/record for our scratch session.
    Dead sessions are remembered in the user-level session_info cache (not in
    VCF_SOCK), so a re-used name must be explicitly deleted; kill-session
    alone leaves the dead record behind."""
    cli("kill-session", SESSION)
    cli("delete-session", SESSION)
    for root, _dirs, files in os.walk(SOCK):
        if SESSION in files:
            try: os.remove(os.path.join(root, SESSION))
            except OSError: pass

# Pinned session names (VCF_SESSION) may collide with a dead record from a
# previous harness run — clear it before the first client starts.
clear_stale_session()

# A fresh session's very first client occasionally dies before rendering
# (server still coming up); the retry clears any stale lease and re-runs.
A, text = run_client("0-printf")
(a_col, a_row), (b_col, b_row) = find_marker_columns(text)
retried_after = None
if a_col is None or b_col is None:
    first_exit = A.proc.poll()
    A.stop()
    clear_stale_session()
    time.sleep(2)
    A, text = run_client("0-printf-retry")
    (a_col, a_row), (b_col, b_row) = find_marker_columns(text)
    retried_after = first_exit

stty_text = ""
if os.path.exists(STTY_OUT):
    with open(STTY_OUT) as f: stty_text = f.read().strip()

result = {
    "stdin_mode": STDIN_MODE,
    "session": SESSION,
    "a": {"row": a_row, "col": a_col},
    "b": {"row": b_row, "col": b_col},
    "onlcr": "onlcr" in stty_text and "-onlcr" not in stty_text,
    "opost": "opost" in stty_text and "-opost" not in stty_text,
    "retried_after_exit": retried_after,
}
if a_col is None or b_col is None:
    result["verdict"] = "INCONCLUSIVE: markers not found in client buffer"
elif b_col == a_col:
    result["verdict"] = "PASS: b renders in the same column as a (ONLCR active)"
else:
    result["verdict"] = f"FAIL: stair-step — b at column {b_col}, a at column {a_col} (LF without CR)"
result["errors"] = A.errors

with open(f"{OUT}/report.json", "w") as f: json.dump(result, f, indent=1)
print("=== stty -a from pane ===")
print(stty_text or "(no stty output captured)")
print("=== buffer around markers ===")
rows = text.split("\n")
if a_row is not None:
    for i in range(max(0, a_row - 1), min(len(rows), (b_row or a_row) + 2)):
        print(f"{i:3d}|{rows[i]}")
print("=== verdict ===")
print(json.dumps(result, indent=1))

cli("kill-session", SESSION)
cli("delete-session", SESSION)
A.stop()
sys.exit(0 if result["verdict"].startswith("PASS") else 1)
