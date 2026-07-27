#!/usr/bin/env python3
"""Static regression tests for the vc-frame release contract."""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


class ReleaseContractTests(unittest.TestCase):
    def run_command(
        self,
        argv: list[str],
        *,
        cwd: pathlib.Path,
        env: dict[str, str],
        expected: int = 0,
    ) -> subprocess.CompletedProcess[str]:
        result = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        if result.returncode != expected:
            self.fail(
                f"command returned {result.returncode}, expected {expected}: "
                f"{' '.join(argv)}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
            )
        return result

    def assert_command_fails(
        self,
        argv: list[str],
        *,
        cwd: pathlib.Path,
        env: dict[str, str],
        contains: str,
    ) -> None:
        result = subprocess.run(
            argv,
            cwd=cwd,
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        self.assertNotEqual(
            result.returncode,
            0,
            f"command unexpectedly passed: {' '.join(argv)}",
        )
        self.assertIn(contains, result.stdout + result.stderr)

    def test_version_surfaces_match(self) -> None:
        with (ROOT / "Cargo.toml").open("rb") as fh:
            version = tomllib.load(fh)["workspace"]["package"]["version"]
        installer = read("tools/install.sh")
        match = re.search(
            r'^VERSION="\$\{VCFRAME_VERSION:-([^}]*)\}"$', installer, re.MULTILINE
        )
        self.assertIsNotNone(match, "installer default version is missing")
        self.assertEqual(match.group(1), version)
        self.assertIn(f"## [{version}]", read("CHANGELOG.md"))

    def test_version_sync_owns_installer_default(self) -> None:
        release_sync = read("tools/release_sync.py")
        self.assertIn("INSTALLER = ROOT /", release_sync)
        self.assertIn("(INSTALLER, transform_installer)", release_sync)
        self.assertIn("INSTALLER_DEFAULT_RE", release_sync)

    def test_installer_pins_actual_signer_and_release_identity(self) -> None:
        installer = read("tools/install.sh")
        installer_test = read("tools/install_test.sh")
        for required in (
            "--status-fd 3",
            '"VALIDSIG"',
            "signed_primary_fingerprints",
            "verify_binary_release_identity",
            'reported_semver="${reported_version%%+*}"',
            'if [ "$build_version" != "$VERSION" ]',
            'if [ "$build_git_sha" != "$expected_git_sha" ]',
            'verify_gpg_signature "$manifest_file"',
            "MANIFEST_GIT_SHA",
        ):
            self.assertIn(required, installer)
        self.assertLess(
            installer.index('verify_gpg_signature "$manifest_file"'),
            installer.index('validate_manifest "$manifest_file"'),
            "the manifest signature must be pinned before any field is trusted",
        )
        for dynamic_case in (
            "pinned signing subkey installs",
            "foreign signer in imported bundle fails closed",
            "missing manifest signature fails closed",
            "pinned signed old-binary replay preserves existing install",
            "pinned signed wrong-commit binary preserves existing install",
        ):
            self.assertIn(dynamic_case, installer_test)

    def test_release_targets_are_canonical_and_fail_closed(self) -> None:
        makefile = read("Makefile")
        preflight = makefile.split("\nrelease-preflight:\n", 1)[1].split(
            "\nrelease-check:", 1
        )[0]
        for required in (
            "./scripts/release-provenance.zsh preflight",
            "tools/release_sync.py check --require-version-section",
            "$(MAKE) release-contract-test",
            "$(MAKE) plugins-parity",
            "$(MAKE) semgrep",
            "$(MAKE) ci",
            "$(CARGO) build --bin vc-frame",
            "$(MAKE) triage-runtime-e2e",
            "$(MAKE) install-test",
        ):
            self.assertIn(required, preflight)
        self.assertEqual(
            preflight.count("./scripts/release-provenance.zsh preflight"),
            2,
            "release provenance must bracket the complete quality cone",
        )
        self.assertIn("release-check: release-preflight", makefile)
        self.assertIn("release-tag: release-preflight", makefile)
        self.assertIn(
            './scripts/release-provenance.zsh create-tag "$(TAG)"',
            makefile,
        )
        self.assertIn(
            './scripts/release-provenance.zsh push-tag "$(TAG)"',
            makefile,
        )
        self.assertNotIn('git push origin "$(TAG)"', makefile)

        provenance = read("scripts/release-provenance.zsh")
        for required in (
            "release_worktree_dirty",
            "symbolic-ref --quiet --short HEAD",
            "'+refs/heads/main:refs/remotes/origin/main'",
            '[[ "$head" == "$origin_main" ]]',
            '[[ "$object_type" == "tag" ]]',
            '[[ "$tag_target" == "$head" ]]',
            "verify-tag --raw",
            '[[ "$tag_primary_fingerprint" == "$fingerprint" ]]',
            "ls-remote --tags origin",
            '"${VERIFIED_RELEASE_TAG_OBJECT}:refs/tags/$tag"',
        ):
            self.assertIn(required, provenance)

    def test_release_ref_and_tag_guards_falsify_bypasses(self) -> None:
        for executable in ("git", "gpg", "gpgconf", "zsh"):
            self.assertIsNotNone(
                shutil.which(executable),
                f"{executable} is required to test the release trust contract",
            )

        short_tmp = pathlib.Path("/tmp")
        if not short_tmp.is_dir():
            short_tmp = pathlib.Path(tempfile.gettempdir())
        with tempfile.TemporaryDirectory(prefix="vcr-", dir=short_tmp) as raw:
            fixture = pathlib.Path(raw)
            repo = fixture / "repo"
            origin = fixture / "origin.git"
            gpg_home = fixture / "gnupg"
            fake_home = fixture / "home"
            (repo / "scripts").mkdir(parents=True)
            (repo / "tools").mkdir()
            gpg_home.mkdir(mode=0o700)
            fake_home.mkdir()
            shutil.copy2(
                ROOT / "scripts/release-provenance.zsh",
                repo / "scripts/release-provenance.zsh",
            )

            env = os.environ.copy()
            env.update(
                {
                    "HOME": str(fake_home),
                    "GNUPGHOME": str(gpg_home),
                    "VCFRAME_GPG_HOMEDIR": str(gpg_home),
                    "RELEASE_KEYS_DIR": str(fake_home / ".keys"),
                    "GIT_CONFIG_GLOBAL": os.devnull,
                    "GIT_CONFIG_NOSYSTEM": "1",
                }
            )
            self.run_command(
                [
                    "gpgconf",
                    "--homedir",
                    str(gpg_home),
                    "--launch",
                    "gpg-agent",
                ],
                cwd=fixture,
                env=env,
            )

            def generate_key(identity: str, *, signing_subkey: bool = False) -> str:
                self.run_command(
                    [
                        "gpg",
                        "--homedir",
                        str(gpg_home),
                        "--batch",
                        "--pinentry-mode",
                        "loopback",
                        "--passphrase",
                        "",
                        "--quick-generate-key",
                        identity,
                        "ed25519",
                        "cert" if signing_subkey else "sign",
                        "0",
                    ],
                    cwd=fixture,
                    env=env,
                )
                listing = self.run_command(
                    [
                        "gpg",
                        "--homedir",
                        str(gpg_home),
                        "--batch",
                        "--with-colons",
                        "--list-secret-keys",
                        identity,
                    ],
                    cwd=fixture,
                    env=env,
                ).stdout
                fingerprints = [
                    line.split(":")[9]
                    for line in listing.splitlines()
                    if line.startswith("fpr:")
                ]
                self.assertTrue(fingerprints)
                fingerprint = fingerprints[0].upper()
                if signing_subkey:
                    self.run_command(
                        [
                            "gpg",
                            "--homedir",
                            str(gpg_home),
                            "--batch",
                            "--pinentry-mode",
                            "loopback",
                            "--passphrase",
                            "",
                            "--quick-add-key",
                            fingerprint,
                            "ed25519",
                            "sign",
                            "0",
                        ],
                        cwd=fixture,
                        env=env,
                    )
                return fingerprint

            pinned = generate_key(
                "Pinned Release <pinned@example.invalid>",
                signing_subkey=True,
            )
            foreign = generate_key("Foreign Release <foreign@example.invalid>")
            (repo / "Cargo.toml").write_text(
                '[workspace]\n[workspace.package]\nversion = "1.2.3"\n',
                encoding="utf-8",
            )
            (repo / "tools/install.sh").write_text(
                f'DEFAULT_GPG_FINGERPRINT="{pinned}"\n',
                encoding="utf-8",
            )
            (repo / "CURRENT").write_text("base\n", encoding="utf-8")

            self.run_command(["git", "init", "-b", "main"], cwd=repo, env=env)
            for key, value in (
                ("user.name", "Release Fixture"),
                ("user.email", "release@example.invalid"),
                ("commit.gpgSign", "false"),
                ("tag.gpgSign", "false"),
                ("gpg.format", "openpgp"),
                ("gpg.program", shutil.which("gpg") or "gpg"),
            ):
                self.run_command(
                    ["git", "config", key, value],
                    cwd=repo,
                    env=env,
                )
            self.run_command(["git", "add", "."], cwd=repo, env=env)
            self.run_command(
                ["git", "commit", "-m", "base release fixture"],
                cwd=repo,
                env=env,
            )
            (repo / "CURRENT").write_text("current\n", encoding="utf-8")
            self.run_command(["git", "add", "CURRENT"], cwd=repo, env=env)
            self.run_command(
                ["git", "commit", "-m", "current release fixture"],
                cwd=repo,
                env=env,
            )
            self.run_command(
                ["git", "init", "--bare", "--initial-branch=main", str(origin)],
                cwd=fixture,
                env=env,
            )
            self.run_command(
                ["git", "remote", "add", "origin", str(origin)],
                cwd=repo,
                env=env,
            )
            self.run_command(
                ["git", "push", "-u", "origin", "main"],
                cwd=repo,
                env=env,
            )

            script = ["zsh", "scripts/release-provenance.zsh"]
            self.run_command(script + ["preflight"], cwd=repo, env=env)

            with self.subTest("tracked drift"):
                (repo / "CURRENT").write_text("dirty\n", encoding="utf-8")
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="tracked files differ from HEAD",
                )
                self.run_command(
                    ["git", "restore", "CURRENT"],
                    cwd=repo,
                    env=env,
                )

            with self.subTest("untracked drift"):
                untracked = repo / "UNTRACKED"
                untracked.write_text("shadow input\n", encoding="utf-8")
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="tracked or untracked drift",
                )
                untracked.unlink()

            with self.subTest("wrong branch"):
                self.run_command(
                    ["git", "switch", "-c", "develop"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="requires branch main",
                )
                self.run_command(["git", "switch", "main"], cwd=repo, env=env)

            with self.subTest("detached head"):
                self.run_command(
                    ["git", "switch", "--detach"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="not a detached HEAD",
                )
                self.run_command(["git", "switch", "main"], cwd=repo, env=env)

            with self.subTest("head ahead of origin main"):
                (repo / "AHEAD").write_text("ahead\n", encoding="utf-8")
                self.run_command(["git", "add", "AHEAD"], cwd=repo, env=env)
                self.run_command(
                    ["git", "commit", "-m", "unpublished"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="is not exactly origin/main",
                )
                self.run_command(
                    ["git", "reset", "--hard", "origin/main"],
                    cwd=repo,
                    env=env,
                )

            with self.subTest("origin main advanced"):
                writer = fixture / "writer"
                self.run_command(
                    ["git", "clone", str(origin), str(writer)],
                    cwd=fixture,
                    env=env,
                )
                self.run_command(
                    ["git", "config", "user.name", "Remote Writer"],
                    cwd=writer,
                    env=env,
                )
                self.run_command(
                    ["git", "config", "user.email", "writer@example.invalid"],
                    cwd=writer,
                    env=env,
                )
                (writer / "REMOTE").write_text("remote\n", encoding="utf-8")
                self.run_command(["git", "add", "REMOTE"], cwd=writer, env=env)
                self.run_command(
                    ["git", "commit", "-m", "advance origin"],
                    cwd=writer,
                    env=env,
                )
                self.run_command(
                    ["git", "push", "origin", "main"],
                    cwd=writer,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["preflight"],
                    cwd=repo,
                    env=env,
                    contains="is not exactly origin/main",
                )
                self.run_command(
                    ["git", "merge", "--ff-only", "origin/main"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(script + ["preflight"], cwd=repo, env=env)

            with self.subTest("workspace version mismatch"):
                self.assert_command_fails(
                    script + ["create-tag", "v9.9.9"],
                    cwd=repo,
                    env=env,
                    contains="does not match workspace version",
                )

            with self.subTest("valid pinned signed tag"):
                self.run_command(
                    script + ["create-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    script + ["verify-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )

            with self.subTest("lightweight tag"):
                self.run_command(
                    ["git", "tag", "-d", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    ["git", "tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["verify-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="is lightweight",
                )

            with self.subTest("tag targets a different commit"):
                self.run_command(
                    ["git", "tag", "-d", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    [
                        "git",
                        "tag",
                        "-s",
                        "-u",
                        pinned,
                        "v1.2.3",
                        "HEAD^",
                        "-m",
                        "stale target",
                    ],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["verify-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="not current HEAD",
                )

            with self.subTest("foreign signer"):
                self.run_command(
                    ["git", "tag", "-d", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    [
                        "git",
                        "tag",
                        "-s",
                        "-u",
                        foreign,
                        "v1.2.3",
                        "-m",
                        "foreign signer",
                    ],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["verify-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="does not match pinned",
                )

            with self.subTest("immutable remote tag"):
                self.run_command(
                    ["git", "tag", "-d", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    script + ["create-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    script + ["push-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["push-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="release tags are immutable and never rewritten",
                )

            with self.subTest("rewritten local tag"):
                remote_tag_object = self.run_command(
                    [
                        "git",
                        "--git-dir",
                        str(origin),
                        "rev-parse",
                        "refs/tags/v1.2.3",
                    ],
                    cwd=fixture,
                    env=env,
                ).stdout.strip()
                self.run_command(
                    ["git", "tag", "-d", "v1.2.3"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    [
                        "git",
                        "tag",
                        "-s",
                        "-u",
                        pinned,
                        "v1.2.3",
                        "-m",
                        "rewritten local tag",
                    ],
                    cwd=repo,
                    env=env,
                )
                local_tag_object = self.run_command(
                    ["git", "rev-parse", "refs/tags/v1.2.3"],
                    cwd=repo,
                    env=env,
                ).stdout.strip()
                self.assertNotEqual(local_tag_object, remote_tag_object)
                self.assert_command_fails(
                    script + ["push-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="release tags are immutable and never rewritten",
                )

            with self.subTest("missing pinned trust root"):
                (repo / "tools/install.sh").write_text(
                    'DEFAULT_GPG_FINGERPRINT=""\n',
                    encoding="utf-8",
                )
                self.run_command(
                    ["git", "add", "tools/install.sh"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    ["git", "commit", "-m", "remove trust root"],
                    cwd=repo,
                    env=env,
                )
                self.run_command(
                    ["git", "push", "origin", "main"],
                    cwd=repo,
                    env=env,
                )
                self.assert_command_fails(
                    script + ["verify-tag", "v1.2.3"],
                    cwd=repo,
                    env=env,
                    contains="no pinned DEFAULT_GPG_FINGERPRINT",
                )

    def test_workflow_gates_and_publishes_verified_downloads(self) -> None:
        workflow = read(".github/workflows/release.yml")
        for action in re.findall(r"^\s*uses:\s*(\S+)", workflow, re.MULTILINE):
            if action.startswith("./"):
                continue
            self.assertRegex(action, r"@[0-9a-f]{40}$")
        checkout_count = workflow.count("uses: actions/checkout@")
        self.assertEqual(workflow.count("persist-credentials: false"), checkout_count)
        self.assertNotIn("actions/upload-release-asset@", workflow)
        self.assertNotIn("needs.create-release.outputs.upload_url", workflow)
        self.assertNotIn("--clobber", workflow)
        self.assertEqual(
            workflow.count("gh release upload"),
            11,
            "every release asset class must use the maintained GitHub CLI path",
        )
        for required in (
            "make release-contract-test",
            "make plugins-parity",
            "cargo build --bin vc-frame",
            "make triage-runtime-e2e",
            "fetch-depth: 0",
            'git cat-file -t "refs/tags/$tag"',
            'git verify-tag --raw "refs/tags/$tag"',
            'git merge-base --is-ancestor "$GITHUB_SHA" refs/remotes/origin/main',
            "tag_primary_fingerprint",
            "tools/release_sync.py notes --output",
            "releases/tags/$tag",
            "--method DELETE",
            "GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
            'gh release upload "${{ env.RELEASE_TAG }}"',
            "Cold-install exact downloaded release",
            "env -u GH_TOKEN -u GITHUB_TOKEN",
            'sh "$release_dir/install.sh"',
            "manifest.json.sig",
            'printf \'  "git_sha": "%s",\\n\' "$git_sha"',
            "manifest_primary_fingerprint",
            'test "${#signatures[@]}" -eq 9',
            'test "$manifest_git_sha" = "$GITHUB_SHA"',
        ):
            self.assertIn(required, workflow)
        self.assertEqual(
            workflow.count('--local-user "$expected_fingerprint"'),
            4,
            "canary, both Unix artifact lanes, and manifest must select the pinned key",
        )
        self.assertGreaterEqual(
            workflow.count("--status-fd 1"),
            5,
            "canary, artifact lanes, manifest, and final verification must expose VALIDSIG",
        )
        self.assertIn("canary_primary_fingerprint", workflow)
        self.assertIn("signature_primary_fingerprint", workflow)

        release_docs = read("docs/RELEASE.md")
        self.assertIn("make release-preflight", release_docs)
        self.assertIn("make release-tag", release_docs)
        self.assertIn("make release-push", release_docs)
        self.assertIn("manifest.json.sig", release_docs)
        self.assertIn("byte-for-byte untouched", release_docs)
        self.assertNotIn('git tag -a "v$version"', release_docs)
        self.assertIn("workflow_dispatch", workflow)
        self.assertIn("./scripts/release-provenance.zsh guard", workflow)
        self.assertNotIn("release-preflight", workflow)

    def test_workflows_use_pinned_protoc_without_release_api_discovery(self) -> None:
        workflow_paths = (
            ".github/workflows/e2e.yml",
            ".github/workflows/release.yml",
            ".github/workflows/rust.yml",
        )
        workflows = "\n".join(read(path) for path in workflow_paths)
        rust_workflow = read(".github/workflows/rust.yml")
        action = read(".github/actions/setup-protoc/action.yml")

        self.assertIn(
            "name: triage-runtime-e2e-${{ github.run_id }}-"
            "${{ github.run_attempt }}-${{ matrix.os }}",
            rust_workflow,
        )
        self.assertNotIn("arduino/setup-protoc", workflows)
        self.assertEqual(
            workflows.count("uses: ./.github/actions/setup-protoc"),
            8,
            "every build and test lane must use the same pinned Protoc installer",
        )
        self.assertNotIn("api.github.com", action)
        self.assertNotIn("releases?page=", action)
        self.assertIn('asset="protoc-${version}-linux-x86_64.zip"', action)
        self.assertIn('asset="protoc-${version}-osx-aarch_64.zip"', action)
        self.assertIn("protoc-$Version-win64.zip", action)
        self.assertIn("--retry 5", action)
        self.assertEqual(
            len(re.findall(r'expected_sha256="[0-9a-f]{64}"', action)),
            4,
        )
        self.assertEqual(
            len(re.findall(r'\$ExpectedSha256 = "[0-9a-f]{64}"', action)),
            1,
        )

    def test_legacy_e2e_runner_uses_vc_frame_binary(self) -> None:
        runner = read("src/tests/e2e/remote_runner.rs")
        self.assertIn("unknown-linux-musl/release/vc-frame", runner)
        self.assertNotIn("unknown-linux-musl/release/zellij", runner)


if __name__ == "__main__":
    unittest.main()
