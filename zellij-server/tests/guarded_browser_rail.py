#!/usr/bin/env python3
"""Installed-shaped product rail -> actual Strider -> guarded browser transition.

Uses the shared private Runtime Pack materializer, genuine wrapper/libexec engine,
canonical prepare owner and real headless Chromium. Never drives the desktop.
Needs Chromium, websockets and the selected Runtime Pack Python prerequisites.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

from guarded_browser import CDP, free_port, refused_guarded_terminal, tabs_when_ready
from peer_sessions_pty import parse_client_memberships, tab_spans


async def main(args):
    import websockets
    from installed_runtime_fixture import materialize_installed_runtime, runtime_snapshot, thaw_private_runtime

    binary = args.binary.resolve(strict=True)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix="vcwrail-", dir="/tmp")).resolve()
    projects = scratch / "projects"
    projects.mkdir()
    project = projects / "browser-project"
    project.mkdir()
    subprocess.run(["git", "init", "--quiet", project], check=True)
    events = out / "workloads.jsonl"
    observer = scratch / "observer.py"
    observer.write_text('''import json,os,sys,time
with open(%r,'a') as f:f.write(json.dumps(dict(pid=os.getpid(),ppid=os.getppid(),cwd=os.getcwd(),session=os.environ.get('ZELLIJ_SESSION_NAME'),identity={k:v for k,v in os.environ.items() if k.startswith('VIBECRAFTED_WORKSPACE_')}))+'\\n')
print('BROWSER WORKLOAD',flush=True)
while True: time.sleep(1)
''' % str(events))

    def mutate(config_dir, fixture_env, runtime):
        config_dir = Path(config_dir)
        config = config_dir / "config.kdl"
        text = config.read_text()
        for key, value in dict(default_shell='"/bin/bash"', default_mode='"normal"',
                               show_startup_tips="false", show_release_notes="false",
                               session_serialization="false", auto_lock_after_seconds="0",
                               web_sharing='"on"', web_server="false").items():
            text = re.sub(r"^" + key + r"\s+[^\n]*$", "", text, flags=re.M) + f"\n{key} {value}\n"
        config.write_text(text)
        layout = config_dir / "layouts/browser-source.kdl"
        text = (config_dir / "layouts/operator.kdl").read_text()
        # Keep the full session canvas and actual Agents. Only the other tab
        # programs become harmless observers inside this disposable fixture.
        for start, _, _, end, name in reversed(list(tab_spans(text))):
            if name != "Agents":
                label = "Source work" if name == "Launchpad" else name
                text = text[:start] + f'tab name={json.dumps(label)} {{ pane command={json.dumps(fixture_env["VIBECRAFTED_PYTHON"])} {{ args {json.dumps(str(observer))}; }}; }}' + text[end:]
        layout.write_text(text)

    try:
        pack = materialize_installed_runtime(args.runtime_root.resolve(strict=True), binary,
                                             args.python.resolve(strict=True), scratch, config_mutator=mutate, require_clean=not args.allow_dirty_runtime)
    except BaseException as error:
        (out / "receipt.json").write_text(json.dumps(dict(status="failed", stage="fixture-bootstrap",
            error=f"{type(error).__name__}: {error}", native_started=False, browser_started=False), indent=2) + "\n")
        thaw_private_runtime(scratch)
        shutil.rmtree(scratch)
        raise
    env, entry = pack["env"], Path(pack["entry"])
    config_dir = Path(env["VC_FRAME_CONFIG_DIR"])
    base = [str(entry)]
    receipt = dict(status="running", binary=str(binary),
                   binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                   fixture_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                   runtime=str(pack["runtime"]), runtime_source_sha=pack["source_sha"], runtime_source_clean=pack["source_clean"],
                   runtime_stamp=pack["runtime_stamp"],
                   runtime_resolution=pack["runtime_resolution"], donor=pack["donor"],
                   materialization_commands=pack["materialization_commands"],
                   entry=str(entry), engine=str(pack["engine"]),
                   commands=[], cases=[], limits="Private materialized installed-shaped Runtime Pack; no signed installed app, desktop, provider or Windows runtime acceptance.")
    children, logs, owned_sessions = [], [], {"browser-source", "vc-host", "browser-project"}

    def cli(*parts, check=True):
        argv = [*base, *map(str, parts)]
        result = subprocess.run(argv, cwd=projects, env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True, timeout=30)
        receipt["commands"].append(dict(argv=argv, exit=result.returncode, output=result.stdout))
        if check:
            assert result.returncode == 0, (argv, result.stdout)
        return result.stdout

    def inventory(session):
        panes = json.loads(cli("--session", session, "action", "list-panes", "--all", "--json", "--command"))
        # Startup workloads can move or hide their cursor without any attach.
        # Preserve geometry, identities and commands, not transient cursor state.
        for pane in panes:
            pane.pop("cursor_coordinates_in_pane", None)
        return dict(members=parse_client_memberships(cli("--session", session, "action", "list-clients")),
                    panes=panes,
                    tabs=json.loads(cli("--session", session, "action", "list-tabs", "--all", "--json")))

    def spawn(argv, name):
        log = (out / f"{name}.log").open("wb")
        logs.append(log)
        process = subprocess.Popen(argv, cwd=projects, env=env, stdout=log,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        children.append(process)
        return process

    async def wait(check, label):
        end = time.monotonic() + args.timeout
        while time.monotonic() < end:
            result = check()
            if asyncio.iscoroutine(result):
                result = await result
            if result:
                return result
            await asyncio.sleep(.05)
        raise AssertionError(label)

    def http(url):
        try:
            raw = urllib.request.urlopen(url, timeout=1).read().decode()
            try:
                return json.loads(raw)
            except ValueError:
                return raw
        except OSError:
            return None

    try:
        receipt["build_info"] = json.loads(cli("--build-info"))
        assert receipt["build_info"]["version"] == "4.5.0" and receipt["build_info"]["git_dirty"] is False
        cli("--layout", config_dir / "layouts/browser-source.kdl", "attach", "--create-background", "browser-source")
        await wait(lambda: events.exists() and events.read_text().strip(), "source-workload")
        original_workloads = [json.loads(line) for line in events.read_text().splitlines()]
        source_terminals = [p for p in inventory("browser-source")["panes"] if not p["is_plugin"]]
        token = cli("web", "--create-token", "--token-name", "private-rail-browser").strip().split(": ", 1)[1]
        port, debug_port = free_port(), free_port()
        origin = f"http://127.0.0.1:{port}"
        spawn([*base, "web", "--port", str(port)], "web-server")
        await wait(lambda: http(origin + "/info/version"), "web-listener")
        spawn([str(args.chromium), "--headless=new", "--no-first-run", "--no-default-browser-check",
               "--disable-gpu", "--disable-background-networking", "--no-proxy-server",
               "--no-sandbox", "--password-store=basic", "--use-mock-keychain",
               f"--user-data-dir={scratch / 'browser-profile'}", f"--remote-debugging-port={debug_port}",
               "--window-size=1600,1000", "about:blank"], "chromium")
        pages = await wait(lambda: http(f"http://127.0.0.1:{debug_port}/json/list"), "headless-browser")
        page = next(item for item in pages if item["type"] == "page")
        async with websockets.connect(page["webSocketDebuggerUrl"], max_size=16 * 1024 * 1024) as ws:
            cdp = CDP(ws)
            reader = asyncio.create_task(cdp.read())
            try:
                await cdp.call("Page.enable")
                await cdp.call("Network.enable")
                await cdp.call("Page.navigate", url=origin + "/info/version")
                await wait(lambda: cdp.evaluate("location.origin === " + json.dumps(origin)), "browser-origin")
                assert await cdp.evaluate("fetch('/command/login',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({auth_token:" + json.dumps(token) + ",remember_me:true})}).then(r=>r.ok)")
                await cdp.call("Page.navigate", url=origin + "/browser-source")
                await wait(lambda: bool(inventory("browser-source")["members"]), "browser-source-attached")

                async def screen():
                    return await cdp.evaluate("window.term ? Array.from({length:term.rows},(_,y)=>term.buffer.active.getLine(y)?.translateToString()||'') : []")

                async def visible(text):
                    return any(text in line for line in await screen())

                async def cell_click(x, y):
                    point = await cdp.evaluate(f"(()=>{{const r=term.element.querySelector('.xterm-screen').getBoundingClientRect(); const c=term._core._renderService.dimensions.css.cell;return {{x:r.left+({x}+.5)*c.width,y:r.top+({y}+.5)*c.height}};}})()")
                    for kind in ("mousePressed", "mouseReleased"):
                        await cdp.call("Input.dispatchMouseEvent", type=kind, button="left", clickCount=1, **point)

                async def key(key, code, virtual, text=None):
                    packet = dict(key=key, code=code, windowsVirtualKeyCode=virtual, nativeVirtualKeyCode=virtual)
                    if text is not None:
                        packet["text"] = text
                    await cdp.call("Input.dispatchKeyEvent", type="keyDown", **packet)
                    packet.pop("text", None)
                    await cdp.call("Input.dispatchKeyEvent", type="keyUp", **packet)

                await wait(lambda: visible("Source work"), "source-tab-visible")
                await cell_click(4, 1)
                stable = dict(signature=None, since=0, row=None)

                async def door_ready():
                    rows = await screen()
                    doors = [i for i, line in enumerate(rows) if "+ Open" in line[:24]]
                    focused = [p for p in inventory("browser-source")["panes"] if p["is_plugin"] and p["is_focused"] and p.get("plugin_url") == "session-manager"]
                    if len(doors) != 1 or len(focused) != 1:
                        stable["signature"] = None
                        return False
                    signature = (doors[0], tuple(line[:24] for line in rows[:doors[0] + 1]))
                    if signature != stable["signature"]:
                        stable.update(signature=signature, since=time.monotonic(), row=doors[0])
                        return False
                    return time.monotonic() - stable["since"] >= 1

                await wait(door_ready, "stable-focused-real-rail")
                await cell_click(4, stable["row"])
                await wait(lambda: visible(project.name), "actual-strider-directory-list")
                await cdp.call("Input.insertText", text=project.name)
                await wait(lambda: visible("FIND: " + project.name), "actual-strider-search")
                await key("Enter", "Enter", 13, "\r")

                async def directory_ready():
                    rows = await screen()
                    return (any("PATH: " + str(project) in line for line in rows)
                            and any("FIND: _" in line for line in rows))

                # Strider clears its search when it enters the real directory.
                # Do not send ESC immediately before Enter: the terminal's
                # deliberate bare-ESC parse window would turn that into Alt+Enter.
                await wait(directory_ready, "actual-strider-selected-directory")
                browser_document = await cdp.evaluate("performance.timeOrigin")
                start = len(cdp.events)
                await key("Enter", "Enter", 13, "\r")
                url = await wait(lambda: cdp.evaluate("location.pathname !== '/browser-source' && new URL(location.href).searchParams.has('expected_session_incarnation') ? location.href : null"), "guarded-rail-browser-transition")
                parsed = urllib.parse.urlparse(url)
                session = urllib.parse.unquote(parsed.path.rsplit("/", 1)[1])
                owned_sessions.add(session)
                expected = urllib.parse.parse_qs(parsed.query)["expected_session_incarnation"][0]
                assert await cdp.evaluate("document.title") == session
                admitted = inventory(session)
                assert {tab["session_incarnation"] for tab in admitted["tabs"]} == {expected}, admitted
                assert len(admitted["members"]) == 1 and not inventory("browser-source")["members"], admitted
                assert await cdp.evaluate("performance.timeOrigin") == browser_document, "guarded transition reloaded browser"
                assert not [event for event in cdp.events[start:] if event.get("method") == "Network.webSocketCreated"], "guarded transition replaced IPC with a fresh browser transport"
                controls = [json.loads(event["params"]["response"]["payloadData"]) for event in cdp.events[start:] if event.get("method") == "Network.webSocketFrameReceived" and event["params"]["response"].get("opcode") == 1 and event["params"]["response"].get("payloadData", "").startswith('{"type":"SwitchedSession"')]
                assert any(message.get("expected_session_incarnation") == expected and message.get("new_session_name") == session for message in controls), controls
                receipt["cases"].append(dict(case="installed-real-rail-guarded-browser-transition", url=url, admitted=admitted, controls=controls))
                current_source = {p["id"]: p for p in inventory("browser-source")["panes"] if not p["is_plugin"]}
                for pane in source_terminals:
                    assert current_source[pane["id"]]["pane_command"] == pane["pane_command"]
                for row in original_workloads:
                    os.kill(row["pid"], 0)
                cli("kill-session", session)
                await wait(lambda: session not in cli("list-sessions", "--no-formatting", check=False), "old-admitted-seat-dead")
                cli("--layout", config_dir / "layouts/operator.kdl", "attach", "--create-background", session)
                replacement = await wait(lambda: (tabs := tabs_when_ready(cli, session)) and tabs[0]["session_incarnation"] != expected and tabs[0]["session_incarnation"], "replacement-seat-ready")
                before = inventory(session)
                assert not before["members"]
                start = len(cdp.events)
                await cdp.call("Page.reload", ignoreCache=True)
                await wait(lambda: refused_guarded_terminal(cdp.events, start, expected), "guarded-reload-refused")
                assert inventory(session) == before, "same-name replacement was touched by stale browser guard"
                terminal_urls = [event["params"]["url"] for event in cdp.events[start:] if event.get("method") == "Network.webSocketCreated" and "/ws/terminal" in event["params"]["url"]]
                assert terminal_urls and all(urllib.parse.parse_qs(urllib.parse.urlparse(value).query).get("expected_session_incarnation") == [expected] for value in terminal_urls), terminal_urls
                assert await cdp.evaluate("location.href") == url
                receipt["cases"].append(dict(case="installed-guarded-transition-then-replacement-reload-refused", original=expected, replacement=replacement, terminal_urls=terminal_urls, unchanged_inventory=before))
                for row in original_workloads:
                    os.kill(row["pid"], 0)
                assert runtime_snapshot(pack["runtime"]) == pack["snapshot"], "startup mutated immutable carried Runtime Pack"
                receipt["runtime_generation_unchanged"] = True
                receipt["status"] = "passed"
            finally:
                receipt["browser_events"] = cdp.events
                receipt["final_screen"] = await cdp.evaluate("window.term ? Array.from({length:term.rows},(_,y)=>term.buffer.active.getLine(y)?.translateToString()||'') : []")
                reader.cancel()
    except BaseException as error:
        receipt.update(status="failed", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        # Stop browser/web ingress before killing the native fixture seats.
        for process in reversed(children):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        for session in owned_sessions:
            try:
                cli("kill-session", session, check=False)
            except (OSError, subprocess.TimeoutExpired):
                pass
        for log in logs:
            log.close()
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        # Unseal only the disposable carried generation after all native/browser
        # processes have exited, for deletion. Startup never writes this tree.
        runtime = Path(pack["runtime"])
        assert runtime.is_relative_to(scratch)
        thaw_private_runtime(scratch)
        shutil.rmtree(scratch)
    print(json.dumps(dict(status=receipt["status"], receipt=str(out / "receipt.json"))))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binary", "runtime-root", "python", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--chromium", type=Path, default=Path(shutil.which("chromium") or "chromium"))
    parser.add_argument("--timeout", type=float, default=130)
    parser.add_argument("--allow-dirty-runtime", action="store_true", help="Diagnostic only; never final acceptance")
    asyncio.run(main(parser.parse_args()))
