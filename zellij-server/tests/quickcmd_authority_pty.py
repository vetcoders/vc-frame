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

import quickcmd_discovery_projection as discovery

QUICK_CMD_TITLE = "❯_ Quick cmd"
COMPOSER_TITLE = "✍ Composer · ⧉ Paste stack"
ABOUT_PLUGIN_URL = "vc-frame:about"
ABOUT_FIXTURE_TITLE = "About fixture"
SEED_TITLES = ("seed-alpha", "seed-beta")
# Printed by the private seed pane. Absence from the client grid is visual
# evidence. It is not a second inventory parser.
SEED_MARKER = "SEED_FLOAT_READY"


def pane_key(pane):
    return (bool(pane["is_plugin"]), pane["id"])


def hidden_float_keys(panes):
    return {
        pane_key(pane) for pane in panes
        if pane.get("is_suppressed") and pane.get("is_floating") and not pane.get("exited")
    }


def assert_float_visibility(panes, visible_ids, focused_id, concealed_ids):
    """Reject an inventory where a concealed float is visible.

    ``visible_ids`` and ``concealed_ids`` are ``(is_plugin, id)`` pairs.
    ``focused_id`` is the requested target. A row that merely exists is not
    enough: each concealed id must still be the same floating suppressed pane,
    and the requested id must be visible and focused. Already-visible requested
    panes may stay in ``visible_ids``; this does not require every other float
    to hide.
    """
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in concealed_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating") or not pane.get("is_suppressed"):
            raise AssertionError(("concealed float became visible or changed", key, pane))
        if pane.get("is_focused"):
            raise AssertionError(("concealed float is focused", key, pane))
    for key in visible_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating") or pane.get("is_suppressed"):
            raise AssertionError(("requested float is not visible", key, pane))
    if focused_id is None:
        if visible_ids:
            raise AssertionError(("visible floats have no requested focus", visible_ids))
        return
    focused = by_key.get(focused_id)
    if focused_id not in visible_ids or focused is None or not focused.get("is_focused"):
        raise AssertionError(("requested target is not focused", focused_id, focused))


def assert_layer_hidden(panes, tab, live_ids, grid_text, marker=SEED_MARKER):
    """Whole-layer hide from ``hide-floating-panes``.

    That action clears the conceal set and sets ``show_panes`` false.
    ``is_suppressed`` is only ``pane_is_concealed``, so it stays false.
    The public layer flag is ``TabInfo.are_floating_panes_visible``.
    The named panes must stay live floating panes on this tab, and ``marker``
    must be absent from the rendered grid. Individual conceal is a different
    state and fails this check.
    """
    if tab.get("are_floating_panes_visible") is not False:
        raise AssertionError((
            "floating layer still visible", tab.get("tab_id"), tab.get("are_floating_panes_visible")))
    if marker and marker in grid_text:
        raise AssertionError(("seed marker rendered while layer hidden", marker))
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in live_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or not pane.get("is_floating"):
            raise AssertionError(("hidden-layer pane is not the same live float", key, pane))
        if pane.get("tab_id") != tab.get("tab_id"):
            raise AssertionError(("hidden-layer pane left its tab", key, pane))
        if pane.get("is_suppressed"):
            raise AssertionError(("whole-layer hide reported individual conceal", key, pane))
        if pane.get("is_focused"):
            raise AssertionError(("hidden-layer pane is focused", key, pane))


def assert_not_revealed(panes, tab, live_ids, grid_text, marker=SEED_MARKER):
    """Reject a pane that leaves either hidden representation.

    Whole-layer hide is ``are_floating_panes_visible`` false with
    ``is_suppressed`` false. Individual conceal is ``is_suppressed`` true.
    The layer flag must be a bool. Each named pane must stay the same live
    float on this tab. A float that is not suppressed on a visible layer
    was revealed. A pane that is no longer floating is not that hidden float.
    """
    if marker and marker in grid_text:
        raise AssertionError(("hidden pane marker rendered", marker))
    layer_visible = tab.get("are_floating_panes_visible")
    if not isinstance(layer_visible, bool):
        raise AssertionError((
            "floating layer visibility is not explicit",
            tab.get("tab_id"), layer_visible))
    keys = [pane_key(pane) for pane in panes]
    if len(keys) != len(set(keys)):
        dupes = sorted(key for key in set(keys) if keys.count(key) > 1)
        raise AssertionError(("duplicate pane ids", dupes))
    by_key = {pane_key(pane): pane for pane in panes}
    for key in live_ids:
        pane = by_key.get(key)
        if pane is None or pane.get("exited") or pane.get("is_floating") is not True:
            raise AssertionError(("hidden pane is not the same live float", key, pane))
        if pane.get("tab_id") != tab.get("tab_id"):
            raise AssertionError(("hidden pane left its tab", key, pane))
        if pane.get("is_focused"):
            raise AssertionError(("hidden pane is focused", key, pane))
        if not pane.get("is_suppressed") and layer_visible:
            raise AssertionError(("hidden pane was revealed", key, pane))


