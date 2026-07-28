#!/usr/bin/env zsh
# release-provenance — vc-frame packaging and publication provenance truth
#
# Modes:
#   guard        Refuse tracked drift; print packaging identity
#   preflight    Require a fully clean main exactly equal to fetched origin/main
#   create-tag   Create and verify the expected pinned-fingerprint release tag
#   verify-tag   Re-verify the release checkout and local signed tag
#   push-tag     Re-verify everything, refuse an existing remote tag, then push
#   package      guard -> release build -> archive -> checksum -> receipt
#   self-test    Prove the packaging guard rejects tracked drift
#
# The receipt is the answer to "which exact bytes did we ship, built from what?"
# It records the packaging context AND the identity the packaged binary reports
# about itself, so the two can never silently disagree.
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI
set -euo pipefail

export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin:${PATH:-}"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"
DIST="$REPO/target/dist"
BIN_NAME="vc-frame"
RELEASE_KEYS_DIR="${RELEASE_KEYS_DIR:-${HOME}/.keys}"

die() { print -u2 -- "release-provenance: $*"; exit 1; }
info() { print -- "  $*"; }

resolve_python() {
  local candidate
  for candidate in "${PYTHON:-}" python3.14 python3.13 python3.12 python3.11 python3; do
    [[ -n "$candidate" ]] || continue
    command -v "$candidate" >/dev/null 2>&1 || continue
    "$candidate" -c 'import tomllib' >/dev/null 2>&1 || continue
    command -v "$candidate"
    return 0
  done
  die "Python 3.11+ with tomllib is required for release metadata"
}

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

file_size() { wc -c < "$1" | tr -d ' '; }

host_target() { rustc -vV | sed -n 's/^host: //p'; }

workspace_version() {
  local python_bin
  python_bin="$(resolve_python)"
  "$python_bin" -c '
import pathlib, tomllib, sys
p = pathlib.Path(sys.argv[1])
data = tomllib.loads(p.read_text(encoding="utf-8"))
print(data.get("workspace", {}).get("package", {}).get("version")
      or data.get("package", {}).get("version")
      or "")
' "$REPO/Cargo.toml"
}

# Tracked-state cleanliness. Untracked files are Living Tree noise and do not
# enter an archive; tracked drift does, so only that is a packaging blocker.
worktree_dirty() {
  [[ -n "$(git -C "$REPO" status --porcelain --untracked-files=no)" ]]
}

# Publication is stricter than packaging: an untracked source or release input
# can affect a build without being represented by HEAD, so tags require the
# complete checkout to be clean (ignored build outputs remain ignored).
release_worktree_dirty() {
  [[ -n "$(git -C "$REPO" status --porcelain --untracked-files=all)" ]]
}

# RETURNS non-zero rather than exiting, so callers can use it as a predicate
# (the self-test must survive a deliberate rejection to restore its canary).
# The dispatcher at the bottom turns a failed guard into a non-zero exit.
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
    return 1
  fi

  local sha; sha="$(git -C "$REPO" rev-parse HEAD)"
  info "commit: $sha"
  info "clean:  yes (tracked files match HEAD)"
  print -- "== guard passed =="
  return 0
}

require_expected_tag() {
  local tag="${1:-}"
  [[ -n "$tag" ]] || die "release tag is required"
  print -r -- "$tag" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' \
    || die "release tag must have the exact stable form vX.Y.Z (got: $tag)"

  local version expected
  version="$(workspace_version)"
  [[ -n "$version" ]] || die "could not read version from Cargo.toml"
  expected="v$version"
  [[ "$tag" == "$expected" ]] \
    || die "release tag $tag does not match workspace version $version (expected $expected)"
}

pinned_release_fingerprint() {
  local raw fingerprint
  raw="$(
    sed -n 's/^DEFAULT_GPG_FINGERPRINT="\([A-Fa-f0-9]*\)"/\1/p' \
      "$REPO/tools/install.sh"
  )"
  fingerprint="$(print -rn -- "$raw" | tr '[:lower:]' '[:upper:]')"
  [[ -n "$fingerprint" ]] \
    || die "tools/install.sh has no pinned DEFAULT_GPG_FINGERPRINT"
  case "$fingerprint" in
    (*[!A-F0-9]*) die "pinned release fingerprint is not hexadecimal" ;;
  esac
  case "${#fingerprint}" in
    (40|64) ;;
    (*) die "pinned release fingerprint must be a full 40- or 64-hex fingerprint" ;;
  esac
  print -r -- "$fingerprint"
}

