"""Actual compact-bar mouse route in a private tmux PTY (no Python packages).

python3 default-plugins/compact-bar/tests/font_chooser_pty.py \
    --binary /owned/vc-frame --output /owned/receipt
Requires tmux. Add --workshop /canonical/vc-agent-workshop.py to exercise the
real sibling typography scene and explicit Apply with isolated font policy.
"""
import argparse
import hashlib
import json
import os
import runpy
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--workshop', type=Path,
                        help='Canonical source chooser; otherwise capture argv only')
    args = parser.parse_args()
    workshop = args.workshop.resolve() if args.workshop else None
    if workshop is not None and not workshop.is_file():
        parser.error('--workshop must name the canonical source script')
    args.output.mkdir(parents=True, exist_ok=False)
    scratch = Path(tempfile.mkdtemp(prefix='vcfont-', dir='/tmp')).resolve()
    binary = args.binary.resolve()
    tmux_bin = shutil.which('tmux')
    if tmux_bin is None:
        parser.error('tmux is required for private PTY screen capture')
    env = {key: os.environ[key] for key in ('PATH', 'USER', 'LOGNAME', 'LANG') if key in os.environ}
    env.update(TERM='xterm-256color', COLORTERM='truecolor', SHELL='/bin/sh')
    for key in ('HOME', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME',
                'XDG_STATE_HOME', 'XDG_RUNTIME_DIR', 'VIBECRAFTED_HOME',
                'VIBECRAFTED_CONTROL_PLANE', 'VC_FRAME_SOCKET_DIR', 'VC_FRAME_CONFIG_DIR'):
        path = scratch / key.lower()
        path.mkdir()
        env[key] = str(path)
    env['ZELLIJ_SOCKET_DIR'] = env['VC_FRAME_SOCKET_DIR']
    root = Path(env['VC_FRAME_CONFIG_DIR'])
    events = scratch / 'argv.jsonl'
    env.update(FONT_EVENTS=str(events), FONT_PYTHON=sys.executable)
    (root / 'pane-python').write_text('#!/bin/sh\nexec "$FONT_PYTHON" "$@"\n')
    (root / 'pane-python').chmod(0o700)
    fixture_script = '''import json, os, sys
with open(os.environ['FONT_EVENTS'], 'a') as target:
    target.write(json.dumps(sys.argv) + '\\n')
'''
    if workshop is None:
        fixture_script += '''import signal
print('CANONICAL FONT ARGV CAPTURE', flush=True)
signal.pause()
'''
    else:
        fixture_script += f'''import runpy
sys.argv = [{str(workshop)!r}, *sys.argv[1:]]
runpy.run_path({str(workshop)!r}, run_name='__main__')
'''
    (root / 'vc-agent-workshop.py').write_text(fixture_script)
    config = scratch / 'config.kdl'
    config.write_text('''default_shell "/bin/sh"
theme_dark "tokyo-night-dark"
theme_light "tokyo-night-light"
show_startup_tips false
show_release_notes false
session_serialization false
auto_lock_after_seconds 0
plugins { compact-bar location="zellij:compact-bar"; }
''')
    layout = scratch / 'layout.kdl'
    layout.write_text('''layout {
  pane size=1 borderless=true { plugin location="compact-bar" { left_inset "6"; }; }
  pane command="/bin/sh" name="Owned shell" { args "-c" "printf 'OWNED SHELL\\n'; exec /bin/sleep 300"; }
}
''')
    tmux_config = scratch / 'tmux.conf'
    tmux_config.write_text('set -g status off\nset -g default-terminal xterm-256color\nset -g allow-rename off\n')
    tmux_base = [tmux_bin, '-S', str(scratch / 'tmux.sock'), '-f', str(tmux_config)]
    session = f'font{os.getpid()}'
    base = [str(binary), '--config', str(config), '--config-dir', env['XDG_CONFIG_HOME']]
    commands = []
    receipt = {'status': 'failed', 'session': session, 'scratch': str(scratch),
               'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
               'limit': 'Private tmux PTY, actual screen capture and isolated argv fixture; native visual/font and installed sibling acceptance remain external.'}

    def run(argv, check=True):
        result = subprocess.run(argv, env=env, capture_output=True, text=True,
                                check=False, timeout=20)
        commands.append({'argv': argv, 'exit': result.returncode,
                         'stdout': result.stdout, 'stderr': result.stderr})
        if check and result.returncode:
            raise AssertionError(result.stderr or result.stdout)
        return result.stdout

    def cli(*argv, check=True):
        return run(base + list(argv), check=check)

    def tmux(*argv, check=True):
        return run(tmux_base + list(argv), check=check)

    def panes():
        data = json.loads(cli('--session', session, 'action', 'list-panes', '--all', '--json'))
        return data if isinstance(data, list) else data['panes']

    def screen():
        return tmux('capture-pane', '-p', '-t', 'probe:0.0')

    def top():
        return screen().splitlines()[0]

    def until(predicate, label):
        end = time.monotonic() + 30
        last_error = None
        while time.monotonic() < end:
            try:
                if predicate():
                    return
            except (AssertionError, ValueError, KeyError, StopIteration, IndexError) as error:
                last_error = str(error)
            time.sleep(0.2)
        raise AssertionError(f'timeout: {label}; last_error={last_error}; screen={screen()}')

    def click(col):
        # Literal mouse wire protocol into this owned Frame PTY, never keys
        # sent to a Founder session or an arbitrary focused agent terminal.
        tmux('send-keys', '-l', '-t', 'probe:0.0',
             f'\x1b[<0;{col + 1};1M\x1b[<0;{col + 1};1m')
        time.sleep(0.5)

    def snapshot(name):
        (args.output / f'{name}.screen.txt').write_text(screen())
        (args.output / f'{name}.panes.json').write_text(json.dumps(panes(), indent=2) + '\n')

    def fonts(rows):
        return [p for p in rows if p.get('title') == 'Aa · Terminal Typography']

    try:
        tmux('new-session', '-d', '-s', 'probe', '-x', '120', '-y', '32',
             shlex.join(base + ['--new-session-with-layout', str(layout), '--session', session]))
        tmux('pipe-pane', '-t', 'probe:0.0', 'cat > ' + shlex.quote(str(args.output / 'terminal.ansi')))
        until(lambda: 'Aa' in top() and '☾' in top(), 'mainbar ready')
        assert not events.exists(), 'startup must not launch the chooser'
        line = top()
        assert line.index('Aa') == line.index('☾') + 3, line
        snapshot('normal')
        click(line.index('☾'))
        until(lambda: '☼' in top(), 'theme click still toggles')
        assert not events.exists(), 'theme must not route to font chooser'
        click(top().index('☼'))
        until(lambda: '☾' in top(), 'theme toggles back')
        for width in (84, 81, 80, 85, 120):
            tmux('resize-window', '-t', 'probe:0', '-x', str(width), '-y', '32')
            def settled_toolbar(width=width):
                line = top()
                return (line.startswith('        𝚅') and
                        ('Aa' in line) == (width >= 85) and
                        ('☾' in line) == (width >= 81) and
                        all(label in line for label in ('Quick cmd', 'Panels', 'Composer')))
            until(settled_toolbar, f'width {width} shedding')
            line = top()
            assert all(label in line for label in ('Quick cmd', 'Panels', 'Composer')), line
            snapshot(f'width-{width}')
        assert not events.exists(), 'resize must not launch the chooser'
        click(top().index('Aa'))
        until(events.exists, 'canonical launcher argv')
        argv = json.loads(events.read_text().splitlines()[0])
        assert argv == [str(root / 'vc-agent-workshop.py'), 'launcher', '--font'], argv
        until(lambda: len(fonts(panes())) == 1, 'font pane title')
        first = panes()
        font = fonts(first)[0]
        assert font['is_floating'] and not font['is_plugin'], font
        ready_text = 'Enter apply' if workshop else 'CANONICAL FONT ARGV CAPTURE'
        until(lambda: ready_text in screen(), 'chooser visible')
        snapshot('font-open')
        # Focus the owned shell first: the repeat click must really refocus.
        cli('--session', session, 'action', 'focus-pane-id', 'terminal_0')
        click(top().index('Aa') + 1)
        second = panes()
        assert len(fonts(second)) == 1 and fonts(second)[0]['id'] == font['id'], second
        assert fonts(second)[0]['is_focused'] and not fonts(second)[0]['is_suppressed'], second
        assert len(events.read_text().splitlines()) == 1, 'repeat click must not spawn'
        snapshot('font-refocused')
        lifecycle = []
        for cycle in range(2):
            current = fonts(panes())[0]
            target = f"terminal_{current['id']}"
            if cycle == 0:
                # Remove the discovery title, then close through the real host.
                cli('--session', session, 'action', 'rename-pane', 'sh', '--pane-id', target)
                until(lambda: not fonts(panes()), 'untitled pane publication')
            cli('--session', session, 'action', 'close-pane', '--pane-id', target)
            until(lambda current=current: all(
                p['is_plugin'] or p['id'] != current['id'] for p in panes()),
                  'owned chooser removed')
            before = len(events.read_text().splitlines())
            tmux('resize-window', '-t', 'probe:0', '-x', '121', '-y', '32')
            until(lambda: 'Aa' in top(), 'toolbar after close')
            assert len(events.read_text().splitlines()) == before, 'close/resize must not reopen'
            click(top().index('Aa'))
            until(lambda before=before: len(fonts(panes())) == 1 and
                  len(events.read_text().splitlines()) == before + 1, 'explicit reopen')
            reopened = fonts(panes())[0]
            assert reopened['is_focused'] and reopened['is_floating'], reopened
            click(top().index('Aa'))
            assert len(fonts(panes())) == 1, 'reopened chooser remains singleton'
            assert len(events.read_text().splitlines()) == before + 1
            lifecycle.append({'closed': current, 'reopened': reopened})
            snapshot(f'font-reopened-{cycle}')
        receipt['lifecycle'] = lifecycle
        if workshop is not None:
            policy = Path(env['XDG_CONFIG_HOME']) / 'vibecrafted/terminal-policy.toml'
            assert not policy.exists(), 'opening and closing must not apply a font'
            backend = runpy.run_path(str(workshop))['command_bridge_onboarding']
            chosen = next(item for item in backend.inspect_font_variants(
                policy_path=policy) if item.selectable)
            expected_faces = [
                {'family': chosen.family, 'style': style} for style in chosen.face_styles
            ]
            until(lambda: 'Enter apply' in screen(), 'real chooser ready after reopen')
            tmux('send-keys', '-l', '-t', 'probe:0.0', '\r')
            def applied():
                if not policy.is_file():
                    return False
                font = tomllib.loads(policy.read_text())['font']
                return [font[key] for key in ('normal', 'bold', 'italic')] == expected_faces
            until(applied, 'explicit Apply persists genuine selected faces')
            snapshot('font-applied')
            receipt.update(real_workshop=str(workshop),
                           workshop_sha256=hashlib.sha256(workshop.read_bytes()).hexdigest(),
                           applied_font=tomllib.loads(policy.read_text())['font'],
                           limit='Actual source chooser in a private Frame/tmux PTY; '
                                 'installed desktop, signing and consent remain external.')
        receipt.update(status='passed', argv=argv, panes_before=first, panes_after=second,
                       checks=['adjacent ASCII Aa in actual PTY', 'no startup/resize launch',
                               'theme round trip', '84/81/80/85/120 width shedding',
                               'real mouse canonical argv',
                               'visible real typography scene and explicit Apply'
                               if workshop else 'visible floating argv fixture',
                               'repeat click singleton and refocus',
                               'untitled and titled close/reopen', 'no passive reopen'])
    except (AssertionError, OSError, ValueError, KeyError, StopIteration, IndexError,
            subprocess.TimeoutExpired) as error:
        receipt['error'] = str(error)
    finally:
        cli('kill-session', session, check=False)
        tmux('kill-server', check=False)
        (args.output / 'commands.json').write_text(json.dumps(commands, indent=2) + '\n')
        (args.output / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
        print(json.dumps(receipt, indent=2))
    return 0 if receipt['status'] == 'passed' else 1


if __name__ == '__main__':
    raise SystemExit(main())
