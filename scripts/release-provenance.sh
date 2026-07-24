#!/usr/bin/env bash
# release-provenance — vc-frame packaging provenance truth
#
# Universal shell: runs identically under bash (3.2+) and zsh. The Makefile
# invokes it through whichever of the two the machine has (SCRIPT_SHELL);
# the shebang is only the direct-execution default.
#
# Modes:
#   guard        Refuse to proceed unless the worktree is clean; print identity
#   package      guard -> release build -> archive -> checksum -> receipt
#   self-test    Prove the guard rejects a dirty tree and accepts a clean one
#
# The receipt is the answer to "which exact bytes did we ship, built from what?"
# It records the packaging context AND the identity the packaged binary reports
# about itself, so the two can never silently disagree.
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
set -euo pipefail

export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin:${PATH:-}"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
DIST="$REPO/target/dist"
BIN_NAME="vc-frame"

say() { printf '%s\n' "$*"; }
errln() { printf '%s\n' "$*" >&2; }
die() { errln "release-provenance: $*"; exit 1; }
info() { say "  $*"; }

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    die "neither shasum nor sha256sum found on PATH (macOS ships shasum; Linux: coreutils sha256sum)"
  fi
}

file_size() { wc -c < "$1" | tr -d ' '; }

host_target() { rustc -vV | sed -n 's/^host: //p'; }

# Tracked-state cleanliness. Untracked files are Living Tree noise and do not
# enter an archive; tracked drift does, so only that is a packaging blocker.
worktree_dirty() {
  [[ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]]
}

# RETURNS non-zero rather than exiting, so callers can use it as a predicate
# (the self-test must survive a deliberate rejection to restore its canary).
# The dispatcher at the bottom turns a failed guard into a non-zero exit.
guard() {
  say "== release provenance guard =="
  git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1 \
    || die "not a git checkout; packaging requires a verifiable source commit"

  if worktree_dirty; then
    errln ""
    errln "REFUSING TO PACKAGE: tracked files differ from HEAD."
    errln "A release archive must correspond to an exact published commit."
    errln ""
    git -C "$REPO" status --porcelain --untracked-files=no >&2
    errln ""
    return 1
  fi

  local sha; sha="$(git -C "$REPO" rev-parse HEAD)"
  info "commit: $sha"
  info "clean:  yes (tracked files match HEAD)"
  say "== guard passed =="
  return 0
}

