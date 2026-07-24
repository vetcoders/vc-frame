#!/usr/bin/env bash
# plugins-parity — deterministic bundled-plugin truth for vc-frame
#
# Universal shell: runs identically under bash (3.2+) and zsh. The Makefile
# invokes it through whichever of the two the machine has (SCRIPT_SHELL);
# the shebang is only the direct-execution default.
#
# Canonical asset producer:
#   cargo xtask build --release --plugins-only
#   (Makefile: make plugins-assets)
#
# Modes:
#   check            Verify assets match SHA256SUMS (CI-fast; default)
#   write-manifest   Hash current assets into SHA256SUMS
#   rebuild-once     Release-build plugins into assets/
#   double-rebuild   Two isolated rebuilds; hashes must match exactly
#   self-test        Deliberate perturbation fails check; restore passes
#
# 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
set -euo pipefail

# Ensure standard userland tools are visible even when the invoking
# environment has a minimal PATH (agent/runtime sandboxes).
export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin:${PATH:-}"

say() { printf '%s\n' "$*"; }
err() { printf '%s\n' "$*" >&2; }

REPO="$(cd "$(dirname "$0")/.." && pwd)"
ASSETS="$REPO/zellij-utils/assets/plugins"
MANIFEST="$ASSETS/SHA256SUMS"
TARGET_WASM="$REPO/target/wasm32-wasip1/release"
CARGO="${CARGO:-cargo}"

# Hash tool: shasum (macOS default) or sha256sum (Linux coreutils) — whichever
# this machine has. Same output format either way.
sha_file() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    err "ERROR: neither shasum nor sha256sum found on PATH"
    err "  Install one: macOS ships shasum; Linux: coreutils provides sha256sum"
    return 1
  fi
}

# Plugin crates whose release .wasm is copied into assets/ (xtask workspace list).
# fixture-plugin-for-tests is built and tracked but not embedded in ASSET_MAP.
PLUGIN_WASMS=(
  about.wasm
  compact-bar.wasm
  configuration.wasm
  fixture-plugin-for-tests.wasm
  layout-manager.wasm
  link.wasm
  multiple-select.wasm
  plugin-manager.wasm
  session-manager.wasm
  share.wasm
  status-bar.wasm
  strider.wasm
  tab-bar.wasm
  vc-tab-title.wasm
)

# Runtime-embedded plugins (zellij-utils ASSET_MAP) — excludes fixture-only.
RUNTIME_WASMS=(
  about.wasm
  compact-bar.wasm
  configuration.wasm
  layout-manager.wasm
  link.wasm
  multiple-select.wasm
  plugin-manager.wasm
  session-manager.wasm
  share.wasm
  status-bar.wasm
  strider.wasm
  tab-bar.wasm
  vc-tab-title.wasm
)

hash_dir_receipt() {
  local dir="$1"
  local name
  local artifact
  for name in "${PLUGIN_WASMS[@]}"; do
    artifact="$dir/$name"
    if [[ ! -f "$artifact" ]]; then
      err "ERROR: missing plugin artifact: $artifact"
      return 1
    fi
    say "$(sha_file "$artifact")  $name"
  done | sort -k2
}

write_manifest_from_assets() {
  hash_dir_receipt "$ASSETS" >"$MANIFEST"
  say "wrote $MANIFEST ($(wc -l <"$MANIFEST" | tr -d ' ') entries)"
}

check_manifest() {
  if [[ ! -f "$MANIFEST" ]]; then
    err "ERROR: missing $MANIFEST — run: $0 write-manifest"
    return 1
  fi
  local expected actual
  expected="$(sort -k2 "$MANIFEST")"
  actual="$(hash_dir_receipt "$ASSETS")"
  if [[ "$expected" != "$actual" ]]; then
    err "ERROR: plugin artifact hash mismatch vs SHA256SUMS"
    err "--- expected (manifest) ---"
    err "$expected"
    err "--- actual (assets) ---"
    err "$actual"
    return 1
  fi
  say "✓ assets match SHA256SUMS ($(printf '%s\n' "$actual" | wc -l | tr -d ' ') plugins)"
}

rebuild_once() {
  cd "$REPO"
  say "→ cargo xtask build --release --plugins-only"
  "$CARGO" xtask build --release --plugins-only
  # Confirm every expected artifact landed.
  local name
  for name in "${PLUGIN_WASMS[@]}"; do
    [[ -f "$ASSETS/$name" ]] || {
      err "ERROR: rebuild did not produce $ASSETS/$name"
      return 1
    }
  done
}