selected_gpg_home() {
  local selected="${VCFRAME_GPG_HOMEDIR:-}" candidate
  if [[ -n "$selected" ]]; then
    [[ -d "$selected" ]] || die "VCFRAME_GPG_HOMEDIR is not a directory"
  else
    for candidate in \
      "$RELEASE_KEYS_DIR/vc-frame-gnupg" \
      "$RELEASE_KEYS_DIR/vetcoders-gnupg"; do
      [[ -d "$candidate" ]] || continue
      [[ -z "$selected" ]] \
        || die "multiple release GPG homes found under $RELEASE_KEYS_DIR"
      selected="$candidate"
    done
  fi
  print -r -- "$selected"
}

with_gpg_home() {
  local gpg_home="$1"
  shift
  if [[ -n "$gpg_home" ]]; then
    GNUPGHOME="$gpg_home" "$@"
  else
    "$@"
  fi
}

require_signing_secret_key() {
  local fingerprint="$1" gpg_home="$2" key_listing
  command -v gpg >/dev/null 2>&1 \
    || die "gpg is required to create release tags"
  key_listing="$(
    with_gpg_home "$gpg_home" \
      gpg --batch --with-colons --list-secret-keys "$fingerprint" 2>/dev/null \
      || true
  )"

  local actual_fingerprint capabilities
  actual_fingerprint="$(
    print -r -- "$key_listing" |
      awk -F: '$1 == "sec" {want=1; next} want && $1 == "fpr" {print toupper($10); exit}'
  )"
  capabilities="$(
    print -r -- "$key_listing" |
      awk -F: '$1 == "sec" || $1 == "ssb" {caps = caps $12} END {print caps}'
  )"
  [[ "$actual_fingerprint" == "$fingerprint" ]] \
    || die "secret key for the pinned release fingerprint is unavailable"
  case "$capabilities" in
    (*s*|*S*) ;;
    (*) die "pinned secret key is not signing-capable" ;;
  esac
}

# Resolve origin/main immediately before trusting it. A candidate workflow is
# intentionally not routed through this function; only local production tag
# creation/publication requires a checked-out local main branch.
release_ref_guard() {
  print -- "== release ref preflight =="
  guard >/dev/null \
    || die "release preflight requires tracked files to match HEAD"

  if release_worktree_dirty; then
    print -u2 -- "REFUSING RELEASE: the checkout contains tracked or untracked drift."
    git -C "$REPO" status --short --untracked-files=all >&2
    return 1
  fi

  local branch
  branch="$(git -C "$REPO" symbolic-ref --quiet --short HEAD 2>/dev/null)" \
    || die "release requires the main branch, not a detached HEAD"
  [[ "$branch" == "main" ]] \
    || die "release requires branch main (current branch: $branch)"
  git -C "$REPO" remote get-url origin >/dev/null 2>&1 \
    || die "release requires an origin remote"

  git -C "$REPO" fetch --quiet --no-tags origin \
    '+refs/heads/main:refs/remotes/origin/main' \
    || die "could not refresh origin/main"

  local head origin_main
  head="$(git -C "$REPO" rev-parse HEAD)"
  origin_main="$(git -C "$REPO" rev-parse --verify refs/remotes/origin/main 2>/dev/null)" \
    || die "origin/main does not exist after fetch"
  [[ "$head" == "$origin_main" ]] \
    || die "HEAD $head is not exactly origin/main $origin_main"
  release_worktree_dirty \
    && die "release checkout changed while origin/main was being resolved"

  info "branch: main"
  info "commit: $head"
  info "origin/main: exact"
  info "clean: yes (tracked and untracked)"
  print -- "== release ref preflight passed =="
}

