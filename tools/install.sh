#!/bin/sh
# vc-frame installer — canonical curl | sh entry point.
#
# Usage:
#   VCFRAME_GPG_FINGERPRINT=<pinned-fingerprint> \
#     sh -c "$(curl -fsSL https://github.com/vetcoders/vc-frame/releases/latest/download/install.sh)"
#
# Env overrides:
#   VCFRAME_VERSION        release version (default: 0.47.2)
#   INSTALL_DIR            where the `vc-frame` binary is placed (default: ~/.local/bin)
#   VCFRAME_BASE_URL       release base URL (default: GitHub Releases download root)
#                          Per-version artifacts live under $BASE_URL/v$VERSION/.
#   VCFRAME_GPG_KEY_URL    release signing public key URL
#                          (default: $BASE_URL/v$VERSION/vc-frame-signing.asc)
#   VCFRAME_GPG_FINGERPRINT  expected release key fingerprint. The imported
#                          public key MUST match it. Strict mode rejects an
#                          empty fingerprint.
#   VCFRAME_REQUIRE_GPG=1  fail if the GPG key or .sig sidecar is unavailable
#                          (default: 1; exact enum: 0 or 1). Set 0 to allow
#                          install without a signature (still enforces the
#                          SHA256 sidecar and manifest/binary consistency).
#   VCFRAME_NO_PROFILE_UPDATE=1  do not edit ~/.zshrc when PATH is missing
#
# Design notes (mirrors the loctree 0.12 installer hardening):
#   - NO silent source fallback. vc-frame is a prebuilt binary, not a crate;
#     any download/verification failure fails loudly.
#   - GPG is the trust root under VCFRAME_REQUIRE_GPG=1; the SHA256 sidecar is
#     always enforced.
#   - manifest.json is MANDATORY and is the only source of the archive name.
#     A missing, malformed, foreign or version-mismatched manifest aborts the
#     install; strict mode authenticates it before parsing via manifest.json.sig.
#     Its full git_sha must match the binary's embedded build provenance. There
#     is no guessed-filename fallback. A guessed name can only ever agree with
#     the release by luck, and "by luck" is not provenance.
#   - Linux targets resolve to the musl-static build (`-unknown-linux-musl`),
#     which is the maximally-portable choice for this standalone binary — the
#     name MUST match what .github/workflows/release.yml uploads.
#   - The release tarball contains a bare `vc-frame` binary at its root.
#   - Post-install smoke is a contract, not a liveness ping: `--version`,
#     `--build-info` (embedded provenance), `setup --check`, and one real
#     session command must all behave. Any failure aborts the install.
#   - Whole script runs through main() invoked on the last line, so a truncated
#     `curl | sh` transfer executes nothing.

set -eu
umask 022

VERSION="${VCFRAME_VERSION:-0.47.2}"
INSTALL_DIR="${INSTALL_DIR:-"$HOME/.local/bin"}"
BASE_URL="${VCFRAME_BASE_URL:-https://github.com/vetcoders/vc-frame/releases/download}"
if [ "${VCFRAME_REQUIRE_GPG+x}" = x ]; then
  REQUIRE_GPG="$VCFRAME_REQUIRE_GPG"
else
  REQUIRE_GPG=1
fi
DEFAULT_GPG_FINGERPRINT=""
GPG_FINGERPRINT="${VCFRAME_GPG_FINGERPRINT:-$DEFAULT_GPG_FINGERPRINT}"
BIN_NAME="vc-frame"
MANIFEST_GIT_SHA=""

red() { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[0;33m%s\033[0m\n' "$*"; }
blue() { printf '\033[0;34m%s\033[0m\n' "$*"; }

validate_configuration() {
  case "$REQUIRE_GPG" in
    0|1) ;;
    *)
      red "VCFRAME_REQUIRE_GPG must be exactly 0 or 1 (got: ${REQUIRE_GPG:-<empty>})"
      exit 1
      ;;
  esac
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    red "missing required command: $1"
    exit 1
  fi
}

sha256_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    red "missing shasum or sha256sum for release verification"
    exit 1
  fi
}

