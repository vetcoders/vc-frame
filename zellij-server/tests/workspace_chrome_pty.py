"""F12: real compact-bar -> MessageFromPlugin -> projection owner, private PTYs.

uv run --with pyte python zellij-server/tests/workspace_chrome_pty.py \
  --binary target/debug/vc-frame --output /absolute/new-receipt-directory
No actions target the user's sessions. HOME and installation are preserved.
"""
import argparse
import codecs
import fcntl
import json
import os
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
from pathlib import Path

import pyte


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='vcf12-', dir='/tmp')).resolve()
    env = {key: os.environ[key] for key in ('PATH', 'HOME', 'USER', 'LOGNAME', 'LANG') if key in os.environ}
    env.update(TERM='xterm-256color', SHELL='/bin/sh')
    for key in ('TMPDIR', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME',
                'XDG_RUNTIME_DIR', 'VIBECRAFTED_HOME', 'VIBECRAFTED_CONTROL_PLANE'):
        directory = scratch / key.lower()
        directory.mkdir()
        env[key] = str(directory)
    sockets = scratch / 'sockets'
    sockets.mkdir()
    env.update(VC_FRAME_SOCKET_DIR=str(sockets), ZELLIJ_SOCKET_DIR=str(sockets))
    cfg = scratch / 'config.kdl'
    cfg.write_text('default_shell "/bin/sh"\ndefault_mode "normal"\n'
                   'show_startup_tips false\nshow_release_notes false\n'
                   'session_serialization false\nauto_lock_after_seconds 0\n'
                   'plugins { compact-bar location="vc-frame:compact-bar"; '
                   'status-bar location="vc-frame:status-bar"; '
                   'session-manager location="vc-frame:session-manager"; '
                   'frame-host location="vc-frame:session-manager" { frame_host true; rail true; '
                   'session_canvas true; session_canvas_kind "session-manager"; }; }\n')
    # Product chrome and ownership geometry; a shell Home permits a real input
    # assertion there without starting the external dashboard control plane.
    layout = scratch / 'host.kdl'
    canonical = Path(__file__).resolve().parents[2] / 'zellij-utils/assets/layouts/vibecrafted-host.kdl'
    source = canonical.read_text()
    start = source.index('        pane name="Home" {')
    end = source.index('\n    tab name="Workspace"', start)
    layout.write_text(source[:start] + '        pane name="Home";\n    }\n' + source[end:])
    guest_layout = scratch / 'guest.kdl'
    guest_layout.write_text('layout { tab name="Start" { pane; }; tab name="Agents" { pane; }; tab name="Shell" { pane; }; }')
    host, guest = f'f12h{os.getpid()}', f'f12g{os.getpid()}'
    host_log = Path(f'/tmp/vc-frame-{os.getuid()}/vc-frame-log') / host / 'vc-frame.log'
    base = [str(binary), '--config', str(cfg), '--config-dir', env['XDG_CONFIG_HOME']]
    fd = None
    pid = None
    log = (args.output / 'commands.jsonl').open('w')
    raw = (args.output / 'host.ansi').open('wb')

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            os.write(fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f'\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R')
            else:
                super().report_device_status(mode)

    screen = Terminal(160, 45)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder('utf-8')('replace')

    def pump(duration=0.1):
        end = time.monotonic() + duration
        while time.monotonic() < end:
            if fd is None:
                time.sleep(min(0.05, duration))
            elif select.select([fd], [], [], min(0.1, max(0, end - time.monotonic())))[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                raw.write(data)
                raw.flush()
                stream.feed(decoder.decode(data))

    def cli(session, *command, check=True):
        argv = base + (['--session', session] if session else []) + list(command)
        process = subprocess.Popen(argv, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        end = time.monotonic() + 35
        while process.poll() is None and time.monotonic() < end:
            pump()
        if process.poll() is None:
            process.kill()
        output = process.communicate()[0]
        log.write(json.dumps({'argv': argv, 'exit': process.returncode, 'output': output}) + '\n')
        log.flush()
        if check:
            assert process.returncode == 0, output
        return output

    def wait(predicate, label, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            pump(0.15)
            if predicate():
                return
        raise AssertionError(label + '\n' + '\n'.join(screen.display))

    def click(text, row=0):
        wait(lambda: text in screen.display[row], 'click target ' + text)
        col = screen.display[row].index(text) + 1
        os.write(fd, f'\x1b[<0;{col};{row + 1}M'.encode())
        pump(0.15)
        os.write(fd, f'\x1b[<0;{col};{row + 1}m'.encode())
        pump(0.3)

    def tab_active(session, name):
        tabs = json.loads(cli(session, 'action', 'list-tabs', '--json'))
        return any(tab['name'] == name and tab['active'] for tab in tabs)

    def active_chip(name):
        # Selected ribbons start with the filled fisheye; empty chips use ○.
        row = screen.display[0]
        at = row.find(name)
        return at >= 0 and '◉' in row[max(0, at - 4):at]

    def checkpoint(label, host_tab, guest_tab=None):
        wait(lambda: active_chip(guest_tab or host_tab), label + ' selected chip')
        assert tab_active(host, host_tab)
        clients = cli(host, 'action', 'list-clients')
        focused = [line.split()[1] for line in clients.splitlines()
                   if line.split() and line.split()[0].isdigit()]
        assert len(focused) == 1 and focused[0].startswith('terminal_'), clients
        if guest_tab:
            assert 'visit ' + guest in clients, clients
            assert tab_active(guest, guest_tab)
            guest_clients = cli(guest, 'action', 'list-clients')
            ids = [line.split()[0] for line in guest_clients.splitlines() if line.split() and line.split()[0].isdigit()]
            assert len(ids) == 1, guest_clients
            # Tab selection and client admission precede the visitor's first
            # render. Input belongs to the attached, ready visitor, not its
            # terminal negotiation phase.
            pane = focused[0].removeprefix('terminal_')
            wait(lambda: host_log.exists() and any(
                'workspace_projection visitor_ready ' in line
                and f'pane={pane} guest={guest} ' in line
                for line in host_log.read_text().splitlines()), label + ' visitor ready')
        marker = 'F12_' + label.replace('-', '_')
        # No write-chars/focus-pane escape hatch: this is ordinary host input.
        os.write(fd, ("printf '" + marker + "\\n'\r").encode())
        wait(lambda: any(marker in line and 'printf' not in line for line in screen.display[2:]), label + ' typed input')
        panes = json.loads(cli(host, 'action', 'list-panes', '--all', '--json'))
        visible = [p for p in panes if not p['is_plugin'] and not p['is_suppressed'] and p['tab_position'] == (0 if host_tab == 'Home' else 1)]
        assert len(visible) == 1, panes
        dump = cli(host, 'action', 'dump-screen', '--pane-id', 'terminal_' + str(visible[0]['id']))
        assert any(marker in line and 'printf' not in line for line in dump.splitlines()), dump
        (args.output / (label + '.json')).write_text(json.dumps({
            'host_tabs': json.loads(cli(host, 'action', 'list-tabs', '--json')),
            'guest_tabs': json.loads(cli(guest, 'action', 'list-tabs', '--json')),
            'host_clients': clients, 'screen': screen.display, 'dump': dump,
        }, indent=2))
        print('PASS', label, flush=True)

    try:
        (args.output / 'provenance.json').write_text(json.dumps({
            'binary': str(binary), 'build_info': cli(None, '--build-info'),
            'scratch': str(scratch), 'host': host, 'guest': guest,
        }, indent=2))
        cli(guest, '--guest-workspace', '--new-session-with-layout', str(guest_layout),
            'attach', '--create-background', guest)
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(str(binary), base + ['--new-session-with-layout', str(layout), '--session', host], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 45, 160, 0, 0))
        checkpoint('initial-home', 'Home')
        cli(host, 'action', 'switch-mode', 'locked')
        cli(host, 'action', 'go-to-tab-name', 'Workspace')
        # Rail-only first projection: no CLI project/pipe can seed its lease.
        guest_label = guest[0].upper() + guest[1:]
        wait(lambda: any(guest_label in line[:24] for line in screen.display), 'guest rail row')
        row = next(i for i, line in enumerate(screen.display) if guest_label in line[:24])
        click(guest_label, row)
        # The rail opens the first canonical organ, Agents, before other tabs.
        checkpoint('rail-agents-locked', 'Workspace', 'Agents')
        click('Start')
        checkpoint('start-locked', 'Workspace', 'Start')
        for mode in ('locked', 'normal'):
            cli(host, 'action', 'switch-mode', mode)
            click('Agents')
            checkpoint('agents-' + mode, 'Workspace', 'Agents')
            click('Shell')
            checkpoint('shell-' + mode, 'Workspace', 'Shell')
            # Guest changes reach both viewport and chrome.
            cli(guest, 'action', 'go-to-tab-name', 'Start')
            checkpoint('guest-start-' + mode, 'Workspace', 'Start')
            cli(host, 'action', 'go-to-tab-name', 'Home')
            checkpoint('home-' + mode, 'Home')
            assert 'Agents' not in screen.display[0], screen.display[0]
            # Owner stays awake on Home and refreshes its cached guest tabs.
            cli(guest, 'action', 'go-to-tab-name', 'Agents')
            pump(2)
            assert active_chip('Home'), screen.display[0]
            click('Workspace')
            checkpoint('return-agents-' + mode, 'Workspace', 'Agents')
            click('Shell')
            checkpoint('return-shell-' + mode, 'Workspace', 'Shell')
        (args.output / 'result.json').write_text(json.dumps({'passed': True, 'scope': 'F1 S2 F2 S1 LOCK+Normal rail-only and plugin-to-plugin'}))
    finally:
        for session in (host, guest):
            cli(None, 'kill-session', session, check=False)
        if fd is not None:
            os.close(fd)
        if pid is not None:
            try:
                os.kill(pid, signal.SIGHUP)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        # Socket namespaces include a contract-version directory. Check the
        # actual tree and visitor argv, rather than nonexistent root paths.
        deadline = time.monotonic() + 10
        while True:
            live_sockets = [str(path) for path in sockets.rglob('*') if path.is_socket()]
            processes = subprocess.check_output(['ps', '-Ao', 'pid,command'], text=True)
            survivors = [line for line in processes.splitlines()
                         if str(sockets) in line or ('visit ' + guest) in line]
            if not live_sockets and not survivors:
                break
            assert time.monotonic() < deadline, (live_sockets, survivors)
            time.sleep(0.1)
        (args.output / 'cleanup.json').write_text(json.dumps({
            'live_sockets': live_sockets, 'surviving_servers_and_visitors': survivors,
        }, indent=2))
        for session in (host, guest):
            session_log = host_log.parent.parent / session / 'vc-frame.log'
            if session_log.exists():
                (args.output / (session + '.log')).write_text(session_log.read_text())
        raw.close()
        log.close()


if __name__ == '__main__':
    main()