verify_release_tag_local() {
  local tag="$1" fingerprint="$2" gpg_home="$3"
  require_expected_tag "$tag"
  command -v gpg >/dev/null 2>&1 \
    || die "gpg is required to verify release tags"

  local ref="refs/tags/$tag" object_type tag_object
  git -C "$REPO" show-ref --verify --quiet "$ref" \
    || die "release tag $tag does not exist locally"
  object_type="$(git -C "$REPO" cat-file -t "$ref")"
  [[ "$object_type" == "tag" ]] \
    || die "release ref $tag is lightweight; an annotated signed tag is required"
  tag_object="$(git -C "$REPO" rev-parse "$ref")"

  local tag_target tag_target_type declared_tag head
  tag_target="$(
    git -C "$REPO" cat-file -p "$tag_object" |
      sed -n 's/^object //p' |
      sed -n '1p'
  )"
  tag_target_type="$(
    git -C "$REPO" cat-file -p "$tag_object" |
      sed -n 's/^type //p' |
      sed -n '1p'
  )"
  declared_tag="$(
    git -C "$REPO" cat-file -p "$tag_object" |
      sed -n 's/^tag //p' |
      sed -n '1p'
  )"
  head="$(git -C "$REPO" rev-parse HEAD)"
  [[ "$tag_target_type" == "commit" ]] \
    || die "release tag $tag does not point directly to a commit"
  [[ "$tag_target" == "$head" ]] \
    || die "release tag $tag points to $tag_target, not current HEAD $head"
  [[ "$declared_tag" == "$tag" ]] \
    || die "release ref $tag contains a foreign tag object named $declared_tag"

  local verification tag_primary_fingerprint
  verification="$(
    with_gpg_home "$gpg_home" \
      git -C "$REPO" -c gpg.format=openpgp verify-tag --raw "$ref" 2>&1
  )" || {
    print -u2 -r -- "$verification"
    die "release tag $tag has no valid OpenPGP signature"
  }
  tag_primary_fingerprint="$(
    print -r -- "$verification" |
      awk '
        $1 == "[GNUPG:]" && $2 == "VALIDSIG" {
          candidate = toupper($NF)
          if (candidate ~ /^[0-9A-F]+$/ &&
              (length(candidate) == 40 || length(candidate) == 64)) {
            print candidate
          } else {
            print toupper($3)
          }
          exit
        }
      '
  )"
  [[ -n "$tag_primary_fingerprint" ]] \
    || die "release tag $tag produced no VALIDSIG fingerprint"
  [[ "$tag_primary_fingerprint" == "$fingerprint" ]] \
    || die "release tag signer $tag_primary_fingerprint does not match pinned $fingerprint"

  typeset -g VERIFIED_RELEASE_TAG_OBJECT="$tag_object"
  info "tag: $tag"
  info "tag object: $tag_object"
  info "target: $head"
  info "signer: $tag_primary_fingerprint"
}

verify_release_tag() {
  local tag="${1:-}" fingerprint gpg_home
  release_ref_guard
  fingerprint="$(pinned_release_fingerprint)"
  gpg_home="$(selected_gpg_home)"
  verify_release_tag_local "$tag" "$fingerprint" "$gpg_home"
  print -- "== release tag verification passed =="
}

create_release_tag() {
  local tag="${1:-}" fingerprint gpg_home
  release_ref_guard
  require_expected_tag "$tag"
  fingerprint="$(pinned_release_fingerprint)"
  gpg_home="$(selected_gpg_home)"
  require_signing_secret_key "$fingerprint" "$gpg_home"
  git -C "$REPO" show-ref --verify --quiet "refs/tags/$tag" \
    && die "release tag $tag already exists locally"

  with_gpg_home "$gpg_home" \
    git -C "$REPO" -c gpg.format=openpgp tag -s -u "$fingerprint" \
      "$tag" -m "Release $tag"
  typeset -g CREATED_RELEASE_TAG="$tag"
  trap 'git -C "$REPO" tag -d "$CREATED_RELEASE_TAG" >/dev/null 2>&1 || true' \
    EXIT INT TERM
  verify_release_tag_local "$tag" "$fingerprint" "$gpg_home"
  trap - EXIT INT TERM
  print -- "Created and verified signed tag $tag"
  print -- "Push with: make release-push"
}

