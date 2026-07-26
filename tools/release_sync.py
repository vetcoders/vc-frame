#!/usr/bin/env python3
"""Synchronize vc-frame release surfaces around a single semantic version.

Single source of truth: workspace.package.version in the root Cargo.toml.

Also keeps path-dependency pins in sync:
  zellij-client / zellij-server / zellij-utils = { path = "...", version = "X.Y.Z" }
and the default version advertised and installed by tools/install.sh.

Usage:
    python3 tools/release_sync.py bump patch
    python3 tools/release_sync.py bump 0.46.0
    python3 tools/release_sync.py check
    python3 tools/release_sync.py check --require-version-section
    python3 tools/release_sync.py notes
    python3 tools/release_sync.py notes --output dist/release-notes.md
"""

from __future__ import annotations

import argparse
import pathlib
import re
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CHANGELOG = ROOT / "CHANGELOG.md"
INSTALLER = ROOT / "tools" / "install.sh"
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+$")
VERSION_HEADER_RE = re.compile(
    r"^## \[(?P<version>v?\d+\.\d+\.\d+)](?: - .+)?$", re.MULTILINE
)
# Path-dep pins for the three crates published/consumed with the workspace version.
PATH_DEP_VERSION_RE = re.compile(
    r'^(?P<pre>(?:zellij-client|zellij-server|zellij-utils)\s*=\s*\{[^}]*?\bversion\s*=\s*")'
    r'(?P<ver>[^"]*)'
    r'(?P<post>")',
    re.MULTILINE,
)
WORKSPACE_PACKAGE_VERSION_RE = re.compile(
    r'(^\[workspace\.package\]\s*(?:\n(?!\[)[^\n]*)*\nversion\s*=\s*")[^"]*(")',
    re.MULTILINE,
)
INSTALLER_DEFAULT_RE = re.compile(
    r'^(?P<pre>VERSION="\$\{VCFRAME_VERSION:-)[^}]*(?P<post>\}")$',
    re.MULTILINE,
)
INSTALLER_COMMENT_RE = re.compile(
    r"^(?P<pre>#\s+VCFRAME_VERSION\s+release version \(default: )"
    r"\d+\.\d+\.\d+(?P<post>\))$",
    re.MULTILINE,
)


def root_relative(path: pathlib.Path) -> str:
    try:
        return path.relative_to(ROOT).as_posix()
    except ValueError:
        return path.as_posix()


def read_cargo_version() -> str:
    with CARGO_TOML.open("rb") as fh:
        data = tomllib.load(fh)
    try:
        return data["workspace"]["package"]["version"]
    except KeyError as exc:
        raise SystemExit(
            "Cargo.toml missing [workspace.package].version — cannot release"
        ) from exc


def compute_bumped_version(current: str, target: str) -> str:
    parts = [int(part) for part in current.split(".")]
    if target == "patch":
        parts[2] += 1
    elif target == "minor":
        parts[1] += 1
        parts[2] = 0
    elif target == "major":
        parts[0] += 1
        parts[1] = 0
        parts[2] = 0
    elif SEMVER_RE.fullmatch(target):
        return target
    else:
        raise SystemExit(
            f"Invalid VERSION/TYPE: {target!r}. Use patch|minor|major|x.y.z"
        )
    return ".".join(str(part) for part in parts)


def transform_root_cargo(text: str, version: str) -> str:
    def _ws(m: re.Match[str]) -> str:
        return f"{m.group(1)}{version}{m.group(2)}"

    new_text, n = WORKSPACE_PACKAGE_VERSION_RE.subn(_ws, text, count=1)
    if n != 1:
        raise SystemExit(
            'Could not find [workspace.package] version = "..." in Cargo.toml'
        )

    def _path_dep(m: re.Match[str]) -> str:
        return f"{m.group('pre')}{version}{m.group('post')}"

    return PATH_DEP_VERSION_RE.sub(_path_dep, new_text)


def transform_installer(text: str, version: str) -> str:
    def _replace(m: re.Match[str]) -> str:
        return f"{m.group('pre')}{version}{m.group('post')}"

    updated, default_count = INSTALLER_DEFAULT_RE.subn(_replace, text, count=1)
    if default_count != 1:
        raise SystemExit(
            "Could not find VERSION=${VCFRAME_VERSION:-...} in tools/install.sh"
        )
    updated, comment_count = INSTALLER_COMMENT_RE.subn(_replace, updated, count=1)
    if comment_count != 1:
        raise SystemExit(
            "Could not find the VCFRAME_VERSION default comment in tools/install.sh"
        )
    return updated


