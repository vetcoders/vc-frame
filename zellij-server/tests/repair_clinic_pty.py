"""Owned, isolated acceptance of `vc-frame doctor` and `repair servers|plugins|sockets|layouts`.

python3 zellij-server/tests/repair_clinic_pty.py \
  --clinic /path/to/new/vc-frame --baseline /path/to/unchanged/vc-frame \
  --old-engine /path/to/older/vc-frame --output /new/receipt/dir

Every session, socket, lease and process lives under one private scratch root
(VC_FRAME_SOCKET_DIR, HOME, config, cache, TMPDIR). Sessions are torn down by
name inside that root only; no Founder session is ever addressed, because the
clinic's socket namespace IS the scratch one.

The agent pane runs a small program that records its pid and echoes every
input line to a log — "the same agent process" is proven by pid AND by the
same pid answering input after a repair.
"""
import argparse
import hashlib
import json
import os
import pty
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

AGENT = r'''
import os, sys
log = sys.argv[1]
with open(log, "a") as out:
    out.write(f"start pid={os.getpid()}\n")
print(f"AGENT READY pid={os.getpid()}", flush=True)
for line in sys.stdin:
    with open(log, "a") as out:
        out.write(f"echo pid={os.getpid()} {line.strip()}\n")
    print(f"AGENT ECHO {line.strip()}", flush=True)
'''

CONFIG = '''
default_shell "/bin/sh"
default_mode "locked"
show_startup_tips false
show_release_notes false
session_serialization false
auto_lock_after_seconds 0
plugins {
    compact-bar location="zellij:compact-bar"
    status-bar location="zellij:status-bar"
    session-manager location="zellij:session-manager"
}
'''


def sha(data):
    return hashlib.sha256(data).hexdigest()


# Serialized plugin configuration is string-valued: `rail "true"`.
RAIL = re.compile(r'\brail\s+"?true"?')


def tab_block(dumped, name):
    """The text of one `tab … name="<name>" …` node of a dump-layout."""
    match = re.search(r'\btab\b[^\n{]*\bname="' + re.escape(name) + '"', dumped)
    if not match:
        return ''
    start = match.start()
    depth, index = 0, dumped.index('{', start)
    for index in range(index, len(dumped)):
        depth += {'{': 1, '}': -1}.get(dumped[index], 0)
        if depth == 0:
            break
    return dumped[start:index + 1]