skip_signature_verification() {
  reason="$1"
  if [ "$REQUIRE_GPG" = "1" ]; then
    red "strict mode (VCFRAME_REQUIRE_GPG=1) requires GPG verification: $reason"
    exit 1
  fi
  yellow "$reason; skipping signature verification (VCFRAME_REQUIRE_GPG=0)"
}

normalize_fingerprint() {
  printf '%s' "$1" | tr -d '[:space:]' | tr '[:lower:]' '[:upper:]'
}

# Linux -> musl (the standalone, maximally-portable build). The names here MUST
# match the targets release.yml uploads.
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

# POSIX sh JSON reads, no jq. Split on commas/braces (URLs and file names
# contain neither) so each key/value lands on its own line.
#
# Both helpers match on the KEY, never on the shape of the value: a manifest
# that merely happens to mention a string ending in "-<target>.tar.gz" must not
# satisfy a lookup for that target.
manifest_string_field() {
  tr ',{}' '\n\n\n' < "$1" \
    | sed -n 's/^[[:space:]]*"'"$2"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1
}

manifest_artifact_name() {
  manifest_file="$1"
  mf_target="$2"
  raw="$(tr ',{}' '\n\n\n' < "$manifest_file" \
    | sed -n 's/^[[:space:]]*"'"$mf_target"'"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n 1)"
  [ -n "$raw" ] || return 1
  # Strip any URL path prefix; we only want the asset file name.
  printf '%s\n' "${raw##*/}"
}

# The manifest is the provenance root for the download. Reject anything that is
# not a well-formed vc-frame manifest for exactly the version being installed.
validate_manifest() {
  manifest_file="$1"

  if ! grep -q '"artifacts"' "$manifest_file" 2>/dev/null; then
    red "release manifest is malformed: no \"artifacts\" section"
    exit 1
  fi

  mf_product="$(manifest_string_field "$manifest_file" product)"
  if [ "$mf_product" != "vc-frame" ]; then
    red "release manifest is not a vc-frame manifest (product: ${mf_product:-<missing>})"
    exit 1
  fi

  mf_version="$(manifest_string_field "$manifest_file" version)"
  if [ -z "$mf_version" ]; then
    red "release manifest does not declare a version"
    exit 1
  fi
  if [ "$mf_version" != "$VERSION" ]; then
    red "release manifest version mismatch"
    printf 'requested: %s\nmanifest:  %s\n' "$VERSION" "$mf_version"
    exit 1
  fi

  mf_git_sha="$(manifest_string_field "$manifest_file" git_sha)"
  mf_git_sha="$(printf '%s' "$mf_git_sha" | tr '[:upper:]' '[:lower:]')"
  case "$mf_git_sha" in
    ""|*[!0-9a-f]*)
      red "release manifest does not declare a hexadecimal git_sha"
      exit 1
      ;;
  esac
  if [ "${#mf_git_sha}" -ne 40 ]; then
    red "release manifest git_sha is not a full 40-character commit identity"
    exit 1
  fi
  MANIFEST_GIT_SHA="$mf_git_sha"

  green "manifest ok: vc-frame $mf_version ($MANIFEST_GIT_SHA)"
}