def sync_versions(version: str, *, write: bool) -> list[str]:
    changed: list[str] = []
    surfaces = (
        (CARGO_TOML, transform_root_cargo),
        (INSTALLER, transform_installer),
    )
    for path, transform in surfaces:
        original = path.read_text(encoding="utf-8")
        updated = transform(original, version)
        if original != updated:
            changed.append(root_relative(path))
            if write:
                path.write_text(updated, encoding="utf-8")
    return changed


def version_header_regex(version: str) -> re.Pattern[str]:
    escaped = re.escape(version)
    escaped_v = re.escape(f"v{version}")
    return re.compile(
        rf"^## \[(?:{escaped}|{escaped_v})](?: - .+)?$",
        re.MULTILINE,
    )


def extract_version_notes(version: str) -> str:
    text = CHANGELOG.read_text(encoding="utf-8")
    match = version_header_regex(version).search(text)
    if match is None:
        raise SystemExit(
            f"CHANGELOG.md does not contain a dedicated section for version {version}"
        )
    body_start = match.end()
    next_match = VERSION_HEADER_RE.search(text, body_start)
    body = text[body_start : next_match.start() if next_match else len(text)].strip()
    if not body:
        return "No detailed release notes were recorded for this version."
    return body


def command_bump(args: argparse.Namespace) -> int:
    current = read_cargo_version()
    new_version = compute_bumped_version(current, args.target)
    changed = sync_versions(new_version, write=True)
    if not changed:
        print(f"Release surfaces already synced to {new_version}")
        return 0
    print(f"Release surfaces synced: {current} -> {new_version}")
    for path in changed:
        print(f"  - {path}")
    return 0


def command_check(args: argparse.Namespace) -> int:
    expected = args.version or read_cargo_version()
    changed = sync_versions(expected, write=False)

    errors: list[str] = []
    if not CHANGELOG.is_file():
        errors.append("CHANGELOG.md is missing")
    else:
        changelog_text = CHANGELOG.read_text(encoding="utf-8")
        if "## [Unreleased]" not in changelog_text:
            errors.append("CHANGELOG.md is missing '## [Unreleased]'")
        if (
            args.require_version_section
            and version_header_regex(expected).search(changelog_text) is None
        ):
            errors.append(f"CHANGELOG.md is missing dedicated section for {expected}")

    if changed:
        errors.append(
            f"Release surfaces are out of sync for {expected}: {', '.join(changed)}"
        )

    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        return 1

    print(f"Release surfaces are synced to {expected}")
    if CHANGELOG.is_file():
        changelog_text = CHANGELOG.read_text(encoding="utf-8")
        if version_header_regex(expected).search(changelog_text):
            print(f"CHANGELOG section for {expected}: present")
        else:
            print(
                f"CHANGELOG section for {expected}: not yet closed "
                "(Unreleased still open)"
            )
    return 0


def command_notes(args: argparse.Namespace) -> int:
    version = args.version or read_cargo_version()
    notes = extract_version_notes(version)
    if args.output:
        output_path = pathlib.Path(args.output)
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(notes + "\n", encoding="utf-8")
        print(f"Wrote release notes for {version} to {output_path}")
        return 0
    print(notes)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    bump_parser = subparsers.add_parser("bump", help="Bump and sync release surfaces")
    bump_parser.add_argument("target", help="patch|minor|major|x.y.z")
    bump_parser.set_defaults(func=command_bump)

    check_parser = subparsers.add_parser(
        "check", help="Verify release surfaces are synced"
    )
    check_parser.add_argument(
        "version", nargs="?", help="Expected version; defaults to Cargo.toml"
    )
    check_parser.add_argument(
        "--require-version-section",
        action="store_true",
        help="Fail if CHANGELOG.md does not yet contain a dedicated section",
    )
    check_parser.set_defaults(func=command_check)

    notes_parser = subparsers.add_parser(
        "notes", help="Extract release notes from CHANGELOG.md"
    )
    notes_parser.add_argument(
        "version", nargs="?", help="Version to extract; defaults to Cargo.toml"
    )
    notes_parser.add_argument(
        "--output", help="Write notes to a file instead of stdout"
    )
    notes_parser.set_defaults(func=command_notes)

    return parser


def main() -> int:
    parser = build_parser()
    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
