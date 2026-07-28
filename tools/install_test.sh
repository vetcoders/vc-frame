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
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
INSTALLER="$REPO/tools/install.sh"
TEST_VERSION="0.45.4"
REPLAY_VERSION="0.44.9"
TEST_GIT_SHA="deadbeef00000000000000000000000000000000"
REPLAY_GIT_SHA="1111111111111111111111111111111111111111"
WORK="$(mktemp -d)"
GPG_HOME="$WORK/signing-gnupg"

cleanup() {
  if command -v gpgconf >/dev/null 2>&1; then
    gpgconf --homedir "$GPG_HOME" --kill all >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

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
  reported_version="${3:-$TEST_VERSION}"
  build_version="${4:-$reported_version}"
  build_git_sha="${5:-$TEST_GIT_SHA}"
  cat >"$dest" <<STUB
#!/bin/sh
case "\$1" in
  --version) [ "$stub_mode" = "bad-version" ] && exit 3
             echo "vc-frame $reported_version+gdeadbeef" ;;
  --build-info) [ "$stub_mode" = "bad-build-info" ] && exit 3
             printf '{\n  "product": "vc-frame",\n  "version": "$build_version",\n  "git_sha": "$build_git_sha"\n}\n' ;;
  setup) [ "$stub_mode" = "bad-setup" ] && exit 3
             echo "[Version]: $reported_version+gdeadbeef" ;;
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
  reported_version="${3:-$TEST_VERSION}"
  build_version="${4:-$reported_version}"
  build_git_sha="${5:-$TEST_GIT_SHA}"
  manifest_git_sha="${6:-$TEST_GIT_SHA}"
  rel="$root/v$TEST_VERSION"
  rm -rf "$root"
  mkdir -p "$rel/stage"

  write_stub_binary "$rel/stage/vc-frame" "$stub_mode" \
    "$reported_version" "$build_version" "$build_git_sha"
  archive="vc-frame-$TARGET.tar.gz"
  (cd "$rel/stage" && tar czf "../$archive" vc-frame)
  rm -rf "$rel/stage"
  printf '%s  %s\n' "$(sha256_of "$rel/$archive")" "$archive" >"$rel/$archive.sha256"

  cat >"$rel/manifest.json" <<MANIFEST
{
  "product": "vc-frame",
  "version": "$TEST_VERSION",
  "git_sha": "$manifest_git_sha",
  "tag": "v$TEST_VERSION",
  "signing_key": "vc-frame-signing.asc",
  "artifacts": {
    "$TARGET": "vc-frame-$TARGET.tar.gz"
  }
}
MANIFEST
}

generate_test_keys() {
  command -v gpg >/dev/null 2>&1 || {
    red "gpg is required for dynamic installer trust tests"
    exit 1
  }
  command -v gpgconf >/dev/null 2>&1 || {
    red "gpgconf is required for dynamic installer trust tests"
    exit 1
  }
  mkdir -p "$GPG_HOME"
  chmod 700 "$GPG_HOME"

  gpg --homedir "$GPG_HOME" --batch --pinentry-mode loopback --passphrase "" \
    --quick-generate-key "Pinned Installer <pinned@example.invalid>" \
    ed25519 cert 0 >/dev/null 2>&1
  PINNED_FINGERPRINT="$(
    gpg --homedir "$GPG_HOME" --batch --with-colons \
      --list-secret-keys "Pinned Installer" |
      awk -F: '$1 == "fpr" { print toupper($10); exit }'
  )"
  gpg --homedir "$GPG_HOME" --batch --pinentry-mode loopback --passphrase "" \
    --quick-add-key "$PINNED_FINGERPRINT" ed25519 sign 0 >/dev/null 2>&1

  gpg --homedir "$GPG_HOME" --batch --pinentry-mode loopback --passphrase "" \
    --quick-generate-key "Foreign Installer <foreign@example.invalid>" \
    ed25519 sign 0 >/dev/null 2>&1
  FOREIGN_FINGERPRINT="$(
    gpg --homedir "$GPG_HOME" --batch --with-colons \
      --list-secret-keys "Foreign Installer" |
      awk -F: '$1 == "fpr" { print toupper($10); exit }'
  )"

  [ -n "$PINNED_FINGERPRINT" ] && [ -n "$FOREIGN_FINGERPRINT" ] || {
    red "could not read generated GPG primary fingerprints"
    exit 1
  }
  [ "$PINNED_FINGERPRINT" != "$FOREIGN_FINGERPRINT" ] || {
    red "dynamic GPG fixtures unexpectedly share a fingerprint"
    exit 1
  }
}

