"""F12: real compact-bar -> MessageFromPlugin -> projection owner, private PTYs.

uv run --with pyte python zellij-server/tests/workspace_chrome_pty.py \
  --binary target/debug/vc-frame --output /absolute/new-receipt-directory
No actions target the user's sessions. HOME and installation are preserved.

--repetitions 20 --route-diagnostics measures chrome at Screen's accepted
projection commit and again within 60 seconds of input. It needs a donor with
the `workspace_projection committed` receipt. Strict acceptance also fails on
commit-time mismatch, even if the bounded convergence check later passes.

--rail-case host --rail-config /path/to/config.kdl checks the fixed rail's
session/tab clicks, ordinary keyboard focus, and Super Up/Down routing in
locked and normal modes. --rail-case ordinary checks non-host navigation.

The OLD product-host bridge alternates rail clicks and Super Right. It checks
the original correlated Screen commit and exact-task input without corrective
CLI projections. Only the stock bridge uses explicit project-workspace replies.
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
import tempfile
import termios
import time
from pathlib import Path

import pyte


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cut2-case', choices=('keys', 'recovery-empty', 'recovery-cached', 'recovery-close'))
    parser.add_argument('--repetitions', type=int, default=0,
                        help='Repeat Agents -> Shell -> Home -> Workspace via clicks and Super keys')
    parser.add_argument('--route-diagnostics', action='store_true')
    parser.add_argument('--rail-case', choices=('host', 'ordinary'))
    parser.add_argument('--rail-config', type=Path,
                        help='Copy this config into the private namespace for the rail scenario')
    parser.add_argument('--rail-mode', choices=('locked', 'normal'))
    parser.add_argument('--rail-commit-receipts', action='store_true',
                        help='Require the accepted Screen receipt before injecting body input')
    parser.add_argument('--legacy-binary', type=Path, help='Exercise an OLD ten-tab chrome layout through this new host')
    parser.add_argument('--bridge-rounds', type=int, default=2)
    parser.add_argument('--legacy-product-layout', type=Path, help='Exact installed OLD product host layout; adds retained work and a coexisting guest')
    parser.add_argument('--legacy-config', type=Path, help='Exact OLD product config, copied into the private namespace')
    args = parser.parse_args()
    if (args.legacy_product_layout or args.legacy_config) and not args.legacy_binary:
        parser.error('--legacy-product-layout/--legacy-config require --legacy-binary')
    if args.legacy_product_layout and args.bridge_rounds < 2:
        parser.error('the product-host proof requires at least two complete traversals')
    if args.legacy_binary:
        if not args.rail_config or args.bridge_rounds < 1:
            parser.error('--legacy-binary requires --rail-config and positive --bridge-rounds')
        return run_legacy_bridge(args)
    if args.repetitions < 0:
        parser.error('--repetitions must be non-negative')
    if args.repetitions and args.cut2_case:
        parser.error('--repetitions and --cut2-case are separate scenarios')
    if args.rail_case and (args.repetitions or args.cut2_case or not args.rail_config):
        parser.error('--rail-case requires --rail-config and is a separate scenario')
    binary = args.binary.resolve()
    args.output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='vcf12-', dir='/tmp')).resolve()
    env = {key: os.environ[key] for key in ('PATH', 'HOME', 'USER', 'LOGNAME', 'LANG') if key in os.environ}
    env.update(TERM='xterm-256color', SHELL='/bin/sh')
    if args.route_diagnostics:
        env['VC_FRAME_ROUTE_DIAGNOSTICS'] = '1'
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
    if args.rail_case:
        # Keep the published aliases/keybindings verbatim. Only isolate shell,
        # persistence and inactivity policy; never write the published file.
        source_config = args.rail_config.read_text()
        for key, value in [('default_shell', '"/bin/sh"'),
                           ('session_serialization', 'false'),
                           ('auto_lock_after_seconds', '0')]:
            source_config = re.sub(r'^' + key + r' .+$', '', source_config, flags=re.M)
            source_config += f'\n{key} {value}\n'
        cfg.write_text(source_config)
        (args.output / 'config.kdl').write_text(source_config)
    # Product chrome and ownership geometry; a shell Home permits a real input
    # assertion there without starting the external dashboard control plane.
    layout = scratch / 'host.kdl'
    canonical = Path(__file__).resolve().parents[2] / 'zellij-utils/assets/layouts/vibecrafted-host.kdl'
    source = canonical.read_text()
    start = source.index('        pane name="Home" {')
    end = source.index('\n    tab name="Workspace"', start)
    layout.write_text(source[:start] + '        pane name="Home";\n    }\n' + source[end:])
    if args.rail_case == 'ordinary':
        layout.write_text((canonical.parent / 'default.kdl').read_text())
    guest_layout = scratch / 'guest.kdl'
    guest_layout.write_text('layout { tab name="Start" { pane; }; tab name="Agents" { pane; }; tab name="Shell" { pane; }; }')
    host, guest = f'f12h{os.getpid()}', f'f12g{os.getpid()}'
    other_guest = f'f12z{os.getpid()}'
    owned_sessions = (host, guest, other_guest) if args.rail_case else (host, guest)
    host_log = Path(f'/tmp/vc-frame-{os.getuid()}/vc-frame-log') / host / 'vc-frame.log'
    base = [str(binary), '--config', str(cfg), '--config-dir', env['XDG_CONFIG_HOME']]
    fd = None
    pid = None
    log = (args.output / 'commands.jsonl').open('w')
    raw = (args.output / 'host.ansi').open('wb')
    chrome_log = (args.output / 'chrome.jsonl').open('w')
    last_chrome = None

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
        nonlocal last_chrome
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
                if screen.display[0] != last_chrome:
                    last_chrome = screen.display[0]
                    chrome_log.write(json.dumps({'time': time.time(), 'row': last_chrome}) + '\n')
                    chrome_log.flush()

    def cli(session, *command, check=True):
        argv = base + (['--session', session] if session else []) + list(command)
        process = subprocess.Popen(argv, env=env, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        end = time.monotonic() + 60
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

    def wait(predicate, label, timeout=60):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            pump(0.15)
            if predicate():
                return
        raise AssertionError(label + '\n' + '\n'.join(screen.display))

    def click(text, row=0):
        wait(lambda: text in screen.display[row], 'click target ' + text)
        col = screen.display[row].index(text) + 1
        log.write(json.dumps({'mouse_target': text, 'row': row, 'column': col,
                              'press_hex': f'\x1b[<0;{col};{row + 1}M'.encode().hex(),
                              'release_hex': f'\x1b[<0;{col};{row + 1}m'.encode().hex(),
                              'time': time.time()}) + '\n')
        log.flush()
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

    def checkpoint(label, host_tab, guest_tab=None, guest_visitors=1):
        wait(lambda: active_chip(guest_tab or host_tab), label + ' selected chip')
        wait(lambda: tab_active(host, host_tab), label + ' host tab committed')
        clients = cli(host, 'action', 'list-clients')
        focused = [line.split()[1] for line in clients.splitlines()
                   if line.split() and line.split()[0].isdigit()]
        assert len(focused) == 1 and focused[0].startswith('terminal_'), clients
        if guest_tab:
            assert 'visit ' + guest in clients, clients
            wait(lambda: tab_active(guest, guest_tab), label + ' guest tab committed')
            wait(lambda: active_chip(guest_tab), label + ' settled guest chip')
            guest_clients = cli(guest, 'action', 'list-clients')
            ids = [line.split()[0] for line in guest_clients.splitlines() if line.split() and line.split()[0].isdigit()]
            assert len(ids) == guest_visitors, guest_clients
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

    def rail_checkpoint(label):
        panes = json.loads(cli(host, 'action', 'list-panes', '--all', '--json'))
        managers = [p for p in panes if p['is_plugin'] and
                    ((p.get('plugin_url') or '').endswith('session-manager') or
                     p.get('plugin_url') in ('frame-host', 'session-manager', 'session-rail'))]
        rails = [p for p in managers if p['title'] == 'Sessions']
        receipt = {'panes': panes, 'manager_panes': len(managers),
                   'manager_runtimes': sorted({p.get('plugin_runtime_id', p['id']) for p in managers}),
                   'rail_panes': len(rails),
                   'rail_runtimes': sorted({p.get('plugin_runtime_id', p['id']) for p in rails}),
                   'clients': cli(host, 'action', 'list-clients'),
                   'guest_tabs': json.loads(cli(guest, 'action', 'list-tabs', '--json')),
                   'screen': screen.display, 'time': time.time()}
        (args.output / (label + '.json')).write_text(json.dumps(receipt, indent=2))
        return receipt

    def run_rail_case():
        trials = []
        wait(lambda: host in cli(None, 'list-sessions', '--short', check=False),
             'private host admitted')
        wait(lambda: bool(re.search(r'^\s*\d+\s',
                                   cli(host, 'action', 'list-clients', check=False), re.M)),
             'private client admitted')
        if args.rail_case == 'host':
            cli(host, 'action', 'go-to-tab-name', 'Workspace')
        guest_label = guest[0].upper() + guest[1:]
        wait(lambda: any(guest_label in line[:24] for line in screen.display), 'guest rail row')
        baseline = rail_checkpoint('rail-before')
        if args.rail_case == 'ordinary':
            mode = args.rail_mode or 'normal'
            cli(host, 'action', 'switch-mode', mode)
            os.write(fd, b'\x1b[1;9B')
            target = None

            def ordinary_target_connected():
                nonlocal target
                for candidate in (guest, other_guest):
                    if re.search(r'^\s*\d+\s', cli(candidate, 'action', 'list-clients'), re.M):
                        target = candidate
                        return True
                return False
            transitioned = False
            try:
                wait(ordinary_target_connected, 'ordinary rail navigation', timeout=20)
                marker = 'F12_ORDINARY_' + mode
                os.write(fd, ("printf '" + marker + "\\n'\r").encode())
                wait(lambda: any(marker in line and 'printf' not in line for line in screen.display),
                     'ordinary navigation focus', timeout=20)
                transitioned = True
            except AssertionError:
                pass
            after = rail_checkpoint('rail-' + mode + '-down')
            passed = (transitioned and after['rail_panes'] == baseline['rail_panes']
                      and after['rail_runtimes'] == baseline['rail_runtimes'])
            (args.output / 'result.json').write_text(json.dumps({'passed': passed,
                'scope': 'rail-ordinary', 'mode': mode, 'target': target,
                'body_and_focus': transitioned,
                'rail_panes_before': baseline['rail_panes'], 'rail_panes_after': after['rail_panes'],
                'rail_runtimes_before': baseline['rail_runtimes'],
                'rail_runtimes_after': after['rail_runtimes']}, indent=2))
            assert passed, after
            return

        def visitor_committed(session):
            clients = cli(host, 'action', 'list-clients')
            for line in clients.splitlines():
                fields = line.split()
                if len(fields) > 2 and fields[0].isdigit() and fields[1].startswith('terminal_'):
                    if 'visit ' + session not in line:
                        continue
                    pane = fields[1].removeprefix('terminal_')
                    return host_log.exists() and any(
                        'workspace_projection committed ' in event and
                        f'pane={pane} guest={session} ' in event
                        for event in host_log.read_text().splitlines())
            return False

        def await_body(session):
            if args.rail_commit_receipts:
                wait(lambda: visitor_committed(session), 'accepted visitor body ' + session)

        for mode in ((args.rail_mode,) if args.rail_mode else ('locked', 'normal')):
            trial = {'mode': mode, 'scope': args.rail_case}
            cli(host, 'action', 'switch-mode', mode)
            rail_checkpoint('rail-' + mode + '-before')
            row = next(i for i, line in enumerate(screen.display) if guest_label in line[:24])
            started = time.monotonic()
            click(guest_label, row)
            marker = 'F12_RAIL_' + mode
            try:
                wait(lambda: ('visit ' + guest in cli(host, 'action', 'list-clients'))
                     if args.rail_case == 'host' else
                     bool(re.search(r'^\s*\d+\s', cli(guest, 'action', 'list-clients'), re.M)),
                     'rail click delivered')
                await_body(guest)
                os.write(fd, ("printf '" + marker + "\\n'\r").encode())
                wait(lambda: any(marker in line and 'printf' not in line for line in screen.display[2:]),
                     'rail keyboard focus')
                trial['click_and_focus'] = True
                if args.rail_case == 'host':
                    shell_row = next(i for i, line in enumerate(screen.display) if 'Shell' in line[:24])
                    click('Shell', shell_row)
                    wait(lambda: tab_active(guest, 'Shell'), 'rail Shell tab')
                    # A tab query alone precedes admission of the replacement visitor.
                    wait(lambda: ('visit ' + guest + ' --tab 3' in
                                  cli(host, 'action', 'list-clients')),
                         'rail Shell visitor')
                    await_body(guest)
                    os.write(fd, ("printf '" + marker + "_SHELL\\n'\r").encode())
                    wait(lambda: any(marker + '_SHELL' in line and 'printf' not in line
                                     for line in screen.display[2:]),
                         'rail Shell keyboard focus')
                    trial['tab_click_and_focus'] = True
            except AssertionError as error:
                trial['click_and_focus'] = False
                trial['click_error'] = str(error)
            trial['click_seconds'] = time.monotonic() - started
            trial['after_click'] = rail_checkpoint('rail-' + mode + '-click')
            trial['navigation'] = []
            for direction, sequence, target in [('down', b'\x1b[1;9B', other_guest),
                                                ('up', b'\x1b[1;9A', guest)]:
                nav_started = time.monotonic()
                log.write(json.dumps({'key': 'Super ' + direction, 'hex': sequence.hex(),
                                      'time': time.time()}) + '\n')
                log.flush()
                os.write(fd, sequence)
                transitioned = False
                try:
                    wait(lambda: 'visit ' + target in cli(host, 'action', 'list-clients'),
                         'rail navigation body ' + target)
                    nav_marker = 'F12_NAV_' + mode + '_' + direction
                    await_body(target)
                    os.write(fd, ("printf '" + nav_marker + "\\n'\r").encode())
                    wait(lambda: any(nav_marker in line and 'printf' not in line
                                     for line in screen.display[2:]),
                         'rail navigation keyboard focus')
                    transitioned = True
                except AssertionError:
                    pass
                after = rail_checkpoint('rail-' + mode + '-' + direction)
                trial['navigation'].append({'direction': direction,
                    'seconds': time.monotonic() - nav_started,
                    'body_and_focus': transitioned,
                    'same_panes': after['rail_panes'] == baseline['rail_panes'],
                    'same_runtimes': after['rail_runtimes'] == baseline['rail_runtimes'],
                    'host_client_present': bool(re.search(r'^\s*\d+\s', after['clients'], re.M)),
                    'clients': after['clients']})
            trial['passed'] = trial.get('tab_click_and_focus', False) and all(
                n['same_panes'] and n['same_runtimes'] and n['host_client_present'] and n['body_and_focus']
                for n in trial['navigation'])
            trials.append(trial)
            (args.output / 'result.json').write_text(json.dumps({'passed': all(t['passed'] for t in trials),
                'scope': 'rail-' + args.rail_case, 'trials': trials}, indent=2))
            print('RAIL', mode, trial['passed'], flush=True)
            if not trial['passed']:
                break
        assert all(t.get('passed', t['click_and_focus']) for t in trials), trials

    try:
        (args.output / 'provenance.json').write_text(json.dumps({
            'binary': str(binary), 'build_info': cli(None, '--build-info'),
            'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'harness_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
            'scratch': str(scratch), 'host': host, 'guest': guest,
        }, indent=2))
        cli(guest, '--guest-workspace', '--new-session-with-layout', str(guest_layout),
            'attach', '--create-background', guest)
        if args.rail_case:
            cli(other_guest, '--guest-workspace', '--new-session-with-layout', str(guest_layout),
                'attach', '--create-background', other_guest)
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(str(binary), base + ['--new-session-with-layout', str(layout), '--session', host], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', 45, 160, 0, 0))
        if args.rail_case:
            run_rail_case()
            return
        checkpoint('initial-home', 'Home')
        cli(host, 'action', 'switch-mode', 'locked')
        cli(host, 'action', 'go-to-tab-name', 'Workspace')
        if args.repetitions:
            trials = []
            for iteration in range(args.repetitions):
                trial = {'iteration': iteration + 1, 'input': 'click' if iteration % 2 == 0 else 'Super Right'}
                try:
                    cli(host, 'action', 'switch-mode', 'locked' if iteration % 2 == 0 else 'normal')
                    receipt = json.loads(cli(host, 'project-workspace', guest, '--tab', '2'))
                    assert receipt['status'] == 'Handled', receipt
                    wait(lambda: active_chip('Agents'), 'repeat reset Agents')
                    old_log_size = len(host_log.read_text())
                    started = time.monotonic()
                    if iteration % 2 == 0:
                        click('Shell')
                    else:
                        os.write(fd, b'\x1b[1;9C')
                    wait(lambda offset=old_log_size: any('workspace_projection committed ' in line
                                     and f'guest={guest} tab=Some(2)' in line
                                     for line in host_log.read_text()[offset:].splitlines()),
                         'repeat Shell projection committed')
                    trial['commit_seconds'] = time.monotonic() - started
                    trial['chrome_at_commit'] = screen.display[0]
                    trial['mismatch_at_commit'] = not active_chip('Shell')
                    ready_at = time.monotonic()
                    try:
                        # Preserve cut 2's bound from the input, rather than
                        # accidentally giving chrome another minute after ready.
                        wait(lambda: active_chip('Shell'), 'repeat Shell chrome after ready',
                             timeout=max(0.15, 60 - (time.monotonic() - started)))
                        trial['mismatch'] = False
                    except AssertionError:
                        trial['mismatch'] = True
                    trial['chrome_settle_seconds'] = time.monotonic() - ready_at
                    trial['chrome_after_wait'] = screen.display[0]
                    trial['screen'] = screen.display
                    wait(lambda: tab_active(guest, 'Shell'), 'repeat Shell body committed')
                    trial['body_committed'] = True
                    if not trial['mismatch']:
                        marker = f'F12_REPEAT_{iteration + 1}'
                        os.write(fd, ("printf '" + marker + "\\n'\r").encode())
                        wait(lambda expected=marker: any(expected in line and 'printf' not in line
                                         for line in screen.display[2:]), 'repeat Shell input')
                        cli(host, 'action', 'go-to-tab-name', 'Home')
                        wait(lambda: active_chip('Home') and any(
                            'F12_initial_home' in line and 'printf' not in line
                            for line in screen.display[2:]), 'repeat Home body and chrome')
                        click('Workspace')
                        wait(lambda expected=marker: active_chip('Shell') and any(
                            expected in line and 'printf' not in line
                            for line in screen.display[2:]), 'repeat return body and chrome')
                        trial['return_screen'] = screen.display
                    trial['passed'] = not trial['mismatch']
                except AssertionError as error:
                    trial['passed'] = False
                    trial['error'] = str(error)
                trials.append(trial)
                summary = {'requested': args.repetitions, 'completed': len(trials),
                           'committed_body_trials': sum(t.get('body_committed', False) for t in trials),
                           'mismatches_at_commit': sum(t.get('mismatch_at_commit', False) for t in trials),
                           'settle_bound_seconds_from_input': 60,
                           'mismatches': sum(t.get('mismatch', False) for t in trials),
                           'other_failures': sum(not t['passed'] and not t.get('mismatch', False) for t in trials),
                           'bounded_passed': len(trials) == args.repetitions and all(t['passed'] for t in trials),
                           'passed': len(trials) == args.repetitions and all(
                               t['passed'] and not t.get('mismatch_at_commit', False) for t in trials),
                           'trials': trials}
                (args.output / 'result.json').write_text(json.dumps(summary, indent=2))
                print('TRIAL', iteration + 1, 'mismatch', trial.get('mismatch'), 'bounded_passed', trial['passed'], flush=True)
            assert summary['passed'], (f"repeat failures: {summary['mismatches_at_commit']} at commit, "
                                      f"{summary['mismatches']} after bound, {summary['other_failures']} other")
            return
        if args.cut2_case and args.cut2_case.startswith('recovery-'):
            cached = args.cut2_case != 'recovery-empty'
            if cached:
                receipt = json.loads(cli(host, 'project-workspace', guest, '--tab', '1'))
                assert receipt['status'] == 'Handled', receipt
                checkpoint('before-recovery', 'Workspace', 'Start')
                panes = json.loads(cli(host, 'action', 'list-panes', '--all', '--json'))
                target = next('terminal_' + str(p['id']) for p in panes
                              if not p['is_plugin'] and not p['is_suppressed'] and p['tab_position'] == 1)
            else:
                panes = json.loads(cli(host, 'action', 'list-panes', '--all', '--json'))
                target = next('plugin_' + str(p['id']) for p in panes
                              if p['is_plugin'] and not p['is_suppressed'] and p['title'] == 'VC Guest')
                # Recovery before registration: a raw visitor replaces the slot,
                # without calling project-workspace / populating Screen's cache.
                clients = cli(host, 'action', 'list-clients')
                focused = [line.split()[1] for line in clients.splitlines()
                           if line.split() and line.split()[0].isdigit()]
                if target not in focused:
                    cli(host, 'action', 'focus-pane-id', target)
                cli(host, 'action', 'new-pane', '--in-place',
                    '--', str(binary), 'visit', guest, '--tab', '1')
                panes = json.loads(cli(host, 'action', 'list-panes', '--all', '--json'))
                target = next('terminal_' + str(p['id']) for p in panes
                              if not p['is_plugin'] and not p['is_suppressed'] and p['tab_position'] == 1)
            flags = ['--close-replaced-pane'] if args.cut2_case == 'recovery-close' else []
            cli(host, 'action', 'launch-plugin', 'vc-frame:session-manager', '--in-place',
                '--configuration', 'workspace_surface=true', *flags)
            receipt = json.loads(cli(host, 'project-workspace', guest, '--tab', '2'))
            (args.output / 'recovery-receipt.json').write_text(json.dumps(receipt, indent=2))
            assert receipt['status'] == 'Handled', receipt
            checkpoint(args.cut2_case, 'Workspace', 'Agents',
                       guest_visitors=1 if args.cut2_case == 'recovery-close' else 2)
            (args.output / 'result.json').write_text(json.dumps({'passed': True, 'scope': args.cut2_case}))
            return
        if args.cut2_case == 'keys':
            receipt = json.loads(cli(host, 'project-workspace', guest, '--tab', '2'))
            assert receipt['status'] == 'Handled', receipt
            checkpoint('keys-initial-agents', 'Workspace', 'Agents')
        else:
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
            if args.cut2_case == 'keys':
                click('Agents')
                checkpoint('keys-agents-' + mode, 'Workspace', 'Agents')
                for direction, expected, label in (
                    ('Left', 'Agents', 'left-boundary'),
                    ('Right', 'Shell', 'right'),
                    ('Right', 'Start', 'right-end'),
                    ('Right', 'Start', 'right-boundary'),
                    ('Left', 'Shell', 'left'),
                ):
                    os.write(fd, ('\x1b[1;9' + ('C' if direction == 'Right' else 'D')).encode())
                    pump(0.5)
                    checkpoint('keys-' + label + '-' + mode, 'Workspace', expected)
                cli(host, 'action', 'go-to-tab-name', 'Home')
                checkpoint('keys-home-' + mode, 'Home')
                os.write(fd, b'\x1b[1;9C')
                pump(0.5)
                checkpoint('keys-home-right-' + mode, 'Workspace', 'Shell')
                cli(host, 'action', 'go-to-tab-name', 'Home')
                checkpoint('keys-home-before-left-' + mode, 'Home')
                os.write(fd, b'\x1b[1;9D')
                pump(0.5)
                checkpoint('keys-home-left-' + mode, 'Workspace', 'Shell')
                continue
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
        (args.output / 'result.json').write_text(json.dumps({'passed': True, 'scope': args.cut2_case or 'F1 S2 F2 S1 LOCK+Normal rail-only and plugin-to-plugin'}))
    finally:
        for session in owned_sessions:
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
                         if str(sockets) in line or
                         any(('visit ' + session) in line for session in owned_sessions)]
            if not live_sockets and not survivors:
                break
            assert time.monotonic() < deadline, (live_sockets, survivors)
            time.sleep(0.1)
        (args.output / 'cleanup.json').write_text(json.dumps({
            'live_sockets': live_sockets, 'surviving_servers_and_visitors': survivors,
        }, indent=2))
        for session in owned_sessions:
            session_log = host_log.parent.parent / session / 'vc-frame.log'
            if session_log.exists():
                (args.output / (session + '.log')).write_text(session_log.read_text())
        raw.close()
        chrome_log.close()
        log.close()



def run_legacy_bridge(args):
    """Owned OLD stock/product fixture, physical selection and exact-task return."""
    import hashlib, tempfile, subprocess
    ROOT=Path(__file__).resolve().parents[2]
    OUT=args.output.resolve();OUT.mkdir(parents=True,exist_ok=False)
    OLD=args.legacy_binary.resolve();NEW=args.binary.resolve()
    product=bool(args.legacy_product_layout); count=13 if product else 10
    def tab_name(i):return 'Workspace' if product and i==1 else f'Slot{i:02}'
    scratch=Path(tempfile.mkdtemp(prefix='vcbridge-',dir='/tmp')).resolve(); start=time.monotonic(); budget=900 if product else 1500; deadline=start+budget
    baseenv={k:os.environ[k] for k in ('PATH','HOME','USER','LOGNAME','LANG') if k in os.environ};baseenv.update(TERM='xterm-256color',SHELL='/bin/sh',VC_FRAME_ROUTE_DIAGNOSTICS='1',VC_SERVER_URL='http://127.0.0.1:9')
    for k in ('TMPDIR','XDG_CACHE_HOME','XDG_RUNTIME_DIR','VIBECRAFTED_HOME','VIBECRAFTED_CONTROL_PLANE'):
     d=scratch/k.lower();d.mkdir();baseenv[k]=str(d)
    sockets=scratch/'sockets';sockets.mkdir();baseenv.update(VC_FRAME_SOCKET_DIR=str(sockets),ZELLIJ_SOCKET_DIR=str(sockets))
    envs={}
    for role in ['old','new']:
     env=baseenv.copy()
     for k in ['XDG_CONFIG_HOME','XDG_DATA_HOME']:
      d=scratch/(role+'-'+k.lower());d.mkdir();env[k]=str(d)
     envs[role]=env
    cfg=scratch/'config.kdl';config=args.rail_config.read_text()
    for k,v in [('default_shell','"/bin/sh"'),('session_serialization','false'),('auto_lock_after_seconds','0')]:
     config=re.sub(r'^'+k+r' .+$','',config,flags=re.M)+f'\n{k} {v}\n'
    cfg.write_text(config);(OUT/'config.kdl').write_text(config)
    oldcfg=scratch/'old-config.kdl'
    oldconfig=args.legacy_config.read_text() if args.legacy_config else config
    for k,v in [('default_shell','"/bin/sh"'),('session_serialization','false'),('auto_lock_after_seconds','0')]:
     oldconfig=re.sub(r'^'+k+r' .+$','',oldconfig,flags=re.M)+f'\n{k} {v}\n'
    oldcfg.write_text(oldconfig);(OUT/'old-config.kdl').write_text(oldconfig)
    guest=f'legacy{os.getpid()}' if product else f'bridgeold{os.getpid()}';host=f'bridgenew{os.getpid()}';other=f'guest{os.getpid()}';clients=[];log=(OUT/'commands.jsonl').open('w');result={'accepted':False,'tabs':[],'stages':[],'scratch':str(scratch),'guest':guest,'host':host}
    class Term(pyte.Screen):
     def __init__(self):super().__init__(180,55);self.fd=None
     def write_process_input(self,data):os.write(self.fd,data.encode())
     def report_device_status(self,mode,private=False):
      if private and mode==6:self.write_process_input(f'\x1b[?{self.cursor.y+1};{self.cursor.x+1}R')
      else:super().report_device_status(mode)
    def pump(duration=.15):
     until=min(time.monotonic()+duration,deadline)
     while time.monotonic()<until:
      ready=select.select([c['fd'] for c in clients if c['open']],[],[],min(.1,max(0,until-time.monotonic())))[0]
      for c in clients:
       if c['fd'] not in ready:continue
       try:data=os.read(c['fd'],65536)
       except OSError:c['open']=False;continue
       if not data:c['open']=False;continue
       c['raw'].write(data);c['raw'].flush();c['stream'].feed(c['decoder'].decode(data))
    def save(): (OUT/'result.json').write_text(json.dumps(result,indent=2)+'\n')
    def cli(role,session,*args,check=True):
     binary=OLD if role=='old' else NEW;env=envs[role];argv=[str(binary),'--config',str(oldcfg if role=='old' else cfg),'--config-dir',env['XDG_CONFIG_HOME']]+(['--session',session] if session else [])+[str(a) for a in args]
     t=time.monotonic();pr=subprocess.Popen(argv,env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
     end=min(deadline,t+55)
     while pr.poll() is None and time.monotonic()<end:pump()
     timeout=pr.poll() is None
     if timeout:pr.kill()
     output=pr.communicate()[0];rec={'argv':argv,'exit':pr.returncode,'seconds':time.monotonic()-t,'timeout':timeout,'output':output};log.write(json.dumps(rec)+'\n');log.flush()
     if check:assert pr.returncode==0,rec
     return output
    def wait(pred,label,seconds=55):
     end=min(deadline,time.monotonic()+seconds)
     while time.monotonic()<end:
      pump()
      if pred():return
     raise AssertionError(label)
    def spawn(role,session,layout=None):
     binary=OLD if role=='old' else NEW;env=envs[role];argv=[str(binary),'--config',str(oldcfg if role=='old' else cfg),'--config-dir',env['XDG_CONFIG_HOME']]
     argv+=['--new-session-with-layout',str(layout),'--session',session] if layout else ['--layout',str(gl if role=='old' else hl),'attach',session]
     pid,fd=pty.fork()
     if pid==0:os.execve(str(binary),argv,env)
     fcntl.ioctl(fd,termios.TIOCSWINSZ,struct.pack('HHHH',55,180,0,0));term=Term();term.fd=fd
     c={'pid':pid,'fd':fd,'open':True,'screen':term,'stream':pyte.Stream(term),'decoder':codecs.getincrementaldecoder('utf-8')('replace'),'raw':(OUT/f'{role}-client-{pid}.ansi').open('wb')};clients.append(c);return c
    def screen(c):return c['screen'].display
    def task_pids():return {str(i):int((scratch/f'pid-{i}').read_text()) for i in range(1,count+1)}
    def task_identity():
     pids=task_pids();return {k:subprocess.check_output(['ps','-p',str(v),'-o','pid=,ppid=,lstart=,command='],text=True).strip() for k,v in pids.items()}
    def snapshot(label):
     d={'guest_tabs':json.loads(cli('old',guest,'action','list-tabs','--json')),'guest_panes':json.loads(cli('old',guest,'action','list-panes','--all','--json')),'guest_clients':cli('old',guest,'action','list-clients'),'tasks':task_identity(),'time':time.time()}
     d['owned_servers']=[line for line in subprocess.check_output(['ps','-Ao','pid,ppid,lstart,command'],text=True).splitlines() if (' --server '+str(sockets)) in line]
     d['guest_server']=[line.strip() for line in d['owned_servers'] if guest in line]
     assert len(d['guest_server'])==1,d['guest_server']
     assert d['guest_server']==result.setdefault('guest_server_identity',d['guest_server'])
     try:d.update(host_panes=json.loads(cli('new',host,'action','list-panes','--all','--json')),host_clients=cli('new',host,'action','list-clients'))
     except Exception as ex:d['host_error']=str(ex)
     d['host_floating_technical']=[p for p in d.get('host_panes',[]) if p.get('is_floating') and any(x in str(p.get('plugin_url')) for x in ('frame-host','session-manager','vc-tab-title','link'))]
     d['guest_floating_technical']=[p for p in d['guest_panes'] if p.get('is_floating') and any(x in str(p.get('plugin_url')) for x in ('session-manager','vc-tab-title','link'))]
     d['host_rail_panes']=[p['id'] for p in d.get('host_panes',[]) if p.get('plugin_url')=='frame-host']
     d['host_rail_runtimes']=sorted({p['plugin_runtime_id'] for p in d.get('host_panes',[]) if p.get('plugin_url')=='frame-host'})
     assert len(d['host_rail_runtimes'])==1,d
     assert d['host_rail_panes']==result.setdefault('rail_panes',d['host_rail_panes'])
     assert d['host_rail_runtimes']==result.setdefault('rail_runtimes',d['host_rail_runtimes'])
     if product:
      d['guest_owner_panes']=[p['id'] for p in d['guest_panes'] if p.get('plugin_url')=='frame-host']
      d['guest_owner_runtimes']=sorted({p['plugin_runtime_id'] for p in d['guest_panes'] if p.get('plugin_url')=='frame-host'})
      assert len(d['guest_tabs'])==13 and len(d['guest_owner_runtimes'])==1,d
      assert d['guest_owner_panes']==result.setdefault('guest_owner_panes',d['guest_owner_panes'])
      assert d['guest_owner_runtimes']==result.setdefault('guest_owner_runtimes',d['guest_owner_runtimes'])
      assert not any(t['are_floating_panes_visible'] for t in d['guest_tabs']),d['guest_tabs']
      assert {p['id'] for p in d['guest_floating_technical']}==set(result['initial_helper_ids']),d['guest_floating_technical']
     else:assert not d['guest_floating_technical'],d['guest_floating_technical']
     assert not d['host_floating_technical'],d['host_floating_technical']
     (OUT/(label+'.json')).write_text(json.dumps(d,indent=2)+'\n');return d
    def local_tab(c,i):
     row=screen(c)[0];at=row.find(tab_name(i))
     for _ in range(count+1):
      if at>=0:break
      previous=row;os.write(c['fd'],b'\x1b[1;9D')
      wait(lambda:screen(c)[0]!=previous,'client-local previous-tab',seconds=5)
      row=screen(c)[0];at=row.find(tab_name(i))
     assert at>=0, ('client-local tab is not visible',i,row)
     os.write(c['fd'],f'\x1b[<0;{at+2};1M\x1b[<0;{at+2};1m'.encode())
     wait(lambda:any(f'SLOT{i:02}_PID' in line or f'ACK_SLOT{i:02}_' in line for line in screen(c)),f'client-local body {i}',seconds=30)
    def highlight(c,i):
     row=screen(c)[0];at=row.find(tab_name(i))
     return at>=0 and '◉' in row[max(0,at-4):at]
    def body_matches(c,i):
     pid=task_pids()[str(i)]
     return any(f'SLOT{i:02}_PID{pid}_READY' in line or (f'ACK_SLOT{i:02}_' in line and re.search(r'_PID'+str(pid)+r'\b',line)) for line in screen(c))
    def render_receipt(c,i):
     return {'target_body':bool(body_matches(c,i)),'target_chip':highlight(c,i),'top_row':screen(c)[0]}
    def host_input(c,i,label):
     token=f'BRIDGE_{label}_{i:02}';os.write(c['fd'],(token+'\r').encode());ack=f'ACK_SLOT{i:02}_{token}_PID{task_pids()[str(i)]}'
     wait(lambda:any(re.search(re.escape(ack)+r'\b',line) for line in screen(c)),f'ordinary task input {i}',seconds=30)
     return token
    try:
     provenance={'start':time.time(),'deadline_seconds':budget,'env':{r:{k:v for k,v in en.items() if k not in ['PATH','HOME','USER','LOGNAME','LANG']} for r,en in envs.items()},'guest':guest,'host':host,'binaries':{}}
     for role,b in [('old',OLD),('new',NEW)]:provenance['binaries'][role]={'path':str(b),'sha256':hashlib.sha256(b.read_bytes()).hexdigest(),'build_info':json.loads(cli(role,None,'--build-info'))}
     provenance['config_hashes']={'old_private':hashlib.sha256(oldcfg.read_bytes()).hexdigest(),'new_private':hashlib.sha256(cfg.read_bytes()).hexdigest()}
     provenance['harness_sha256']=hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
     (OUT/'provenance.json').write_text(json.dumps(provenance,indent=2)+'\n')
     oldlayout=args.legacy_product_layout.read_text() if product else cli('old',None,'setup','--dump-layout','default');at=oldlayout.rfind('}')
     if product:result['old_product_layout_sha256']=hashlib.sha256(oldlayout.encode()).hexdigest()
     tabs=[]
     for i in range(1,count+1):
      task=scratch/f'task-{i}.sh';task.write_text(f'#!/bin/sh\necho $$ > "{scratch}/pid-{i}"\nprintf "SLOT{i:02}_PID%s_READY\\n" "$$"\nwhile IFS= read -r line; do printf "ACK_SLOT{i:02}_%s_PID%s\\n" "$line" "$$"; done\n')
      if product and i==1:
       at_workspace=oldlayout.rfind('    }')
       oldlayout=oldlayout[:at_workspace]+f'        pane name="Task01" focus=true command="/bin/sh" {{ args "{task}"; }}\n        floating_panes {{ pane {{ plugin location="vc-frame:link"; }}; pane {{ plugin location="vc-frame:vc-tab-title"; }}; }}\n'+oldlayout[at_workspace:]
       at=oldlayout.rfind('}')
      else:tabs.append(f' tab name="Slot{i:02}"'+(' focus=true' if i==1 else '')+f' {{ pane name="Task{i:02}" command="/bin/sh" {{ args "{task}"; }}; }}\n')
     gl=scratch/'old-ten.kdl';gl.write_text(oldlayout[:at]+''.join(tabs)+oldlayout[at:]);(OUT/'old-ten.kdl').write_text(gl.read_text())
     original=spawn('old',guest,gl);wait(lambda:all((scratch/f'pid-{i}').exists() for i in range(1,count+1)),'all ten OLD tasks started')
     before=task_identity();result['tasks_before']=before;result['stages'].append('ten-old-tasks-running');save()
     cli('old',guest,'action','switch-mode','locked');cli('old',guest,'action','go-to-tab',1)
     if product:
      initial=json.loads(cli('old',guest,'action','list-panes','--all','--json'))
      floating=[p for p in initial if p.get('is_floating')]
      assert len(floating)==2 and len({p['tab_id'] for p in floating})==1 and all(p.get('plugin_url') in ['vc-frame:link','vc-frame:vc-tab-title'] and not p.get('is_suppressed') for p in floating),floating
      result['initial_helper_ids']=[p['id'] for p in floating];result['initial_visible_helpers']=floating
      (OUT/'initial-product-panes.json').write_text(json.dumps(initial,indent=2))
      # OLD supports only a tab-local floating-layer hide. Apply it solely
      # after proving this owned fixture layer contains exactly technical helpers.
      cli('old',guest,'action','hide-floating-panes','--tab-id',str(floating[0]['tab_id']))
      result['helper_resolution']='OLD tab-local layer hidden after complete technical-only inventory; no panes closed'
      ordinary_layout=scratch/'ordinary.kdl';ordinary_layout.write_text('layout { tab name="Ordinary" { pane; }; }')
      ordinary=spawn('new',other,ordinary_layout)
      wait(lambda:bool(re.search(r'^\s*\d+\s',cli('new',other,'action','list-clients'),re.M)),'ordinary guest client')
      cli('new',other,'action','detach');pump(.3)
     host_input(original,1,'ORIGINAL');cli('old',guest,'action','detach');pump(1)
     source=(ROOT/'zellij-utils/assets/layouts/vibecrafted-host.kdl').read_text();a=source.index('        pane name="Home" {');b=source.index('\n    tab name="Workspace"',a);hl=scratch/'host.kdl';hl.write_text(source[:a]+'        pane name="Home";\n    }\n'+source[b:]);(OUT/'host.kdl').write_text(hl.read_text())
     visual=spawn('new',host,hl);wait(lambda:bool(re.search(r'^\s*\d+\s',cli('new',host,'action','list-clients',check=False),re.M)),'new host client admitted')
     cli('new',host,'action','switch-mode','locked');cli('new',host,'action','go-to-tab-name','Workspace')
     snapshot('before-bridge')
     hostlog=Path(f'/tmp/vc-frame-{os.getuid()}/vc-frame-log')/host/'vc-frame.log'
     def rail_click(c,name):
      label=name[0].upper()+name[1:]
      wait(lambda:any(label in row[:24] for row in screen(c)),f'rail discoverability {name}',seconds=15)
      before_events=hostlog.read_text().splitlines() if hostlog.exists() else []
      row=next(n for n,line in enumerate(screen(c)) if label in line[:24]);col=screen(c)[row].index(label)+2
      os.write(c['fd'],f'\x1b[<0;{col};{row+1}M\x1b[<0;{col};{row+1}m'.encode())
      if name in [guest,other]:
       wait(lambda:hostlog.exists() and any('workspace_projection committed ' in e and f'guest={name} ' in e for e in hostlog.read_text().splitlines()[len(before_events):]),f'accepted rail selection {name}',seconds=45)
     def product_select(c,i):
      before_events=hostlog.read_text().splitlines() if hostlog.exists() else []
      # Sequential even cards use the physical shortcut; odd cards use mouse
      # bytes through the existing rail. No CLI projection rescues either path.
      if i%2==0:os.write(c['fd'],b'\x1b[1;9C')
      else:rail_click(c,tab_name(i))
      def committed():
       events=hostlog.read_text().splitlines()[len(before_events):] if hostlog.exists() else []
       return [e for e in events if 'workspace_projection committed ' in e and f'guest={guest} ' in e and f'observed_tab=Some({i-1})' in e]
      wait(committed,f'physical committed target {i}',seconds=45)
      return committed()[-1]
     if product:
      rail_click(visual,guest)
      wait(lambda:any('SLOT01_PID' in line for line in screen(visual)),'retained OLD product body',seconds=45)
      result['rail_discovery']=True;result['rail_initial_input']=host_input(visual,1,'RAIL_INITIAL')
      rail_click(visual,other)
      wait(lambda:any('Ordinary' in line for line in screen(visual)),'leave retained work for ordinary guest')
      other_token='ORDINARY_'+str(os.getpid());os.write(visual['fd'],f"printf '{other_token}\\n'\r".encode())
      wait(lambda:any(other_token in line and 'printf' not in line for line in screen(visual)),'ordinary guest typed ACK',seconds=30)
      result['ordinary_input']=other_token
      rail_click(visual,guest)
      wait(lambda:any('SLOT01_PID' in line or 'ACK_SLOT01_' in line for line in screen(visual)),'return retained old work')
      result['rail_return_input']=host_input(visual,1,'RAIL_RETURN');save()
     for i in range(1,count+1):
      t=time.monotonic();receipt={'tab':i,'name':tab_name(i),'mode':'locked' if product else None,'physical_action':('Super Right' if i%2==0 else 'rail click') if product else None}
      try:
       if product:
        receipt['physical_commit']=product_select(visual,i);receipt['at_physical_commit']=render_receipt(visual,i);receipt['strict_at_physical_commit']=all(receipt['at_physical_commit'][key] for key in ['target_body','target_chip'])
        wait(lambda:body_matches(visual,i),f'physical target body {i}',seconds=45)
        receipt['physical_input_token']=host_input(visual,i,'PHYSICAL');receipt['at_physical_input']=render_receipt(visual,i)
       response=None if product else cli('new',host,'project-workspace',guest,'--tab',str(i),check=False);receipt['projection_response']=response
       if not product:assert 'Handled' in response and 'refus' not in response.lower() and 'error' not in response.lower(),response
       receipt['at_post_input' if product else 'at_handled_response']=render_receipt(visual,i)
       wait(lambda:any(f'SLOT{i:02}_PID' in line or f'ACK_SLOT{i:02}_' in line for line in screen(visual)),f'target body Slot{i:02}',seconds=45)
       receipt['row_at_body']=screen(visual)[0];receipt['matching_at_body']=highlight(visual,i)
       convergence=time.monotonic()
       try:wait(lambda:highlight(visual,i),f'chrome selection {i}',seconds=2)
       except AssertionError:pass
       receipt['chrome_convergence_seconds']=time.monotonic()-convergence
       receipt['token']=host_input(visual,i,'VISIT');panes=json.loads(cli('old',guest,'action','list-tabs','--json'));receipt['guest_tabs']=panes
       receipt['host_top_row']=screen(visual)[0];receipt['host_highlight_matches']=highlight(visual,i)
       receipt['input_received']=True;receipt['tasks_preserved']=task_identity()==before;receipt['passed']=receipt['host_highlight_matches'] and receipt['tasks_preserved'] and receipt.get('strict_at_physical_commit',True) and all(receipt['at_post_input' if product else 'at_handled_response'][key] for key in ['target_chip','target_body']) and (not product or all(receipt['at_physical_input'][key] for key in ['target_chip','target_body']))
       (OUT/f'body-slot-{i:02}.txt').write_text('\n'.join(screen(visual)))
      except Exception as ex:receipt.update(passed=False,error=str(ex))
      receipt['seconds']=time.monotonic()-t;result['tabs'].append(receipt);save();snapshot(f'after-slot-{i:02}');print('TAB',i,receipt['passed'],receipt.get('error',''),flush=True)
      if receipt.get('error'):break
     result['stages'].append('bridge-attempt-ended');result['tasks_after_bridge']=task_identity();save()
     # Detach/re-attach the disposable host, never the old task server.
     cli('new',host,'action','detach',check=False);pump(1)
     result['tasks_after_host_detach']=task_identity();visual2=spawn('new',host);wait(lambda:bool(re.search(r'^\s*\d+\s',cli('new',host,'action','list-clients',check=False),re.M)),'reattached visual host')
     cli('new',host,'action','go-to-tab-name','Workspace')
     if product:cli('new',host,'action','switch-mode','normal')
     result['tasks_after_host_reattach']=task_identity();snapshot('host-reattached')
     result['reattached_tabs']=[]
     for r in range(args.bridge_rounds-1):
      for i in range(1,count+1):
       physical=product_select(visual2,i) if product else None
       at_commit=render_receipt(visual2,i) if product else None
       strict=all(at_commit[key] for key in ['target_body','target_chip']) if product else True
       if product:
        wait(lambda:body_matches(visual2,i),f'physical reattached target {i}',seconds=45)
        physical_token=host_input(visual2,i,'PHYSICAL_REATTACH');at_input=render_receipt(visual2,i)
       response=None if product else cli('new',host,'project-workspace',guest,'--tab',i)
       if not product:assert 'Handled' in response,response
       at_handled=render_receipt(visual2,i)
       wait(lambda:any(f'SLOT{i:02}_PID' in line or f'ACK_SLOT{i:02}_' in line for line in screen(visual2)),f'reattached body {i}',seconds=45)
       try:wait(lambda:highlight(visual2,i),f'reattached chrome {i}',seconds=2)
       except AssertionError:pass
       token=host_input(visual2,i,f'REATTACH{r}');preserved=task_identity()==before
       result['reattached_tabs'].append({'round':r,'tab':i,'mode':'normal' if product else None,'physical_action':('Super Right' if i%2==0 else 'rail click') if product else None,'row':screen(visual2)[0],'physical_commit':physical,'at_physical_commit':at_commit,'strict_at_physical_commit':strict,'projection_response':response,('at_post_input' if product else 'at_handled_response'):at_handled,
        'physical_input_token':physical_token if product else None,'at_physical_input':at_input if product else None,
        'token':token,'input_received':True,'tasks_preserved':preserved,
        'passed':highlight(visual2,i) and preserved and strict and all(at_handled[key] for key in ['target_chip','target_body']) and (not product or all(at_input[key] for key in ['target_chip','target_body']))});save();print('REATTACH',r,i,flush=True)
     cli('new',host,'action','detach',check=False);pump(1)
     rollback=spawn('old',guest);wait(lambda:bool(re.search(r'^\s*\d+\s',cli('old',guest,'action','list-clients',check=False),re.M)),'rollback old client')
     result['rollback_tabs']=[]
     for i in range(1,count+1):
      local_tab(rollback,i);host_input(rollback,i,'ROLLBACK');result['rollback_tabs'].append(i);print('ROLLBACK',i,flush=True)
     local_tab(rollback,1);host_input(rollback,1,'ORIGINAL_RETURN');result['rollback_input']=True;result['tasks_after_rollback']=task_identity();snapshot('rollback-original')
     result['all_tasks_same']=all(result.get(k)==before for k in ['tasks_after_bridge','tasks_after_host_detach','tasks_after_host_reattach','tasks_after_rollback']);result['accepted']=len(result['tabs'])==count and all(x['passed'] for x in result['tabs']) and len(result['reattached_tabs'])==count*(args.bridge_rounds-1) and all(x['passed'] for x in result['reattached_tabs']) and result['all_tasks_same'] and result['rollback_input'];save()
    except Exception as ex:result['probe_error']=str(ex);save();print('PROBE ERROR',ex,flush=True)
    finally:
     result['elapsed_seconds']=time.monotonic()-start;save()
     deadline=max(deadline,time.monotonic()+180)
     # Owned fixtures only; preservation verdicts were sealed BEFORE teardown.
     for role,session in [('new',host),('old',guest)]+([('new',other)] if product else []):cli(role,None,'kill-session',session,check=False)
     for c in clients:
      try:os.close(c['fd'])
      except OSError:pass
      try:
       until=time.monotonic()+45
       while os.waitpid(c['pid'],os.WNOHANG)==(0,0):
        if time.monotonic()>=until:
         os.kill(c['pid'],signal.SIGTERM);break
        time.sleep(.05)
      except ChildProcessError:pass
      c['raw'].close()
     cleanup_deadline=time.monotonic()+50
     while time.monotonic()<cleanup_deadline:
      ps=subprocess.check_output(['ps','-Ao','pid,command'],text=True);survivors=[x for x in ps.splitlines() if ('visit '+guest) in x or str(sockets) in x];live=[str(x) for x in sockets.rglob('*') if x.is_socket()]
      tasks_alive={k:v for k,v in (locals().get('before') or {}).items() if subprocess.run(['ps','-p',v.split()[0]],stdout=subprocess.DEVNULL).returncode==0}
      if not survivors and not live and not tasks_alive:break
      time.sleep(.2)
     (OUT/'cleanup.json').write_text(json.dumps({'live_sockets':live,'surviving_owned_servers_visitors':survivors,'surviving_task_identities':tasks_alive},indent=2)+'\n')
     for session in [guest,host]+([other] if product else []):
      lf=Path(f'/tmp/vc-frame-{os.getuid()}/vc-frame-log')/session/'vc-frame.log'
      if lf.exists():(OUT/(session+'.log')).write_text(lf.read_text())
     log.close()
    print(json.dumps({'accepted':result['accepted'],'tabs_proven':sum(x.get('passed',False) for x in result['tabs']),'all_tasks_same':result.get('all_tasks_same'),'rollback':result.get('rollback_input'),'elapsed':result.get('elapsed_seconds')}),flush=True)

    assert result['accepted'], result.get('probe_error', 'legacy bridge acceptance failed')

if __name__ == '__main__':
    main()