push_release_tag() {
  local tag="${1:-}" fingerprint gpg_home first_tag_object remote_refs
  release_ref_guard
  fingerprint="$(pinned_release_fingerprint)"
  gpg_home="$(selected_gpg_home)"
  verify_release_tag_local "$tag" "$fingerprint" "$gpg_home"
  first_tag_object="$VERIFIED_RELEASE_TAG_OBJECT"

  remote_refs="$(
    git -C "$REPO" ls-remote --tags origin \
      "refs/tags/$tag" "refs/tags/$tag^{}"
  )" || die "could not inspect origin tag $tag"
  [[ -z "$remote_refs" ]] \
    || die "origin already contains $tag; release tags are immutable and never rewritten"

  # Re-check after the remote read and push the exact verified object ID rather
  # than a mutable local ref. The final command is the first network write.
  release_ref_guard
  verify_release_tag_local "$tag" "$fingerprint" "$gpg_home"
  [[ "$VERIFIED_RELEASE_TAG_OBJECT" == "$first_tag_object" ]] \
    || die "release tag $tag changed during verification"
  git -C "$REPO" push origin \
    "${VERIFIED_RELEASE_TAG_OBJECT}:refs/tags/$tag"
  print -- "Published immutable signed tag $tag"
}

package() {
  guard || exit 1

  local sha version target
  sha="$(git -C "$REPO" rev-parse HEAD)"
  version="$(workspace_version)"
  target="$(host_target)"
  [[ -n "$version" ]] || die "could not read version from Cargo.toml"
  [[ -n "$target" ]] || die "could not resolve host target triple"

  # Fail before the expensive host build if committed plugin bytes drifted.
  # This inventory is embedded into the final receipt so every bundled WASM
  # remains attributable even though distribution ships one host binary.
  local plugin_inventory
  plugin_inventory="$("$REPO/scripts/plugins-parity.zsh" receipt-json)" \
    || die "bundled plugin inventory is not release-ready"

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
  "bundled_plugins": $plugin_inventory,
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

  print -- "[1/4] release Python must provide tomllib"
  local python_bin; python_bin="$(resolve_python)"
  "$python_bin" -c 'import tomllib' \
    || die "resolved release Python cannot import tomllib: $python_bin"
  print -- "  PASS ($python_bin)"

  print -- "[2/4] clean tree must pass"
  guard >/dev/null || die "guard rejected a clean tree"
  print -- "  PASS"

  local canary="$REPO/Cargo.toml"
  # Script-scope, not local: the EXIT trap body is evaluated when it fires,
  # by which time a function-local would already be out of scope.
  typeset -g SELFTEST_CANARY="$canary"
  typeset -g SELFTEST_BACKUP; SELFTEST_BACKUP="$(mktemp)"
  cp "$canary" "$SELFTEST_BACKUP"
  # Restore the tracked file no matter how we leave this function.
  trap 'cp "$SELFTEST_BACKUP" "$SELFTEST_CANARY"; rm -f "$SELFTEST_BACKUP"' EXIT INT TERM

  print -- "[3/4] dirty tree must be rejected"
  printf '\n# release-provenance self-test canary\n' >> "$canary"
  if guard >/dev/null 2>&1; then
    die "guard ACCEPTED a dirty tree — the packaging gate is not fail-closed"
  fi
  print -- "  PASS"

  cp "$SELFTEST_BACKUP" "$canary"; rm -f "$SELFTEST_BACKUP"; trap - EXIT INT TERM

  print -- "[4/4] restored tree must pass again"
  if worktree_dirty; then
    die "self-test failed to restore $canary"
  fi
  guard >/dev/null || die "guard rejected the restored tree"
  print -- "  PASS"

  print -- "== self-test passed =="
}

case "${1:-guard}" in
  guard) guard ;;
  preflight) release_ref_guard ;;
  create-tag) create_release_tag "${2:-}" ;;
  verify-tag) verify_release_tag "${2:-}" ;;
  push-tag) push_release_tag "${2:-}" ;;
  package) package ;;
  self-test) self_test ;;
  *) die "unknown mode: $1 (expected: guard | preflight | create-tag | verify-tag | push-tag | package | self-test)" ;;
esac