isolate_wasm_target() {
  # Drop only the plugin product binaries so the next build re-links them.
  # Full clean of wasm32-wasip1 is optional via PLUGINS_PARITY_FULL_CLEAN=1.
  if [[ "${PLUGINS_PARITY_FULL_CLEAN:-0}" == "1" ]]; then
    say "→ full clean: cargo clean --target wasm32-wasip1"
    (cd "$REPO" && "$CARGO" clean --target wasm32-wasip1)
    return 0
  fi
  mkdir -p "$TARGET_WASM"
  local name
  for name in "${PLUGIN_WASMS[@]}"; do
    rm -f "$TARGET_WASM/$name" "$TARGET_WASM/${name%.wasm}.d"
  done
  # Force recompile of plugin crates by touching a shared dependency stamp.
  # Removing .wasm alone is enough for move_plugin_to_assets; cargo still
  # rebuilds if sources changed. For isolated double-build we also delete
  # the crate output fingerprints for plugin packages when present.
  # rm -rf with an unmatched glob is harmless in both bash and zsh here:
  # the fingerprint dir exists check gates the loop, and -f tolerates
  # literal non-matches on shells that pass the pattern through.
  if [[ -d "$REPO/target/wasm32-wasip1/release/.fingerprint" ]]; then
    local p
    for p in about compact-bar configuration fixture-plugin-for-tests \
      layout-manager link multiple-select plugin-manager session-manager \
      share status-bar strider tab-bar; do
      rm -rf "$REPO/target/wasm32-wasip1/release/.fingerprint/${p}-"* 2>/dev/null || true
      rm -rf "$REPO/target/wasm32-wasip1/release/deps/${p}-"* 2>/dev/null || true
    done
  fi
}

double_rebuild() {
  local tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/vc-frame-plugins-parity.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT

  say "══ rebuild #1 (isolated) ══"
  isolate_wasm_target
  rebuild_once
  hash_dir_receipt "$ASSETS" >"$tmp/build1.sha256"
  say "receipt #1:"
  cat "$tmp/build1.sha256"

  say "══ rebuild #2 (isolated) ══"
  isolate_wasm_target
  rebuild_once
  hash_dir_receipt "$ASSETS" >"$tmp/build2.sha256"
  say "receipt #2:"
  cat "$tmp/build2.sha256"

  if ! diff -u "$tmp/build1.sha256" "$tmp/build2.sha256"; then
    err "ERROR: consecutive rebuilds produced different hashes (nondeterministic)"
    return 1
  fi
  say "✓ two isolated rebuilds produced identical hashes"

  # Refresh committed manifest to the proven receipt.
  cp "$tmp/build2.sha256" "$MANIFEST"
  say "✓ updated SHA256SUMS from double-rebuild receipt"
}

self_test() {
  say "══ self-test: positive check ══"
  check_manifest

  local victim="$ASSETS/about.wasm"
  local backup
  backup="$(mktemp "${TMPDIR:-/tmp}/about.wasm.bak.XXXXXX")"
  # Preserve mode/ownership/xattrs so restore does not dirty git on content-identical files.
  cp -p "$victim" "$backup"

  say "══ self-test: deliberate perturbation (expect FAIL) ══"
  printf 'perturbed\n' >>"$victim"
  if check_manifest; then
    cp -p "$backup" "$victim"
    rm -f "$backup"
    err "ERROR: parity check did not fail after artifact perturbation"
    return 1
  fi
  say "✓ perturbation correctly failed parity"

  say "══ self-test: restore (expect PASS) ══"
  cp -p "$backup" "$victim"
  rm -f "$backup"
  check_manifest
  say "✓ restoration passed parity"
  say "✓ plugins-parity self-test complete"
}

usage() {
  cat <<EOF
Usage: $0 <check|write-manifest|rebuild-once|double-rebuild|self-test>

  check            Verify assets == SHA256SUMS (default)
  write-manifest   Write SHA256SUMS from current assets
  rebuild-once     cargo xtask build --release --plugins-only
  double-rebuild   Two isolated rebuilds; require identical hashes
  self-test        Positive + negative (perturb) + restore

Env:
  CARGO                      cargo binary (default: cargo)
  PLUGINS_PARITY_FULL_CLEAN  set to 1 for cargo clean --target wasm32-wasip1
EOF
}

mode="${1:-check}"
case "$mode" in
  check) check_manifest ;;
  write-manifest) write_manifest_from_assets ;;
  rebuild-once) rebuild_once ;;
  double-rebuild) double_rebuild ;;
  self-test) self_test ;;
  -h|--help|help) usage ;;
  *)
    usage
    exit 2
    ;;
esac
