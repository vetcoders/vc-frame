#!/usr/bin/env python3
"""Rail + actual Strider -> selected-runtime owner, in disposable native PTYs.

python3 zellij-server/tests/rail_project_owner_pty.py --binary /candidate/vc-frame \
    --runtime-root /selected/vibecrafted --output /new/receipt-directory

Requires pyte and the selected runtime's Python dependencies. Never builds,
installs, opens a desktop window, launches a provider or uses live config.
The copied product layout keeps the real Agents scene; only Launchpad/Shell
workloads become harmless context observers. No filepicker replies are forged.
The default tests cold host preparation. --existing-host explicitly tests the
separate case of a real, already initialized native Operator host.
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
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

from peer_sessions_pty import parse_client_memberships, tab_spans, verify_build_info
from installed_runtime_fixture import materialize_installed_runtime, runtime_snapshot

WES_KEYS = ("VIBECRAFTED_WORKSPACE_ROOT", "VIBECRAFTED_WORKSPACE_ID",
            "VIBECRAFTED_SESSION_ID", "VIBECRAFTED_WORKSPACE_INSTANCE_ID",
            "VIBECRAFTED_BUILD_ID")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "runtime-root", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--python", default=sys.executable, type=Path)
    parser.add_argument("--timeout", type=float, default=130,
                        help="Observe the product's 120s open deadline plus event delivery; never outrun its verdict")
    parser.add_argument("--installed", action="store_true",
                        help="Use the real product wrapper, physical engine and sealed RuntimePack")
    parser.add_argument("--existing-host", action="store_true",
                        help="Create and observe a ready native vc-host before rail actions; default tests cold preparation")
    args = parser.parse_args()
    import pyte

    binary = args.binary.resolve(strict=True)
    source_runtime = args.runtime_root.resolve(strict=True)
    # Preserve a selected venv interpreter's invocation path. Resolving its
    # executable symlink would silently substitute the base Python environment.
    python = args.python.expanduser().absolute()
    source_config = source_runtime / "vibecrafted-core/vibecrafted_core/config/vc-frame"
    assert (source_config / "vc-agent-workshop.py").is_file(), source_config
    assert os.access(binary, os.X_OK) and os.access(python, os.X_OK)
    args.output.mkdir(parents=True, exist_ok=False)
    out = args.output.resolve()
    scratch = Path(tempfile.mkdtemp(prefix="vcrp-", dir="/tmp")).resolve()
    installed = None
    if args.installed:
        installed = materialize_installed_runtime(source_runtime, binary, python, scratch)
        runtime, private_config, env = installed['runtime'], installed['config'], installed['env']
        core, bindir = runtime / 'vibecrafted-core', runtime / 'bin'
        python = installed['python']
        sockets = Path(env['VC_FRAME_SOCKET_DIR'])
        binary = installed['entry']
    else:
        runtime = scratch / "runtime"
        shutil.copytree(source_runtime, runtime, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", ".venv", "target", "node_modules", "build", "dist",
                                                      ".vibecrafted", ".prview", ".remember", ".ai-context",
                                                      ".loctree", "__pycache__", "*.pyc", ".pytest_cache",
                                                      ".ruff_cache", ".mypy_cache"))
        core = runtime / "vibecrafted-core"
        private_config = core / "vibecrafted_core/config/vc-frame"
        # Keep the copied baseline for evidence; publish host-adapted config only
        # inside the private developer runtime, where the canonical facade pins it.
        private_config.rename(private_config.with_name("vc-frame-baseline"))
        bindir = runtime / "bin"
        bindir.mkdir(exist_ok=True)
        for name, target in (("vc-frame", binary), ("python3", python)):
            destination = bindir / name
            if destination.is_symlink() or destination.exists():
                destination.unlink()
            if name == "python3":
                # Invoke the exact selected interpreter path so a venv keeps its
                # package prefix even from this private Runtime Pack's bin door.
                # Preserve the real owner's bytes and status for diagnosing a
                # first preparation refusal whose rail notice is width-limited.
                prefix = shlex.quote(str(out / "owner-prepare-")) + '"$$"'
                destination.write_text(
                    "#!/bin/sh\n"
                    + 'if [ "${1:-}" = ' + shlex.quote(str(private_config / "vc-agent-workshop.py"))
                    + ' ] && [ "${2:-}" = prepare ]; then\n'
                    + "  diagnostic=" + prefix + "\n"
                    + '  printf "%s\\0" "$@" > "$diagnostic.argv.bin"\n'
                    + "  " + shlex.quote(str(target)) + ' "$@" > "$diagnostic.stdout.bin" 2> "$diagnostic.stderr.bin"\n'
                    + '  result=$?\n'
                    + '  printf "%s\\n" "$result" > "$diagnostic.exit"\n'
                    + '  /bin/cat "$diagnostic.stdout.bin"\n'
                    + '  /bin/cat "$diagnostic.stderr.bin" >&2\n'
                    + '  exit "$result"\n'
                    + "fi\nexec " + shlex.quote(str(target)) + ' "$@"\n'
                )
                destination.chmod(0o755)
            else:
                destination.symlink_to(target)
        env = {key: os.environ[key] for key in ("PATH", "USER", "LOGNAME", "LANG") if key in os.environ}
        env.update(TERM="xterm-256color", COLORTERM="truecolor", SHELL="/bin/bash",
                   VC_FRAME_SERVER_FOREGROUND="1", VC_FRAME_ROUTE_DIAGNOSTICS="1",
                   VIBECRAFTED_PREFER_REPO_VC_FRAME="1", VIBECRAFTED_RUNTIME_ROOT=str(runtime),
                   VIBECRAFTED_RUNTIME_BIN=str(bindir), VIBECRAFTED_CORE_DIR=str(core), VIBECRAFTED_PYTHON=str(python),
                   VIBECRAFTED_VC_FRAME_BIN=str(binary), PYTHONPATH=str(core))
        for key in ("HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                    "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "VIBECRAFTED_HOME"):
            path = scratch / key.lower()
            path.mkdir()
            env[key] = str(path)
        sockets = scratch / "s"
        sockets.mkdir()
        env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets),
                   VC_FRAME_CONFIG_DIR=str(private_config),
                   VIBECRAFTED_CONTROL_PLANE=str(scratch / "disabled-control-plane.json"))
        env["PATH"] = str(bindir) + ":" + env.get("PATH", "/usr/bin:/bin")
    projects = scratch / "projects"
    projects.mkdir()
    receipt = {"status": "running", "scratch": str(scratch), "binary": str(binary),
               "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
               "runtime_root": str(runtime), "runtime_source": str(source_runtime),
               "host_precondition": {"mode": "existing" if args.existing_host else "cold", "prepared": False},
               "commands": [], "steps": [], "cleanup": [],
               "limits": "Private installed-shape PTYs" if installed else "Private source-runtime PTYs"}
    clients = []
    known = set()
    context_log = out / "initial-context.jsonl"
    workload_log = out / "workloads.jsonl"
    helper = scratch / "observer.py"
    helper.write_text('''import importlib.util,json,os,pathlib,sys,time
from types import SimpleNamespace
config=pathlib.Path(os.environ['VC_FRAME_CONFIG_DIR'])
spec=importlib.util.spec_from_file_location('rail_workshop',config/'vc-agent-workshop.py')
m=importlib.util.module_from_spec(spec);sys.modules[spec.name]=m;spec.loader.exec_module(m)
label=sys.argv[1]
before={k:os.environ.get(k) for k in %r}
form=m.Workshop(SimpleNamespace(),mode='launcher')
data=dict(label=label,pid=os.getpid(),ppid=os.getppid(),cwd=os.getcwd(),
          tty=os.ttyname(0),isatty=os.isatty(0),session=os.environ.get('VC_FRAME_SESSION_NAME'),
          initial=before,path=form.path,error=form.error,
          recovered={k:os.environ.get(k) for k in %r})
with open(%r,'a') as f:f.write(json.dumps(data)+'\\n')
print('CONTEXT_READY:'+label,flush=True)
while True:time.sleep(.1)
''' % (WES_KEYS, WES_KEYS, str(context_log)))
    workload = scratch / "workload.py"
    workload.write_text('''import json,os,sys,time
data=dict(label=sys.argv[1],pid=os.getpid(),ppid=os.getppid(),cwd=os.getcwd(),
          tty=os.ttyname(0),isatty=os.isatty(0),session=os.environ.get('VC_FRAME_SESSION_NAME'))
with open(%r,'a') as f:f.write(json.dumps(data)+'\\n')
print('WORKLOAD_READY:'+sys.argv[1],flush=True)
while True:time.sleep(.1)
''' % str(workload_log))

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(self.fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f"\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R")
            else:
                super().report_device_status(mode)

    def pump(seconds=.15):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            live = [c for c in clients if c["fd"] >= 0]
            if not live:
                time.sleep(.02)
                continue
            ready, _, _ = select.select([c["fd"] for c in live], [], [], .05)
            for client in live:
                if client["fd"] not in ready:
                    continue
                try:
                    data = os.read(client["fd"], 65536)
                except OSError:
                    data = b""
                client["raw"].write(data)
                client["raw"].flush()
                client["stream"].feed(client["decoder"].decode(data))
                prompt = "\n".join(client["screen"].display)
                permission = "Allow?" in prompt and "(y/n)" in prompt
                if permission and not client.get("permission_prompt"):
                    # Only a real permission prompt in this exact fixture PTY
                    # receives a consent key. No blanket permission cache edit.
                    os.write(client["fd"], b"y")
                    receipt["steps"].append({"permission_granted": True, "frontend_pid": client["pid"]})
                client["permission_prompt"] = permission

    def save(label):
        for index, c in enumerate(clients):
            (out / f"{label}-{index}.screen.txt").write_text("\n".join(c["screen"].display))
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")

    def wait(predicate, label):
        receipt["checkpoint"] = label
        print("checkpoint: " + label, flush=True)
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            pump()
            if predicate():
                return
        save(label + "-timeout")
        raise AssertionError(label)

    def run(argv, *, check=True, data=None):
        with tempfile.TemporaryFile() as log:
            process = subprocess.Popen(list(map(str, argv)), env=env, cwd=projects,
                                       stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                                       stdout=log, stderr=log)
            if data is not None:
                process.stdin.write(json.dumps(data).encode())
                process.stdin.close()
            deadline = time.monotonic() + args.timeout
            while process.poll() is None and time.monotonic() < deadline:
                pump()
            if process.poll() is None:
                process.kill()
                process.wait()
                raise AssertionError(("fixture command timeout", argv))
            log.seek(0)
            text = log.read().decode(errors="replace")
        receipt["commands"].append({"argv": list(map(str, argv)), "status": process.returncode, "output": text})
        if check:
            assert process.returncode == 0, (argv, text)
        return process.returncode, text

    base = [binary] if installed else [binary, "--config", private_config / "config.kdl", "--config-dir", private_config]

    def cli(*argv, check=True):
        return run([*base, *argv], check=check)[1]

    def live():
        code, text = run([*base, "list-sessions", "--short", "--no-formatting"], check=False)
        if code:
            assert "No active" in text, text
            return set()
        return set(text.splitlines())

    def members(session):
        return parse_client_memberships(cli("--session", session, "action", "list-clients"))

    def panes(session):
        return json.loads(cli("--session", session, "action", "list-panes", "--all", "--json", "--command"))

    def workload_guard(session):
        all_panes = panes(session)
        stable = ("id", "tab_id", "terminal_command", "pane_command", "pane_cwd",
                  "pane_x", "pane_y", "pane_rows", "pane_columns")
        return {"pane_ids": sorted((p["is_plugin"], p["id"]) for p in all_panes),
                "terminals": [{key: p.get(key) for key in stable} for p in all_panes if not p["is_plugin"]],
                "clients": members(session)}

    def rows(path):
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def python_code(code, data=None):
        return run([python, "-c", code], data=data)[1]

    def attach(session):
        before = set(members(session))
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(projects)
            os.execve(binary, list(map(str, [*base, "attach", session])), env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 46, 200, 0, 0))
        screen = Terminal(200, 46)
        screen.fd = fd
        client = dict(pid=pid, fd=fd, screen=screen, stream=pyte.Stream(screen),
                      decoder=codecs.getincrementaldecoder("utf-8")("replace"),
                      raw=(out / f"client-{len(clients)}.ansi").open("wb"), session=session)
        clients.append(client)
        wait(lambda: len(set(members(session)) - before) == 1, "attach-" + session)
        client["id"] = (set(members(session)) - before).pop()
        return client

    def send(c, value):
        os.write(c["fd"], value)
        pump(.3)

    def click(c, x, y):
        send(c, f"\x1b[<0;{x + 1};{y + 1}M\x1b[<0;{x + 1};{y + 1}m".encode())

    def rail(c, text):
        wait(lambda: any(text.casefold() in line[:24].casefold() for line in c["screen"].display), "rail-" + text)
        y = next(i for i, line in enumerate(c["screen"].display) if text.casefold() in line[:24].casefold())
        click(c, 4, y)

    def is_picker(pane):
        location = str(pane.get("plugin_url"))
        return pane["is_plugin"] and (location == "filepicker" or "strider" in location)

    def picker(c, project):
        # Reconcile the rendered peer/tab row count with native inventory.
        # Socket readiness alone does not mean background metadata has reached
        # this plugin, and a late peer/tab row would move the Open door.
        inventory = {name: json.loads(cli("--session", name, "action", "list-tabs", "--all", "--json"))
                     for name in sorted(live())}
        expected_rows = len(inventory) + sum(len(tabs) for tabs in inventory.values())
        receipt["steps"].append({"picker_native_tabs": {name: len(tabs) for name, tabs in inventory.items()}})
        # SessionUpdate can insert the source tab's metadata row after the
        # rail's first frame. Do not sample the Open row before that content
        # exists, or a later insertion can move the door under the pointer.
        wait(lambda: any("Source work" in line[:24] for line in c["screen"].display),
             "source-tab-row-before-picker")
        # Observe frontend focus before invoking the door. The header has no
        # action, so the readiness check cannot launch a second picker.
        click(c, 4, 1)
        stable = {"signature": None, "since": 0.0, "row": None}

        def ready_door():
            display = c["screen"].display
            doors = [i for i, line in enumerate(display) if "+ Open" in line[:24]]
            rail_panes = [p for p in panes(c["session"]) if p["is_plugin"]
                          and p["is_focused"] and p.get("plugin_url") == "session-manager"]
            if len(doors) != 1 or len(rail_panes) != 1 or not any("Source work" in line[:24] for line in display):
                stable["signature"] = None
                return False
            pane = rail_panes[0]
            expected_door = pane["pane_content_y"] + 1 + expected_rows
            header = display[pane["pane_content_y"]][:24]
            if doors[0] != expected_door or f"SESSIONS {len(inventory)} · Source" not in header:
                stable["signature"] = None
                return False
            signature = (doors[0], tuple(line[:24] for line in display[:doors[0] + 1]), pane["id"],
                         pane["pane_x"], pane["pane_y"], pane["pane_columns"], pane["pane_rows"])
            if signature != stable["signature"]:
                stable.update(signature=signature, since=time.monotonic(), row=doors[0])
                return False
            return time.monotonic() - stable["since"] >= 1.0

        wait(ready_door, "stable-focused-rail-door")
        # Exactly one activation, at the observed stable door coordinate.
        click(c, 4, stable["row"])
        wait(lambda: any(is_picker(p) for p in panes(c["session"])), "actual-strider")
        assert sum(is_picker(p) for p in panes(c["session"])) == 1, "duplicate actual filepickers"
        # Every fresh picker inherits this source session's projects cwd.
        # Enter a uniquely named real directory, then choose that directory.
        def listed():
            return any(project.name in line and not any(marker in line for marker in ("FIND:", "PATH:", "Help:"))
                       for line in c["screen"].display)
        wait(listed, "picker-files-loaded-" + project.name)
        send(c, project.name.encode())
        wait(lambda: listed() and any("FIND: " + project.name in line for line in c["screen"].display),
             "picker-search-result-" + project.name)
        send(c, b"\r")
        wait(lambda: str(project) in "\n".join(c["screen"].display), "picker-directory-" + project.name)
        send(c, b"\x1b")
        send(c, b"\r")
        wait(lambda: not any(is_picker(p) for p in panes(c["session"])), "picker-closed")

    def return_source(c):
        if c["session"] == "source":
            return
        origin, old_id = c["session"], c["id"]
        before = set(members("source"))
        rail(c, "source")
        wait(lambda: len(set(members("source")) - before) == 1 and old_id not in members(origin), "return-source")
        c.update(session="source", id=(set(members("source")) - before).pop())

    def open_project(c, project, *, refuse=False):
        return_source(c)
        before = live()
        old_id = c["id"]
        previous_owner_calls = set(out.glob("owner-prepare-*.argv.bin"))
        picker(c, project)
        if refuse and installed:
            wait(lambda: any(line.lstrip().startswith("Error:") for line in c["screen"].display),
                 "actual-owner-refusal-" + project.name)
            assert live() == before and old_id in members("source"), (before, live())
            receipt["steps"].append({"project": str(project), "refused": True, "live": sorted(before)})
            save("refused-" + project.name)
            return None
        if refuse:
            def owner_refused():
                calls = set(out.glob("owner-prepare-*.argv.bin")) - previous_owner_calls
                if len(calls) != 1:
                    return False
                invocation = next(iter(calls))
                prefix = invocation.with_suffix("").with_suffix("")
                status = prefix.with_suffix(".exit")
                if not status.is_file():
                    return False
                args = invocation.read_bytes().split(b"\0")
                assert os.fsencode(project) in args, args
                assert int(status.read_text()) != 0, "owner admitted a project that must be refused"
                error = prefix.with_suffix(".stderr.bin").read_bytes()
                assert error.strip(), "owner refusal must retain its actual reason"
                receipt["steps"].append({"owner_refusal": str(status), "stderr": error.decode(errors="replace")})
                return True
            wait(owner_refused, "actual-owner-refusal-" + project.name)
            wait(lambda: any(line.lstrip().startswith("Error:") for line in c["screen"].display),
                 "refusal-notice-" + project.name)
            assert live() == before and old_id in members("source"), (before, live())
            receipt["steps"].append({"project": str(project), "refused": True, "live": sorted(before)})
            save("refused-" + project.name)
            return None
        wait(lambda: old_id not in members("source"), "source-client-left")
        def observed():
            return [row for row in rows(context_log) if row["cwd"] == str(project)]
        wait(lambda: bool(observed()), "initial-context-" + project.name)
        context = observed()[0]
        seat = context["session"]
        wait(lambda: len(members(seat)) == 1, "destination-client-admitted")
        c.update(session=seat, id=next(iter(members(seat))))
        assert context["isatty"] and context["path"] == str(project) and not context["error"], context
        assert all(context["initial"].get(key) for key in WES_KEYS), context
        assert context["initial"] == context["recovered"], context
        assert context["initial"]["VIBECRAFTED_WORKSPACE_ROOT"] == str(project), context
        assert set(live()) - before <= {seat, "vc-host"}, (before, live(), seat)
        wait(lambda: "Agents" in c["screen"].display[0], "Agents-chip")
        click(c, c["screen"].display[0].index("Agents") + 2, 0)
        wait(lambda: "New agent" in "\n".join(c["screen"].display), "real-Agents-scene")
        wait(lambda: project.name in "\n".join(c["screen"].display), "real-Agents-project-field")
        assert any(p.get("tab_name") == "Origin proof" for p in panes(seat)), "selected full layout discarded"
        receipt["steps"].append({"project": str(project), "seat": seat, "initial_context": context,
                                  "client_id": c["id"], "live": sorted(live())})
        known.update(live())
        save("opened-" + project.name)
        return context

    def naked(name):
        cli("--layout", source_layout, "attach", "--create-background", name)
        known.add(name)
        # Native background create acknowledges spawning before the child
        # socket/layout are ready. Observe the real private seat before an
        # attach, inventory comparison or catalog control can address it.
        def ready():
            code, text = run([*base, "--session", name, "action", "list-panes", "--all", "--json"], check=False)
            if code:
                return False
            try:
                return bool(json.loads(text))
            except ValueError:
                return False
        wait(ready, "native-seat-ready-" + name)
        wait(lambda: any(row["session"] == name for row in rows(workload_log)),
             "native-workload-ready-" + name)

    def project(name):
        path = projects / name
        path.mkdir()
        run(["git", "init", "--quiet", path])
        return path

    try:
        receipt["build_info"] = verify_build_info(cli("--build-info"))
        assert receipt["build_info"]["version"] == "4.5.0", receipt["build_info"]
        if installed:
            receipt['installed_runtime'] = {key: str(installed[key]) for key in ('entry', 'engine', 'python', 'config')}
            receipt['installed_runtime'].update(source_sha=installed['source_sha'], source_clean=installed['source_clean'],
                                                engine_sha256=installed['binary_sha256'], wrapper_sha256=installed['wrapper_sha256'],
                                                python_artifact=installed['python_artifact'])
        else:
            # A real, disposable Git root selects the documented source developer
            # facade without manufacturing an installed Runtime Pack admission.
            run(["git", "init", "--quiet", runtime])
            receipt["runtime_source_sha"] = run(["git", "-C", source_runtime, "rev-parse", "HEAD"])[1].strip()
            assert not run(["git", "-C", source_runtime, "status", "--porcelain"])[1].strip(), "runtime source must be clean"
            # Stamp only this private VC fixture from its real source identity;
            # the Frame candidate's Cargo version/embedded SHA are never changed.
            runtime_version = (source_runtime / "VERSION").read_text().strip().split("+", 1)[0]
            runtime_version += "+g" + receipt["runtime_source_sha"][:8]
            for version_file in (runtime / "VERSION", core / "vibecrafted_core/VERSION"):
                version_file.write_text(runtime_version + "\n")
            receipt["runtime_fixture_version"] = runtime_version
            python_code("from pathlib import Path;from vibecrafted_core.vc_frame_staging import materialize_vc_frame_config;"
                        f"materialize_vc_frame_config(Path({str(source_config)!r}),Path({env['VC_FRAME_CONFIG_DIR']!r}),pane_shell='/bin/bash',clipboard_command=None)")
        config = private_config / "config.kdl"
        text = config.read_text()
        for key, value in {"default_shell": '"/bin/bash"', "default_mode": '"normal"',
                           "show_startup_tips": "false", "show_release_notes": "false",
                           "session_serialization": "false", "auto_lock_after_seconds": "0"}.items():
            text = re.sub(r"^" + key + r"\s+[^\n]*$", "", text, flags=re.M) + f"\n{key} {value}\n"
        config.write_text(text)
        layout = private_config / "layouts/operator.kdl"
        text = layout.read_text()
        for start, _, _, end, name in reversed(list(tab_spans(text))):
            if name != "Agents":
                title = "Origin proof" if name == "Launchpad" else name
                body = (f'tab name={json.dumps(title)} {{ pane command={json.dumps(str(python))} '
                        f'{{ args {json.dumps(str(helper))} {json.dumps(title)}; }}; }}')
                text = text[:start] + body + text[end:]
        layout.write_text(text)
        receipt["inputs"] = {str(p): hashlib.sha256(p.read_bytes()).hexdigest()
                             for p in (layout, config, source_config / "vc-agent-workshop.py", Path(__file__))}
        source_layout = private_config / "layouts/source.kdl" if installed else scratch / "source.kdl"
        text = layout.read_text()
        for start, _, _, end, _ in reversed(list(tab_spans(text))):
            text = text[:start] + text[end:]
        end = text.rfind("}")
        text = text[:end] + f'tab name="Source work" {{ pane command={json.dumps(str(python))} {{ args {json.dumps(str(workload))} "source"; }}; }}\n' + text[end:]
        source_layout.write_text(text)
        if args.existing_host:
            # An explicit scenario precondition, never a fallback after an
            # owner refusal. Use the native host entry and inspect its actual
            # serialized layout before any source session or rail request.
            cli("attach", "--create-background", "vc-host")
            known.add("vc-host")

            def host_ready():
                code, dump = run([*base, "--session", "vc-host", "action", "dump-layout"], check=False)
                if code != 0 or not re.search(r'\bframe_host\s+(?:true\b|"true")', dump):
                    return False
                path = out / "pre-existing-host-layout.kdl"
                path.write_text(dump)
                receipt["host_precondition"].update(prepared=True, session="vc-host", layout=str(path),
                                                     layout_sha256=hashlib.sha256(dump.encode()).hexdigest())
                return True

            wait(host_ready, "pre-existing-native-host-ready")
            save("pre-existing-host-ready")
        naked("source")
        c = attach("source")
        wait(lambda: any(row["label"] == "source" for row in rows(workload_log)), "source-workload")
        source_pid = next(row["pid"] for row in rows(workload_log) if row["label"] == "source")
        source_work = workload_guard("source")["terminals"]
        first, other, occupied = project("alpha"), project("beta"), project("occupied")
        initial = open_project(c, first)
        first_seat = initial["session"]
        first_work = [p for p in panes(first_seat) if not p["is_plugin"]]
        open_project(c, first)
        assert c["session"] == first_seat
        # The real Workshop owner and pane argv open harmless tasks twice;
        # workload terminal identity is observed independently of CLI success.
        before = live()
        python_code('''import importlib.util,json,os,subprocess,sys
d=json.load(sys.stdin);os.environ.update(d['identity'])
s=importlib.util.spec_from_file_location('real_workshop',d['workshop']);m=importlib.util.module_from_spec(s);sys.modules[s.name]=m;s.loader.exec_module(m)
for label in ('harmless-agent-1','harmless-agent-2'):
 dest=m.destination_session_for_workspace(d['root'])
 admitted=m.ensure_live_destination(__import__('pathlib').Path(d['root']),dest,d['live'])
 assert admitted.session==d['seat'],admitted
 subprocess.run(m.launch_pane_argv(label,d['root'],[d['python'],d['workload'],label],session=admitted.session),check=True)
''', {"identity": initial["initial"], "workshop": str(private_config / "vc-agent-workshop.py"),
      "root": str(first), "seat": first_seat, "live": sorted(before),
      "python": str(python), "workload": str(workload)})
        wait(lambda: {"harmless-agent-1", "harmless-agent-2"} <= {r["label"] for r in rows(workload_log)}, "two-harmless-agents")
        assert live() == before
        after = {(p["is_plugin"], p["id"]): p for p in panes(first_seat)}
        for pane in first_work:
            current = after[pane["is_plugin"], pane["id"]]
            assert current["tab_id"] == pane["tab_id"] and current.get("pane_command") == pane.get("pane_command")
        for row in rows(workload_log):
            if row["label"].startswith("harmless-agent"):
                assert row["session"] == first_seat and row["cwd"] == str(first) and row["isatty"], row
        open_project(c, other)
        if installed:
            long_project = project("long-project-folder-over-24-characters")
            long_context = open_project(c, long_project)
            assert len(long_context['session']) <= 24, long_context
            pathological = project("-space and back\\slash")
            # Native owner, with no explicit name, must sanitize a legitimate
            # folder which cannot itself be an explicit native session name.
            auto = json.loads(cli("prepare-project", "--root", pathological))
            assert auto['workspace_root'] == str(pathological), auto
            assert auto['session_name'] != pathological.name, auto
            receipt['automatic_names'] = {'long': long_context['session'], 'pathological': auto}
        return_source(c)
        naked("occupied")
        foreign_panes = workload_guard("occupied")
        occupied_context = open_project(c, occupied)
        assert occupied_context["session"] != "occupied", occupied_context
        assert workload_guard("occupied") == foreign_panes
        # Persisted ownership can be stale while the physical incarnation and
        # workload are still live. Inject a valid UUID relation mismatch into
        # the real private ledger, then restore the exact original atomically.
        # A dead old incarnation replaced by a foreign seat is instead covered
        # by guarded_attach_pty and the canonical collision-safe policy.
        stale = project("stale")
        stale_context = open_project(c, stale)
        return_source(c)
        stale_seat = stale_context["session"]
        stale_panes = workload_guard(stale_seat)
        fault = json.loads(python_code('''import base64,hashlib,json,os,pathlib,sys,tempfile
from vibecrafted_core import workspace_catalog as w
e=json.load(sys.stdin);p=w.instance_path(e['VIBECRAFTED_WORKSPACE_INSTANCE_ID'])
original=p.read_bytes();payload=json.loads(original);payload['vibecrafted_session_id']=w.new_uuid7()
injected=(json.dumps(payload,indent=2)+'\\n').encode()
with tempfile.NamedTemporaryFile(dir=p.parent,suffix='.fault',delete=False) as f:
 f.write(injected);temporary=f.name
os.replace(temporary,p)
print(json.dumps(dict(path=str(p),original=base64.b64encode(original).decode(),original_sha256=hashlib.sha256(original).hexdigest(),injected_sha256=hashlib.sha256(injected).hexdigest(),injected_session_id=payload['vibecrafted_session_id'])))
''', stale_context["initial"]))
        receipt["stale_ledger_fault"] = fault
        try:
            open_project(c, stale, refuse=True)
            assert workload_guard(stale_seat) == stale_panes
            os.kill(stale_context["pid"], 0)
        finally:
            python_code('''import base64,hashlib,json,os,pathlib,sys,tempfile
d=json.load(sys.stdin);p=pathlib.Path(d['path']);original=base64.b64decode(d['original'])
unchanged=hashlib.sha256(p.read_bytes()).hexdigest()==d['injected_sha256']
with tempfile.NamedTemporaryFile(dir=p.parent,suffix='.restore',delete=False) as f:
 f.write(original);temporary=f.name
os.replace(temporary,p)
assert hashlib.sha256(p.read_bytes()).hexdigest()==d['original_sha256']
assert unchanged,'owner rewrote inconsistent ledger'
''', fault)
            receipt["stale_ledger_restored"] = True
        # Two actual physical incarnations attributed to one logical project
        # through the catalog's real writer constitute an ambiguous owner.
        ambiguous = project("ambiguous")
        ambiguous_context = open_project(c, ambiguous)
        return_source(c)
        naked("ambiguous-peer")
        python_code('''import json,sys
from vibecrafted_core import workspace_catalog as w
d=json.load(sys.stdin);e=d['identity']
i=w.materialize_instance(workspace_id=e['VIBECRAFTED_WORKSPACE_ID'],root=d['root'],vibecrafted_session_id=w.new_uuid7(),build_id=w.compute_build_id(d['root'],exact=False))
w.record_runtime_session_attachment(workspace_id=i.workspace_id,vibecrafted_session_id=i.vibecrafted_session_id,workspace_instance_id=i.workspace_instance_id,runtime='vc-frame',runtime_session_id='ambiguous-peer',state='live',socket_dir=d['sockets'])
''', {"identity": ambiguous_context["initial"], "root": str(ambiguous), "sockets": str(sockets)})
        open_project(c, ambiguous, refuse=True)
        # The actual vc-host is deliberately ownerless, even when a selected
        # project exists in the same process environment.
        host_live = sorted(live())
        assert "vc-host" in host_live, host_live
        host = json.loads(python_code('''import json,os,sys
from vibecrafted_core import workspace_catalog as w
d=json.load(sys.stdin);os.environ.update(d['identity'])
print(json.dumps(dict(context=w.current_workspace_context(runtime_session_id='vc-host',socket_dir=os.environ['VC_FRAME_SOCKET_DIR'],live_names=d['live']),owner=w.runtime_session_owner_root('vc-host',os.environ['VC_FRAME_SOCKET_DIR'],runtime_state='live'))))
''', {"identity": initial["initial"], "live": host_live}))
        assert host == {"context": {}, "owner": None}, host
        # Explicit selection reaches the same owner with exact retained KDL;
        # a later request with different creation input must reuse its panes.
        selected = project("selected-layout")
        selected_text = layout.read_text().replace('name="Origin proof"', 'name="Selected custom proof"')
        selected_file = projects / "selected-input.kdl"
        selected_file.write_text(selected_text)
        swap_text = 'swap_tiled_layout name="Retained swap" { tab max_panes=2 { pane split_direction="vertical" { children; }; }; }\n'
        selected_file.with_suffix('.swap.kdl').write_text(swap_text)
        selected_json = json.dumps({"File": [str(selected_file), {"tabs": [], "creation_time": "", "update_time": ""}]})
        admitted = json.loads(cli("prepare-project", "--root", selected,
                                  "--requested-session", "selected-custom", "--layout-json", selected_json))
        assert admitted["session_name"] == "selected-custom", admitted
        selected_panes = panes(admitted["session_name"])
        if installed:
            published = [p for p in (private_config / 'layouts').glob('*.kdl') if p.read_text() == selected_text]
            assert published, 'selected bytes never reached approved product layouts'
            assert any(p.with_suffix('.swap.kdl').read_text() == swap_text for p in published), 'swap bytes changed'
            receipt['selected_published'] = [str(p) for p in published]
        assert any(p.get("tab_name") == "Selected custom proof" for p in selected_panes), selected_panes
        wait(lambda: any(row["cwd"] == str(selected) for row in rows(context_log)), "selected-layout-context")
        selected_context = next(row for row in rows(context_log) if row["cwd"] == str(selected))
        assert selected_context["initial"] == selected_context["recovered"], selected_context
        assert all(selected_context["initial"].get(key) for key in WES_KEYS), selected_context
        selected_guard = workload_guard(admitted["session_name"])
        before = live()
        reused = json.loads(cli("prepare-project", "--root", selected,
                                "--requested-session", "selected-custom", "--layout-json",
                                json.dumps({"Stringified": 'layout { pane; }'})))
        assert reused == admitted, (admitted, reused)
        assert live() == before and workload_guard(admitted["session_name"]) == selected_guard
        receipt["selected_layout"] = {"admission": admitted, "context": selected_context,
                                      "panes": selected_panes, "reuse_preserved": True}
        os.kill(source_pid, 0)
        assert workload_guard("source")["terminals"] == source_work
        for row in rows(workload_log):
            os.kill(row["pid"], 0)
        if installed:
            assert runtime_snapshot(runtime) == installed['snapshot'], 'RuntimePack mutated on startup'
            receipt['installed_runtime']['generation_unchanged'] = True
        receipt.update(status="passed", vc_host=host, workloads=rows(workload_log), live=sorted(live()))
        save("final")
    except BaseException as exc:
        receipt.update(status="failed", error=f"{type(exc).__name__}: {exc}")
        save("failure")
        raise
    finally:
        # Discover only the private socket namespace, never a global process
        # inventory. Exact fixture session teardown precedes frontend cleanup.
        try:
            known.update(live())
        except (AssertionError, OSError):
            pass
        for session in sorted(known):
            try:
                cli("kill-session", session, check=False)
                receipt["cleanup"].append(session)
            except (AssertionError, OSError):
                receipt["cleanup"].append({"session": session, "failed": True})
        for c in clients:
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
