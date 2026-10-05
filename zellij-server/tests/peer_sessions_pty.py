#!/usr/bin/env python3
"""Real peer-session routing acceptance. SOURCE ONLY until builds are authorized.

Requires an explicit committed Frame binary, canonical product config and layouts,
and the existing pyte dependency. Never invokes Cargo, installs or uses live config.
Operator tab bodies are private ACK fixtures: this does not certify their backends.
Physical CSI keys prove Frame routing; VC Terminal physical key delivery is separate.
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

KEY = {"up": b"\x1b[1;9A", "down": b"\x1b[1;9B",
       "right": b"\x1b[1;9C", "left": b"\x1b[1;9D"}
OPERATOR_TABS = ["Dashboard", "Active runs", "Config", "Doctor", "Projects"]


def tab_spans(source):
    """Direct layout tab nodes only; quotes/comments never alter brace depth."""
    tokens = re.finditer(r'[\w.-]+=(?:"(?:\\.|[^"\\])*"|[^\s{};]+)|"(?:\\.|[^"\\])*"|//[^\n]*|/\*[\s\S]*?\*/|[{};]|[^\s{};]+', source)
    depth, start, opening, name = 0, None, None, None
    for match in tokens:
        token = match.group()
        if token.startswith(("//", "/*")):
            continue
        if depth == 1 and start is None and token == "tab":
            start, name = match.start(), None
        if start is not None and opening is None and token.startswith("name="):
            name = json.loads(token[5:]) if token[5:].startswith('"') else token[5:]
        # name="quoted words" may be tokenized as name= then a quoted token.
        if start is not None and opening is None and token.startswith('"'):
            prefix = source[start:match.start()]
            if re.search(r'name=\s*$', prefix):
                name = json.loads(token)
        if token == "{":
            depth += 1
            if start is not None and opening is None:
                opening = match.end()
        elif token == "}":
            depth -= 1
            if start is not None and opening is not None and depth == 1:
                yield start, opening, match.start(), match.end(), name
                start, opening, name = None, None, None


def fixture_layout(source, tabs, task, role, cwd):
    spans = list(tab_spans(source))
    if role == "operator":
        assert [span[4] for span in spans] == OPERATOR_TABS, "canonical Operator tab contract differs"
    assert source.rstrip().endswith("}"), "layout must have one outer block"
    # Keep canonical chrome/plugin/role declarations outside direct tab bodies.
    for start, _, _, end, _ in reversed(spans):
        source = source[:start] + source[end:]
    declarations = []
    for index, name in enumerate(tabs):
        panes = []
        for side in ("left", "right") if role != "operator" else ("left",):
            label = f"{role}:{name}:{side}"
            panes.append(f'pane name={json.dumps(label)} command={json.dumps(sys.executable)} '
                         f'cwd={json.dumps(str(cwd))} {{ args {json.dumps(str(task))} {json.dumps(label)}; }}')
        declarations.append(f'tab name={json.dumps(name)}' + (' focus=true' if index == 0 else '') + ' { pane split_direction="vertical" { '
                            + "; ".join(panes) + "; }; }")
    end = source.rfind("}")
    return source[:end] + "\n" + "\n".join(declarations) + "\n" + source[end:]



def parse_client_memberships(text):
    lines = text.splitlines()
    assert lines and lines[0].split() == ["CLIENT_ID", "ZELLIJ_PANE_ID", "RUNNING_COMMAND"], "list-clients schema differs"
    result = {}
    for line in lines[1:]:
        if not line.strip():
            continue
        fields = line.split()
        assert len(fields) >= 2 and fields[0].isdigit(), ("malformed client row", line)
        client_id = int(fields[0])
        assert client_id not in result and re.fullmatch(r"(?:terminal|plugin)_\d+", fields[1]), line
        result[client_id] = fields[1]
    return result


def parse_pane_inventory(text):
    rows = json.loads(text)
    assert isinstance(rows, list), "list-panes must return an array"
    required = {"id", "is_plugin", "is_selectable", "is_suppressed", "exited", "tab_id", "tab_name",
                "pane_x", "pane_y", "pane_columns", "pane_rows", "plugin_url"}
    for row in rows:
        assert isinstance(row, dict) and required <= row.keys(), ("list-panes schema differs", row)
        assert isinstance(row["id"], int) and isinstance(row["is_plugin"], bool), row
        if not row["is_plugin"]:
            assert isinstance(row.get("pane_command"), str) and row["pane_command"], ("unobserved command", row)
    return rows


def verify_build_info(text):
    info = json.loads(text)
    assert isinstance(info, dict) and info.get("product") == "vc-frame", "foreign binary"
    assert info.get("git_dirty") is False, "requires an explicit clean embedded git_dirty=false"
    assert isinstance(info.get("git_sha"), str) and re.fullmatch(r"[0-9a-f]{40}", info["git_sha"]), "missing full build SHA"
    return info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for option in ("binary", "config", "operator-layout", "project-layout", "scratch", "output"):
        parser.add_argument("--" + option, type=Path, required=True)
    parser.add_argument("--status-text-a", default="{session}", help="Expected A footer identity (default: current session name)")
    parser.add_argument("--status-text-b", default="{session}", help="Expected B footer identity (default: current session name)")
    parser.add_argument("--timeout", type=float, default=30)
    args = parser.parse_args()
    import pyte  # existing harness dependency; no installation fallback

    binary = args.binary.resolve(strict=True)
    assert os.access(binary, os.X_OK), "explicit binary must be executable"
    args.scratch.mkdir(parents=True, exist_ok=False)
    args.output.mkdir(parents=True, exist_ok=False)
    scratch, out = args.scratch.resolve(), args.output.resolve()
    assert len(str(scratch / "s")) < 75, "use short private socket root on macOS"
    env = {key: os.environ[key] for key in ("PATH", "USER", "LOGNAME", "LANG") if key in os.environ}
    env.update(TERM="xterm-256color", COLORTERM="truecolor", SHELL="/bin/sh",
               VC_FRAME_SERVER_FOREGROUND="1", VC_FRAME_ROUTE_DIAGNOSTICS="1")
    for key, name in {"HOME": "home", "TMPDIR": "tmp", "XDG_CONFIG_HOME": "config",
                      "XDG_CACHE_HOME": "cache", "XDG_DATA_HOME": "data", "XDG_STATE_HOME": "state",
                      "XDG_RUNTIME_DIR": "runtime", "VIBECRAFTED_HOME": "vc",
                      "VC_FRAME_CONFIG_DIR": "config/vc-frame", "VC_FRAME_SOCKET_DIR": "s"}.items():
        directory = scratch / name
        directory.mkdir(parents=True, exist_ok=True)
        env[key] = str(directory)
    if sys.platform == "darwin":
        (Path(env["HOME"]) / "Library/Application Support/io.vetcoders.vc-frame").mkdir(parents=True)
    env["ZELLIJ_SOCKET_DIR"] = env["VC_FRAME_SOCKET_DIR"]
    env["VIBECRAFTED_CONTROL_PLANE"] = str(scratch / "no-control-plane.json")
    events = out / "workloads.jsonl"
    task = scratch / "task.py"
    task.write_text("import json,os,sys\n"
                    f"fd=os.open({str(events)!r},os.O_WRONLY|os.O_CREAT|os.O_APPEND,0o600)\n"
                    "label=sys.argv[1]; pid=os.getpid(); pane=os.environ.get('ZELLIJ_PANE_ID')\n"
                    "def record(kind,value):\n"
                    " os.write(fd,(json.dumps(dict(label=label,pid=pid,pane=pane,kind=kind,value=value))+'\\n').encode())\n"
                    "record('start',os.getcwd())\n"
                    "print('READY:'+label+':'+str(pid),flush=True)\n"
                    "print('RGB:\\x1b[38;2;17;93;201mRGB_SENTINEL\\x1b[0m',flush=True)\n"
                    "for line in sys.stdin:\n"
                    " token=line.rstrip();record('input',token);print('ACK:'+label+':'+str(pid)+':'+token,flush=True)\n")
    config = scratch / "config.kdl"
    source_config = args.config.read_text()
    for key, value in {"default_shell": json.dumps(sys.executable), "default_mode": '"normal"',
                       "show_startup_tips": "false", "show_release_notes": "false",
                       "session_serialization": "false", "auto_lock_after_seconds": "0",
                       "mirror_session": "false"}.items():
        source_config = re.sub(r"^" + key + r"\s+[^\n]*$", "", source_config, flags=re.M)
        source_config += f"\n{key} {value}\n"
    config.write_text(source_config)
    names = {"operator": f"op{os.getpid()}", "a": f"a-peer{os.getpid()}", "b": f"b-peer{os.getpid()}"}
    tabs = {"operator": OPERATOR_TABS, "a": ["A-one", "A-two"], "b": ["B-one", "B-two"]}
    layouts = {}
    for role in names:
        cwd = scratch / ("repo-" + role)
        cwd.mkdir()
        if role != "operator":
            subprocess.run(["git", "init", "--initial-branch", "proof-" + role, str(cwd)], env=env,
                           check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        source = (args.operator_layout if role == "operator" else args.project_layout).read_text()
        layouts[role] = scratch / (role + ".kdl")
        layouts[role].write_text(fixture_layout(source, tabs[role], task, role, cwd))
    receipt = {"status": "running", "source_only_limit": "Does not certify Operator backends, packaged launch or macOS physical keys",
               "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "harness_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
               "sessions": names, "scratch": str(scratch), "steps": [], "cleanup": []}
    for path in (args.config, args.operator_layout, args.project_layout):
        receipt.setdefault("inputs", {})[str(path.resolve())] = hashlib.sha256(path.read_bytes()).hexdigest()
    clients = []
    command_number = 0
    base = [str(binary), "--config", str(config), "--config-dir", env["VC_FRAME_CONFIG_DIR"]]

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(self.fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y+1};{self.cursor.x+1}R")
            else:
                super().report_device_status(mode)

    def pump(seconds=0.15):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            live = [client for client in clients if client["fd"] >= 0]
            if not live:
                time.sleep(0.02)
                continue
            ready, _, _ = select.select([client["fd"] for client in live], [], [], 0.05)
            for client in live:
                if client["fd"] not in ready:
                    continue
                try:
                    data = os.read(client["fd"], 65536)
                except OSError:
                    data = b""
                if not data:
                    continue
                client["raw"].write(data)
                client["raw"].flush()
                client["stream"].feed(client["decoder"].decode(data))

    def snapshot(label):
        for index, client in enumerate(clients):
            (out / f"{label}-client-{index}.txt").write_text("\n".join(client["screen"].display))
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2))

    def wait(predicate, label):
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            pump()
            if predicate():
                return
        snapshot(label + "-timeout")
        raise AssertionError(label)

    def cli(role, *command, check=True):
        nonlocal command_number
        command_number += 1
        argv = base + (["--session", names[role]] if role else []) + list(map(str, command))
        log = out / f"command-{command_number}.log"
        with log.open("wb") as stream:
            process = subprocess.Popen(argv, env=env, stdout=stream, stderr=stream)
            deadline = time.monotonic() + args.timeout
            while process.poll() is None and time.monotonic() < deadline:
                pump()
            if process.poll() is None:
                process.kill()  # only this fixture-owned diagnostic command
                process.wait(timeout=5)
                raise AssertionError(f"CLI watchdog: {argv}; {log}")
        text = log.read_text(errors="replace")
        receipt.setdefault("commands", []).append({"argv": argv, "exit": process.returncode, "log": str(log)})
        if check:
            assert process.returncode == 0, (argv, text)
        return text

    def launch(role):
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(binary, base + ["attach", names[role]], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 180, 0, 0))
        screen = Terminal(180, 40)
        screen.fd = fd
        client = dict(pid=pid, fd=fd, screen=screen, stream=pyte.Stream(screen),
                      decoder=codecs.getincrementaldecoder("utf-8")("replace"),
                      raw=(out / f"client-{len(clients)}.ansi").open("wb"))
        clients.append(client)
        return client

    def pane_inventory(role):
        return parse_pane_inventory(cli(role, "action", "list-panes", "--all", "--json", "--command"))

    def memberships(role):
        return parse_client_memberships(cli(role, "action", "list-clients"))

    def records():
        return [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []

    def starts():
        return {item["label"]: item for item in records() if item["kind"] == "start"}

    def chip(client, name):
        row = client["screen"].display[0]
        at = row.find(name)
        return at >= 0 and "◉" in row[max(0, at - 4):at]

    def click(client, column, row):
        os.write(client["fd"], f"\x1b[<0;{column+1};{row+1}M\x1b[<0;{column+1};{row+1}m".encode())
        pump()

    def ack(client, role, tab, side, token):
        label = f"{role}:{tab}:{side}"
        expected = starts()[label]
        os.write(client["fd"], (token + "\r").encode())
        wait(lambda: any(item["kind"] == "input" and item["value"] == token for item in records()), token)
        received = [item for item in records() if item["kind"] == "input" and item["value"] == token]
        assert len(received) == 1 and received[0]["label"] == label and received[0]["pid"] == expected["pid"], received
        wait(lambda: "ACK:" + label + ":" + str(expected["pid"]) + ":" + token in
             "\n".join(client["screen"].display), "rendered-" + token)
        pane_id = str(expected["pane"])
        if not pane_id.startswith("terminal_"):
            pane_id = "terminal_" + pane_id
        assert client["role"] == role, ("wrong frontend session binding", client["role"], role)
        assert memberships(role).get(client["client_id"]) == pane_id, (role, client["client_id"], pane_id, memberships(role))
        return pane_id

    def select(client, role, tab, side):
        wait(lambda: tab in client["screen"].display[0], "tab-visible-" + tab)
        click(client, client["screen"].display[0].index(tab) + 1, 0)
        wait(lambda: chip(client, tab), "tab-selected-" + tab)
        expected = starts()[f"{role}:{tab}:{side}"]
        pane = next(p for p in pane_inventory(role) if not p["is_plugin"] and str(p["id"]) == str(expected["pane"]).removeprefix("terminal_"))
        click(client, pane["pane_x"] + max(1, pane["pane_columns"] // 2),
              pane["pane_y"] + max(1, pane["pane_rows"] // 2))

    def peer_unchanged(client, tab, pane):
        assert chip(client, tab), client["screen"].display[0]
        assert client["role"] == "a" and memberships("a").get(client["client_id"]) == pane, memberships("a")

    def checkpoint(label, client, role, tab, side, expected_count):
        wait(lambda: chip(client, tab), label + "-chip")
        pane = ack(client, role, tab, side, label)
        assert len(memberships(role)) == expected_count, (role, memberships(role))
        row = client["screen"].display[0]
        assert names[role] in client["screen"].display[-1], ("footer session differs from attached session", client["screen"].display[-1])
        active_at = row.index(tab)
        assert all(client["screen"].buffer[0][col].bold for col in range(active_at, active_at+len(tab))), ("active tab is not bold", row)
        count = 1 if role == "operator" else 2
        assert re.search(re.escape(tab) + r"\s+\(" + str(count) + r"\)", row), ("wrong selectable pane counter", row)
        next_chip = re.search(r"[◉○]\s", row[active_at+len(tab):])
        close_end = active_at+len(tab)+next_chip.start() if next_chip else len(row)
        close_at = row.find("✕", active_at+len(tab), close_end)
        if role != "operator":
            assert close_at >= 0, ("project tab close glyph missing", row)
            assert row[close_at-1:close_at+2] == " ✕ ", ("three-cell close visual", row)
            background = client["screen"].buffer[0][active_at].bg
            assert all(client["screen"].buffer[0][col].bg == background for col in range(close_at-1, close_at+2)), ("close separated from tab title", row)
        if role != "operator":
            for other_role in names:
                if other_role != role:
                    assert all(name not in row for name in tabs[other_role]), (role, row)
            expected_status = (args.status_text_a if role == "a" else args.status_text_b).format(session=names[role])
            assert expected_status in client["screen"].display[-1], client["screen"].display[-1]
        assert len([line for line in client["screen"].display if "Vibecrafted." in line]) == 1
        assert "SESSIONS" in "\n".join(line[:28] for line in client["screen"].display)
        rail = [(i, line[:28]) for i, line in enumerate(client["screen"].display)]
        for number, expected_role in enumerate(("operator", "a", "b")):
            title = "Operator Frame" if expected_role == "operator" else names[expected_role]
            matching = [(i, line) for i, line in rail if re.search(r"\b" + f"{number:02}" + r"\b", line)
                        and title.lower() in line.lower()]
            assert len(matching) == 1, (number, title, rail)
            if expected_role == role:
                i, line = matching[0]
                at = line.lower().index(title.lower())
                attrs = {client["screen"].buffer[i][col].bg for col in range(at, min(at+len(title), 28))}
                assert len(attrs) == 1 and attrs != {"default"}, ("selected rail has no background", attrs)
        assert starts() == original_starts, "workload process identity changed during routing"
        for item in original_starts.values():
            os.kill(item["pid"], 0)
        for candidate in clients:
            assert os.waitpid(candidate["pid"], os.WNOHANG) == (0, 0), "outer client exited"
        receipt["steps"].append({"label": label, "role": role, "tab": tab, "pane": pane,
                                  "frontend_pid": client["pid"], "client_id": client["client_id"],
                                  "memberships": {r: memberships(r) for r in names}})
        snapshot(label)
        return pane

    def bind_transition(client, role, before_destination, origin_role, origin_client_id):
        # Serial fixture input gives an independent admission correlation: this
        # frontend alone leaves the origin and adds exactly one destination ID.
        # A shared pane cannot substitute a different frontend's membership.
        wait(lambda: len(set(memberships(role)) - before_destination) == 1 and
             origin_client_id not in memberships(origin_role), "exact-frontend-admission-" + role)
        destination = memberships(role)
        added = set(destination) - before_destination
        assert len(added) == 1 and before_destination <= set(destination), ("peer admission changed", destination)
        client.update(role=role, client_id=added.pop())

    def route(client, direction, role, tab):
        origin_role, origin_client_id = client["role"], client["client_id"]
        assert origin_role != role
        before_destination = set(memberships(role))
        os.write(client["fd"], KEY[direction])
        bind_transition(client, role, before_destination, origin_role, origin_client_id)
        wait(lambda: (chip(client, tab) if tab is not None else
                      names[role] in client["screen"].display[-1]), f"route-{direction}-{role}")

    def inventory_truth():
        inventories = {role: pane_inventory(role) for role in names}
        for role, panes in inventories.items():
            for pane in panes:
                if not pane["is_plugin"]:
                    command = pane.get("pane_command")
                    assert command, ("unobserved terminal command", pane)
                    assert not re.search(r"vc-frame\s+(visit|attach)|workspace-projection", command), pane
            for suffix in ("compact-bar", "status-bar", "session-manager"):
                chrome = [p for p in panes if p["is_plugin"] and
                          (str(p.get("plugin_url", "")).split(":")[-1] == suffix)]
                assert chrome, (role, "missing chrome", suffix)
                assert len({p.get("plugin_runtime_id") for p in chrome}) == 1, (role, suffix, chrome)
                assert all(not p["exited"] and not p["is_suppressed"] for p in chrome), chrome
        (out / "all-panes.json").write_text(json.dumps(inventories, indent=2))
        # Socket-owner roots give a fixture-only ancestry boundary. Never judge
        # the Founder's unrelated Frame processes or use all-process kill.
        owner_output = subprocess.check_output(["lsof", "-n", "-P", "-U", "-Fpn"], text=True)
        roots, current_pid = set(), None
        for line in owner_output.splitlines():
            if line.startswith("p"):
                current_pid = int(line[1:])
            elif line.startswith("n") and current_pid is not None:
                path = Path(line[1:].split(" -> ")[0])
                try:
                    path.resolve(strict=True).relative_to(Path(env["VC_FRAME_SOCKET_DIR"]))
                except (OSError, ValueError):
                    continue
                roots.add(current_pid)
        assert roots, "fixture socket-owner ancestry unavailable"
        table_text = subprocess.check_output(["ps", "-axo", "pid=,ppid=,command="], text=True)
        table = {}
        for line in table_text.splitlines():
            fields = line.strip().split(None, 2)
            if len(fields) == 3:
                table[int(fields[0])] = (int(fields[1]), fields[2])
        owned = roots | {c["pid"] for c in clients}
        while True:
            expanded = owned | {pid for pid, (parent, _) in table.items() if parent in owned}
            if expanded == owned:
                break
            owned = expanded
        rows = {pid: table[pid] for pid in owned if pid in table}
        client_pids = {c["pid"] for c in clients}
        server_rows = []
        for pid, (_, command) in rows.items():
            executable = shlex.split(command)[0] if command else ""
            if executable == str(binary) or Path(executable).name == binary.name:
                if pid not in client_pids:
                    assert "--server" in command, ("nested Frame client", pid, command)
                    server_rows.append((pid, command))
        assert len(server_rows) == 3, ("one server per peer session", server_rows)
        receipt["owned_process_tree"] = rows
        receipt["no_nested_frame"] = True

    try:
        receipt["build_info"] = verify_build_info(cli(None, "--build-info"))
        for role in names:
            cli(None, "--layout", layouts[role], "attach", "-b", "-c", names[role])
        expected_labels = {f"{role}:{tab}:{side}" for role in names for tab in tabs[role]
                           for side in (("left",) if role == "operator" else ("left", "right"))}
        wait(lambda: set(starts()) == expected_labels, "all-fixture-workloads-ready")
        original_starts = starts()
        assert not memberships("a"), "fixture project already has clients before first PTY"
        first = launch("a")
        wait(lambda: len(memberships("a")) == 1, "first-frontend-admitted")
        first.update(role="a", client_id=next(iter(memberships("a"))))
        before_peer = set(memberships("a"))
        peer = launch("a")
        wait(lambda: len(memberships("a")) == 2 and len(set(memberships("a"))-before_peer) == 1,
             "second-frontend-admitted")
        peer.update(role="a", client_id=(set(memberships("a"))-before_peer).pop())
        select(first, "a", "A-two", "right")
        select(peer, "a", "A-one", "left")
        peer_pane = ack(peer, "a", "A-one", "left", "peer-before")
        checkpoint("first-before", first, "a", "A-two", "right", 2)
        # Two clients can focus the same pane. Exact frontend binding remains
        # mandatory; a pane present in any client row is not a valid oracle.
        select(first, "a", "A-one", "left")
        checkpoint("same-pane-distinct-frontends", first, "a", "A-one", "left", 2)
        assert first["client_id"] != peer["client_id"]
        assert memberships("a")[first["client_id"]] == memberships("a")[peer["client_id"]] == peer_pane
        select(first, "a", "A-two", "right")
        checkpoint("first-restored-after-same-pane", first, "a", "A-two", "right", 2)
        for mode, sequence in (("normal", None), ("locked", b"\x07")):
            if sequence:
                os.write(first["fd"], sequence)
                wait(lambda: "⚿ L" in first["screen"].display[0], "locked-mode")
            route(first, "down", "b", "B-one" if mode == "normal" else "B-two")
            select(first, "b", "B-two", "right")
            checkpoint(mode + "-b", first, "b", "B-two", "right", 1)
            peer_unchanged(peer, "A-one", peer_pane)
            route(first, "up", "a", "A-two")
            checkpoint(mode + "-return-a", first, "a", "A-two", "right", 2)
            peer_unchanged(peer, "A-one", peer_pane)
            route(first, "up", "operator", "Dashboard")
            checkpoint(mode + "-operator", first, "operator", "Dashboard", "left", 1)
            route(first, "down", "a", "A-two")
            checkpoint(mode + "-from-operator", first, "a", "A-two", "right", 2)
            os.write(first["fd"], KEY["left"])
            wait(lambda: chip(first, "A-one"), mode + "-local-tab-left")
            select(first, "a", "A-one", "right")
            checkpoint(mode + "-left", first, "a", "A-one", "right", 2)
            os.write(first["fd"], KEY["right"])
            wait(lambda: chip(first, "A-two"), mode + "-local-tab-right")
            checkpoint(mode + "-right", first, "a", "A-two", "right", 2)
            peer_unchanged(peer, "A-one", peer_pane)
            route(first, "down", "b", "B-two")
            checkpoint(mode + "-remember-b", first, "b", "B-two", "right", 1)
            route(first, "up", "a", "A-two")
        # Reverse roles: the second outer client also keeps its own history.
        route(peer, "down", "b", None)
        select(peer, "b", "B-one", "left")
        checkpoint("peer-b", peer, "b", "B-one", "left", 1)
        route(peer, "up", "a", "A-one")
        checkpoint("peer-return-a", peer, "a", "A-one", "left", 2)
        # Inspect actual rendered direct-RGB cells, not only emitted source text.
        rgb_found = []
        for client in clients:
            for line_no, line in enumerate(client["screen"].display):
                at = line.find("RGB_SENTINEL")
                if at >= 0:
                    colors = [client["screen"].buffer[line_no][col].fg for col in range(at, at+12)]
                    assert all(color == "115dc9" for color in colors), ("direct RGB was changed", colors)
                    rgb_found.append({"client_pid": client["pid"], "row": line_no, "fg": colors})
        assert len({item["client_pid"] for item in rgb_found}) == 2, "RGB sentinel not observed in each client"
        receipt["direct_rgb_cells"] = rgb_found
        # Real rail session and cross-session tab clicks, with native membership.
        def rail_click(client, text, role):
            title = "Operator Frame" if role == "operator" else names[role]
            lines = client["screen"].display
            session_row = next(i for i, line in enumerate(lines) if title.lower() in line[:28].lower())
            if text == title:
                row = session_row
            else:
                row = next(i for i in range(session_row+1, len(lines)) if text in lines[i][:28])
                assert not any(re.search(r"^\s*\d{2}\b", lines[i][:28]) for i in range(session_row+1, row+1)), "tab belongs to another session row"
            column = lines[row].lower().index(text.lower()) + 1
            origin_role, origin_client_id = client["role"], client["client_id"]
            before_destination = set(memberships(role))
            click(client, column, row)
            if origin_role != role:
                bind_transition(client, role, before_destination, origin_role, origin_client_id)
            else:
                assert origin_client_id in memberships(role), "same-session click lost its frontend"
        rail_click(first, names["b"], "b")
        checkpoint("rail-b", first, "b", "B-two", "right", 1)
        rail_click(first, "A-two", "a")
        checkpoint("rail-cross-tab-a", first, "a", "A-two", "right", 2)
        # C2 reattached with a new numeric id; compare the exact chosen pane
        # rather than assuming IDs survive session reconnects.
        assert chip(peer, "A-one") and memberships("a").get(peer["client_id"]) == peer_pane
        inventory_truth()
        receipt["status"] = "passed_peer_routing_scenario"
        receipt["unverified"] = ["Operator backends", "packaged/installed launch", "physical macOS Cmd delivery",
                                  "GitHub network receipts", "font/opacity/blur", "close hit target behavior and narrow +N overflow"]
    except Exception as error:
        receipt.update(status="failed", error=repr(error))
        snapshot("failure")
        raise
    finally:
        for role in names:
            try:
                text = cli(None, "kill-session", names[role], "--yes", check=False)
                receipt["cleanup"].append({"session": names[role], "output": text})
            except Exception as error:
                receipt["cleanup"].append({"session": names[role], "error": repr(error)})
        for client in clients:
            try:
                os.close(client["fd"])
            except OSError:
                pass
            client["fd"] = -1
            client["raw"].close()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    if os.waitpid(client["pid"], os.WNOHANG)[0]:
                        break
                except ChildProcessError:
                    break
                time.sleep(0.05)
            else:
                receipt["cleanup"].append({"unreaped_owned_client": client["pid"]})
        snapshot("final")


if __name__ == "__main__":
    main()
