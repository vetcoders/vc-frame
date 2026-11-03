"""Execute the embedded POSIX runner against private canonical-owner fixtures."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

RUNNER = Path(__file__).resolve().parents[1] / 'assets/font-chooser.sh'
CAPTURE = '#!/bin/sh\nprintf "%s\\n" "$@" > "$FONT_ARGV"\n'


class FontRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='frame-font-runner-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / 'config with spaces'
        self.root.mkdir()
        self.argv = Path(self.temp.name) / 'argv'
        self.env = {'PATH': os.environ['PATH'], 'HOME': self.temp.name,
                    'VC_FRAME_CONFIG_DIR': str(self.root), 'FONT_ARGV': str(self.argv)}

    def fixture(self, name, executable=True):
        path = self.root / name
        path.write_text(CAPTURE)
        path.chmod(0o700 if executable else 0o600)
        return path

    def run_script(self):
        return subprocess.run(['/bin/sh', str(RUNNER)], env=self.env,
                              capture_output=True, text=True, check=False)

    def test_pane_python_is_preferred_and_gets_exact_argv(self):
        self.fixture('pane-python')
        launcher = self.fixture('vc-agent-workshop.py', executable=False)
        self.env['VIBECRAFTED_PYTHON'] = '/must/not/be/used'
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.argv.read_text().splitlines(), [str(launcher), 'launcher', '--font'])

    def test_explicit_python_fallback(self):
        self.fixture('vc-agent-workshop.py')
        python = self.fixture('verified python')
        self.env['VIBECRAFTED_PYTHON'] = str(python)
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.argv.read_text().splitlines(),
                         [str(self.root / 'vc-agent-workshop.py'), 'launcher', '--font'])

    def test_executable_script_fallback(self):
        self.fixture('vc-agent-workshop.py')
        self.assertEqual(self.run_script().returncode, 0)
        self.assertEqual(self.argv.read_text().splitlines(), ['launcher', '--font'])

    def test_xdg_and_home_fallbacks(self):
        for xdg in (True, False):
            with self.subTest(xdg=xdg):
                self.env.pop('VC_FRAME_CONFIG_DIR', None)
                base = Path(self.temp.name) / ('xdg' if xdg else '.config')
                if xdg:
                    self.env['XDG_CONFIG_HOME'] = str(base)
                else:
                    self.env.pop('XDG_CONFIG_HOME', None)
                root = base / 'vibecrafted/vc-frame'
                root.mkdir(parents=True)
                runner = root / 'pane-python'
                runner.write_text(CAPTURE)
                runner.chmod(0o700)
                self.assertEqual(self.run_script().returncode, 0)
                self.assertEqual(self.argv.read_text().splitlines(),
                                 [str(root / 'vc-agent-workshop.py'), 'launcher', '--font'])

    def test_missing_owner_reports_failure_without_opening_shell(self):
        self.env['SHELL'] = '/must/not/be/run'
        result = self.run_script()
        self.assertEqual(result.returncode, 127)
        self.assertIn(str(self.root / 'vc-agent-workshop.py'), result.stderr)
        self.assertFalse(self.argv.exists())


if __name__ == '__main__':
    unittest.main()