class Fixture:
    def __init__(self, args):
        self.args = args
        self.out = args.output
        self.out.mkdir(parents=True, exist_ok=False)
        # Short root: Unix socket paths are limited to 104 bytes on macOS.
        self.root = Path(tempfile.mkdtemp(prefix='vcrc-', dir='/tmp')).resolve()
        env = {k: os.environ[k] for k in ('PATH', 'USER', 'LOGNAME', 'LANG') if k in os.environ}
        env.update(TERM='xterm-256color', SHELL='/bin/sh')
        for key in ('HOME', 'TMPDIR', 'XDG_CONFIG_HOME', 'XDG_CACHE_HOME', 'XDG_DATA_HOME',
                    'XDG_STATE_HOME', 'VIBECRAFTED_HOME', 'VC_FRAME_CONFIG_DIR'):
            path = self.root / key.lower()
            path.mkdir()
            env[key] = str(path)
        self.sock = self.root / 'sock'
        self.sock.mkdir()
        env['VC_FRAME_SOCKET_DIR'] = env['ZELLIJ_SOCKET_DIR'] = str(self.sock)
        self.namespace = self.sock / 'contract_version_4'
        self.namespace.mkdir()
        self.env = env
        (self.root / 'agent.py').write_text(AGENT)
        self.cfg = self.root / 'config.kdl'
        self.cfg.write_text(CONFIG)
        self.commands = (self.out / 'commands.jsonl').open('w')
        self.sessions = []  # (engine, name)
        self.helpers = []
        self.pty = None
        self.checks = {}
        self.facts = {}

    # -------------------------------------------------------------- running --
    def run(self, binary, *argv, timeout=60, input_text=None):
        command = [str(binary), '--config', str(self.cfg),
                   '--config-dir', self.env['XDG_CONFIG_HOME'], *argv]
        started = time.monotonic()
        try:
            proc = subprocess.run(command, env=self.env, capture_output=True, text=True,
                                  timeout=timeout, input=input_text)
            code, stdout, stderr = proc.returncode, proc.stdout, proc.stderr
        except subprocess.TimeoutExpired as error:
            code, stdout, stderr = 124, error.stdout or '', f'TIMEOUT after {timeout}s'
        row = {'binary': Path(binary).name, 'argv': list(argv), 'exit': code,
               'seconds': round(time.monotonic() - started, 2),
               'stdout': stdout[-6000:], 'stderr': stderr[-3000:]}
        self.commands.write(json.dumps(row) + '\n')
        self.commands.flush()
        return code, stdout, stderr

    def check(self, name, value, evidence=None):
        self.checks[name] = bool(value)
        if evidence is not None:
            self.facts[name] = evidence
        mark = 'PASS' if value else 'FAIL'
        print(f'[{mark}] {name}' + (f' — {evidence}' if evidence is not None and not value else ''),
              flush=True)
        return value

    # ------------------------------------------------------------- sessions --
    def layout(self, engine, rail=True, brand=None):
        """The engine's own built-in `vibecrafted` chrome around a private
        workload tab: a shell and an agent, side by side."""
        _, text, _ = self.run(engine, 'setup', '--dump-layout', 'vibecrafted')
        layer = text[text.index('session_layer'):text.index('default_tab_template')]
        if not rail:
            layer = layer.replace('rail true', '', 1)
        if brand is not None:
            layer = re.sub(r'brand_text "[^"]*"', f'brand_text "{brand}"', layer, count=1)
        python = sys.executable
        return ('layout {\n    ' + layer + '''
    default_tab_template {
        pane { children; }
    }
    tab name="Work" focus=true {
        pane split_direction="vertical" {
            pane name="shell"
            pane name="agent" command="%s" {
                args "%s" "@AGENT_LOG@"
            }
        }
    }
}
''' % (python, self.root / 'agent.py'))

    def start(self, engine, name, layout_text):
        layout = self.root / f'{name}.kdl'
        layout.write_text(layout_text.replace('@AGENT_LOG@', str(self.agent_log(name))))
        code, out, err = self.run(engine, '--layout', str(layout), 'attach',
                                  '--create-background', name, timeout=60)
        self.sessions.append((engine, name))
        deadline = time.monotonic() + 40
        while time.monotonic() < deadline:
            panes = self.panes(engine, name)
            agent = [p for p in panes if 'agent.py' in str(p.get('terminal_command') or '')]
            if agent and self.agent_pid(name) and len(
                    [p for p in panes if p.get('is_plugin')]) >= 3:
                return True
            time.sleep(0.5)
        raise AssertionError(f'session {name} did not come up: {code} {out} {err}')

    def panes(self, engine, name):
        code, out, _ = self.run(engine, '--session', name, 'action', 'list-panes', '--json',
                                '--all', timeout=20)
        if code != 0:
            return []
        value = json.loads(out)
        return value if isinstance(value, list) else value.get('panes', [])

    def agent_log(self, name):
        return self.root / f'agent-{name}.log'

    def agent_pid(self, name):
        log = self.agent_log(name)
        if not log.exists():
            return None
        starts = [int(m) for m in re.findall(r'start pid=(\d+)', log.read_text())]
        return starts[-1] if starts else None

    def peer(self, name):
        """The listener pid the kernel names for a session endpoint."""
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.settimeout(2)
            s.connect(str(self.namespace / name))
            return struct.unpack('i', s.getsockopt(0, 2, 4))[0]  # SOL_LOCAL, LOCAL_PEERPID
        except OSError:
            return None
        finally:
            s.close()

    def lstart(self, pid):
        out = subprocess.run(['ps', '-o', 'lstart=', '-p', str(pid)], capture_output=True,
                             text=True).stdout.strip()
        return out or None

    def workload_processes(self, pid):
        """The long-lived pane processes under a server: the shell and the
        agent. Short helpers the chrome spawns (vitals) come and go."""
        table = subprocess.run(['ps', '-axo', 'pid=,ppid=,command='], capture_output=True,
                               text=True).stdout.split('\n')
        found = []
        for row in table:
            parts = row.split(None, 2)
            if len(parts) == 3 and int(parts[1]) == pid and (
                    parts[2].strip() == '/bin/sh' or 'agent.py' in parts[2]):
                found.append((int(parts[0]), self.lstart(int(parts[0])),
                              'agent' if 'agent.py' in parts[2] else 'shell'))
        return sorted(found)

    def workload(self, engine, name):
        """Everything that must survive a repair, in one comparable record."""
        server = self.peer(name)
        panes = self.panes(engine, name)
        agent = self.agent_pid(name)
        return {
            'server_pid': server,
            'server_start': self.lstart(server) if server else None,
            'workload': self.workload_processes(server) if server else [],
            'agent_pid': agent,
            'agent_start': self.lstart(agent) if agent else None,
            'socket_inode': os.stat(self.namespace / name).st_ino,
            'panes': sorted(
                (p['id'], p['is_plugin'], p['tab_id'], p['tab_name'], p['pane_x'], p['pane_y'],
                 p['pane_columns'], p['pane_rows'], p.get('plugin_url'))
                for p in panes),
        }

    def echo(self, engine, name, token):
        """Write a line to the agent pane; prove the same pid answered it."""
        panes = self.panes(engine, name)
        agent = next(p for p in panes if 'agent.py' in str(p.get('terminal_command') or ''))
        self.run(engine, '--session', name, 'action', 'write-chars', '--pane-id',
                 f"terminal_{agent['id']}", token + '\n')
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            echoed = re.findall(r'echo pid=(\d+) ' + re.escape(token),
                                self.agent_log(name).read_text())
            if echoed:
                return int(echoed[-1])
            time.sleep(0.2)
        return None

    def attach_pty(self, engine, name, columns=160, rows=40):
        """A real client in a native PTY, so geometry is a client's geometry."""
        import fcntl
        import termios
        pid, fd = pty.fork()
        if pid == 0:
            os.execve(str(engine), [str(engine), '--config', str(self.cfg), '--config-dir',
                                    self.env['XDG_CONFIG_HOME'], 'attach', name], self.env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack('HHHH', rows, columns, 0, 0))
        self.pty = (pid, fd, (self.out / f'{name}.pty.ansi').open('wb'))
        self.pump(4)

    def pump(self, seconds):
        if not self.pty:
            time.sleep(seconds)
            return
        _, fd, raw = self.pty
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if select.select([fd], [], [], 0.1)[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                raw.write(data)

    # ------------------------------------------------------------- endpoints --
    def orphan(self, name, namespace=None):
        """A socket file nobody will ever answer on — a crashed server's leftover."""
        path = (namespace or self.namespace) / name
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.bind(str(path))
        s.close()
        st = os.stat(path)
        return (st.st_dev, st.st_ino)

    def hold_lease(self, name):
        script = ('import fcntl,os,sys,time\n'
                  'p=sys.argv[1]\nos.makedirs(os.path.dirname(p),exist_ok=True)\n'
                  'f=open(p,"a+")\nfcntl.flock(f,fcntl.LOCK_EX)\nprint("held",flush=True)\n'
                  'time.sleep(600)\n')
        lock = self.sock / '.vc-frame-socket-leases' / 'contract_version_4' / f'{name}.lock'
        helper = subprocess.Popen([sys.executable, '-c', script, str(lock)],
                                  stdout=subprocess.PIPE, text=True)
        helper.stdout.readline()
        self.helpers.append(helper)

    def foreign_listener(self, name):
        script = ('import socket,sys,time\n'
                  's=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)\ns.bind(sys.argv[1])\n'
                  's.listen(8)\nprint("listening",flush=True)\n'
                  'while True:\n    c,_=s.accept()\n    c.close()\n')
        helper = subprocess.Popen([sys.executable, '-c', script, str(self.namespace / name)],
                                  stdout=subprocess.PIPE, text=True)
        helper.stdout.readline()
        self.helpers.append(helper)

    def dump(self, engine, name, label):
        """A full dump-layout, kept as a private raw snapshot. The stock CLI
        gives up when the route's 1s budget elapses before the dump arrives
        (debug build, attached client), so ask again rather than keep an
        empty snapshot."""
        out = ''
        for _ in range(5):
            code, out, err = self.run(engine, '--session', name, 'action', 'dump-layout',
                                      timeout=30)
            if out.strip():
                break
            time.sleep(1)
        (self.out / f'{name}.{label}.dump.kdl').write_text(out)
        return out

    def clinic(self, *argv, binary=None, timeout=120):
        code, out, err = self.run(binary or self.args.clinic, *argv, timeout=timeout)
        document = None
        if '--json' in argv:
            try:
                document = json.loads(out)
            except ValueError:
                document = None
        return code, document, out, err

    # ------------------------------------------------------------- teardown --
    def close(self):
        for engine, name in reversed(self.sessions):
            self.run(engine, 'kill-session', name, timeout=20)
        if self.pty:
            pid, fd, raw = self.pty
            # Close the master first: an exiting client drains its tty, and
            # nobody reads a master we are about to wait on.
            os.close(fd)
            raw.close()
            for sig in (signal.SIGTERM, signal.SIGKILL):
                try:
                    os.kill(pid, sig)
                except ProcessLookupError:
                    break
                deadline = time.monotonic() + 3
                while time.monotonic() < deadline:
                    try:
                        if os.waitpid(pid, os.WNOHANG)[0]:
                            break
                    except ChildProcessError:
                        break
                    time.sleep(0.1)
                else:
                    continue
                break
        for helper in self.helpers:
            helper.kill()
            helper.wait()
        self.commands.close()


def red(fx):
    """The unchanged baseline: the verbs do not exist, doctor does not see."""
    for verb in ('servers', 'plugins', 'sockets', 'layouts'):
        code, _, out, err = fx.clinic('repair', verb, '--dry-run', binary=fx.args.baseline)
        fx.check(f'red_baseline_has_no_repair_{verb}', code != 0 and (
            'unrecognized' in (out + err).lower() or 'found argument' in (out + err).lower()
            or 'invalid' in (out + err).lower()), (code, (out + err)[-200:]))
    code, document, _, _ = fx.clinic('doctor', '--json', binary=fx.args.baseline)
    ids = sorted({f['id'] for f in (document or {}).get('findings', [])})
    fx.facts['red_baseline_doctor_finding_ids'] = ids
    fx.check('red_baseline_doctor_is_blind_to_runtime_surfaces',
             not {'socket-endpoints', 'plugin-generation', 'layout-contract'} & set(ids), ids)


def sockets(fx):
    live = 'sk-live'
    fx.start(fx.args.clinic, live, fx.layout(fx.args.clinic))
    before = fx.workload(fx.args.clinic, live)
    one = fx.orphan('sk-orphan1')
    fx.hold_lease('sk-leased')
    fx.orphan('sk-leased')
    fx.foreign_listener('sk-foreign')
    (fx.namespace / 'sk-file').write_text('not a socket')

    code, document, _, _ = fx.clinic('doctor', '--json')
    found = {(f['id'], f['severity']) for f in document['findings']} if document else set()
    titles = ' | '.join(f['title'] for f in (document or {}).get('findings', []))
    fx.check('doctor_names_the_orphan', ('socket-endpoints', 'warn') in found
             and 'sk-orphan1' in titles, titles)
    fx.check('doctor_names_the_foreign_listener', 'sk-foreign' in titles and 'foreign-owner' in titles,
             titles)

    code, document, _, _ = fx.clinic('repair', 'sockets', '--dry-run', '--json')
    fx.check('dry_run_targets_the_only_orphan', code == 0 and document['verdict'] == 'would-repair'
             and document['scope'].get('session') == 'sk-orphan1', document)
    fx.check('dry_run_changed_nothing', (os.stat(fx.namespace / 'sk-orphan1').st_dev,
                                         os.stat(fx.namespace / 'sk-orphan1').st_ino) == one)

    fx.orphan('sk-orphan2')
    code, document, _, _ = fx.clinic('repair', 'sockets', '--json')
    fx.check('two_orphans_need_a_choice', code == 2 and document['verdict'] == 'needs-selection'
             and len(document['choices']) == 2
             and (fx.namespace / 'sk-orphan1').exists() and (fx.namespace / 'sk-orphan2').exists(),
             document)

    code, document, _, _ = fx.clinic('repair', 'sockets', '--session', 'sk-orphan1', '--json')
    fx.check('orphan_repaired_and_verified', code == 0 and document['verdict'] == 'repaired'
             and document['steps'][0]['state'] == 'verified'
             and not (fx.namespace / 'sk-orphan1').exists(), document)
    code, document, _, _ = fx.clinic('repair', 'sockets', '--session', 'sk-orphan1', '--json')
    fx.check('repeat_is_a_clean_no_op', code == 0 and document['verdict'] == 'healthy', document)

    for name, label in (('sk-leased', 'rebinding'), ('sk-foreign', 'foreign-owner'),
                        ('sk-file', 'protected')):
        code, document, _, _ = fx.clinic('repair', 'sockets', '--session', name, '--json')
        fx.check(f'{name}_is_protected', code == 2 and (fx.namespace / name).exists()
                 and document['steps'][0]['state'] == 'refused', document)
    code, document, _, _ = fx.clinic('repair', 'sockets', '--session', live, '--json')
    fx.check('live_endpoint_is_healthy_and_untouched', code == 0 and document['verdict'] == 'healthy')

    # Failure in the middle: one name in two namespaces, one of them read-only.
    legacy = fx.sock / 'contract_version_3'
    legacy.mkdir()
    fx.orphan('sk-twin')
    fx.orphan('sk-twin', legacy)
    os.chmod(legacy, 0o500)
    try:
        code, document, _, _ = fx.clinic('repair', 'sockets', '--session', 'sk-twin', '--json')
    finally:
        os.chmod(legacy, 0o700)
    states = sorted(step['state'] for step in document['steps'])
    fx.check('mid_failure_is_an_honest_partial', code == 1 and document['verdict'] == 'partial'
             and states == ['failed', 'verified'] and (legacy / 'sk-twin').exists()
             and not (fx.namespace / 'sk-twin').exists(), document)

    after = fx.workload(fx.args.clinic, live)
    fx.check('socket_repairs_left_the_live_session_identical', after == before, (before, after))


def servers_and_plugins(fx):
    old, new = 'sv-old', 'sv-new'
    fx.start(fx.args.old_engine, old, fx.layout(fx.args.old_engine))
    fx.start(fx.args.clinic, new, fx.layout(fx.args.clinic))
    before = fx.workload(fx.args.old_engine, old)
    token_before = fx.echo(fx.args.old_engine, old, 'before-servers')

    code, document, _, _ = fx.clinic('doctor', '--json')
    titles = ' | '.join(f"{f['id']}:{f['title']}" for f in (document or {}).get('findings', []))
    fx.check('doctor_names_the_superseded_engine', f'server-drift:session {old} runs' in titles
             and f'session {new} runs' not in titles, titles)
    fx.check('doctor_names_the_plugin_generation', f'plugin-generation:session {old}' in titles,
             titles)

    code, document, _, _ = fx.clinic('repair', 'servers', '--session', new, '--json')
    fx.check('current_engine_is_healthy', code == 0 and document['verdict'] == 'healthy', document)
    code, document, _, _ = fx.clinic('repair', 'servers', '--session', old, '--json')
    step = (document or {}).get('steps', [{}])[0]
    fx.check('drifted_engine_is_refused_by_name', code == 2 and document['verdict'] == 'refused'
             and str(step.get('missing_capability', '')).startswith('engine-handover'), document)
    code, document, _, _ = fx.clinic('repair', 'servers', '--json')
    fx.check('several_live_sessions_need_a_choice', code == 2
             and document['verdict'] == 'needs-selection', document)

    code, document, out, _ = fx.clinic('repair', 'plugins', '--session', new, '--dry-run', '--json')
    fx.check('intended_plugin_generation_is_healthy', code == 0 and document['verdict'] == 'healthy',
             document)
    fx.facts['plugin_digests_new_engine'] = [n for n in document['notes'] if n.startswith('  ')]
    code, document, _, _ = fx.clinic('repair', 'plugins', '--session', old, '--dry-run', '--json')
    refused = [s for s in document['steps'] if s['state'] == 'refused']
    fx.check('older_plugin_generation_is_refused_not_reloaded', code == 2 and refused
             and all(str(s.get('missing_capability', '')).startswith('engine-handover')
                     for s in refused), document)
    fx.facts['plugin_digests_old_engine'] = [n for n in document['notes'] if n.startswith('  ')]

    after = fx.workload(fx.args.old_engine, old)
    token_after = fx.echo(fx.args.old_engine, old, 'after-servers')
    fx.check('refusals_left_the_old_session_identical', after == before, (before, after))
    fx.check('the_same_agent_answers_before_and_after',
             token_before == token_after == before['agent_pid'] and token_before is not None,
             (token_before, token_after, before['agent_pid']))


def foreign_bytes(fx):
    """A session whose layout points a plugin at missing and at foreign bytes."""
    name = 'pl-files'
    dump = fx.root / 'old-plugins'
    fx.run(fx.args.old_engine, 'setup', '--dump-plugins', str(dump))
    foreign = fx.root / 'foreign-status.wasm'
    foreign.write_bytes((dump / 'plugins' / 'status-bar.wasm').read_bytes())
    layout = fx.layout(fx.args.clinic).replace(
        'tab name="Work" focus=true {',
        'tab name="Files" {\n        pane {\n            plugin location="file:%s"\n        }\n'
        '        pane {\n            plugin location="file:%s"\n        }\n    }\n'
        '    tab name="Work" focus=true {' % (foreign, fx.root / 'missing.wasm'))
    fx.start(fx.args.clinic, name, layout)
    code, document, _, _ = fx.clinic('repair', 'plugins', '--session', name, '--json')
    evidence = [line for s in (document or {}).get('steps', []) for line in s['evidence']]
    fx.check('missing_bytes_are_refused_by_name', any(e.startswith('missing bytes') for e in evidence),
             evidence)
    fx.check('foreign_bytes_are_refused_by_name', any(
        e.startswith('foreign bytes') for e in evidence), evidence)


def layouts(fx):
    name, old = 'ly-drift', 'ly-old'
    fx.start(fx.args.clinic, name, fx.layout(fx.args.clinic, rail=False, brand=' Fixture '))
    fx.attach_pty(fx.args.clinic, name)
    before = fx.workload(fx.args.clinic, name)
    agent_before = fx.echo(fx.args.clinic, name, 'before-layouts')
    fx.dump(fx.args.clinic, name, 'before-repair')
    _, status_before, _ = fx.run(fx.args.clinic, '--session', name, 'action', 'override-layout',
                                 '--template-status')

    code, document, _, _ = fx.clinic('doctor', '--json')
    titles = ' | '.join(f"{f['id']}:{f['title']}" for f in (document or {}).get('findings', []))
    fx.check('doctor_names_the_template_drift', f"layout-contract:{name}'s session template" in titles,
             titles)

    code, document, _, _ = fx.clinic('repair', 'layouts', '--session', name, '--dry-run', '--json')
    _, status_dry, _ = fx.run(fx.args.clinic, '--session', name, 'action', 'override-layout',
                              '--template-status')
    states = sorted((s['state'], s.get('missing_capability') or '') for s in document['steps'])
    fx.check('layout_dry_run_plans_and_changes_nothing', code == 1
             and document['verdict'] == 'partial' and status_dry == status_before
             and states[0][0] == 'planned'
             and states[1][1].startswith('chrome-authority-reconfiguration'), document)

    code, document, _, _ = fx.clinic('repair', 'layouts', '--session', name, '--json')
    by_state = {s['state']: s for s in document['steps']}
    fx.check('layout_template_verified_running_chrome_refused_by_name', code == 1
             and document['verdict'] == 'partial' and 'verified' in by_state
             and str(by_state.get('refused', {}).get('missing_capability', '')).startswith(
                 'chrome-authority-reconfiguration'), document)
    fx.pump(2)
    dumped = fx.dump(fx.args.clinic, name, 'after-repair')
    layer = dumped[dumped.find('session_layer'):]
    layer = layer[:layer.find('new_tab_template')] if 'new_tab_template' in layer else layer
    fx.check('template_now_holds_the_rail_role', bool(RAIL.search(layer)), layer[:1200])
    fx.check('personal_brand_survived', 'brand_text " Fixture "' in dumped)
    after = fx.workload(fx.args.clinic, name)
    agent_after = fx.echo(fx.args.clinic, name, 'after-layouts')
    fx.check('layout_repair_left_every_pane_process_and_geometry', after == before, (before, after))
    fx.check('the_same_agent_answers_after_the_layout_repair',
             agent_before == agent_after == before['agent_pid'] and agent_before is not None,
             (agent_before, agent_after))

    _, status_repeat_before, _ = fx.run(fx.args.clinic, '--session', name, 'action',
                                        'override-layout', '--template-status')
    code, document, _, _ = fx.clinic('repair', 'layouts', '--session', name, '--json')
    _, status_repeat_after, _ = fx.run(fx.args.clinic, '--session', name, 'action',
                                       'override-layout', '--template-status')
    fx.check('layout_repeat_publishes_nothing_and_keeps_the_refusal', code == 2
             and document['verdict'] == 'refused'
             and all(s['state'] == 'refused' for s in document['steps'])
             and status_repeat_before == status_repeat_after, (document, status_repeat_before,
                                                               status_repeat_after))

    fx.run(fx.args.clinic, '--session', name, 'action', 'new-tab', '--name', 'Future')
    fx.pump(3)
    dumped = fx.dump(fx.args.clinic, name, 'future-tab')
    future = tab_block(dumped, 'Future')
    fx.check('a_future_tab_gets_the_repaired_chrome', bool(RAIL.search(future)), future[:1500])

    fx.start(fx.args.old_engine, old, fx.layout(fx.args.old_engine, rail=False))
    old_before = fx.workload(fx.args.old_engine, old)
    code, document, _, _ = fx.clinic('repair', 'layouts', '--session', old, '--json')
    step = (document or {}).get('steps', [{}])[-1]
    fx.check('an_engine_without_template_only_adoption_refuses_by_name', code == 2
             and str(step.get('missing_capability', '')).startswith('template-only adoption'),
             document)
    fx.check('the_refusing_old_session_is_untouched',
             fx.workload(fx.args.old_engine, old) == old_before)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--clinic', type=Path, required=True)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--old-engine', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--only', choices=('red', 'sockets', 'servers', 'files', 'layouts'))
    args = parser.parse_args()
    fx = Fixture(args)
    receipt = {'status': 'failed', 'scratch': str(fx.root)}
    for role in ('clinic', 'baseline', 'old_engine'):
        binary = getattr(args, role)
        _, info, _ = fx.run(binary, '--build-info')
        receipt[role] = {'path': str(binary), 'build': json.loads(info)}
    scenarios = {'red': red, 'sockets': sockets, 'servers': servers_and_plugins,
                 'files': foreign_bytes, 'layouts': layouts}
    try:
        for key, scenario in scenarios.items():
            if args.only and key != args.only:
                continue
            print(f'== {key}', flush=True)
            try:
                scenario(fx)
            except Exception as error:  # a broken scenario is a failed check, not a crash
                fx.check(f'{key}_scenario_completed', False, repr(error))
    finally:
        fx.close()
        receipt['checks'] = fx.checks
        receipt['facts'] = fx.facts
        receipt['status'] = 'passed' if fx.checks and all(fx.checks.values()) else 'failed'
        (args.output / 'receipt.json').write_text(json.dumps(receipt, indent=2, default=str) + '\n')
        print(json.dumps({'status': receipt['status'],
                          'passed': sum(fx.checks.values()), 'total': len(fx.checks)}))
    return 0 if receipt['status'] == 'passed' else 1


if __name__ == '__main__':
    raise SystemExit(main())