def run_visibility_controls():
    def row(pane_id, suppressed, focused, plugin=False):
        return {
            "is_plugin": plugin, "id": pane_id, "is_floating": True,
            "is_suppressed": suppressed, "is_focused": focused, "exited": False,
        }

    negative = [row(1, False, True), row(2, False, False)]
    try:
        assert_float_visibility(negative, [(False, 1)], (False, 1), [(False, 2)])
    except AssertionError as error:
        if "concealed float" not in str(error):
            raise
    else:
        raise AssertionError("negative inventory accepted an unrelated visible float")

    positive = [row(1, False, True), row(2, True, False), row(3, True, False)]
    assert_float_visibility(positive, [(False, 1)], (False, 1), [(False, 2), (False, 3)])

    stacked = [row(1, False, False), row(4, False, True), row(2, True, False), row(3, True, False)]
    assert_float_visibility(
        stacked, [(False, 4), (False, 1)], (False, 4), [(False, 2), (False, 3)])

    unfocused = [row(1, False, False), row(2, True, False)]
    try:
        assert_float_visibility(unfocused, [(False, 1)], (False, 1), [(False, 2)])
    except AssertionError as error:
        if "not focused" not in str(error):
            raise
    else:
        raise AssertionError("visible target without focus was accepted")

    duplicated = [row(1, False, True), row(1, True, False)]
    try:
        assert_float_visibility(duplicated, [(False, 1)], (False, 1), [])
    except AssertionError as error:
        if "duplicate" not in str(error):
            raise
    else:
        raise AssertionError("duplicate pane id was accepted")

    def layer_row(pane_id, suppressed, focused):
        return {
            "is_plugin": False, "id": pane_id, "is_floating": True, "tab_id": 7,
            "is_suppressed": suppressed, "is_focused": focused, "exited": False,
        }

    def layer_tab(visible):
        return {"tab_id": 7, "are_floating_panes_visible": visible}

    hidden_layer = [layer_row(1, False, False), layer_row(2, False, False)]
    assert_layer_hidden(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "Lena fork\n")

    try:
        assert_layer_hidden(
            hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "SEED_FLOAT_READY\n")
    except AssertionError as error:
        if "seed marker" not in str(error):
            raise
    else:
        raise AssertionError("rendered seed marker accepted as layer hide")

    try:
        assert_layer_hidden(hidden_layer, layer_tab(True), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "floating layer" not in str(error):
            raise
    else:
        raise AssertionError("visible layer accepted as whole-layer hide")

    individually = [layer_row(1, True, False), layer_row(2, True, False)]
    try:
        assert_layer_hidden(individually, layer_tab(False), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "individual conceal" not in str(error):
            raise
    else:
        raise AssertionError("individual conceal accepted as whole-layer hide")

    # Same-tab activation opens the layer and conceals the other floats.
    # That outcome stays is_suppressed, not the layer flag.
    opened = [layer_row(4, False, True), layer_row(1, True, False), layer_row(2, True, False)]
    opened[0]["is_focused"] = True
    assert_float_visibility(opened, [(False, 4)], (False, 4), [(False, 1), (False, 2)])
    try:
        assert_layer_hidden(opened, layer_tab(True), [(False, 1), (False, 2)], "")
    except AssertionError as error:
        if "floating layer" not in str(error):
            raise
    else:
        raise AssertionError("activated layer accepted as whole-layer hide")

    assert_not_revealed(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], "Lena fork\n")
    assert_not_revealed(individually, layer_tab(True), [(False, 1), (False, 2)], "")
    revealed = [layer_row(1, False, False), layer_row(2, True, False)]
    try:
        assert_not_revealed(revealed, layer_tab(True), [(False, 1)], "")
    except AssertionError as error:
        if "was revealed" not in str(error):
            raise
    else:
        raise AssertionError("unrelated visible float accepted as still hidden")
    try:
        assert_not_revealed(hidden_layer, layer_tab(False), [(False, 1), (False, 2)], SEED_MARKER)
    except AssertionError as error:
        if "marker rendered" not in str(error):
            raise
    else:
        raise AssertionError("rendered seed marker accepted after activation")

    # Parent counterexample 1: a missing layer flag is not a hidden layer.
    # The panes below are live, unsuppressed floats.
    try:
        assert_not_revealed(
            hidden_layer, {"tab_id": 7}, [(False, 1), (False, 2)], "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("missing layer visibility accepted an unsuppressed pane")

    try:
        assert_not_revealed(
            hidden_layer,
            {"tab_id": 7, "are_floating_panes_visible": None},
            [(False, 1)],
            "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("null layer visibility accepted an unsuppressed pane")

    try:
        assert_not_revealed(
            hidden_layer,
            {"tab_id": 7, "are_floating_panes_visible": "false"},
            [(False, 1)],
            "Lena fork\n")
    except AssertionError as error:
        if "not explicit" not in str(error):
            raise
    else:
        raise AssertionError("non-bool layer visibility accepted an unsuppressed pane")

    # Parent counterexample 2: a pane that left the floating layer is not
    # the same hidden float, even when the layer flag is an explicit false.
    tiled = [dict(layer_row(1, False, False), is_floating=False)]
    try:
        assert_not_revealed(tiled, layer_tab(False), [(False, 1)], "Lena fork\n")
    except AssertionError as error:
        if "same live float" not in str(error):
            raise
    else:
        raise AssertionError("tiled pane accepted as the same hidden float")

    ambiguous = [layer_row(1, False, False), layer_row(1, True, False)]
    try:
        assert_not_revealed(ambiguous, layer_tab(False), [(False, 1)], "")
    except AssertionError as error:
        if "duplicate" not in str(error):
            raise
    else:
        raise AssertionError("ambiguous pane id accepted as hidden")

    print("quickcmd_authority_pty: visibility controls passed")


def main():
    import pyte
    import pyte.screens

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--modes", nargs="+", choices=("tab", "normal", "locked", "normal-after-B-A", "click-tab"),
                        default=("tab", "normal", "locked", "normal-after-B-A", "click-tab"))
    parser.add_argument("--allow-dirty", action="store_true", help="diagnostic build only")
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--resize-burst", type=int, default=0,
                        help="bounded real SIGWINCH event burst before each action")
    parser.add_argument("--max-ready-seconds", type=float)
    parser.add_argument("--context-layers", action="store_true",
                        help="also exercise Quick cmd -> Panels -> details -> same Quick cmd")
    parser.add_argument("--panels-contract", action="store_true",
                        help="exercise narrow/repeated-open/native-close inventory contract")
    parser.add_argument("--frontend-fixture", type=Path,
                        help="private three product-owned startup commands, never user argv")
    args = parser.parse_args()
    if args.panels_contract and (not args.context_layers or not args.frontend_fixture):
        parser.error("--panels-contract requires --context-layers and --frontend-fixture")
    columns = 167 if args.panels_contract else 120
    terminal_rows = 45 if args.panels_contract else 30
    assert 1 <= args.repeat <= 10 and 0 <= args.resize_burst <= 100
    args.modes = list(args.modes) * args.repeat
    args.output.mkdir(parents=True, exist_ok=False)
    args.scratch.mkdir(parents=True, exist_ok=False)
    binary = str(args.binary.resolve())
    session = f"qca{os.getpid()}"
    env = {k: os.environ[k] for k in ("PATH", "USER", "LANG") if k in os.environ}
    env.update(TERM="xterm-256color", SHELL="/bin/sh", VC_FRAME_SERVER_FOREGROUND="1",
               VC_FRAME_ROUTE_DIAGNOSTICS="1")
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
    if args.panels_contract:
        frontends = args.scratch / "frontends"
        frontends.mkdir()
        env["VC_FRAME_CONFIG_DIR"] = str(frontends)
        env["FIXTURE_PYTHON"] = sys.executable
        frontend = (f"#!{sys.executable}\nimport signal\n"
                    "print('OWNED FRONTEND FIXTURE', flush=True)\nsignal.pause()\n")
        for name in ("vc-start-here.py", "vc-agent-workshop.py"):
            (frontends / name).write_text(frontend)
        runner = frontends / "pane-python"
        runner.write_text('#!/bin/sh\nexec "$FIXTURE_PYTHON" "$@"\n')
        runner.chmod(0o700)
        private_bin = args.scratch / "bin"
        private_bin.mkdir()
        voc = private_bin / "vibecrafted"
        voc.write_text('#!/bin/sh\n[ "$1" = tui ] || exit 97\nexec "$FIXTURE_PYTHON" "' + str(frontends / "vc-agent-workshop.py") + '"\n')
        voc.chmod(0o700)
        env["PATH"] = str(private_bin) + os.pathsep + env.get("PATH", "")
        fixture = json.loads(args.frontend_fixture.read_text())
        assert len(fixture["rows"]) == 3
        panes = []
        for row in fixture["rows"]:
            command = row["terminal_command"]
            if command.startswith("bash -lc "):
                executable, option, body = "bash", "-lc", command[9:]
            elif command.startswith("sh -c "):
                executable, option, body = "sh", "-c", command[6:]
            else:
                raise AssertionError("unrecognized owned fixture command vector")
            panes.append('pane command=' + json.dumps(executable) + ' name=' + json.dumps(row["title"], ensure_ascii=False) + ' { args ' + json.dumps(option) + ' ' + json.dumps(body, ensure_ascii=False) + '; }')
        for title in ("Panels", "Composer"):
            panes.append('pane command=' + json.dumps(str(workload)) + ' name=' + json.dumps(title) + ';')
        # Private argv fixture. The holder is the pane shell. Its child is the
        # discovered command: basename junie, then the same child execs basename
        # codex with --model m1. The pane title stays "Junie". These scripts do
        # not start a provider; FIXTURE_NOTICE says so.
        fixture_cwd = args.scratch / "fx"
        fixture_live = fixture_cwd / "seen" / "live"
        fixture_live.mkdir(parents=True)
        swap_trigger = args.scratch / "argv-swap"
        junie_script = private_bin / "junie"
        codex_script = private_bin / "codex"
        holder_script = private_bin / "argv-fixture-holder"
        env["VC_FIXTURE_NOTICE"] = discovery.FIXTURE_NOTICE
        env["VC_FIXTURE_ARGV_SWAP"] = str(swap_trigger)
        env["VC_FIXTURE_JUNIE"] = str(junie_script)
        env["VC_FIXTURE_CODEX"] = str(codex_script)
        env["VC_FIXTURE_REPO"] = str(fixture_live)
        junie_script.write_text(
            f"#!{sys.executable}\n"
            "import os, sys, time\n"
            "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
            "print('FIXTURE_ARGV junie', flush=True)\n"
            "trigger = os.environ['VC_FIXTURE_ARGV_SWAP']\n"
            "while not os.path.exists(trigger):\n"
            "    time.sleep(0.05)\n"
            "print('FIXTURE_ARGV swap', flush=True)\n"
            "codex = os.environ['VC_FIXTURE_CODEX']\n"
            "os.execv(sys.executable, [sys.executable, codex, '--model', 'm1'])\n"
        )
        codex_script.write_text(
            f"#!{sys.executable}\n"
            "import os, signal\n"
            "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
            "print('FIXTURE_ARGV codex', flush=True)\n"
            "signal.pause()\n"
        )
        holder_script.write_text(
            f"#!{sys.executable}\n"
            "import os, sys, time\n"
            "# Stay at the layout cwd, step to the parent of the repo directory,\n"
            "# then step into the repo directory. The second step is the cwd\n"
            "# the drawer can observe after the host records the earlier one.\n"
            "time.sleep(0.4)\n"
            "os.chdir(os.path.dirname(os.environ['VC_FIXTURE_REPO']))\n"
            "print('FIXTURE_ARGV cwd-step', os.getcwd(), flush=True)\n"
            "time.sleep(0.4)\n"
            "os.chdir(os.environ['VC_FIXTURE_REPO'])\n"
            "print(os.environ['VC_FIXTURE_NOTICE'], flush=True)\n"
            "print('FIXTURE_ARGV cwd', os.getcwd(), flush=True)\n"
            "pid = os.fork()\n"
            "if pid == 0:\n"
            "    os.setpgrp()\n"
            "    os.execv(sys.executable, [sys.executable, os.environ['VC_FIXTURE_JUNIE']])\n"
            "try:\n"
            "    os.tcsetpgrp(sys.stdout.fileno(), os.getpgid(pid))\n"
            "except OSError as error:\n"
            "    print('FIXTURE_ARGV tcsetpgrp-failed', error, flush=True)\n"
            "    raise SystemExit(1)\n"
            "print('FIXTURE_ARGV holder-waiting', flush=True)\n"
            "while True:\n"
            "    try:\n"
            "        os.waitpid(pid, 0)\n"
            "        break\n"
            "    except InterruptedError:\n"
            "        continue\n"
            "print('FIXTURE_ARGV holder-child-exited', flush=True)\n"
            "import signal\n"
            "signal.pause()\n"
        )
        for path in (junie_script, codex_script, holder_script):
            path.chmod(0o700)
        panes.append(
            'pane command=' + json.dumps(str(holder_script))
            + ' name="Junie" cwd=' + json.dumps(str(fixture_cwd)) + ';'
        )
        assert fixture_live.name == discovery.CWD_BASENAME
        junie_label = discovery.JUNIE_LABEL
        codex_label = discovery.CODEX_LABEL
        panes.append('pane name="Files" { plugin location="zellij:strider"; }')
        frontend_layout = args.scratch / "frontends.kdl"
        frontend_layout.write_text('layout {\n' + '\n'.join(panes) + '\n}\n')
    config = args.scratch / "config.kdl"
    config.write_text(
        f'default_shell "{workload}"\nshow_startup_tips false\n'
        'show_release_notes false\nsession_serialization false\ndefault_mode "normal"\n'
        # Keep mode isolation observable beyond the default 30-second idle
        # lock. This fixture tests action ownership, not the idle timer.
        'auto_lock_after_seconds 0\n'
        'plugins { compact-bar location="zellij:compact-bar"; }\n'
        'keybinds { shared { bind "Super Shift ." { '
        'MessagePlugin "compact-bar" { name "vc_quick_cmd"; }; }; }; }\n'
    )
    repo = Path(__file__).resolve().parents[2]
    layout = args.scratch / "layout.kdl"
    template = (repo / "zellij-utils/assets/layouts/default.kdl").read_text()
    if args.panels_contract:
        fixture_tabs = '\n tab name="A" { pane name="Lena fork"; }\n tab name="B" {\n' + '\n'.join(panes) + '\n}\n}\n'
    else:
        fixture_tabs = '\n tab name="A" { pane; }\n tab name="B" { pane; }\n}\n'
    layout.write_text(template[:template.rfind("}")] + fixture_tabs)
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
    assert args.allow_dirty or not receipt["build_info"]["git_dirty"], "requires a committed binary"
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
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", terminal_rows, columns, 0, 0))
        screen = Terminal(columns, terminal_rows)
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

    def mode_visible(child, mode):
        top, bottom = child["screen"].display[0], child["screen"].display[-1]
        if mode == "normal":
            return "▷ N" in top and all(word in bottom for word in ("LOCK", "PANE", "TAB", "RESIZE"))
        if mode == "tab":
            return "𝌁 T" in top and "Break out" in bottom
        if mode == "locked":
            return "⚿ L" in top and "LOCK" in bottom and "PANE" not in bottom
        raise ValueError(mode)

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

    def cli(*command, timeout=10, allow=(0,)):
        result = subprocess.run([binary, "--session", session, *command], env=env,
                                capture_output=True, text=True, timeout=timeout)
        assert result.returncode in allow, (command, result.returncode, result.stderr, result.stdout)
        return result.stdout

    def inventory(name):
        data = json.loads(cli("action", "list-panes", "--all", "--json"))
        (args.output / f"{name}-panes.json").write_text(json.dumps(data, indent=2))
        return data

    def chrome(panes):
        rows = [p for p in panes if p["is_plugin"] and p["title"] != "Panels" and
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
            if args.panels_contract:
                cli("action", "rename-pane", "Lena fork")
                cli("action", "new-tab", "--name", "B", "--layout", str(frontend_layout))
            else:
                cli("action", "new-tab", "--name", "B")
            cli("action", "go-to-tab", "1")
            wait(lambda: "◉ A" in first["screen"].display[0], "two-tab-fixture")
        active = launch(False)
        wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
             and mode_visible(active, "normal"), "second-client", 60)
        before_clients = cli("action", "list-clients")
        receipt["clients_before"] = before_clients
        receipt["client_pids"] = [child["pid"] for child in children]
        receipt["panes_before"] = inventory("before")
        if args.panels_contract:
            frontend_hashes = []
            for row in fixture["rows"]:
                actual = [pane for pane in receipt["panes_before"]
                          if not pane["is_plugin"] and pane["title"] == row["title"]]
                assert len(actual) == 1 and actual[0]["terminal_command"] == row["terminal_command"], row["title"]
                frontend_hashes.append({"title": row["title"], "id": actual[0]["id"],
                                        "sha256": hashlib.sha256(actual[0]["terminal_command"].encode()).hexdigest()})
            assert len([pane for pane in receipt["panes_before"] if pane["is_plugin"]
                        and pane["plugin_url"] == "vc-frame:strider"]) == 1
            receipt["product_frontend_command_hashes"] = frontend_hashes
        chrome_before = chrome(receipt["panes_before"])
        terminal_ids = {p["id"] for p in receipt["panes_before"] if not p["is_plugin"]}
        receipt["tabs_before"] = json.loads(cli("action", "list-tabs", "--all", "--json"))
        agent_pid = int(re.search(r"WORKLOAD_PID=(\d+)", "\n".join(active["screen"].display))[1])
        receipt["agent_pid"] = agent_pid
        if args.panels_contract:
            # Source-prepared private scenario. It runs only with the installed
            # binary the caller passes. Visibility is inventory, not a title search.
            origin = next(tab for tab in receipt["tabs_before"] if tab["name"] == "A")
            destination = next(tab for tab in receipt["tabs_before"] if tab["name"] == "B")
            workload_pane = next(pane for pane in receipt["panes_before"]
                                 if not pane["is_plugin"] and pane["title"] == "Lena fork")
            seed_script = args.scratch / "seed-float.py"
            seed_script.write_text(
                f"#!{sys.executable}\nimport signal\n"
                f"print({SEED_MARKER!r}, flush=True)\nsignal.pause()\n"
            )
            seed_script.chmod(0o700)

            def returned_id(text, prefix):
                found = re.findall(rf"{prefix}_(\d+)", text)
                assert len(found) == 1, text
                return int(found[0])

            def click_chip(label):
                wait(lambda: label in active["screen"].display[0], f"chip-{label}")
                column = active["screen"].display[0].index(label) + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())

            def hide_tab(tab_id):
                # 0 = state changed, 2 = already hidden. 1 (missing tab) fails.
                cli("action", "hide-floating-panes", "--tab-id", str(tab_id), allow=(0, 2))

            def focus_existing(token):
                # Exit 1 with this text means the pane is already the client focus.
                result = subprocess.run(
                    [binary, "--session", session, "action", "focus-pane-id", token],
                    env=env, capture_output=True, text=True, timeout=30)
                blob = result.stderr + result.stdout
                assert result.returncode == 0 or (
                    result.returncode == 1 and "already focused" in blob
                ), (token, result.returncode, blob)

            def pane_by_key(panes, key):
                found = [pane for pane in panes if pane_key(pane) == key]
                assert len(found) == 1, (key, found)
                return found[0]

            def rendered():
                return "\n".join(active["screen"].display + first["screen"].display)

            def tab_by_id(tab_id):
                tabs = json.loads(cli("action", "list-tabs", "--all", "--json"))
                return next(tab for tab in tabs if tab["tab_id"] == tab_id)

            seed_ids = []
            for title in SEED_TITLES:
                token = cli("action", "new-pane", "--floating", "--name", title,
                            "--tab-id", str(origin["tab_id"]), "--", str(seed_script), timeout=30)
                seed_id = returned_id(token, "terminal")
                cli("action", "rename-pane", "--pane-id", f"terminal_{seed_id}", title)
                seed_ids.append(seed_id)
            assert len(set(seed_ids)) == 2, seed_ids
            seeded = inventory("float-seeds-open")
            for title, seed_id in zip(SEED_TITLES, seed_ids):
                pane = pane_by_key(seeded, (False, seed_id))
                assert pane["title"] == title and pane["is_floating"] and not pane["is_suppressed"], pane
                assert pane["tab_id"] == origin["tab_id"], pane
            seed_keys = [(False, seed_id) for seed_id in seed_ids]
            # The seed buffer must contain the marker while the layer is open.
            # hide-floating-panes then clears individual conceal and hides the
            # layer, so is_suppressed stays false. The public evidence is the
            # tab flag plus the marker leaving the grid.
            wait(lambda: SEED_MARKER in rendered(), "seed-marker-visible")
            hide_tab(origin["tab_id"])

            def layer_hidden():
                if SEED_MARKER in rendered():
                    return False
                return tab_by_id(origin["tab_id"]).get("are_floating_panes_visible") is False

            wait(layer_hidden, "float-layer-hidden")
            concealed = inventory("float-seeds-hidden")
            assert_layer_hidden(
                concealed, tab_by_id(origin["tab_id"]), seed_keys, rendered(), SEED_MARKER)

            click_chip("Quick cmd")
            def quick_open():
                panes = inventory("float-quick-open")
                opened = [pane for pane in panes if not pane["is_plugin"]
                          and pane["title"] == QUICK_CMD_TITLE and not pane["is_suppressed"]]
                return len(opened) == 1 and opened[0]["is_focused"] and opened[0]["is_floating"]
            wait(quick_open, "quick-cmd-chip")
            after_quick = inventory("float-quick")
            quick_matches = [pane for pane in after_quick if not pane["is_plugin"]
                             and pane["title"] == QUICK_CMD_TITLE]
            assert len(quick_matches) == 1, quick_matches
            quick_id = quick_matches[0]["id"]
            assert quick_id not in seed_ids and quick_id not in terminal_ids
            # new_floating_pane calls activate_only. A hidden layer opens for
            # this pane and conceals every other float. Seeds must be that
            # individual conceal, not merely absent from a still-hidden layer.
            assert_float_visibility(
                after_quick, [(False, quick_id)], (False, quick_id), seed_keys)
            if SEED_MARKER in rendered():
                raise AssertionError(("seed marker rendered after quick cmd", rendered()))
            snapshot("float-quick")

            before_composer = hidden_float_keys(after_quick)
            click_chip("Composer")
            def composer_open():
                panes = inventory("float-composer-open")
                opened = [pane for pane in panes if not pane["is_plugin"]
                          and pane["title"] == COMPOSER_TITLE and not pane["is_suppressed"]]
                return len(opened) == 1 and opened[0]["is_focused"] and opened[0]["is_floating"]
            wait(composer_open, "composer-chip")
            after_composer = inventory("float-composer")
            composer_matches = [pane for pane in after_composer if not pane["is_plugin"]
                                and pane["title"] == COMPOSER_TITLE]
            assert len(composer_matches) == 1, composer_matches
            composer_id = composer_matches[0]["id"]
            assert composer_id not in seed_ids and composer_id != quick_id
            quick_after = pane_by_key(after_composer, (False, quick_id))
            # Composer enters through prepare_context_layer, which suppresses
            # competing context layers only. Quick cmd may stay visible (stack)
            # or become that competing suppressed layer. Seeds must stay
            # individually concealed. The layer flag is not a substitute.
            stay_hidden = set(seed_keys)
            stay_hidden |= before_composer - {(False, composer_id), (False, quick_id)}
            visible = [(False, composer_id)]
            if not quick_after["is_suppressed"]:
                visible.append((False, quick_id))
            else:
                stay_hidden.add((False, quick_id))
            assert_float_visibility(after_composer, visible, (False, composer_id), sorted(stay_hidden))
            if SEED_MARKER in rendered():
                raise AssertionError(("seed marker rendered after composer", rendered()))
            assert chrome(after_composer) == chrome_before
            assert len([row for row in cli("action", "list-clients").splitlines()[1:] if row.strip()]) == 2
            snapshot("float-composer")

            launched = cli("action", "launch-plugin", "--floating", "--tab-id", str(origin["tab_id"]),
                           ABOUT_PLUGIN_URL, timeout=30)
            plugin_id = returned_id(launched, "plugin")
            cli("action", "rename-pane", "--pane-id", f"plugin_{plugin_id}", ABOUT_FIXTURE_TITLE)
            renamed = pane_by_key(inventory("float-plugin-renamed"), (True, plugin_id))
            assert renamed["title"] == ABOUT_FIXTURE_TITLE, renamed
            assert renamed["plugin_url"] == ABOUT_PLUGIN_URL, renamed
            cli("action", "go-to-tab", str(destination["position"] + 1))
            moved = cli("action", "launch-or-focus-plugin", "--floating", "--move-to-focused-tab",
                        ABOUT_PLUGIN_URL, timeout=30)
            assert returned_id(moved, "plugin") == plugin_id, moved
            moved_panes = inventory("float-plugin-moved")
            moved_plugin = pane_by_key(moved_panes, (True, plugin_id))
            assert moved_plugin["tab_id"] == destination["tab_id"], moved_plugin
            assert moved_plugin["title"] == ABOUT_FIXTURE_TITLE, moved_plugin
            assert len([pane for pane in moved_panes if pane.get("plugin_url") == ABOUT_PLUGIN_URL]) == 1
            hide_tab(origin["tab_id"])
            hide_tab(destination["tab_id"])

            def both_layers_hidden():
                if SEED_MARKER in rendered():
                    return False
                origin_tab = tab_by_id(origin["tab_id"])
                destination_tab = tab_by_id(destination["tab_id"])
                return (
                    origin_tab.get("are_floating_panes_visible") is False
                    and destination_tab.get("are_floating_panes_visible") is False
                )

            wait(both_layers_hidden, "plugin-layers-hidden")
            hidden_plugin = inventory("float-plugin-hidden")
            assert_layer_hidden(
                hidden_plugin, tab_by_id(origin["tab_id"]),
                seed_keys + [(False, quick_id), (False, composer_id)], rendered(), SEED_MARKER)
            assert_layer_hidden(
                hidden_plugin, tab_by_id(destination["tab_id"]),
                [(True, plugin_id)], rendered(), SEED_MARKER)
            # FocusPaneWithId returns without revealing a pane that is already
            # the client focus. Point focus at the tiled workload first so the
            # following id selects the plugin and is the reveal. The plugin is
            # not individually suppressed: the whole destination layer is hidden.
            focus_existing(f"terminal_{workload_pane['id']}")
            # Selector is the plugin id. DrawerCommand::Focus sends the same
            # FocusPaneWithId(should_float_if_hidden=true) instruction; a title
            # match is not the producer.
            cli("action", "focus-pane-id", f"plugin_{plugin_id}", timeout=30)
            def plugin_focused():
                panes = inventory("float-plugin-focus")
                pane = pane_by_key(panes, (True, plugin_id))
                return pane["is_focused"] and not pane["is_suppressed"] and pane["title"] == ABOUT_FIXTURE_TITLE
            wait(plugin_focused, "plugin-focused-by-id")
            focused_panes = inventory("float-plugin-focused")
            chrome_urls = {"compact-bar", "session-manager", "status-bar"}
            destination_others = []
            for pane in focused_panes:
                if pane.get("tab_id") != destination["tab_id"] or pane.get("exited"):
                    continue
                if not pane.get("is_floating") or pane_key(pane) == (True, plugin_id):
                    continue
                url = (pane.get("plugin_url") or "").split(":")[-1]
                if pane.get("is_plugin") and url in chrome_urls and pane.get("title") != "Panels":
                    continue
                destination_others.append(pane_key(pane))
            # Same-tab floats opened with this focus must be individually
            # concealed. Seeds live on the other tab: hide cleared their
            # conceal bit, so they stay hidden only while that layer stays shut.
            assert_float_visibility(
                focused_panes, [(True, plugin_id)], (True, plugin_id), sorted(destination_others))
            assert_not_revealed(
                focused_panes, tab_by_id(origin["tab_id"]),
                seed_keys + [(False, quick_id), (False, composer_id)], rendered(), SEED_MARKER)
            focused_plugin = pane_by_key(focused_panes, (True, plugin_id))
            assert focused_plugin["tab_id"] == destination["tab_id"], focused_plugin
            assert chrome(focused_panes) == chrome_before
            tabs_now = json.loads(cli("action", "list-tabs", "--all", "--json"))

            def tab_identity(tabs):
                return sorted((tab["tab_id"], tab["tab_instance_id"], tab["session_incarnation"])
                              for tab in tabs)

            assert tab_identity(tabs_now) == tab_identity(receipt["tabs_before"])
            assert len([row for row in cli("action", "list-clients").splitlines()[1:] if row.strip()]) == 2
            snapshot("float-plugin")
            receipt["float_visibility"] = {
                "seed_ids": seed_ids,
                "quick_cmd_id": quick_id,
                "composer_id": composer_id,
                "plugin_id": plugin_id,
                "plugin_url": ABOUT_PLUGIN_URL,
                "plugin_title": ABOUT_FIXTURE_TITLE,
                "origin_tab_id": origin["tab_id"],
                "destination_tab_id": destination["tab_id"],
                "quick_cmd_producer": "compact-bar chip click on visible Quick cmd",
                "composer_producer": "compact-bar chip click on visible Composer",
                "plugin_move_producer": "action launch-or-focus-plugin --floating --move-to-focused-tab",
                "plugin_focus_producer": "action focus-pane-id plugin_<id>",
                "plugin_focus_kernel": "FocusPaneWithId should_float_if_hidden=true; same instruction as Panels DrawerCommand::Focus via show_pane_with_id",
                "layer_hide_field": "list-tabs TabInfo.are_floating_panes_visible",
                "layer_hide_marker": SEED_MARKER,
                "individual_conceal_field": "list-panes is_suppressed",
                "layer_hide_projection": "hide-floating-panes clears conceal and sets show_panes false; is_suppressed stays false",
                "float_multiple_panes": "not invoked",
            }
            for kind, pane_id in (
                ("terminal", quick_id), ("terminal", composer_id),
                ("plugin", plugin_id), ("terminal", seed_ids[0]), ("terminal", seed_ids[1]),
            ):
                cli("action", "close-pane", "--pane-id", f"{kind}_{pane_id}")
            def scenario_closed():
                panes = inventory("float-scenario-closed")
                closed_keys = {(False, quick_id), (False, composer_id), (True, plugin_id), *seed_keys}
                return not any(pane_key(pane) in closed_keys for pane in panes)
            wait(scenario_closed, "float-scenario-closed")
            cli("action", "go-to-tab", str(origin["position"] + 1))
            focus_existing(f"terminal_{workload_pane['id']}")
            wait(lambda: mode_visible(active, "normal") and mode_visible(first, "normal"),
                 "float-scenario-restored-normal")
            restored = inventory("float-scenario-restored")
            assert {pane["id"] for pane in restored if not pane["is_plugin"]} == terminal_ids
            assert chrome(restored) == chrome_before
        # Deliberately keep the peer in TAB so an accidental all-client Normal
        # transition is observable. Its mode must remain unchanged throughout.
        os.write(first["fd"], b"\x14")
        wait(lambda: mode_visible(first, "tab"), "peer-tab-mode")
        snapshot("before")
        drain(2)  # ordinary warmed session; identical before/after conditions

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
                    wait(lambda: mode_visible(active, "normal")
                         and f"◉ {expected_tab}" in active["screen"].display[0], f"switch-{position}")
                    snapshot(f"switch-{position}")
            elif mode in ("tab", "click-tab"):
                os.write(active["fd"], b"\x14")
                wait(lambda: mode_visible(active, "tab"), "tab-mode")
            elif mode == "locked":
                os.write(active["fd"], b"\x07")
                wait(lambda: mode_visible(active, "locked"), "locked-mode")
            for burst_index in range(args.resize_burst):
                burst_columns = columns - 1 if burst_index % 2 == 0 else columns
                fcntl.ioctl(active["fd"], termios.TIOCSWINSZ,
                            struct.pack("HHHH", terminal_rows, burst_columns, 0, 0))
                os.kill(active["pid"], signal.SIGWINCH)
                drain(0.02)
            if args.resize_burst:
                fcntl.ioctl(active["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", terminal_rows, columns, 0, 0))
                os.kill(active["pid"], signal.SIGWINCH)
            if mode == "click-tab":
                wait(lambda: "Quick cmd" in active["screen"].display[0], "click-target-visible")
            snapshot(f"{index}-{mode}-before")
            opened_at = time.monotonic()
            if mode == "click-tab":
                column = active["screen"].display[0].index("Quick cmd") + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())
            else:
                os.write(active["fd"], bytes.fromhex(receipt["shortcut_hex"]))
            # Measure rendered input readiness before diagnostic CLI round trips.
            wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1])
                 and mode_visible(active, "normal")
                 and mode_visible(first, "tab"), f"{mode}-ready")
            ready_seconds = time.monotonic() - opened_at
            marker = args.output / f"{index}-command-executed"
            command = f"printf QC_EXECUTED_{index}; printf ok > {shlex.quote(str(marker))}\r"
            os.write(active["fd"], command.encode())
            wait(lambda: marker.exists(), f"{mode}-usable-command")
            assert marker.read_text() == "ok"
            executed_seconds = time.monotonic() - opened_at
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
            clients_open = cli("action", "list-clients")
            origin_id = before_clients.splitlines()[2].split()[0]
            origin_row = next(row.split() for row in clients_open.splitlines()[1:]
                              if row.split()[0] == origin_id)
            assert origin_row[1] == f"terminal_{new_panes[0]['id']}", "origin selection lost"
            receipt["steps"].append({"mode": mode, "panes_open": opened,
                                      "clients_open": clients_open, "origin_client_id": origin_id,
                                      "ready_seconds": ready_seconds,
                                      "executed_seconds": executed_seconds,
                                      "resize_burst": args.resize_burst})
            wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1]),
                 f"{mode}-rendered-command")
            snapshot(f"{index}-{mode}-rendered")
            # Pane title and mode chrome arrive on separate render updates.
            # Preserve the first frame, then require both client projections
            # to converge without injecting any input-mode action.
            wait(lambda: mode_visible(active, "normal")
                 and mode_visible(first, "tab"), f"{mode}-input-mode-projections")
            snapshot(f"{index}-{mode}-input-ready")
            receipt["steps"][-1]["peer_mode"] = "Tab"
            receipt["steps"][-1]["open_seconds"] = time.monotonic() - opened_at
            expected_pipes = sum(step["mode"] != "click-tab" for step in receipt["steps"])
            def completed_pipe():
                routes = json.loads(cli("action", "doctor-routes", "--json"))
                receipt["steps"][-1]["routes"] = routes
                metric = routes["metrics"].get("interactive:KeybindPipe", {})
                return metric.get("count", 0) == expected_pipes
            wait(completed_pipe, f"{mode}-pipe-completion")
            for name, metric in receipt["steps"][-1]["routes"]["metrics"].items():
                if name in ("interactive:KeybindPipe", "plugin:SwitchToMode",
                            "plugin:NewFloatingPane", "plugin:RenameTerminalPane"):
                    assert metric["timeouts"] == 0 and metric["failures"] == 0, (name, metric)
            snapshot(f"{index}-{mode}-executed")
            if args.context_layers and index == len(args.modes) - 1:
                quick_id = new_panes[0]["id"]
                wait(lambda: "Panels" in active["screen"].display[0], "panels-chip")
                column = active["screen"].display[0].index("Panels") + 2
                os.write(active["fd"], f"\x1b[<0;{column};1M\x1b[<0;{column};1m".encode())
                wait(lambda: "d details" in "\n".join(active["screen"].display[1:-1]),
                     "panels-open")
                def quick_hidden():
                    panes = inventory("context-panels")
                    return (any(p["id"] == quick_id and not p["is_plugin"]
                                and p["is_suppressed"] and not p["exited"] for p in panes)
                            and any(p["is_plugin"] and p["title"] == "Panels"
                                    and p["is_floating"] and not p["is_suppressed"] for p in panes))
                wait(quick_hidden, "quick-suppressed-with-process-preserved")
                snapshot("context-panels")
                if args.panels_contract:
                    def drawer_pane():
                        return next(p for p in inventory("panels-contract")
                                    if p["is_plugin"] and p["title"] == "Panels")

                    def drawer_open():
                        pane = drawer_pane()
                        return pane["is_floating"] and not pane["is_suppressed"]

                    def drawer_slice():
                        pane = drawer_pane()
                        return "\n".join(
                            line[pane["pane_x"]:pane["pane_x"] + pane["pane_columns"]]
                            for line in active["screen"].display[pane["pane_y"]:pane["pane_y"] + pane["pane_rows"]]
                        )

                    def click(column, row):
                        os.write(active["fd"], f"\x1b[<0;{column + 1};{row + 1}M\x1b[<0;{column + 1};{row + 1}m".encode())

                    def click_panels():
                        top = active["screen"].display[0]
                        assert "Panels" in top
                        click(top.index("Panels") + 2, 0)

                    def check_geometry(expected_columns, expected_rows):
                        pane = drawer_pane()
                        assert pane["pane_x"] + pane["pane_columns"] == expected_columns, pane
                        assert pane["pane_columns"] == expected_columns * 24 // 100, pane
                        assert pane["pane_y"] <= 2 and pane["pane_y"] + pane["pane_rows"] >= expected_rows - 1, pane
                        assert pane["is_focused"] and drawer_open(), "drawer must be focused"
                        return pane

                    pane = check_geometry(columns, terminal_rows)
                    # Lena fork stays: its shell is workload.py, which is not an
                    # interactive agent, so the drawer falls back to the pane title.
                    # Codex fork is not a row. The fixture pane's title is Junie;
                    # the row must be the argv projection, not that title.
                    wait(lambda: junie_label in drawer_slice(), "panels-junie-fixture-label")
                    content = drawer_slice()
                    for title in ("Launchpad", "Lena fork", "Composer", "Quick cmd"):
                        assert title in content, (title, content)
                    assert junie_label in content, (junie_label, content)
                    assert "Codex fork" not in content, content
                    assert "Junie" not in content, content
                    for title in ("Agent Workspaces", "Voc · Host console", "Files", "Strider", "Links", "Tab titles", "Sessions"):
                        assert title not in content, (title, content)
                    assert "PIN" not in content
                    fixture_rows = [p for p in inventory("panels-fixture-junie")
                                    if not p["is_plugin"] and p["title"] == "Junie"]
                    assert len(fixture_rows) == 1, fixture_rows
                    fixture_id = fixture_rows[0]["id"]
                    assert not fixture_rows[0]["exited"]
                    junie_inventory = content
                    initial_drawer_id = pane["id"]
                    click_panels()
                    drain(1)
                    assert check_geometry(columns, terminal_rows)["id"] == initial_drawer_id
                    assert len([p for p in inventory("panels-repeated-open") if p["is_plugin"] and p["title"] == "Panels"]) == 1
                    # Reopen restores product geometry after a manual change.
                    cli("action", "change-floating-pane-coordinates", "--pane-id", f"plugin_{initial_drawer_id}", "--x", "40%", "--width", "58%")
                    click_panels()
                    wait(lambda: drawer_pane()["pane_columns"] == columns * 24 // 100, "panels-reopen-restores-narrow")
                    # One client resize exercises the viewport with its session rail.
                    geometry = []
                    for width, height in ((240, 55), (90, 36), (167, 45)):
                        active["screen"].resize(height, width)
                        fcntl.ioctl(active["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                        os.kill(active["pid"], signal.SIGWINCH)
                        # The peer constrains shared geometry; resize its fixture too.
                        first["screen"].resize(height, width)
                        fcntl.ioctl(first["fd"], termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
                        os.kill(first["pid"], signal.SIGWINCH)
                        wait(lambda width=width: drawer_pane()["pane_x"] + drawer_pane()["pane_columns"] == width
                             and drawer_pane()["pane_columns"] == width * 24 // 100,
                             f"panels-right-edge-{width}")
                        geometry.append(check_geometry(width, height))
                    pane = drawer_pane()
                    x_column = pane["pane_x"] + pane["pane_columns"] - 3
                    wait(lambda pane=pane, x_column=x_column: active["screen"].display[pane["pane_y"]][x_column] == "x", "painted-native-x")
                    before_close = inventory("panels-before-native-close")
                    click(x_column, pane["pane_y"])
                    wait(lambda: not drawer_open(), "native-x-hides-drawer")
                    after_close = inventory("panels-after-native-close")
                    assert {(p["is_plugin"], p["id"]) for p in before_close} == {(p["is_plugin"], p["id"]) for p in after_close}, "x closed another pane"
                    assert all(not p["exited"] for p in after_close if not p["is_plugin"])
                    click_panels()
                    wait(drawer_open, "panels-opens-after-native-close")
                    check_geometry(columns, terminal_rows)
                    wait(lambda: junie_label in drawer_slice(), "panels-junie-label-restored")
                    swap_trigger.write_text("fixture\n")

                    def argv_swapped():
                        text = drawer_slice()
                        return codex_label in text and junie_label not in text

                    wait(argv_swapped, "panels-codex-same-pane-argv")
                    content = drawer_slice()
                    assert "Junie" not in content, content
                    assert "Codex fork" not in content, content
                    swapped_rows = [p for p in inventory("panels-fixture-codex") if p["id"] == fixture_id]
                    assert len(swapped_rows) == 1, swapped_rows
                    assert swapped_rows[0]["title"] == "Junie" and not swapped_rows[0]["exited"], swapped_rows[0]
                    receipt["panels_discovery"] = {
                        "notice": discovery.FIXTURE_NOTICE,
                        "proves_provider_started": False,
                        "cwd": str(fixture_live),
                        "cwd_basename": discovery.CWD_BASENAME,
                        "stale_title": discovery.STALE_TITLE,
                        "state": discovery.STATE,
                        "pane_id": fixture_id,
                        "junie_label": junie_label,
                        "codex_label": codex_label,
                        "junie_inventory": junie_inventory,
                        "launch_command_before": fixture_rows[0].get("terminal_command"),
                        "launch_command_after": swapped_rows[0].get("terminal_command"),
                    }
                    receipt["panels_uncovered"] = [
                        "list-panes PaneInfo.terminal_command is the launch command and has no live argv field",
                        "FloatMultiplePanes is plugin-only; no public CLI calls it, so native execution is not a step",
                        "screen_tests cover hidden-float kernel behavior in source only",
                    ]
                    receipt["panels_contract"] = {"drawer_id": initial_drawer_id, "geometry": geometry,
                                                   "native_x_hid_only_drawer": True, "user_inventory": content}
                os.write(active["fd"], b"d")
                wait(lambda: "Details" in "\n".join(active["screen"].display[1:-1]),
                     "panel-details-expanded")
                snapshot("context-details")
                os.write(active["fd"], b"\x1b")
                wait(lambda: "d details" not in "\n".join(active["screen"].display[1:-1]),
                     "panels-dismissed")
                os.write(active["fd"], bytes.fromhex(receipt["shortcut_hex"]))
                wait(lambda: "❯_ Quick cmd" in "\n".join(active["screen"].display[1:-1]),
                     "same-quick-restored")
                def geometry_restored():
                    return any(not p["is_plugin"] and p["id"] == quick_id
                               and not p["is_suppressed"]
                               and (p["pane_y"], p["pane_rows"]) == (
                                   new_panes[0]["pane_y"], new_panes[0]["pane_rows"])
                               for p in inventory("context-restoring"))
                wait(geometry_restored, "quick-footprint-restored")
                restored = inventory("context-restored")
                quicks = [p for p in restored if not p["is_plugin"] and p["id"] not in terminal_ids]
                assert len(quicks) == 1 and quicks[0]["id"] == quick_id and not quicks[0]["is_suppressed"]
                assert (quicks[0]["pane_y"], quicks[0]["pane_rows"]) == (
                    new_panes[0]["pane_y"], new_panes[0]["pane_rows"]), "restored tool moved over content headers"
                assert all(p["is_suppressed"] for p in restored
                           if p["is_plugin"] and p["title"] == "Panels")
                resumed_marker = args.output / "context-shell-resumed"
                os.write(active["fd"], f"printf resumed > {shlex.quote(str(resumed_marker))}\r".encode())
                wait(resumed_marker.exists, "restored-shell-accepts-input")
                assert resumed_marker.read_text() == "resumed"
                snapshot("context-restored")
                receipt["context_layers"] = {"quick_id": quick_id, "preserved": True,
                                             "panes": restored}
            # Close this command shell's floating pane via the standard pane keys.
            os.write(active["fd"], b"\x10")
            wait(lambda: "Fullscreen" in active["screen"].display[-1], "pane-mode-for-dismissal")
            os.write(active["fd"], b"x")
            wait(lambda: "WORKLOAD_PID=" in "\n".join(active["screen"].display)
                 and mode_visible(active, "normal"), f"{mode}-dismissed")
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
        if args.max_ready_seconds is not None:
            assert all(step["ready_seconds"] <= args.max_ready_seconds for step in receipt["steps"]), (
                "readiness target exceeded", [step["ready_seconds"] for step in receipt["steps"]])
        receipt["status"] = "passed_bounded_pty_scenario"
    except Exception as error:
        receipt.update(status="failed", error=repr(error))
        snapshot("failure")
        raise
    finally:
        # Preserve route latency/failure and raw server receipts even on a
        # failed assertion. Never turn a timeout into a successful scenario.
        try:
            receipt["routes"] = json.loads(cli("action", "doctor-routes", "--json"))
        except Exception as error:
            receipt["routes_error"] = repr(error)
        logs = Path(f"/tmp/vc-frame-{os.getuid()}/vc-frame-log") / session
        for log in logs.glob("*"):
            if log.is_file():
                (args.output / ("server-" + log.name)).write_bytes(log.read_bytes())
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
            # Holding the PTY master keeps owned clients in kernel PTY wait
            # after kill-session returns 0. Close first, then reap.
            try:
                os.close(child["fd"])
            except OSError:
                pass
            child["fd"] = -1
            child["raw"].close()
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
                try:
                    os.kill(child["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
                # A macOS process in kernel exit can outlive SIGKILL. Never
                # lose the scenario receipt to an unbounded blocking waitpid.
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    try:
                        if os.waitpid(child["pid"], os.WNOHANG)[0]:
                            break
                    except ChildProcessError:
                        break
                    time.sleep(0.1)
                else:
                    receipt.setdefault("unreaped_owned_children", []).append(child["pid"])
        for log in logs.glob("*"):
            if log.is_file():
                (args.output / ("server-" + log.name)).write_bytes(log.read_bytes())
        receipt_path.write_text(json.dumps(receipt, indent=2))


if __name__ == "__main__":
    if sys.argv[1:2] == ["--visibility-controls"]:
        run_visibility_controls()
    else:
        main()
