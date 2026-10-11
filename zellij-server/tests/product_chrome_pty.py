"""Fresh product chrome acceptance in an owned, isolated PTY (requires pyte).

uv run --with pyte python zellij-server/tests/product_chrome_pty.py \
  --binary /path/to/vc-frame --layout /path/to/operator.kdl --output /new/receipt
Use --detached-start for the vc-start create-background -> attach path.
Use --cmd-n-default without --layout to exercise Cmd+N -> Session Manager ->
default native workspace with private, sentinel-only product frontends.
Installed layouts/configs are only read; no action targets Founder sessions.
"""
import argparse
import codecs
import fcntl
import hashlib
import json
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
from pathlib import Path

import pyte


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--layout', type=Path)
    parser.add_argument('--config', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--detached-start', action='store_true')
    parser.add_argument('--cmd-n-default', action='store_true')
    parser.add_argument('--route-diagnostics', action='store_true')
    parser.add_argument('--columns', type=int, default=180)
    parser.add_argument('--rows', type=int, default=45)
    parser.add_argument('--initial-columns', type=int,
                        help='Attach initially at this width, then resize to --columns')
    parser.add_argument('--resize-delay', type=float, default=2)
    parser.add_argument('--new-tab', action='store_true',
                        help='Create one additional owned tab before checking chrome')
    parser.add_argument('--allow-tab-overflow', action='store_true',
                        help='Accept a +N overflow indicator for chips in narrow fixtures')
    parser.add_argument('--timeout', type=float, default=35)
    args = parser.parse_args()
    if not args.layout and not args.cmd_n_default:
        parser.error('--layout is required unless --cmd-n-default is selected')
    if args.cmd_n_default and (args.layout or args.detached_start or args.new_tab):
        parser.error('--cmd-n-default owns its bootstrap layout and new-workspace path')
    args.output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='vcchrome-', dir='/tmp')).resolve()
    binary = args.binary.resolve()
    env = {k: os.environ[k] for k in ('PATH', 'USER', 'LOGNAME', 'LANG') if k in os.environ}
    env.update(TERM='xterm-256color', COLORTERM='truecolor', SHELL='/bin/sh')
    if args.route_diagnostics:
        env['VC_FRAME_ROUTE_DIAGNOSTICS'] = '1'
    for key in ('HOME', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME',
                'XDG_RUNTIME_DIR', 'XDG_STATE_HOME', 'VIBECRAFTED_HOME',
                'VIBECRAFTED_CONTROL_PLANE', 'VC_FRAME_SOCKET_DIR', 'VC_FRAME_CONFIG_DIR'):
        path = scratch / key.lower()
        path.mkdir()
        env[key] = str(path)
    env['ZELLIJ_SOCKET_DIR'] = env['VC_FRAME_SOCKET_DIR']
    project = scratch / 'project'
    project.mkdir()
    events = scratch / 'frontends.jsonl'
    if args.cmd_n_default:
        env['FIXTURE_EVENTS'] = str(events)
        env['FIXTURE_PYTHON'] = sys.executable
        frontend = '''import json, os, signal, sys
from pathlib import Path
row = {"frontend": Path(__file__).name, "args": sys.argv[1:],
       "cwd": os.getcwd(), "session": os.environ.get("VC_FRAME_SESSION_NAME",
       os.environ.get("ZELLIJ_SESSION_NAME"))}
with open(os.environ["FIXTURE_EVENTS"], "a") as target:
    target.write(json.dumps(row) + "\\n")
print("FIXTURE " + row["frontend"], flush=True)
signal.pause()
'''
        root = Path(env['VC_FRAME_CONFIG_DIR'])
        for name in ('vc-start-here.py', 'vc-agent-workshop.py'):
            (root / name).write_text(frontend)
        runner = root / 'pane-python'
        runner.write_text('#!/bin/sh\nexec "$FIXTURE_PYTHON" "$@"\n')
        runner.chmod(0o700)
    config = args.config.read_text() if args.config else (
        'plugins { compact-bar location="zellij:compact-bar"; '
        'status-bar location="zellij:status-bar"; '
        'session-manager location="zellij:session-manager"; }\n')
    for key, value in (('default_shell', '"/bin/sh"'), ('default_mode', '"locked"'),
                       ('show_startup_tips', 'false'), ('show_release_notes', 'false'),
                       ('session_serialization', 'false'), ('auto_lock_after_seconds', '0')):
        config = re.sub(r'^\s*' + key + r'\s+[^\n]+', '', config, flags=re.MULTILINE)
        config += f'\n{key} {value}\n'
    if args.cmd_n_default:
        config += '''
keybinds {
    shared {
        bind "Super n" {
            LaunchOrFocusPlugin "session-manager" {
                floating true
                move_to_focused_tab true
            };
        }
    }
}
'''
    cfg, layout = scratch / 'config.kdl', scratch / 'layout.kdl'
    cfg.write_text(config)
    if args.cmd_n_default:
        layout.write_text('layout { pane command="/bin/sh" name="CmdN fixture"; }\n')
    else:
        layout.write_bytes(args.layout.read_bytes())
    (args.output / 'config.kdl').write_text(config)
    (args.output / 'layout.kdl').write_bytes(layout.read_bytes())
    session = f'chrome{os.getpid()}'
    origin_session = session
    new_session = f'cmdn{os.getpid()}'
    base = [str(binary), '--config', str(cfg), '--config-dir', env['XDG_CONFIG_HOME']]
    fd = pid = None
    raw = (args.output / 'terminal.ansi').open('wb')
    commands = (args.output / 'commands.jsonl').open('w')
    transitions = (args.output / 'chrome.jsonl').open('w')
    last = None

    class Terminal(pyte.Screen):
        def write_process_input(self, data):
            if fd is not None:
                os.write(fd, data.encode())

        def report_device_status(self, mode, private=False):
            if private and mode == 6:
                self.write_process_input(f'\x1b[?{self.cursor.y + 1};{self.cursor.x + 1}R')
            else:
                super().report_device_status(mode)

    screen = Terminal(args.initial_columns or args.columns, args.rows)
    stream = pyte.Stream(screen)
    decoder = codecs.getincrementaldecoder('utf-8')('replace')

    def pump(duration=0.1):
        nonlocal last
        end = time.monotonic() + duration
        while time.monotonic() < end:
            if fd is None:
                time.sleep(min(0.05, duration))
            elif select.select([fd], [], [], min(0.1, max(0, end - time.monotonic())))[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                raw.write(data)
                raw.flush()
                stream.feed(decoder.decode(data))
                chrome = (screen.display[0], screen.display[-1])
                if chrome != last:
                    transitions.write(json.dumps({'time': time.time(), 'top': chrome[0], 'bottom': chrome[1]}) + '\n')
                    transitions.flush()
                    last = chrome

    def cli(*command, check=True, timeout=15):
        argv = base + list(command)
        proc = subprocess.Popen(argv, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        deadline = time.monotonic() + timeout
        while proc.poll() is None and time.monotonic() < deadline:
            pump()
        if proc.poll() is None:
            proc.kill()
        output = proc.communicate()[0]
        commands.write(json.dumps({'argv': argv, 'exit': proc.returncode, 'output': output}) + '\n')
        commands.flush()
        if check and proc.returncode != 0:
            raise AssertionError(output)
        return output

    receipt = {'status': 'failed', 'session': session, 'scratch': str(scratch),
               'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'layout_sha256': hashlib.sha256(layout.read_bytes()).hexdigest(),
               'detached_start': args.detached_start, 'columns': args.columns, 'rows': args.rows,
               'initial_columns': args.initial_columns, 'new_tab': args.new_tab}
    receipt['cmd_n_default'] = args.cmd_n_default
    log_path = Path(f'/tmp/vc-frame-{os.getuid()}/vc-frame-log') / session / 'vc-frame.log'
    try:
        if args.detached_start:
            cli('--layout', str(layout), 'attach', '--create-background', session, timeout=30)
            argv = base + ['attach', session]
        else:
            argv = base + ['--new-session-with-layout', str(layout), '--session', session]
        pid, fd = pty.fork()
        if pid == 0:
            if args.cmd_n_default:
                os.chdir(project)
            os.execve(str(binary), argv, env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', args.rows, args.initial_columns or args.columns, 0, 0))
        if args.initial_columns:
            pump(args.resize_delay)
            (args.output / 'initial.screen.txt').write_text('\n'.join(screen.display) + '\n')
            screen.resize(args.rows, args.columns)
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', args.rows, args.columns, 0, 0))
        if args.new_tab:
            pump(3)
            cli('--session', session, 'action', 'new-tab', '--name', 'FixtureNew')
        if args.cmd_n_default:
            pump(3)
            os.write(fd, b'\x1b[110;9u')
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                pump(0.2)
                pane_rows = json.loads(cli('--session', session, 'action', 'list-panes', '--json', '--all'))
                pane_rows = pane_rows if isinstance(pane_rows, list) else pane_rows['panes']
                if any(p.get('is_floating') and p.get('is_plugin')
                       and str(p.get('plugin_url', '')).endswith('session-manager')
                       for p in pane_rows):
                    break
            else:
                raise AssertionError('Cmd+N did not open the native Session Manager')
            (args.output / 'cmd-n.screen.txt').write_text('\n'.join(screen.display) + '\n')
            pump(1)
            os.write(fd, b'\x17')  # Session Manager's existing Ctrl+w: New Session.
            pump(0.5)
            os.write(fd, new_session.encode() + b'\r')
            pump(0.5)
            (args.output / 'default-selection.screen.txt').write_text('\n'.join(screen.display) + '\n')
            os.write(fd, b'\r')  # Accept the untouched default layout selection.
            session = new_session
            receipt.update(session=session, origin_session=origin_session,
                           cmd_n_sequence='CSI 110;9u, Ctrl+w, name, Enter, Enter')
            deadline = time.monotonic() + args.timeout
            while time.monotonic() < deadline:
                pump(0.2)
                if events.exists() and len(events.read_text().splitlines()) == 2:
                    break
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            pump(0.2)
            if (('Launchpad' in screen.display[0] or 'Start here' in screen.display[0])
                    and all(label in screen.display[0] for label in ('Agents', 'Shell', 'Panels', 'Quick cmd'))
                    and all(label in screen.display[-1] for label in ('LIVE', 'CPU', 'MEM', 'DISK', 'HEALTH'))):
                pump(2)
                break
        top, bottom = screen.display[0], screen.display[-1]
        (args.output / 'screen.txt').write_text('\n'.join(screen.display) + '\n')
        (args.output / 'top.txt').write_text(top + '\n')
        (args.output / 'bottom.txt').write_text(bottom + '\n')
        checks = {'launchpad_chip': 'Launchpad' in top or (not args.cmd_n_default and 'Start here' in top)}
        if args.cmd_n_default:
            frontend_events = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
            (args.output / 'frontends.json').write_text(json.dumps(frontend_events, indent=2) + '\n')
            checks['canonical_frontends_executed'] = sorted(
                (event['frontend'], event['args']) for event in frontend_events
            ) == [('vc-agent-workshop.py', ['launcher']), ('vc-start-here.py', [])]
            checks['selected_workspace_cwd'] = len(frontend_events) == 2 and all(
                Path(event['cwd']).resolve() == project for event in frontend_events)
            checks['new_native_target'] = len(frontend_events) == 2 and all(
                event['session'] == new_session for event in frontend_events)
        checks.update({label: label in top for label in ('Agents', 'Shell', 'Panels', 'Quick cmd')})
        checks.update({label: label in bottom for label in ('LIVE', 'CPU', 'MEM', 'DISK', 'HEALTH')})
        panes = json.loads(cli('--session', session, 'action', 'list-panes', '--json', '--all'))
        (args.output / 'panes.json').write_text(json.dumps(panes, indent=2))
        tabs_text = cli('--session', session, 'action', 'list-tabs', '--json')
        (args.output / 'tabs.json').write_text(tabs_text)
        tabs = json.loads(tabs_text)
        if args.cmd_n_default:
            checks['default_product_tabs'] = [tab['name'] for tab in tabs] == ['Launchpad', 'Agents', 'Shell']
            all_panes = panes if isinstance(panes, list) else panes['panes']
            checks['legacy_frontends_absent'] = (not any(
                str(p.get('plugin_url', '')).endswith('about') for p in all_panes)
                and sum(p.get('tab_name') == 'Agents' and p.get('is_plugin')
                        and str(p.get('plugin_url', '')).endswith('session-manager')
                        for p in all_panes) == 1)  # Rail only; no dashboard organ.
        overflow = re.search(r'\+(\d+)', top)
        if args.allow_tab_overflow and overflow:
            tab_names = {tab['name'] for tab in tabs}
            missing = [label for label in ('Agents', 'Shell') if not checks[label]]
            if int(overflow[1]) >= len(missing):
                for label in missing:
                    checks[label] = label in tab_names
        active_tab = next(tab['name'] for tab in tabs if tab['active'])
        rows = panes if isinstance(panes, list) else panes['panes']
        chrome_panes = [p for p in rows if p.get('is_plugin') and any(
            str(p.get('plugin_url', '')).split(':')[-1] == kind for kind in ('compact-bar', 'status-bar'))]
        visible_chrome = [p for p in chrome_panes if p.get('tab_name') == active_tab
                          and not p.get('is_suppressed')]
        for kind, expected_y in (('compact-bar', 0), ('status-bar', args.rows - 1)):
            visible = [p for p in visible_chrome if p['plugin_url'].split(':')[-1] == kind]
            checks[kind + '_unique_full_width'] = (len(visible) == 1 and
                visible[0]['pane_x'] == 0 and visible[0]['pane_y'] == expected_y and
                visible[0]['pane_content_columns'] == args.columns and
                visible[0]['pane_content_rows'] == 1)
        for pane in chrome_panes:
            pane_id = f"plugin_{pane['id']}"
            (args.output / f'{pane_id}.txt').write_text(cli('--session', session, 'action', 'dump-screen', '--pane-id', pane_id, check=False))
            (args.output / f'{pane_id}.ansi').write_text(cli('--session', session, 'action', 'dump-screen', '--pane-id', pane_id, '--ansi', check=False))
        receipt.update(top=top, bottom=bottom, checks=checks, chrome_panes=chrome_panes)
        receipt['build_info'] = json.loads(cli('--build-info'))
        receipt['status'] = 'passed' if all(checks.values()) else 'failed'
    except (AssertionError, OSError, ValueError, KeyError, StopIteration) as error:
        receipt['error'] = str(error)
        (args.output / 'screen.txt').write_text('\n'.join(screen.display) + '\n')
    finally:
        if log_path.exists():
            log_text = log_path.read_text(errors='replace')
            (args.output / 'server.log').write_text(log_text)
            receipt['rename_plugin_timeouts'] = log_text.count('Action RenamePluginPane did not complete')
        for owned_session in ([new_session, origin_session] if args.cmd_n_default else [session]):
            cli('kill-session', owned_session, check=False)
        if pid is not None:
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    pass
                deadline = time.monotonic() + 2
                while time.monotonic() < deadline:
                    try:
                        if os.waitpid(pid, os.WNOHANG)[0]:
                            break
                    except ChildProcessError:
                        break
                    pump(0.1)
                else:
                    continue
                break
        if fd is not None:
            os.close(fd)
        raw.close()
        commands.close()
        transitions.close()
        (args.output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
        print(json.dumps(receipt, indent=2))
    return 0 if receipt['status'] == 'passed' else 1


if __name__ == '__main__':
    raise SystemExit(main())
