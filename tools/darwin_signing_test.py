#!/usr/bin/env python3
"""Exercise the local signer and CI action without Apple credentials or uploads."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parent.parent
SECRET_NAMES = (
    'VC_CERT_P12_B64', 'VC_CERT_PASSWORD', 'VC_SIGNING_IDENTITY',
    'VC_NOTARY_API_KEY_B64', 'VC_NOTARY_API_KEY_ID', 'VC_NOTARY_API_ISSUER',
    'VC_NOTARY_APPLE_ID', 'VC_NOTARY_PASSWORD', 'VC_NOTARY_TEAM_ID',
)
MOCK = r'''
import json, os, pathlib, sys
name = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['CALLS'], 'a') as f:
    f.write(json.dumps([name, *args]) + '\n')
if name == 'uname': print(os.environ.get('MOCK_OS', 'Darwin'))
elif name == 'file': print(os.environ.get('MOCK_FILE', 'Mach-O 64-bit executable arm64'))
elif name == 'security':
    if args[0] == 'find-identity': print(os.environ.get('MOCK_IDENTITIES', ''))
    elif args[0] == 'create-keychain': pathlib.Path(args[-1]).touch()
    elif args[0] == 'delete-keychain': pathlib.Path(args[-1]).unlink(missing_ok=True)
elif name == 'codesign':
    if '--display' in args: print(os.environ.get('MOCK_AUTHORITY', 'Authority=Developer ID Application: Fixture'))
    if '--verify' in args: sys.exit(int(os.environ.get('VERIFY_EXIT', '0')))
    if '--force' in args: sys.exit(int(os.environ.get('SIGN_EXIT', '0')))
elif name == 'openssl': print('disposable-fixture-password')
elif name == 'curl': pathlib.Path(args[args.index('-o')+1]).touch()
elif name == 'ditto': pathlib.Path(args[-1]).touch()
elif name == 'xcrun' and args[1] == 'submit':
    print(json.dumps({'id': 'fixture-submission', 'status': os.environ.get('NOTARY_STATUS', 'Accepted')}))
    sys.exit(int(os.environ.get('NOTARY_EXIT', '0')))
'''


class DarwinSigningTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / 'tools'
        self.bin.mkdir()
        for name in ['uname', 'file', 'security', 'codesign', 'openssl', 'curl', 'ditto', 'xcrun']:
            p = self.bin / name
            p.write_text(f'#!{sys.executable}\n' + textwrap.dedent(MOCK))
            p.chmod(0o755)
        self.binary = self.root / 'binary with spaces'
        self.binary.write_bytes(b'fixture')
        self.binary.chmod(0o755)
        self.runner = self.root / 'runner'
        self.runner.mkdir()
        self.calls = self.root / 'calls.jsonl'
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(('VC_SIGNING_', 'VC_NOTARY_', 'VC_CERT_'))}
        self.env.update(PATH=str(self.bin) + os.pathsep + os.environ['PATH'], CALLS=str(self.calls), RUNNER_TEMP=str(self.runner))

    def run_script(self, *args: str, **env: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(['/bin/bash', str(ROOT / 'scripts/sign-darwin.sh'), *args, str(self.binary)],
                              cwd=ROOT, env=self.env | env, text=True, capture_output=True)

    def recorded(self, tool: str) -> list[list[str]]:
        rows = [json.loads(s) for s in self.calls.read_text().splitlines()] if self.calls.exists() else []
        return [r[1:] for r in rows if r[0] == tool]

    def test_local_fallback_is_ad_hoc_without_timestamp(self) -> None:
        result = self.run_script()
        self.assertEqual(result.returncode, 0, result.stderr)
        sign = self.recorded('codesign')[0]
        self.assertEqual(sign[sign.index('--sign')+1], '-')
        self.assertIn('--timestamp=none', sign)
        self.assertIn('runtime', sign)
        self.assertIn('--verify', self.recorded('codesign')[1])

    def test_auto_developer_id_uses_timestamp_and_optional_keychain(self) -> None:
        result = self.run_script('--require-developer-id', MOCK_IDENTITIES='1) ABC123 "Developer ID Application: Fixture"', VC_SIGNING_KEYCHAIN='/tmp/fixture.keychain')
        self.assertEqual(result.returncode, 0, result.stderr)
        sign = self.recorded('codesign')[0]
        self.assertIn('ABC123', sign)
        self.assertIn('--timestamp', sign)
        self.assertIn('/tmp/fixture.keychain', sign)

    def test_ci_never_falls_back_to_ad_hoc(self) -> None:
        result = self.run_script('--require-developer-id')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.recorded('codesign'), [])

    def test_multiple_developer_ids_require_explicit_selection(self) -> None:
        result = self.run_script(MOCK_IDENTITIES='1) A "Developer ID Application: A"\n2) B "Developer ID Application: B"')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.recorded('codesign'), [])

    def test_explicit_identity_or_verification_failure_is_not_downgraded(self) -> None:
        for env in [dict(SIGN_EXIT='1'), dict(VERIFY_EXIT='1'), dict(MOCK_AUTHORITY='Authority=Apple Distribution: Fixture')]:
            with self.subTest(env=env):
                result = self.run_script(VC_SIGNING_IDENTITY='Fixture', **env)
                self.assertNotEqual(result.returncode, 0)
        self.assertFalse(any('--sign' in r and r[r.index('--sign')+1] == '-' for r in self.recorded('codesign')))

    def test_non_macho_and_non_macos_are_rejected(self) -> None:
        for env in [dict(MOCK_OS='Linux'), dict(MOCK_FILE='ELF executable')]:
            with self.subTest(env=env):
                self.assertNotEqual(self.run_script(**env).returncode, 0)
        self.assertEqual(self.recorded('codesign'), [])

    def run_action(self, **env: str) -> subprocess.CompletedProcess[str]:
        self.assertIsNotNone(shutil.which('jq'), 'jq is required by macOS release runners')
        action = (ROOT / '.github/actions/sign-darwin/action.yml').read_text()
        body = textwrap.dedent(action.split('      run: |\n', 1)[1])
        script = self.root / 'action.sh'
        script.write_text(body)
        inputs = dict.fromkeys(SECRET_NAMES, '')
        inputs.update(VC_CERT_P12_B64='Zml4dHVyZQ==', VC_CERT_PASSWORD='fixture', VC_SIGNING_IDENTITY='Fixture',
                      VC_NOTARY_API_KEY_B64='Zml4dHVyZQ==', VC_NOTARY_API_KEY_ID='fixture', VC_NOTARY_API_ISSUER='fixture', BINARY=str(self.binary))
        return subprocess.run(['/bin/bash', str(script)], cwd=ROOT, env=self.env | inputs | env, text=True, capture_output=True)

    def test_action_requires_complete_credentials_before_import(self) -> None:
        result = self.run_action(VC_NOTARY_API_ISSUER='')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.recorded('security'), [])

    def test_action_api_success_and_apple_id_fallback_scrub_keys(self) -> None:
        for env in [{}, dict(VC_NOTARY_API_KEY_B64='', VC_NOTARY_APPLE_ID='fixture', VC_NOTARY_PASSWORD='fixture', VC_NOTARY_TEAM_ID='fixture')]:
            with self.subTest(env=env):
                result = self.run_action(**env)
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertEqual(list(self.runner.iterdir()), [])
        stores = [r for r in self.recorded('xcrun') if 'store-credentials' in r]
        self.assertIn('--key', stores[0])
        self.assertIn('--apple-id', stores[1])
        self.assertEqual(len([r for r in self.recorded('security') if r[0] == 'delete-keychain']), 2)

    def test_notary_rejection_timeout_and_codesign_failure_scrub_keys(self) -> None:
        for env in [dict(NOTARY_STATUS='Invalid'), dict(NOTARY_STATUS='In Progress'), dict(NOTARY_EXIT='1'), dict(SIGN_EXIT='1')]:
            with self.subTest(env=env):
                result = self.run_action(**env)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(list(self.runner.iterdir()), [])

    def test_both_workflow_variants_sign_before_archiving(self) -> None:
        workflow = (ROOT / '.github/workflows/release.yml').read_text()
        for name, end in [('build-release-normal:', 'build-release-noweb:'), ('build-release-noweb:', 'publish-manifest:')]:
            job = workflow.split('  ' + name, 1)[1].split('  ' + end, 1)[0]
            self.assertLess(job.index('uses: ./.github/actions/sign-darwin'), job.index('name: Create artifact (Unix)'))
            self.assertIn("if: runner.os == 'macOS'\n        uses: ./.github/actions/sign-darwin", job)
            self.assertIn('x86_64-apple-darwin', job)
            self.assertIn('aarch64-apple-darwin', job)
            for secret in SECRET_NAMES:
                self.assertIn('${{ secrets.' + secret + ' }}', job)


if __name__ == '__main__':
    unittest.main()
