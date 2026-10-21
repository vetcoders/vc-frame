#!/usr/bin/env python3
"""Real headless Chromium -> web listener -> native lifetime guard acceptance.

Needs Chromium and Python websockets; uses its own browser profile, native socket
namespace and HOME. No desktop UI, provider, installed app or production state.
The same URL is refreshed after a same-name native replacement. An unguarded
ordinary URL is a control; no browser WebSocket or server result is substituted.
"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.parse
import urllib.request

from peer_sessions_pty import parse_client_memberships


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def refused_guarded_terminal(events, start, expected):
    """Wait for the new guarded transport, not the reload's old socket close."""
    attempts = {
        event["params"]["requestId"]: event["params"]["url"]
        for event in events[start:]
        if event.get("method") == "Network.webSocketCreated"
        and "/ws/terminal" in event["params"]["url"]
    }
    for url in attempts.values():
        assert urllib.parse.parse_qs(urllib.parse.urlparse(url).query).get(
            "expected_session_incarnation") == [expected], url
    closed = {
        event["params"]["requestId"] for event in events[start:]
        if event.get("method") == "Network.webSocketClosed"
    }
    return bool(attempts) and all(request in closed for request in attempts)


def tabs_when_ready(cli, session):
    # --create-background returns before cold Screen/plugin initialization.
    # Poll its real inventory contract; do not treat a not-ready CLI response
    # as a lifetime failure or manufacture an empty successful inventory.
    output = cli("--session", session, "action", "list-tabs", "--all", "--json", check=False)
    try:
        tabs = json.loads(output)
    except json.JSONDecodeError:
        return None
    return tabs if isinstance(tabs, list) and tabs else None


class CDP:
    def __init__(self, ws):
        self.ws, self.serial, self.pending, self.events = ws, 0, {}, []

    async def read(self):
        async for raw in self.ws:
            packet = json.loads(raw)
            if "id" in packet:
                self.pending.pop(packet["id"]).set_result(packet)
            else:
                self.events.append(packet)

    async def call(self, method, **params):
        self.serial += 1
        serial = self.serial
        future = asyncio.get_running_loop().create_future()
        self.pending[serial] = future
        await self.ws.send(json.dumps(dict(id=serial, method=method, params=params)))
        packet = await asyncio.wait_for(future, 15)
        assert "error" not in packet, packet
        return packet.get("result", {})

    async def evaluate(self, expression):
        result = await self.call("Runtime.evaluate", expression=expression,
                                 returnByValue=True, awaitPromise=True)
        assert "exceptionDetails" not in result, result
        return result.get("result", {}).get("value")


