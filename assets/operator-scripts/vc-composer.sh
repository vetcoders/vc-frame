#!/usr/bin/env bash
# vc-composer.sh — Command Composer with Paste Stack integration (spec 1.2 §A)
#
# Contract (same door as compact-bar chip and Super+e / Cmd+E — Alt+e is free
# for Polish `ę` on macOS):
#   1. Draft in vim with: set number, laststatus=0 (clean -- INSERT -- only)
#   2. Ctrl+p inside vim opens the Paste Stack picker and inserts at cursor
#   3. On non-empty :wq/ZZ: push body to Paste Stack, hide floating panes,
#      write-chars into the underlying pane (unexecuted — Enter is human)
#   4. Clean up the temp draft
#
# Install/symlink to:
#   ~/.config/vetcoders/frontier/vc-frame/vc-composer.sh
#   or ~/.config/vc-frame/vc-composer.sh
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
resolve_tool() {
  local name="$1"
  local candidate
  for candidate in \
    "${SCRIPT_DIR}/${name}" \
    "${HOME}/.config/vetcoders/frontier/vc-frame/${name}" \
    "${HOME}/.config/vc-frame/${name}"
  do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

PASTE_STACK="$(resolve_tool paste-stack.sh || true)"

f="$(mktemp "${TMPDIR:-/tmp}/vc-composer.XXXXXX")" || exit 1
cleanup() { rm -f -- "$f"; }
trap cleanup EXIT

# Seed from Paste Stack top unless VC_COMPOSER_SEED=0.
seed="${VC_COMPOSER_SEED:-1}"
if [[ "$seed" != "0" && -n "$PASTE_STACK" ]]; then
  "$PASTE_STACK" top "$f" || true
fi

# Vim profile (spec 1.2 §A): line numbers on, path statusline off, clean mode.
# Ctrl+p → paste-stack pick → insert at cursor (requires paste-stack.sh pick).
vim_paste_cmd=""
if [[ -n "$PASTE_STACK" ]]; then
  # nnoremap: run pick into a temp file, read it under the cursor.
  vim_paste_cmd=$(cat <<EOF
nnoremap <silent> <C-p> :let __vc_ps=tempname() \| execute 'silent !${PASTE_STACK} pick > ' . shellescape(__vc_ps) \| if filereadable(__vc_ps) && getfsize(__vc_ps) > 0 \| execute 'read' __vc_ps \| endif \| call delete(__vc_ps)<CR>
EOF
)
fi

if [[ -n "${VC_COMPOSER:-}" ]]; then
  # Operator override is a full command line (e.g. pensieve --wait).
  # shellcheck disable=SC2086
  ${VC_COMPOSER} "$f"
else
  # Default Vibecrafted vim profile.
  # shellcheck disable=SC2086
  ${EDITOR:-vim} \
    -c 'set number' \
    -c 'set laststatus=0' \
    -c 'set noshowcmd' \
    -c 'set noruler' \
    ${vim_paste_cmd:+-c "$vim_paste_cmd"} \
    "$f"
fi

if [[ -s "$f" ]]; then
  if [[ -n "$PASTE_STACK" ]]; then
    "$PASTE_STACK" push "$f" || true
  fi
  # Hide the floating atelier so write-chars lands on the work pane beneath.
  vc-frame action toggle-floating-panes || true
  vc-frame action write-chars "$(cat -- "$f")"
fi