# GPG verification — the trust root under strict mode.
verify_gpg_signature() {
  file="$1"
  base_url="$2"
  tmp="$3"
  key_url="$4"
  sig_file="$file.sig"
  pub_file="$tmp/vc-frame-signing.asc"
  gnupg_home="$tmp/gnupg"
  expected_fingerprint=""

  if [ "$REQUIRE_GPG" = "1" ] && [ -z "$GPG_FINGERPRINT" ]; then
    red "strict mode requires a pinned VCFRAME_GPG_FINGERPRINT"
    exit 1
  fi

  if ! command -v gpg >/dev/null 2>&1; then
    skip_signature_verification "gpg unavailable"
    return 0
  fi
  if ! curl -fsSL "$key_url" -o "$pub_file" 2>/dev/null; then
    skip_signature_verification "GPG signing key unavailable ($key_url)"
    return 0
  fi

  if [ -n "$GPG_FINGERPRINT" ]; then
    expected_fingerprint="$(normalize_fingerprint "$GPG_FINGERPRINT")"
    case "$expected_fingerprint" in
      ""|*[!A-F0-9]*)
        red "VCFRAME_GPG_FINGERPRINT is not a hexadecimal GPG fingerprint"
        exit 1
        ;;
    esac
    published_primary_fingerprints="$(
      gpg --batch --with-colons --import-options show-only \
        --import "$pub_file" 2>/dev/null |
        awk -F: '
          $1 == "pub" { want_primary_fingerprint = 1; next }
          want_primary_fingerprint && $1 == "fpr" {
            print toupper($10)
            want_primary_fingerprint = 0
          }
        '
    )"
    matching_primary_count="$(
      printf '%s\n' "$published_primary_fingerprints" |
        awk -v expected="$expected_fingerprint" '
          $0 == expected { count += 1 }
          END { print count + 0 }
        '
    )"
    if [ "$matching_primary_count" -ne 1 ]; then
      red "GPG signing key bundle does not contain exactly one pinned primary key"
      printf 'expected: %s\n' "$expected_fingerprint"
      exit 1
    fi
    if [ -z "$published_primary_fingerprints" ]; then
      red "GPG signing key primary fingerprint could not be read"
      exit 1
    fi
    green "GPG key bundle contains pinned primary: $expected_fingerprint"
  fi

  if ! curl -fsSL "$base_url/$(basename "$sig_file")" -o "$sig_file" 2>/dev/null; then
    skip_signature_verification "signature sidecar unavailable ($base_url/$(basename "$sig_file"))"
    return 0
  fi
  mkdir -p "$gnupg_home"
  chmod 700 "$gnupg_home"
  if ! GNUPGHOME="$gnupg_home" gpg --batch --quiet \
    --import "$pub_file" >/dev/null 2>&1; then
    red "GPG signing key import failed"
    exit 1
  fi

  status_file="$tmp/gpg-verify.status"
  if ! GNUPGHOME="$gnupg_home" gpg --batch --quiet --status-fd 3 \
    --verify "$sig_file" "$file" 3>"$status_file" >/dev/null 2>&1; then
    red "signature verification failed for $(basename "$file")"
    exit 1
  fi

  if [ -n "$expected_fingerprint" ]; then
    signed_primary_fingerprints="$(
      awk '
        $1 == "[GNUPG:]" && $2 == "VALIDSIG" {
          print toupper($NF)
        }
      ' "$status_file"
    )"
    if [ "$signed_primary_fingerprints" != "$expected_fingerprint" ]; then
      red "signature signer does not match the pinned GPG primary fingerprint for $(basename "$file")"
      printf 'expected: %s\nactual:   %s\n' \
        "$expected_fingerprint" "${signed_primary_fingerprints:-<missing>}"
      exit 1
    fi
    green "GPG signature signer pinned: $expected_fingerprint"
  else
    green "GPG signature cryptographically valid (unpinned non-strict mode)"
  fi
}