package() {
  guard || exit 1

  local sha version target
  sha="$(git -C "$REPO" rev-parse HEAD)"
  # Prefer [workspace.package] version (vc-frame is a workspace); fall back to first pin.
  version="$(
    python3 -c '
import pathlib, tomllib, sys
p = pathlib.Path(sys.argv[1])
data = tomllib.loads(p.read_text(encoding="utf-8"))
print(data.get("workspace", {}).get("package", {}).get("version")
      or data.get("package", {}).get("version")
      or "")
' "$REPO/Cargo.toml"
  )"
  target="$(host_target)"
  [[ -n "$version" ]] || die "could not read version from Cargo.toml"
  [[ -n "$target" ]] || die "could not resolve host target triple"

  say ""
  say "== packaging vc-frame $version ($target) =="

  # Pass provenance in explicitly: the build must not depend on the build
  # machine happening to have git, and these are the values we just verified.
  VC_FRAME_GIT_SHA="$sha" VC_FRAME_GIT_DIRTY=0 \
    "$CARGO" build --release --bin "$BIN_NAME" --manifest-path "$REPO/Cargo.toml"

  local built="$REPO/target/release/$BIN_NAME"
  [[ -x "$built" ]] || die "release binary not found at $built"

  # The packaged binary must agree with the packaging context. If it does not,
  # the receipt would be a comfortable lie.
  local reported_sha
  reported_sha="$("$built" --build-info | sed -n 's/.*"git_sha": "\([^"]*\)".*/\1/p')"
  [[ "$reported_sha" == "$sha" ]] \
    || die "packaged binary reports sha $reported_sha but HEAD is $sha"
  local reported_dirty
  reported_dirty="$("$built" --build-info | sed -n 's/.*"git_dirty": \([a-z]*\).*/\1/p')"
  [[ "$reported_dirty" == "false" ]] \
    || die "packaged binary reports a dirty build; refusing to package"

  rm -rf "$DIST"; mkdir -p "$DIST/stage"
  cp "$built" "$DIST/stage/$BIN_NAME"

  local archive="$BIN_NAME-$target.tar.gz"
  ( cd "$DIST/stage" && tar czf "../$archive" "$BIN_NAME" )
  rm -rf "$DIST/stage"

  local archive_sha binary_sha
  archive_sha="$(sha256_of "$DIST/$archive")"
  binary_sha="$(sha256_of "$built")"
  printf '%s  %s\n' "$archive_sha" "$archive" > "$DIST/$archive.sha256"

  # The receipt names the exact bytes: the archive, and the binary inside it.
  cat > "$DIST/RECEIPT.json" <<RECEIPT
{
  "product": "vc-frame",
  "version": "$version",
  "human_version": "$("$built" --version | awk '{print $2}')",
  "git_sha": "$sha",
  "git_dirty": false,
  "target": "$target",
  "profile": "release",
  "toolchain": "$(rustc --version)",
  "binary": {
    "name": "$BIN_NAME",
    "sha256": "$binary_sha",
    "bytes": $(file_size "$built")
  },
  "archive": {
    "name": "$archive",
    "sha256": "$archive_sha",
    "bytes": $(file_size "$DIST/$archive")
  },
  "binary_self_reported_build_info": $("$built" --build-info | sed 's/^/  /')
}
RECEIPT

  say ""
  say "== package receipt =="
  cat "$DIST/RECEIPT.json"
  say ""
  info "archive: $DIST/$archive"
  info "receipt: $DIST/RECEIPT.json"
}

# Proves the guard is real: it must reject a dirty tree, not merely warn.
self_test() {
  say "== release provenance guard self-test =="
  if worktree_dirty; then
    die "self-test needs a clean tree to start from (tracked files differ from HEAD)"
  fi

  say "[1/3] clean tree must pass"
  guard >/dev/null || die "guard rejected a clean tree"
  say "  PASS"

  # Script-scope globals, not local: the EXIT trap body is evaluated when it
  # fires, by which time a function-local would already be out of scope.
  SELFTEST_CANARY="$REPO/Cargo.toml"
  SELFTEST_BACKUP="$(mktemp)"
  cp "$SELFTEST_CANARY" "$SELFTEST_BACKUP"
  # Restore the tracked file no matter how we leave this function.
  trap 'cp "$SELFTEST_BACKUP" "$SELFTEST_CANARY"; rm -f "$SELFTEST_BACKUP"' EXIT INT TERM

  say "[2/3] dirty tree must be rejected"
  printf '\n# release-provenance self-test canary\n' >> "$SELFTEST_CANARY"
  if guard >/dev/null 2>&1; then
    die "guard ACCEPTED a dirty tree — the packaging gate is not fail-closed"
  fi
  say "  PASS"

  cp "$SELFTEST_BACKUP" "$SELFTEST_CANARY"; rm -f "$SELFTEST_BACKUP"; trap - EXIT INT TERM

  say "[3/3] restored tree must pass again"
  if worktree_dirty; then
    die "self-test failed to restore $SELFTEST_CANARY"
  fi
  guard >/dev/null || die "guard rejected the restored tree"
  say "  PASS"

  say "== self-test passed =="
}

case "${1:-guard}" in
  guard) guard ;;
  package) package ;;
  self-test) self_test ;;
  *) die "unknown mode: $1 (expected: guard | package | self-test)" ;;
esac
