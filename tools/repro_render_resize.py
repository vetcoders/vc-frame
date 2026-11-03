#!/usr/bin/env python3
"""Headless render-corruption repro for vc-frame (pty + pyte terminal emulator).

Drives a worktree binary through the scenarios reported on 2026-08-27
(ghost frames / overlapping pane borders after resize, ctrl-drag, a second
smaller client, tab switch) and snapshots the *client terminal buffer* after
each step — the buffer, not a screenshot, is the evidence.

Usage:
    uv venv /tmp/vcf-venv && uv pip install --python /tmp/vcf-venv/bin/python pyte
    VCF_SOCK=/tmp/vcf-repro-sock /tmp/vcf-venv/bin/python tools/repro_render_resize.py \
        target/dev-opt/vc-frame zellij-utils/assets/layouts/vc-dashboard.kdl /tmp/vcf-repro/out

Always run with VCF_SOCK pointing at a scratch socket dir so live operator
sessions are never touched. Output: <out>/<client>-<step>.txt buffers,
<out>/report.json with frame-glyph / title metrics per step.
"""
import os, sys, time, fcntl, termios, struct, signal, subprocess, threading, re, json
import pyte

class RobustScreen(pyte.Screen):
    def report_device_status(self, *a, **k): pass
    def report_device_attributes(self, *a, **k): pass
    def debug(self, *a, **k): pass

VCF = sys.argv[1]
LAYOUT = sys.argv[2]
OUT = sys.argv[3]
SOCK = os.environ.get("VCF_SOCK", "/tmp/vcf-repro-sock")
os.makedirs(SOCK, exist_ok=True)
SESSION = os.environ.get("VCF_SESSION", "vcf-repro-A")

class Client:
    def __init__(self, name, cols, rows, args):
        self.name, self.cols, self.rows = name, cols, rows
        self.screen = RobustScreen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.master, slave = os.openpty()
        self.set_size(cols, rows, signal_child=False)
        env = dict(os.environ, TERM="xterm-256color", ZELLIJ_SOCKET_DIR=SOCK, VC_FRAME_SOCKET_DIR=SOCK)
        env.pop("ZELLIJ", None); env.pop("ZELLIJ_SESSION_NAME", None)
        self.proc = subprocess.Popen([VCF] + args, stdin=slave, stdout=slave, stderr=slave, env=env, preexec_fn=lambda: (os.setsid(), fcntl.ioctl(0, termios.TIOCSCTTY, 0)))
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
    def send(self, data: bytes):
        os.write(self.master, data)
    def snapshot(self, label):
        with self.lock: lines = list(self.screen.display)
        text = "\n".join(l.rstrip() for l in lines)
        with open(f"{OUT}/{self.name}-{label}.txt", "w") as f: f.write(text)
        return text
    def stop(self):
        try: os.killpg(os.getpgid(self.proc.pid), signal.SIGTERM)
        except Exception: pass

def metrics(text):
    rows = text.split("\n")
    corners = sum(r.count("┐") + r.count("╮") for r in rows)
    titles = len(re.findall(r"agents|transcript|convergence|sessions", text))
    return {"corner_glyphs": corners, "pane_title_words": titles, "PIN": text.count("PIN")}

def mouse(client, kind, col, row, mods=0):
    # SGR 1006: press=M, release=m; button 0 left, +32 motion; ctrl=16
    b = {"press": 0, "motion": 32, "release": 0}[kind] | mods
    suffix = "m" if kind == "release" else "M"
    client.send(f"\x1b[<{b};{col};{row}{suffix}".encode())

def ctrl_drag(client, c0, r0, c1, r1, steps=6):
    mouse(client, "press", c0, r0, 16); time.sleep(0.15)
    for i in range(1, steps+1):
        c = c0 + (c1-c0)*i//steps; r = r0 + (r1-r0)*i//steps
        mouse(client, "motion", c, r, 16); time.sleep(0.12)
    mouse(client, "release", c1, r1, 16); time.sleep(0.5)

report = {}
A = Client("A", 140, 40, ["--session", SESSION, "--new-session-with-layout", LAYOUT])
time.sleep(6)
report["A0_initial"] = metrics(A.snapshot("0-initial"))
# 1. shrink terminal
A.set_size(100, 30); time.sleep(3)
report["A1_shrunk"] = metrics(A.snapshot("1-shrunk"))
# 2. grow back
A.set_size(140, 40); time.sleep(3)
report["A2_grown"] = metrics(A.snapshot("2-grown"))
# 3. ctrl-drag the vertical border between agents/transcript (roughly mid column of content area)
ctrl_drag(A, 82, 20, 60, 20)
time.sleep(1.5)
report["A3_after_ctrl_drag"] = metrics(A.snapshot("3-ctrl-drag"))
# 4. second, smaller client attaches to the same session/tab
B = Client("B", 90, 25, ["--session", SESSION, "attach", SESSION] if False else ["attach", SESSION])
time.sleep(4)
report["A4_with_small_peer"] = metrics(A.snapshot("4-peer-attached"))
report["B4"] = metrics(B.snapshot("4-attached"))
# 5. peer switches to next tab (Ctrl+t then n? use default keybinding: Ctrl+t -> tab mode, 'n' next) — instead use CLI action
subprocess.run([VCF, "--session", SESSION, "action", "go-to-next-tab"], env=dict(os.environ, ZELLIJ_SOCKET_DIR=SOCK, VC_FRAME_SOCKET_DIR=SOCK), capture_output=True, timeout=10)
time.sleep(3)
report["A5_after_peer_tab_switch"] = metrics(A.snapshot("5-after-cli-tab-switch"))
report["B5"] = metrics(B.snapshot("5-after-cli-tab-switch"))
# 6. peer detaches
B.stop(); time.sleep(3)
report["A6_peer_detached"] = metrics(A.snapshot("6-peer-detached"))
A.snapshot("final")
with open(f"{OUT}/report.json", "w") as f: json.dump(report, f, indent=1)
print(json.dumps(report, indent=1))
subprocess.run([VCF, "--session", SESSION, "kill-session"], env=dict(os.environ, ZELLIJ_SOCKET_DIR=SOCK, VC_FRAME_SOCKET_DIR=SOCK), capture_output=True, timeout=10)
A.stop()
