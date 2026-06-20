#!/bin/sh
# vc-frame installer — canonical curl | sh entry point.
#
# Usage:
#   curl -fsSL https://vibecrafted.io/install.sh | sh
#
# Env overrides:
#   VCFRAME_VERSION        release version (default: 0.45.4)
#   INSTALL_DIR            where the `vc-frame` binary is placed (default: ~/.local/bin)
#   VCFRAME_BASE_URL       release base URL (default: https://vibecrafted.io/releases)
#                          The per-version artifacts live under $BASE_URL/$VERSION/.
#                          Point this at a GitHub release download dir to test:
#                          VCFRAME_BASE_URL=https://github.com/VetCoders/vc-frame/releases/download
#   VCFRAME_GPG_KEY_URL    release signing public key URL
#                          (default: $BASE_URL/$VERSION/vc-frame-signing.asc)
#   VCFRAME_GPG_FINGERPRINT  expected release key fingerprint. When set, the
#                          imported public key MUST match it. When empty, the
#                          published key is trusted on publish (signature still
#                          verified) — set this once the release key is known.
#   VCFRAME_REQUIRE_GPG=1  fail if the GPG key or .sig sidecar is unavailable
#                          (default: 1). Set 0 to allow install without a
#                          signature (still enforces the SHA256 sidecar).
#   VCFRAME_NO_PROFILE_UPDATE=1  do not edit ~/.zshrc when PATH is missing
#
# Design notes (mirrors the loctree 0.12 installer hardening):
#   - NO silent source fallback. vc-frame is a prebuilt binary, not a crate;
#     any download/verification failure fails loudly.
#   - GPG is the trust root under VCFRAME_REQUIRE_GPG=1; the SHA256 sidecar is
#     always enforced.
#   - The per-target archive name is resolved from manifest.json when present,
#     and falls back to the deterministic `vc-frame-<target>.tar.gz` name the
#     release workflow uploads.
#   - Linux targets resolve to the musl-static build (`-unknown-linux-musl`),
#     which is the maximally-portable choice for this standalone binary — the
#     name MUST match what .github/workflows/release.yml uploads.
#   - The release tarball contains a bare `vc-frame` binary at its root.
#   - Runs `vc-frame --version` as a post-install check (hard fail if it does
#     not run) — the same contract the Vibecrafted foundations gate enforces.
#   - Whole script runs through main() invoked on the last line, so a truncated
#     `curl | sh` transfer executes nothing.

set -eu
umask 022

VERSION="${VCFRAME_VERSION:-0.45.4}"
INSTALL_DIR="${INSTALL_DIR:-"$HOME/.local/bin"}"
BASE_URL="${VCFRAME_BASE_URL:-https://vibecrafted.io/releases}"
REQUIRE_GPG="${VCFRAME_REQUIRE_GPG:-1}"
GPG_FINGERPRINT="${VCFRAME_GPG_FINGERPRINT:-}"
BIN_NAME="vc-frame"

red() { printf '\033[0;31m%s\033[0m\n' "$*"; }
green() { printf '\033[0;32m%s\033[0m\n' "$*"; }
yellow() { printf '\033[0;33m%s\033[0m\n' "$*"; }
blue() { printf '\033[0;34m%s\033[0m\n' "$*"; }

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

# Extract the artifact file name for a target from manifest.json. POSIX sh, no
# jq: split JSON on commas/braces (URLs and file names contain neither), then
# pull the first quoted string that ends in "-<target>.tar.gz" and strip any
# URL path prefix.
manifest_artifact_name() {
  manifest_file="$1"
  mf_target="$2"
  raw="$(tr ',{}' '\n\n\n' < "$manifest_file" \
    | sed -n 's/.*"\([^"]*-'"$mf_target"'\.tar\.gz\)".*/\1/p' \
    | head -n 1)"
  [ -n "$raw" ] || return 1
  printf '%s\n' "${raw##*/}"
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
    actual_fingerprint="$(gpg --batch --with-colons --import-options show-only --import "$pub_file" 2>/dev/null | awk -F: '$1 == "fpr" {print $10; exit}')"
    actual_fingerprint="$(normalize_fingerprint "$actual_fingerprint")"
    if [ -z "$actual_fingerprint" ]; then
      red "GPG signing key fingerprint could not be read"
      exit 1
    fi
    if [ "$actual_fingerprint" != "$expected_fingerprint" ]; then
      red "GPG signing key fingerprint mismatch"
      printf 'expected: %s\nactual:   %s\n' "$expected_fingerprint" "$actual_fingerprint"
      exit 1
    fi
    green "GPG key fingerprint ok: $actual_fingerprint"
  else
    yellow "VCFRAME_GPG_FINGERPRINT not set; trusting the published key on publish"
  fi

  if ! curl -fsSL "$base_url/$(basename "$sig_file")" -o "$sig_file" 2>/dev/null; then
    skip_signature_verification "signature sidecar unavailable ($base_url/$(basename "$sig_file"))"
    return 0
  fi
  mkdir -p "$gnupg_home"
  chmod 700 "$gnupg_home"
  if GNUPGHOME="$gnupg_home" gpg --batch --quiet --import "$pub_file" >/dev/null 2>&1 \
    && GNUPGHOME="$gnupg_home" gpg --batch --quiet --verify "$sig_file" "$file" >/dev/null 2>&1; then
    green "GPG signature ok"
  else
    red "signature verification failed for $(basename "$file")"
    exit 1
  fi
}

install_prebuilt() {
  target="$1"
  artifact_base="$BASE_URL/$VERSION"
  key_url="${VCFRAME_GPG_KEY_URL:-$artifact_base/vc-frame-signing.asc}"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT

  blue "[1/5] resolving release artifact"
  need_cmd curl
  need_cmd tar

  archive=""
  manifest_file="$tmp/manifest.json"
  if curl -fsSL "$artifact_base/manifest.json" -o "$manifest_file" 2>/dev/null; then
    if archive="$(manifest_artifact_name "$manifest_file" "$target")"; then
      green "artifact (from manifest): $archive"
    else
      archive=""
      if grep -q '"artifacts"' "$manifest_file" 2>/dev/null; then
        red "release $VERSION does not provide a bundle for $target (per manifest.json)"
        exit 1
      fi
      yellow "manifest.json has no artifact entries; using deterministic name"
    fi
  else
    yellow "manifest.json unavailable for $VERSION; using deterministic name"
  fi
  if [ -z "$archive" ]; then
    archive="vc-frame-$target.tar.gz"
  fi
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

post_install_check() {
  blue "post-install check"
  if [ ! -x "$INSTALL_DIR/$BIN_NAME" ]; then
    red "post-install check failed: $INSTALL_DIR/$BIN_NAME is missing or not executable"
    exit 1
  fi
  if ! version_output="$("$INSTALL_DIR/$BIN_NAME" --version 2>&1)"; then
    red "post-install check failed: $INSTALL_DIR/$BIN_NAME --version exited non-zero"
    printf '%s\n' "$version_output"
    exit 1
  fi
  green "$BIN_NAME --version: $version_output"
}

main() {
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
