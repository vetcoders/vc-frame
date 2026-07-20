#!/bin/sh
# Negative matrix for tools/install.sh — proves the installer fails CLOSED.
#
# Every case builds a synthetic release tree under a temp dir and serves it to
# the installer over `file://`. The "binary" in the archive is a stub that
# answers the same commands a real vc-frame answers, so the post-install smoke
# contract is exercised for real without needing a cross-compiled artifact.
#
# Run: sh tools/install_test.sh   (or: make install-test)
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$REPO/tools/install.sh"
TEST_VERSION="0.45.4"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0
FAIL=0

red() { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

target_triple() {
  os="$(uname -s | tr '[:upper:]' '[:lower:]')"
  arch="$(uname -m)"
  case "$os:$arch" in
    darwin:arm64|darwin:aarch64) printf 'aarch64-apple-darwin' ;;
    darwin:x86_64) printf 'x86_64-apple-darwin' ;;
    linux:x86_64|linux:amd64) printf 'x86_64-unknown-linux-musl' ;;
    linux:aarch64|linux:arm64) printf 'aarch64-unknown-linux-musl' ;;
    *) printf '' ;;
  esac
}

TARGET="$(target_triple)"
if [ -z "$TARGET" ]; then
  red "unsupported host platform for installer tests: $(uname -s)/$(uname -m)"
  exit 1
fi

# A stub that behaves like an installed vc-frame. `stub_mode` selects which
# part of the smoke contract it breaks, so we can prove the installer notices.
write_stub_binary() {
  dest="$1"
  stub_mode="${2:-ok}"
  cat >"$dest" <<STUB
#!/bin/sh
case "\$1" in
  --version) [ "$stub_mode" = "bad-version" ] && exit 3
             echo "vc-frame $TEST_VERSION+gdeadbeef" ;;
  --build-info) [ "$stub_mode" = "bad-build-info" ] && exit 3
             printf '{\n  "product": "vc-frame",\n  "version": "$TEST_VERSION",\n  "git_sha": "deadbeef00000000000000000000000000000000"\n}\n' ;;
  setup) [ "$stub_mode" = "bad-setup" ] && exit 3
             echo "[Version]: $TEST_VERSION+gdeadbeef" ;;
  # Fresh machine: no sessions yet -> exits 1 with this exact message, exactly
  # like the real binary. The installer must accept that and reject anything else.
  list-sessions) [ "$stub_mode" = "bad-session" ] && { echo "session subsystem exploded"; exit 3; }
             echo "No active vc-frame sessions found." >&2; exit 1 ;;
  *) exit 1 ;;
esac
exit 0
STUB
  chmod 0755 "$dest"
}

# Build a complete, valid release tree. Individual cases then break exactly one
# thing, so each failure is attributable.
build_release_tree() {
  root="$1"
  stub_mode="${2:-ok}"
  rel="$root/v$TEST_VERSION"
  rm -rf "$root"
  mkdir -p "$rel/stage"

  write_stub_binary "$rel/stage/vc-frame" "$stub_mode"
  archive="vc-frame-$TARGET.tar.gz"
  (cd "$rel/stage" && tar czf "../$archive" vc-frame)
  rm -rf "$rel/stage"
  printf '%s  %s\n' "$(sha256_of "$rel/$archive")" "$archive" >"$rel/$archive.sha256"

  cat >"$rel/manifest.json" <<MANIFEST
{
  "product": "vc-frame",
  "version": "$TEST_VERSION",
  "tag": "v$TEST_VERSION",
  "signing_key": "vc-frame-signing.asc",
  "artifacts": {
    "$TARGET": "vc-frame-$TARGET.tar.gz"
  }
}
MANIFEST
}