verify_binary_release_identity() {
  binary="$1"
  description="$2"
  expected_git_sha="$3"

  if ! version_output="$("$binary" --version 2>&1)"; then
    smoke_fail "$description --version exited non-zero" "$version_output"
  fi
  case "$version_output" in
    "$BIN_NAME "[![:space:]]*) reported_version="${version_output#"$BIN_NAME "}" ;;
    *) smoke_fail "$description reported an invalid product/version" "$version_output" ;;
  esac
  case "$reported_version" in
    *[[:space:]]*) smoke_fail "$description reported an ambiguous version" "$version_output" ;;
  esac
  reported_semver="${reported_version%%+*}"
  if [ "$reported_semver" != "$VERSION" ]; then
    smoke_fail "$description semantic version does not match the requested release" \
      "requested: $VERSION" "reported:  $reported_version"
  fi

  if ! build_info="$("$binary" --build-info 2>&1)"; then
    smoke_fail "$description --build-info exited non-zero" "$build_info"
  fi
  build_product="$(
    printf '%s\n' "$build_info" | tr ',{}' '\n\n\n' |
      sed -n 's/^[[:space:]]*"product"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"
  build_version="$(
    printf '%s\n' "$build_info" | tr ',{}' '\n\n\n' |
      sed -n 's/^[[:space:]]*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"
  build_git_sha="$(
    printf '%s\n' "$build_info" | tr ',{}' '\n\n\n' |
      sed -n 's/^[[:space:]]*"git_sha"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' |
      head -n 1
  )"
  if [ "$build_product" != "$BIN_NAME" ]; then
    smoke_fail "$description build provenance names a foreign product" "$build_info"
  fi
  if [ "$build_version" != "$VERSION" ]; then
    smoke_fail "$description build provenance version does not match the requested release" \
      "requested: $VERSION" "build:     ${build_version:-<missing>}"
  fi
  build_git_sha="$(printf '%s' "$build_git_sha" | tr '[:upper:]' '[:lower:]')"
  case "$build_git_sha" in
    ""|*[!0-9a-f]*)
      smoke_fail "$description build provenance has no hexadecimal git SHA" "$build_info"
      ;;
  esac
  if [ "${#build_git_sha}" -ne 40 ]; then
    smoke_fail "$description build provenance has no full 40-character git SHA" \
      "$build_info"
  fi
  if [ "$build_git_sha" != "$expected_git_sha" ]; then
    smoke_fail "$description commit does not match the authenticated manifest" \
      "manifest: $expected_git_sha" "build:    $build_git_sha"
  fi

  green "$description identity: $BIN_NAME $reported_version ($build_git_sha)"
}

install_prebuilt() {
  target="$1"
  artifact_base="$BASE_URL/v$VERSION"
  key_url="${VCFRAME_GPG_KEY_URL:-$artifact_base/vc-frame-signing.asc}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  blue "[1/5] resolving release artifact"
  need_cmd curl
  need_cmd tar

  manifest_file="$tmp/manifest.json"
  if ! curl -fsSL "$artifact_base/manifest.json" -o "$manifest_file" 2>/dev/null; then
    red "release manifest unavailable: $artifact_base/manifest.json"
    red "vc-frame will not install from a guessed artifact name."
    exit 1
  fi
  # Authenticate the version, commit and artifact map before parsing or using
  # any manifest field. Non-strict opt-out retains identity consistency checks,
  # but deliberately gives up signer authentication.
  verify_gpg_signature "$manifest_file" "$artifact_base" "$tmp" "$key_url"
  validate_manifest "$manifest_file"

  if ! archive="$(manifest_artifact_name "$manifest_file" "$target")"; then
    red "release $VERSION does not provide a bundle for $target (per manifest.json)"
    exit 1
  fi
  green "artifact (from manifest): $archive"
  url="$artifact_base/$archive"
  sha_url="$url.sha256"

  blue "[2/5] downloading release bundle"
  if ! curl -fsSL "$url" -o "$tmp/$archive"; then
    red "release bundle download failed: $url"
    exit 1
  fi
  if ! curl -fsSL "$sha_url" -o "$tmp/$archive.sha256"; then
    red "checksum sidecar download failed: $sha_url"
    exit 1
  fi

  blue "[3/5] verifying SHA256 + signature"
  expected="$(awk '{print $1}' "$tmp/$archive.sha256")"
  actual="$(sha256_file "$tmp/$archive")"
  if [ "$actual" != "$expected" ]; then
    red "checksum mismatch for $archive"
    printf 'expected: %s\nactual:   %s\n' "$expected" "$actual"
    exit 1
  fi
  green "checksum ok: $actual"

  verify_gpg_signature "$tmp/$archive" "$artifact_base" "$tmp" "$key_url"

  blue "[4/5] installing binary"
  extract_dir="$tmp/extract"
  mkdir -p "$extract_dir"
  tar -xzf "$tmp/$archive" -C "$extract_dir"
  # The release tarball carries a bare `vc-frame` binary at its root.
  bin_src="$extract_dir/$BIN_NAME"
  if [ ! -f "$bin_src" ]; then
    # Tolerate a wrapping directory just in case the layout changes.
    bin_src="$(find "$extract_dir" -type f -name "$BIN_NAME" 2>/dev/null | head -n 1)"
  fi
  if [ -z "$bin_src" ] || [ ! -f "$bin_src" ]; then
    red "release bundle layout unexpected: no '$BIN_NAME' binary found in archive"
    exit 1
  fi
  # Verify identity before touching INSTALL_DIR. A valid signature authenticates
  # bytes, not that an old signed bundle was not replayed under a newer URL.
  verify_binary_release_identity "$bin_src" "release bundle" "$MANIFEST_GIT_SHA"
  mkdir -p "$INSTALL_DIR"
  install -m 0755 "$bin_src" "$INSTALL_DIR/$BIN_NAME"
  printf '  %s -> %s\n' "$BIN_NAME" "$INSTALL_DIR/$BIN_NAME"
}

