#!/usr/bin/env python3
"""Rail + actual Strider -> selected-runtime owner, in disposable native PTYs.

python3 zellij-server/tests/rail_project_owner_pty.py --binary /candidate/vc-frame \
    --runtime-root /selected/vibecrafted --output /new/receipt-directory

Requires pyte and the selected runtime's Python dependencies. Never builds,
installs, opens a desktop window, launches a provider or uses live config.
The copied product layout keeps the real Agents scene; only Launchpad/Shell
workloads become harmless context observers. No filepicker replies are forged.
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

WES_KEYS = ("VIBECRAFTED_WORKSPACE_ROOT", "VIBECRAFTED_WORKSPACE_ID",
            "VIBECRAFTED_SESSION_ID", "VIBECRAFTED_WORKSPACE_INSTANCE_ID",
            "VIBECRAFTED_BUILD_ID")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "runtime-root", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--python", default=sys.executable, type=Path)
    parser.add_argument("--timeout", type=float, default=45)
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
            destination.write_text("#!/bin/sh\nexec " + shlex.quote(str(target)) + ' "$@"\n')
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
               "commands": [], "steps": [], "cleanup": [],
               "limits": "Private source-runtime PTYs; no installed app, desktop keys, provider or Apple delivery."}
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

    base = [binary, "--config", private_config / "config.kdl", "--config-dir", private_config]

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
        wait(lambda: any(text in line[:24] for line in c["screen"].display), "rail-" + text)
        y = next(i for i, line in enumerate(c["screen"].display) if text in line[:24])
        click(c, 4, y)

    def picker(c, project):
        rail(c, "+ Open")
        wait(lambda: any(p["is_plugin"] and "strider" in str(p.get("plugin_url")) for p in panes(c["session"])), "actual-strider")
        # Every fresh picker inherits this source session's projects cwd.
        # Enter a uniquely named real directory, then choose that directory.
        send(c, project.name.encode())
        wait(lambda: project.name in "\n".join(c["screen"].display), "picker-search-" + project.name)
        send(c, b"\r")
        wait(lambda: str(project) in "\n".join(c["screen"].display), "picker-directory-" + project.name)
        send(c, b"\x1b")
        send(c, b"\r")
        wait(lambda: not any(p["is_plugin"] and "strider" in str(p.get("plugin_url")) for p in panes(c["session"])), "picker-closed")

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
        picker(c, project)
        if refuse:
            wait(lambda: any(word in "\n".join(c["screen"].display).lower()
                             for word in ("refus", "ambiguous", "stale", "failed", "conflict", "could not")), "refusal")
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

    def project(name):
        path = projects / name
        path.mkdir()
        run(["git", "init", "--quiet", path])
        return path

    try:
        receipt["build_info"] = verify_build_info(cli("--build-info"))
        assert receipt["build_info"]["version"] == "4.5.0", receipt["build_info"]
        # A real, disposable Git root selects the documented source developer
        # facade without manufacturing an installed Runtime Pack admission.
        run(["git", "init", "--quiet", runtime])
        receipt["runtime_source_sha"] = run(["git", "-C", source_runtime, "rev-parse", "HEAD"])[1].strip()
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
        source_layout = scratch / "source.kdl"
        text = layout.read_text()
        for start, _, _, end, _ in reversed(list(tab_spans(text))):
            text = text[:start] + text[end:]
        end = text.rfind("}")
        text = text[:end] + f'tab name="Source work" {{ pane command={json.dumps(str(python))} {{ args {json.dumps(str(workload))} "source"; }}; }}\n' + text[end:]
        source_layout.write_text(text)
        naked("source")
        c = attach("source")
        wait(lambda: any(row["label"] == "source" for row in rows(workload_log)), "source-workload")
        source_pid = next(row["pid"] for row in rows(workload_log) if row["label"] == "source")
        source_work = [p for p in panes("source") if not p["is_plugin"]]
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
        return_source(c)
        naked("occupied")
        foreign_panes = panes("occupied")
        occupied_context = open_project(c, occupied)
        assert occupied_context["session"] != "occupied", occupied_context
        assert panes("occupied") == foreign_panes
        # Real stale incarnation: retain catalog evidence, replace only this
        # fixture's seat with an unowned native seat at the same name.
        stale = project("stale")
        stale_context = open_project(c, stale)
        return_source(c)
        stale_seat = stale_context["session"]
        cli("--session", stale_seat, "action", "save-session")
        cli("kill-session", stale_seat)
        cli("delete-session", stale_seat, check=False)
        naked(stale_seat)
        stale_panes = panes(stale_seat)
        open_project(c, stale, refuse=True)
        assert panes(stale_seat) == stale_panes
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
        host = json.loads(python_code('''import json,os
from vibecrafted_core import workspace_catalog as w
print(json.dumps(dict(context=w.current_workspace_context(runtime_session_id='vc-host',socket_dir=os.environ['VC_FRAME_SOCKET_DIR'],live_names=['vc-host']),owner=w.runtime_session_owner_root('vc-host',os.environ['VC_FRAME_SOCKET_DIR'],runtime_state='live'))))
'''))
        assert host == {"context": {}, "owner": None}, host
        # Explicit selection reaches the same owner with exact retained KDL;
        # a later request with different creation input must reuse its panes.
        selected = project("selected-layout")
        selected_text = layout.read_text().replace('name="Origin proof"', 'name="Selected custom proof"')
        selected_json = json.dumps({"Stringified": selected_text})
        admitted = json.loads(cli("prepare-project", "--root", selected,
                                  "--requested-session", "selected-custom", "--layout-json", selected_json))
        assert admitted["session_name"] == "selected-custom", admitted
        selected_panes = panes(admitted["session_name"])
        assert any(p.get("tab_name") == "Selected custom proof" for p in selected_panes), selected_panes
        wait(lambda: any(row["cwd"] == str(selected) for row in rows(context_log)), "selected-layout-context")
        selected_context = next(row for row in rows(context_log) if row["cwd"] == str(selected))
        assert selected_context["initial"] == selected_context["recovered"], selected_context
        assert all(selected_context["initial"].get(key) for key in WES_KEYS), selected_context
        before = live()
        reused = json.loads(cli("prepare-project", "--root", selected,
                                "--requested-session", "selected-custom", "--layout-json",
                                json.dumps({"Stringified": 'layout { pane; }'})))
        assert reused == admitted, (admitted, reused)
        assert live() == before and panes(admitted["session_name"]) == selected_panes
        receipt["selected_layout"] = {"admission": admitted, "context": selected_context,
                                      "panes": selected_panes, "reuse_preserved": True}
        os.kill(source_pid, 0)
        assert [p for p in panes("source") if not p["is_plugin"]] == source_work
        for row in rows(workload_log):
            os.kill(row["pid"], 0)
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
