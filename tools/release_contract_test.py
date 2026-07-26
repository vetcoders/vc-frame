#!/usr/bin/env python3
"""Static regression tests for the vc-frame release contract."""

from __future__ import annotations

import pathlib
import re
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


class ReleaseContractTests(unittest.TestCase):
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

    def test_release_tag_is_signed_and_fail_closed(self) -> None:
        makefile = read("Makefile")
        tag_target = makefile.split("\nrelease-tag:\n", 1)[1].split(
            "\nrelease-push:", 1
        )[0]
        self.assertIn('git tag -s -u "$$fingerprint"', tag_target)
        self.assertIn("DEFAULT_GPG_FINGERPRINT", tag_target)
        self.assertIn("--list-secret-keys", tag_target)
        self.assertIn("git verify-tag", tag_target)
        self.assertNotIn("git tag -a", tag_target)

    def test_workflow_gates_and_publishes_verified_downloads(self) -> None:
        workflow = read(".github/workflows/release.yml")
        for action in re.findall(r"^\s*uses:\s*(\S+)", workflow, re.MULTILINE):
            if action.startswith("./"):
                continue
            self.assertRegex(action, r"@[0-9a-f]{40}$")
        checkout_count = workflow.count("uses: actions/checkout@")
        self.assertEqual(workflow.count("persist-credentials: false"), checkout_count)
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
            "Cold-install exact downloaded release",
            "env -u GH_TOKEN -u GITHUB_TOKEN",
            'sh "$release_dir/install.sh"',
        ):
            self.assertIn(required, workflow)

        release_docs = read("docs/RELEASE.md")
        self.assertIn("make release-tag", release_docs)
        self.assertIn("make release-push", release_docs)
        self.assertNotIn('git tag -a "v$version"', release_docs)

    def test_legacy_e2e_runner_uses_vc_frame_binary(self) -> None:
        runner = read("src/tests/e2e/remote_runner.rs")
        self.assertIn("unknown-linux-musl/release/vc-frame", runner)
        self.assertNotIn("unknown-linux-musl/release/zellij", runner)


if __name__ == "__main__":
    unittest.main()
