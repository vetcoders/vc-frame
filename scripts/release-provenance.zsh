#!/usr/bin/env zsh
# release-provenance — vc-frame packaging provenance truth
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

die() { print -u2 -- "release-provenance: $*"; exit 1; }
info() { print -- "  $*"; }

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

file_size() { wc -c < "$1" | tr -d ' '; }

host_target() { rustc -vV | sed -n 's/^host: //p'; }

# Tracked-state cleanliness. Untracked files are Living Tree noise and do not
# enter an archive; tracked drift does, so only that is a packaging blocker.
worktree_dirty() {
  [[ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]]
}

guard() {
  print -- "== release provenance guard =="
  git -C "$REPO" rev-parse --git-dir >/dev/null 2>&1 \
    || die "not a git checkout; packaging requires a verifiable source commit"

  if worktree_dirty; then
    print -u2 -- ""
    print -u2 -- "REFUSING TO PACKAGE: tracked files differ from HEAD."
    print -u2 -- "A release archive must correspond to an exact published commit."
    print -u2 -- ""
    git -C "$REPO" status --porcelain --untracked-files=no >&2
    print -u2 -- ""
    exit 1
  fi

  local sha; sha="$(git -C "$REPO" rev-parse HEAD)"
  info "commit: $sha"
  info "clean:  yes (tracked files match HEAD)"
  print -- "== guard passed =="
}

package() {
  guard

  local sha version target
  sha="$(git -C "$REPO" rev-parse HEAD)"
  version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"
  target="$(host_target)"
  [[ -n "$version" ]] || die "could not read version from Cargo.toml"
  [[ -n "$target" ]] || die "could not resolve host target triple"

  print -- ""
  print -- "== packaging vc-frame $version ($target) =="

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

  print -- ""
  print -- "== package receipt =="
  cat "$DIST/RECEIPT.json"
  print -- ""
  info "archive: $DIST/$archive"
  info "receipt: $DIST/RECEIPT.json"
}

# Proves the guard is real: it must reject a dirty tree, not merely warn.
self_test() {
  print -- "== release provenance guard self-test =="
  if worktree_dirty; then
    die "self-test needs a clean tree to start from (tracked files differ from HEAD)"
  fi

  print -- "[1/3] clean tree must pass"
  guard >/dev/null || die "guard rejected a clean tree"
  print -- "  PASS"

  local canary="$REPO/Cargo.toml"
  local backup; backup="$(mktemp)"
  cp "$canary" "$backup"
  # Restore the tracked file no matter how we leave this function.
  trap 'cp "$backup" "$canary"; rm -f "$backup"' EXIT INT TERM

  print -- "[2/3] dirty tree must be rejected"
  printf '\n# release-provenance self-test canary\n' >> "$canary"
  if guard >/dev/null 2>&1; then
    die "guard ACCEPTED a dirty tree — the packaging gate is not fail-closed"
  fi
  print -- "  PASS"

  cp "$backup" "$canary"; rm -f "$backup"; trap - EXIT INT TERM

  print -- "[3/3] restored tree must pass again"
  worktree_dirty && die "self-test failed to restore $canary"
  guard >/dev/null || die "guard rejected the restored tree"
  print -- "  PASS"

  print -- "== self-test passed =="
}

case "${1:-guard}" in
  guard) guard ;;
  package) package ;;
  self-test) self_test ;;
  *) die "unknown mode: $1 (expected: guard | package | self-test)" ;;
esac