ensure_path() {
  blue "[5/5] checking PATH"
  case ":$PATH:" in
    *":$INSTALL_DIR:"*) green "$INSTALL_DIR is already in PATH"; return ;;
  esac

  if [ "${VCFRAME_NO_PROFILE_UPDATE:-}" = "1" ]; then
    yellow "$INSTALL_DIR is not in PATH; profile update skipped"
    return
  fi

  profile="$HOME/.zshrc"
  if [ -w "$profile" ] && ! grep -q "vc-frame installer" "$profile" 2>/dev/null; then
    {
      printf '\n# vc-frame installer\n'
      printf 'export PATH="%s:$PATH"\n' "$INSTALL_DIR"
    } >>"$profile"
    yellow "added $INSTALL_DIR to $profile; reload your shell or run: source $profile"
  else
    yellow "$INSTALL_DIR is not in PATH"
  fi
}

smoke_fail() {
  red "post-install check failed: $1"
  shift
  [ "$#" -eq 0 ] || printf '%s\n' "$@"
  exit 1
}

post_install_check() {
  blue "post-install check"
  bin="$INSTALL_DIR/$BIN_NAME"

  if [ ! -x "$bin" ]; then
    smoke_fail "$bin is missing or not executable"
  fi

  # 1-2. Recheck exact version + embedded provenance after the copy.
  verify_binary_release_identity "$bin" "installed binary" "$MANIFEST_GIT_SHA"

  # 3. Config/setup subsystem resolves.
  if ! setup_output="$("$bin" setup --check 2>&1)"; then
    smoke_fail "$bin setup --check exited non-zero" "$setup_output"
  fi
  green "$BIN_NAME setup --check: ok"

  # 4. One real session command. On a fresh machine there are no sessions yet,
  #    and `list-sessions` exits 1 for that — which still proves the session
  #    discovery path ran. Anything else is a genuine failure.
  session_output="$("$bin" list-sessions 2>&1)" && session_status=0 || session_status=$?
  if [ "$session_status" -ne 0 ]; then
    case "$session_output" in
      *"No active vc-frame sessions found"*) ;;
      *) smoke_fail "$bin list-sessions failed" "$session_output" ;;
    esac
  fi
  green "$BIN_NAME list-sessions: ok"

  # 5. vc-frame replaces zellij outright; this installer never creates an alias.
  #    A pre-existing foreign `zellij` in the prefix is the user's own file, so
  #    warn rather than fail on something we do not own.
  if [ -e "$INSTALL_DIR/zellij" ]; then
    yellow "note: $INSTALL_DIR/zellij exists and was NOT created by this installer"
    yellow "      vc-frame does not provide or manage a 'zellij' alias"
  fi
}

main() {
  # Reject configuration typos before resolving a platform or touching network.
  validate_configuration

  printf '\n'
  blue "vc-frame installer"
  printf 'version: %s\ninstall: %s\nsource:  %s\n\n' "$VERSION" "$INSTALL_DIR" "$BASE_URL"

  target="$(target_triple)"
  if [ -z "$target" ]; then
    red "no prebuilt vc-frame bundle for this platform: $(uname -s)/$(uname -m)"
    red "supported: macOS (arm64/x86_64), Linux (x86_64/aarch64, musl)."
    exit 1
  fi

  install_prebuilt "$target"
  ensure_path
  post_install_check

  printf '\n'
  green "Installation complete"
  printf 'installed: vc-frame %s\n' "$VERSION"
  printf 'try:\n'
  printf '  %s/vc-frame --version\n' "$INSTALL_DIR"
  printf '\n'
}

main "$@"
