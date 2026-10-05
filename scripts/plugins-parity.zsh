#!/usr/bin/env zsh
# Build-derived plugin truth for vc-frame.
#
# Plugin WASM is never a source-controlled input. zellij-utils/build.rs builds
# this fleet into a lock-isolated Cargo target and embeds exactly those bytes.
set -euo pipefail

export PATH="/usr/bin:/bin:/usr/sbin:/sbin:${HOME}/.cargo/bin:${PATH:-}"

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CARGO="${CARGO:-cargo}"

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

plugin_target_root() {
  local target_root="${CARGO_TARGET_DIR:-$REPO/target}"
  [[ "$target_root" == /* ]] || target_root="$REPO/$target_root"
  print -r -- "$target_root/vc-frame-plugins/wasm32-wasip1/release"
}

sha_file() {
  /usr/bin/shasum -a 256 "$1" | /usr/bin/awk '{print $1}'
}

build_release() {
  (cd "$REPO" && "$CARGO" xtask build --release --plugins-only)
}

hash_dir_receipt() {
  local dir="$1"
  local name artifact
  for name in "${PLUGIN_WASMS[@]}"; do
    artifact="$dir/$name"
    if [[ ! -f "$artifact" ]]; then
      print -u2 "ERROR: missing plugin artifact: $artifact"
      return 1
    fi
    print "$(sha_file "$artifact")  $name"
  done | /usr/bin/sort -k2
}

check_runtime_partition() {
  RUNTIME_WASM_LIST="$(printf '%s\n' "${RUNTIME_WASMS[@]}")" \
    python3 - "$REPO/zellij-utils/src/consts.rs" <<'PY'
import os
import pathlib
import re
import sys

source = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
match = re.search(
    r"pub static ref ASSET_MAP:.*?=\s*\{(?P<body>.*?)^\s+assets\s*$",
    source,
    flags=re.MULTILINE | re.DOTALL,
)
if match is None:
    raise SystemExit("ERROR: could not resolve ASSET_MAP plugin inventory")
asset_map = set(
    re.findall(r'add_plugin!\(\s*assets\s*,\s*"([^"]+)"\s*\)', match["body"])
)
declared = set(os.environ["RUNTIME_WASM_LIST"].splitlines())
if asset_map != declared:
    raise SystemExit(
        "ERROR: RUNTIME_WASMS != ASSET_MAP: "
        f"missing={sorted(asset_map - declared)}, extra={sorted(declared - asset_map)}"
    )
PY
}

check_contract() {
  check_runtime_partition
  build_release
  local receipt
  receipt="$(hash_dir_receipt "$(plugin_target_root)")"
  [[ "$(print -r -- "$receipt" | /usr/bin/wc -l | tr -d ' ')" == 14 ]]
  print "✓ current source built 14 derived plugins (13 runtime, 1 test-only)"
  (cd "$REPO" && "$CARGO" test -p zellij-utils \
    asset_map_matches_current_source_plugin_build -- --nocapture)
}

self_test() {
  check_runtime_partition
  local tmp="$(mktemp -d "${TMPDIR:-/tmp}/vc-frame-plugin-negative.XXXXXX")"
  trap "rm -rf '$tmp'" EXIT
  if hash_dir_receipt "$tmp" >/dev/null 2>&1; then
    print -u2 "ERROR: missing-artifact negative unexpectedly passed"
    return 1
  fi
  print "✓ missing-artifact negative failed closed"
  check_contract
}

receipt_json() {
  build_release
  local dir="$(plugin_target_root)"
  RUNTIME_WASM_LIST="$(printf '%s\n' "${RUNTIME_WASMS[@]}")" \
    python3 - "$dir" "${PLUGIN_WASMS[@]}" <<'PY'
import hashlib
import json
import os
import pathlib
import sys

directory = pathlib.Path(sys.argv[1])
runtime = set(os.environ["RUNTIME_WASM_LIST"].splitlines())
artifacts = []
for name in sys.argv[2:]:
    contents = (directory / name).read_bytes()
    artifacts.append({
        "name": name,
        "sha256": hashlib.sha256(contents).hexdigest(),
        "bytes": len(contents),
        "runtime_embedded": name in runtime,
        "test_only": name not in runtime,
    })
json.dump({
    "source": "target/vc-frame-plugins/wasm32-wasip1/release",
    "count": len(artifacts),
    "runtime_embedded_count": sum(item["runtime_embedded"] for item in artifacts),
    "test_only_count": sum(item["test_only"] for item in artifacts),
    "artifacts": artifacts,
}, sys.stdout, indent=2, sort_keys=True)
sys.stdout.write("\n")
PY
}

double_rebuild() {
  local tmp="$(mktemp -d "${TMPDIR:-/tmp}/vc-frame-plugin-double.XXXXXX")"
  trap "rm -rf '$tmp'" EXIT
  local receipts=()
  local round
  for round in 1 2; do
    print "══ isolated rebuild #$round ══"
    (
      export CARGO_TARGET_DIR="$tmp/target-$round"
      cd "$REPO"
      "$CARGO" xtask build --release --plugins-only
    )
    receipts+=("$tmp/receipt-$round")
    CARGO_TARGET_DIR="$tmp/target-$round" \
      hash_dir_receipt "$tmp/target-$round/vc-frame-plugins/wasm32-wasip1/release" \
      >"${receipts[-1]}"
  done
  diff -u "${receipts[1]}" "${receipts[2]}"
  print "✓ two isolated source builds produced identical plugin receipts"
}

clean_clone() {
  local tmp="$(mktemp -d "${TMPDIR:-/tmp}/vc-frame-plugin-clean-clone.XXXXXX")"
  trap "rm -rf '$tmp'" EXIT
  git clone --local --no-hardlinks "$REPO" "$tmp/vc-frame"
  (
    cd "$tmp/vc-frame"
    "$CARGO" xtask build
    "$CARGO" build --release
    "$CARGO" test -p zellij-utils asset_map_matches_current_source_plugin_build -- --nocapture
    [[ -z "$(git status --porcelain)" ]] || {
      git status --short
      print -u2 "ERROR: clean-clone builds dirtied the source tree"
      return 1
    }
  )
  print "✓ debug + release builds kept a clean clone clean"
}

usage() {
  print "Usage: $0 <check|receipt-json|rebuild-once|double-rebuild|self-test|clean-clone>"
}

case "${1:-check}" in
  check) check_contract ;;
  receipt-json) receipt_json ;;
  rebuild-once) build_release ;;
  double-rebuild) double_rebuild ;;
  self-test) self_test ;;
  clean-clone) clean_clone ;;
  -h|--help|help) usage ;;
  *) usage; exit 2 ;;
esac