sign_release_tree() {
  root="$1"
  manifest_signer="$2"
  archive_signer="${3:-$manifest_signer}"
  rel="$root/v$TEST_VERSION"
  archive="$rel/vc-frame-$TARGET.tar.gz"

  # Publish both public keys deliberately. The installer must bind VALIDSIG's
  # primary fingerprint to the pin, not merely trust any imported bundle key.
  gpg --homedir "$GPG_HOME" --batch --armor \
    --export "$PINNED_FINGERPRINT" "$FOREIGN_FINGERPRINT" \
    >"$rel/vc-frame-signing.asc"
  gpg --homedir "$GPG_HOME" --batch --yes --pinentry-mode loopback \
    --passphrase "" --local-user "$manifest_signer" \
    --detach-sign --output "$rel/manifest.json.sig" "$rel/manifest.json"
  gpg --homedir "$GPG_HOME" --batch --yes --pinentry-mode loopback \
    --passphrase "" --local-user "$archive_signer" \
    --detach-sign --output "$archive.sig" "$archive"
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

# run_case_failure_contains <name> <release-root> <prefix> <needle> [env...]
run_case_failure_contains() {
  name="$1"; root="$2"; prefix="$3"; needle="$4"; shift 4
  rm -rf "$prefix"
  mkdir -p "$prefix"
  log="$WORK/log.contains.$$"

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

  if [ "$actual" = fail ] && grep -F "$needle" "$log" >/dev/null 2>&1; then
    green "  PASS  $name (failed closed at expected boundary)"
    PASS=$((PASS + 1))
  else
    red "  FAIL  $name (expected failure containing: $needle)"
    sed 's/^/        | /' "$log"
    FAIL=$((FAIL + 1))
  fi
}

# Same invocation contract as run_case, but seeds an existing binary and proves
# a rejected release cannot mutate the prefix or leave staging debris behind.
run_protected_failure_case() {
  name="$1"; root="$2"; prefix="$3"; shift 3
  rm -rf "$prefix"
  mkdir -p "$prefix"
  sentinel="$prefix/vc-frame"
  printf '#!/bin/sh\nprintf "existing-vc-frame-sentinel\\n"\n' >"$sentinel"
  chmod 0755 "$sentinel"
  before_sha="$(sha256_of "$sentinel")"
  log="$WORK/log.protected.$$"

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

  after_sha=""
  [ -f "$sentinel" ] && after_sha="$(sha256_of "$sentinel")"
  leftovers="$(
    find "$prefix" -mindepth 1 -maxdepth 1 ! -name vc-frame -print
  )"
  if [ "$actual" = fail ] && [ "$before_sha" = "$after_sha" ] && \
    [ -z "$leftovers" ]; then
    green "  PASS  $name (rejected; existing binary byte-identical; no leftovers)"
    PASS=$((PASS + 1))
  else
    red "  FAIL  $name"
    printf '        | result: %s\n' "$actual"
    printf '        | sentinel before: %s\n' "$before_sha"
    printf '        | sentinel after:  %s\n' "${after_sha:-<missing>}"
    printf '        | leftovers: %s\n' "${leftovers:-<none>}"
    sed 's/^/        | /' "$log"
    FAIL=$((FAIL + 1))
  fi
}

printf '\n== installer negative matrix (target: %s) ==\n\n' "$TARGET"

ROOT="$WORK/release"
PREFIX="$WORK/prefix"

# --- configuration enum (must fail before any download) ---------------------
INVALID_ROOT="$WORK/invalid-release-root-that-does-not-exist"
for invalid_require_gpg in true 2 ""; do
  display_value="${invalid_require_gpg:-<empty>}"
  run_case_failure_contains \
    "VCFRAME_REQUIRE_GPG=$display_value is rejected before downloads" \
    "$INVALID_ROOT" "$PREFIX" "must be exactly 0 or 1" \
    "VCFRAME_REQUIRE_GPG=$invalid_require_gpg"
done

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
sed '/"git_sha":/d' \
  "$ROOT/v$TEST_VERSION/manifest.json" >"$ROOT/v$TEST_VERSION/manifest.tmp"
mv "$ROOT/v$TEST_VERSION/manifest.tmp" "$ROOT/v$TEST_VERSION/manifest.json"
run_case "manifest without full git SHA fails closed" fail "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=0

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

# The positive control uses a cert-only primary plus signing subkey. The public
# bundle also contains a foreign key, which must never become an alternate
# trust root.
generate_test_keys

build_release_tree "$ROOT"
sign_release_tree "$ROOT" "$PINNED_FINGERPRINT"
run_case "pinned signing subkey installs" pass "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

build_release_tree "$ROOT"
sign_release_tree "$ROOT" "$PINNED_FINGERPRINT"
rm -f "$ROOT/v$TEST_VERSION/manifest.json.sig"
run_case "missing manifest signature fails closed" fail "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

build_release_tree "$ROOT"
sign_release_tree "$ROOT" "$FOREIGN_FINGERPRINT"
run_case "foreign signer in imported bundle fails closed" fail "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

build_release_tree "$ROOT"
sign_release_tree "$ROOT" "$PINNED_FINGERPRINT" "$FOREIGN_FINGERPRINT"
run_case_failure_contains \
  "pinned manifest with foreign archive signer fails closed" \
  "$ROOT" "$PREFIX" "for vc-frame-$TARGET.tar.gz" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

# A valid historical signature is not proof that the artifact is the requested
# release. Reject both an old binary replay and a current-semver binary built
# from a different commit, without touching an existing installation.
build_release_tree "$ROOT" ok "$REPLAY_VERSION" "$REPLAY_VERSION" \
  "$REPLAY_GIT_SHA" "$TEST_GIT_SHA"
sign_release_tree "$ROOT" "$PINNED_FINGERPRINT"
run_protected_failure_case \
  "pinned signed old-binary replay preserves existing install" \
  "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

build_release_tree "$ROOT" ok "$TEST_VERSION" "$TEST_VERSION" \
  "$REPLAY_GIT_SHA" "$TEST_GIT_SHA"
sign_release_tree "$ROOT" "$PINNED_FINGERPRINT"
run_protected_failure_case \
  "pinned signed wrong-commit binary preserves existing install" \
  "$ROOT" "$PREFIX" \
  VCFRAME_REQUIRE_GPG=1 VCFRAME_GPG_FINGERPRINT="$PINNED_FINGERPRINT"

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