async def main(args):
    import websockets
    binary = args.binary.resolve(strict=True)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix="vcweb-", dir="/tmp")).resolve()
    env = {key: os.environ[key] for key in ("PATH", "USER", "LOGNAME", "LANG") if key in os.environ}
    env.update(TERM="xterm-256color", SHELL="/bin/sh", VC_FRAME_SERVER_FOREGROUND="1")
    for key in ("HOME", "TMPDIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                "XDG_STATE_HOME", "XDG_RUNTIME_DIR", "VIBECRAFTED_HOME", "VC_FRAME_CONFIG_DIR"):
        path = scratch / key.lower()
        path.mkdir()
        env[key] = str(path)
    sockets = scratch / "s"
    sockets.mkdir()
    env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets),
               VIBECRAFTED_CONTROL_PLANE=str(scratch / "disabled-control-plane.json"))
    config = scratch / "config.kdl"
    config.write_text('default_shell "/bin/sh"\ndefault_mode "normal"\n'
                      'show_startup_tips false\nshow_release_notes false\n'
                      'session_serialization false\nauto_lock_after_seconds 0\n'
                      'web_sharing "on"\nweb_server false\npane_frames false\n')
    base = [str(binary), "--config", str(config), "--config-dir", env["VC_FRAME_CONFIG_DIR"]]
    receipt = dict(status="running", binary=str(binary),
                   binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
                   fixture_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                   commands=[], cases=[], limits="Private headless Chromium and raw native engine; this fixture does not certify Runtime Pack wrappers.")
    children, sessions = [], {"web-target", "web-other", "guarded-missing"}
    log_files = []

    def cli(*parts, check=True):
        argv = [*base, *map(str, parts)]
        result = subprocess.run(argv, cwd=scratch, env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, text=True, timeout=30)
        receipt["commands"].append(dict(argv=argv, exit=result.returncode, output=result.stdout))
        if check:
            assert result.returncode == 0, (argv, result.stdout)
        return result.stdout

    def spawn(argv, name):
        log = (out / f"{name}.log").open("wb")
        log_files.append(log)
        process = subprocess.Popen(argv, cwd=scratch, env=env, stdout=log,
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

    def create(label, session="web-target"):
        layout = scratch / (label + ".kdl")
        layout.write_text('layout { pane command="/bin/sh" { args "-c" ' + json.dumps(
            f"printf 'READY:{label}\\n'; while read line; do printf 'ACK:{label}:%s\\n' \"$line\"; done") + '; }; }\n')
        cli("--layout", layout, "attach", "--create-background", session)

    def inventory(session="web-target"):
        return dict(members=parse_client_memberships(cli("--session", session, "action", "list-clients")),
                    panes=json.loads(cli("--session", session, "action", "list-panes", "--all", "--json", "--command")),
                    tabs=json.loads(cli("--session", session, "action", "list-tabs", "--all", "--json")))

    def incarnation():
        tabs = tabs_when_ready(cli, "web-target")
        if tabs is None:
            return None
        tokens = {row["session_incarnation"] for row in tabs}
        return tokens.pop() if len(tokens) == 1 else None

    def live_members():
        return inventory()["members"]

    def http_json(url):
        try:
            raw = urllib.request.urlopen(url, timeout=1).read().decode()
            try:
                return json.loads(raw)
            except ValueError:
                return raw
        except (OSError, ValueError):
            return None

    try:
        create("original")
        original = await wait(incarnation, "original-native-ready")
        token_output = cli("web", "--create-token", "--token-name", "private-browser")
        token = token_output.strip().split(": ", 1)[1]
        port, devtools = free_port(), free_port()
        origin = f"http://127.0.0.1:{port}"
        spawn([*base, "web", "--port", str(port)], "web-server")
        await wait(lambda: http_json(origin + "/info/version") is not None, "web-listener-ready")
        spawn([str(args.chromium), "--headless=new", "--no-first-run", "--no-default-browser-check",
               "--disable-gpu", "--disable-background-networking", "--no-proxy-server",
               f"--user-data-dir={scratch / 'browser-profile'}", f"--remote-debugging-port={devtools}",
               "--window-size=1280,800", "about:blank"], "chromium")
        pages = await wait(lambda: http_json(f"http://127.0.0.1:{devtools}/json/list"), "headless-browser-ready")
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
                missing_url = origin + "/guarded-missing?" + urllib.parse.urlencode(dict(expected_session_incarnation=original))
                start = len(cdp.events)
                await cdp.call("Page.navigate", url=missing_url)
                await wait(lambda: refused_guarded_terminal(cdp.events, start, original), "missing-guarded-seat-refused")
                assert "guarded-missing" not in cli("list-sessions", "--no-formatting", check=False)
                assert await cdp.evaluate("location.href") == missing_url
                receipt["cases"].append(dict(case="guarded-missing-seat-never-created", url=missing_url))
                guarded_url = origin + "/web-target?" + urllib.parse.urlencode(dict(expected_session_incarnation=original))
                await cdp.call("Page.navigate", url=guarded_url)
                await wait(lambda: bool(live_members()), "exact-browser-attachment")
                await wait(lambda: cdp.evaluate("document.querySelector('.xterm-screen') !== null"), "real-xterm-ready")
                exact = inventory()
                assert len(exact["members"]) == 1, exact
                await wait(lambda: cdp.evaluate("location.href") , "browser-url")
                assert await cdp.evaluate("new URL(location.href).searchParams.get('expected_session_incarnation')") == original
                receipt["cases"].append(dict(case="exact-incarnation-browser", incarnation=original, inventory=exact))
                cli("kill-session", "web-target")
                await wait(lambda: "web-target" not in cli("list-sessions", "--no-formatting", check=False), "original-stopped")
                create("replacement")
                replacement = await wait(lambda: incarnation() != original and incarnation(), "replacement-native-ready")
                before = inventory()
                assert not before["members"], before
                start = len(cdp.events)
                await cdp.call("Page.reload", ignoreCache=True)
                await wait(lambda: refused_guarded_terminal(cdp.events, start, original), "replacement-browser-refusal")
                assert inventory() == before, "old guarded browser mutated replacement membership/config/geometry"
                terminal_urls = [event["params"]["url"] for event in cdp.events[start:]
                                 if event.get("method") == "Network.webSocketCreated" and "/ws/terminal" in event["params"]["url"]]
                assert terminal_urls and all(urllib.parse.parse_qs(urllib.parse.urlparse(url).query).get("expected_session_incarnation") == [original] for url in terminal_urls), terminal_urls
                assert await cdp.evaluate("location.href") == guarded_url
                receipt["cases"].append(dict(case="same-name-replacement-refresh-refused", original=original, replacement=replacement,
                                             terminal_urls=terminal_urls, unchanged_inventory=before))
                # A plain URL still intentionally selects the currently live seat.
                await cdp.call("Page.navigate", url=origin + "/web-target")
                await wait(lambda: len(live_members()) == 1, "ordinary-browser-control")
                assert incarnation() == replacement
                receipt["cases"].append(dict(case="ordinary-unguarded-browser-control", inventory=inventory()))
                create("ordinary-destination", "web-other")
                await wait(lambda: tabs_when_ready(cli, "web-other"), "ordinary-destination-ready")
                original_document = await cdp.evaluate("performance.timeOrigin")
                start = len(cdp.events)
                client_id = next(iter(live_members()))
                cli("--session", "web-target", "--client-id", client_id, "action", "switch-session", "web-other")
                await wait(lambda: len(inventory("web-other")["members"]) == 1 and not live_members(), "ordinary-native-browser-switch")
                await wait(lambda: cdp.evaluate("location.pathname === '/web-other'"), "ordinary-history-updated")
                assert await cdp.evaluate("document.title") == "web-other"
                assert await cdp.evaluate("performance.timeOrigin") == original_document, "session switch reloaded instead of retaining admitted connection"
                assert await cdp.evaluate("new URL(location.href).searchParams.has('expected_session_incarnation')") is False
                assert not [event for event in cdp.events[start:] if event.get("method") == "Network.webSocketCreated"], "session transition opened a fresh unguarded transport"
                await cdp.evaluate("term.focus()")
                await cdp.call("Input.insertText", text="transport-retained")
                await cdp.call("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13, text="\r")
                await cdp.call("Input.dispatchKeyEvent", type="keyUp", key="Enter", code="Enter", windowsVirtualKeyCode=13)
                await wait(lambda: cdp.evaluate("Array.from({length:term.rows},(_,y)=>term.buffer.active.getLine(y)?.translateToString()||'').some(line=>line.includes('ACK:ordinary-destination:transport-retained'))"), "retained-browser-ipc-roundtrip")
                receipt["cases"].append(dict(case="ordinary-transition-retains-browser-and-ipc", inventory=inventory("web-other")))
                receipt["browser_events"] = cdp.events
                receipt["status"] = "passed"
            finally:
                receipt["browser_events"] = cdp.events
                reader.cancel()
    except BaseException as error:
        receipt.update(status="failed", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        # Stop browser/web ingress before killing seats: a failing old client
        # can still be creating its unguarded replacement when a test aborts.
        for process in reversed(children):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
        for session in sessions:
            try:
                cli("kill-session", session, check=False)
            except (OSError, subprocess.TimeoutExpired):
                pass
        for log in log_files:
            log.close()
        (out / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        shutil.rmtree(scratch)
    print(json.dumps(dict(status=receipt["status"], receipt=str(out / "receipt.json"))))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--chromium", type=Path, default=Path(shutil.which("chromium") or "chromium"))
    parser.add_argument("--timeout", type=float, default=30)
    asyncio.run(main(parser.parse_args()))
