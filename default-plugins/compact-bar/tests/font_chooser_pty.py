"""Actual compact-bar mouse route in a private tmux PTY (no Python packages).

python3 default-plugins/compact-bar/tests/font_chooser_pty.py \
    --binary /owned/vc-frame --output /owned/receipt
Requires tmux. Proves Frame routing, not the sibling Agents typography scene.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
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
    (root / 'vc-agent-workshop.py').write_text('''import json, os, signal, sys
with open(os.environ['FONT_EVENTS'], 'a') as target:
    target.write(json.dumps(sys.argv) + '\\n')
print('CANONICAL FONT ARGV CAPTURE', flush=True)
signal.pause()
''')
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
        result = subprocess.run(argv, env=env, capture_output=True, text=True, timeout=20)
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
            def settled_toolbar():
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
        until(lambda: 'CANONICAL FONT ARGV CAPTURE' in screen(), 'fixture visible')
        snapshot('font-open')
        # Focus the owned shell first: the repeat click must really refocus.
        cli('--session', session, 'action', 'focus-pane-id', 'terminal_0')
        click(top().index('Aa') + 1)
        second = panes()
        assert len(fonts(second)) == 1 and fonts(second)[0]['id'] == font['id'], second
        assert fonts(second)[0]['is_focused'] and not fonts(second)[0]['is_suppressed'], second
        assert len(events.read_text().splitlines()) == 1, 'repeat click must not spawn'
        snapshot('font-refocused')
        receipt.update(status='passed', argv=argv, panes_before=first, panes_after=second,
                       checks=['adjacent ASCII Aa in actual PTY', 'no startup/resize launch',
                               'theme round trip', '84/81/80/85/120 width shedding',
                               'real mouse canonical argv', 'visible floating fixture',
                               'repeat click singleton and refocus'])
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
