#!/usr/bin/env bash
# Sign a standalone Mach-O. Local builds may use ad-hoc signing; CI must not.
set -euo pipefail

die() { echo "sign-darwin: $*" >&2; exit 1; }
require_developer_id=false
if [[ "${1:-}" == --require-developer-id ]]; then
  require_developer_id=true
  shift
fi
[[ $# -eq 1 ]] || die "usage: $0 [--require-developer-id] BINARY"
[[ "$(uname -s)" == Darwin ]] || die "macOS is required"
binary="$1"
[[ -f "$binary" && -x "$binary" ]] || die "executable not found: $binary"
case "$(file -b "$binary")" in
  *Mach-O*) ;;
  *) die "expected a Mach-O executable" ;;
esac

identity="${VC_SIGNING_IDENTITY:-}"
if [[ -z "$identity" ]]; then
  security_args=(find-identity -v -p codesigning)
  if [[ -n "${VC_SIGNING_KEYCHAIN:-}" ]]; then
    security_args+=("$VC_SIGNING_KEYCHAIN")
  fi
  identities="$(security "${security_args[@]}")" \
    || die "could not inspect signing identities"
  developer_ids=()
  while IFS= read -r candidate; do
    [[ -z "$candidate" ]] || developer_ids+=("$candidate")
  done < <(printf '%s\n' "$identities" | awk '/"Developer ID Application:/ {print $2}')
  case "${#developer_ids[@]}" in
    0) identity=- ;;
    1) identity="${developer_ids[0]}" ;;
    *) die "multiple Developer ID identities; set VC_SIGNING_IDENTITY explicitly" ;;
  esac
fi
if [[ "$identity" == - ]]; then
  "$require_developer_id" && die "Developer ID is required; refusing ad-hoc signing"
  timestamp=(--timestamp=none)
  echo "sign-darwin: ad-hoc local signature (not distributable as notarized software)"
else
  timestamp=(--timestamp)
  echo "sign-darwin: Developer ID signature with secure timestamp"
fi
# The plugin engine is wasmi (an interpreter), so no JIT or library-validation
# exceptions are needed. Keep the hardened runtime's default protections.
sign_args=(--force --sign "$identity" --identifier io.vetcoders.vc-frame
  --options runtime "${timestamp[@]}")
if [[ -n "${VC_SIGNING_KEYCHAIN:-}" ]]; then
  sign_args+=(--keychain "$VC_SIGNING_KEYCHAIN")
fi
codesign "${sign_args[@]}" "$binary"
codesign --verify --strict --verbose=2 "$binary"
if [[ "$identity" != - ]]; then
  metadata="$(codesign --display --verbose=4 "$binary" 2>&1)"
  case "$metadata" in
    *"Authority=Developer ID Application:"*) ;;
    *) die "signature is not Developer ID Application" ;;
  esac
fi