# run_case <name> <expect: pass|fail> <release-root> <prefix> [extra env assignments...]
run_case() {
  name="$1"; expect="$2"; root="$3"; prefix="$4"; shift 4
  rm -rf "$prefix"
  mkdir -p "$prefix"
  log="$WORK/log.$$"

  if env "$@" \
      VCFRAME_VERSION="$TEST_VERSION" \
      VCFRAME_BASE_URL="file://$root" \
      INSTALL_DIR="$prefix" \
      VCFRAME_NO_PROFILE_UPDATE=1 \
      sh "$INSTALLER" >"$log" 2>&1; then
    actual=pass
  else
    actual=fail
  fi

  if [ "$actual" = "$expect" ]; then
    green "  PASS  $name (installer $actual, expected $expect)"
    PASS=$((PASS + 1))
  else
    red "  FAIL  $name (installer $actual, expected $expect)"
    sed 's/^/        | /' "$log"
    FAIL=$((FAIL + 1))
  fi
}

printf '\n== installer negative matrix (target: %s) ==\n\n' "$TARGET"

ROOT="$WORK/release"
PREFIX="$WORK/prefix"

# --- positive control -------------------------------------------------------
build_release_tree "$ROOT"
run_case "valid release installs" pass "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

# Clean-prefix contract: the binary is there and no `zellij` alias was created.
if [ -x "$PREFIX/vc-frame" ] && [ ! -e "$PREFIX/zellij" ]; then
  green "  PASS  clean prefix has vc-frame and no zellij alias"
  PASS=$((PASS + 1))
else
  red "  FAIL  clean prefix contract (vc-frame present / zellij absent)"
  ls -la "$PREFIX" | sed 's/^/        | /'
  FAIL=$((FAIL + 1))
fi

# --- provenance / manifest --------------------------------------------------
build_release_tree "$ROOT"
rm -f "$ROOT/v$TEST_VERSION/manifest.json"
run_case "missing manifest fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
printf 'not json at all\n' >"$ROOT/v$TEST_VERSION/manifest.json"
run_case "malformed manifest fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
sed 's/"product": "vc-frame"/"product": "some-other-tool"/' \
  "$ROOT/v$TEST_VERSION/manifest.json" >"$ROOT/v$TEST_VERSION/manifest.tmp"
mv "$ROOT/v$TEST_VERSION/manifest.tmp" "$ROOT/v$TEST_VERSION/manifest.json"
run_case "foreign product manifest fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
sed "s/\"version\": \"$TEST_VERSION\"/\"version\": \"9.9.9\"/" \
  "$ROOT/v$TEST_VERSION/manifest.json" >"$ROOT/v$TEST_VERSION/manifest.tmp"
mv "$ROOT/v$TEST_VERSION/manifest.tmp" "$ROOT/v$TEST_VERSION/manifest.json"
run_case "manifest version mismatch fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
sed "s/\"$TARGET\"/\"some-unbuilt-target\"/" \
  "$ROOT/v$TEST_VERSION/manifest.json" >"$ROOT/v$TEST_VERSION/manifest.tmp"
mv "$ROOT/v$TEST_VERSION/manifest.tmp" "$ROOT/v$TEST_VERSION/manifest.json"
run_case "manifest without this target fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

# --- archive / checksum -----------------------------------------------------
build_release_tree "$ROOT"
rm -f "$ROOT/v$TEST_VERSION/vc-frame-$TARGET.tar.gz"
run_case "missing archive fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
rm -f "$ROOT/v$TEST_VERSION/vc-frame-$TARGET.tar.gz.sha256"
run_case "missing checksum sidecar fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT"
printf '%s  %s\n' \
  "0000000000000000000000000000000000000000000000000000000000000000" \
  "vc-frame-$TARGET.tar.gz" >"$ROOT/v$TEST_VERSION/vc-frame-$TARGET.tar.gz.sha256"
run_case "checksum mismatch fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

# --- signature / fingerprint (strict mode) ----------------------------------
build_release_tree "$ROOT"
run_case "strict mode without pinned fingerprint fails closed" fail "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1

build_release_tree "$ROOT"
run_case "strict mode with no published key fails closed" fail "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT=DEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF

# --- post-install smoke contract --------------------------------------------
build_release_tree "$ROOT" bad-version
run_case "binary failing --version fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT" bad-setup
run_case "binary failing 'setup --check' fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT" bad-session
run_case "binary failing a session command fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

build_release_tree "$ROOT" bad-build-info
run_case "binary without embedded provenance fails closed" fail "$ROOT" "$PREFIX" VCFRAME_REQUIRE_GPG=0

printf '\n== %d passed, %d failed ==\n\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
