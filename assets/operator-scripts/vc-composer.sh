#!/usr/bin/env bash
# vc-composer.sh — Command Composer with Paste Stack integration (spec 1.2 §A)
#
# Contract (same door as compact-bar chip and Super+e / Cmd+E — Alt+e is free
# for Polish `ę` on macOS):
#   1. Draft in vim with: number, laststatus=0, nowrap (clean -- INSERT --)
#   2. Ctrl+p opens the Paste Stack picker and inserts at cursor
#   3. On non-empty :wq/ZZ: push body to Paste Stack, hide floating panes,
#      write-chars into the underlying pane (unexecuted — Enter is human)
#   4. Clean up the temp draft
#
# IMPORTANT: all settings go through ONE -u vimrc file. Classic vim hard-caps
# the number of -c / +cmd arguments (~10) and dies with:
#   Too many "+command", "-c command" or "--cmd command" arguments
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
vimrc="$(mktemp "${TMPDIR:-/tmp}/vc-composer-vimrc.XXXXXX")" || exit 1
cleanup() { rm -f -- "$f" "$vimrc"; }
trap cleanup EXIT

# Seed from Paste Stack top unless VC_COMPOSER_SEED=0.
seed="${VC_COMPOSER_SEED:-1}"
if [[ "$seed" != "0" && -n "$PASTE_STACK" ]]; then
  "$PASTE_STACK" top "$f" || true
fi

wrap_line='set nowrap'
if [[ "${VC_COMPOSER_WRAP:-0}" == "1" ]]; then
  wrap_line='set wrap'
fi

# Single sourced profile — never stack a dozen -c flags (vim 9.x hard limit).
{
  cat <<'VIMRC_HEAD'
set nocompatible
set number
set laststatus=0
set noshowcmd
set noruler
set textwidth=0
set nolinebreak
set sidescroll=1
set sidescrolloff=2
nnoremap <silent> <F2> :set wrap! wrap?<CR>
nnoremap <silent> <Leader>w :set wrap! wrap?<CR>
VIMRC_HEAD
  printf '%s\n' "$wrap_line"
  if [[ -n "$PASTE_STACK" ]]; then
    # Escape single quotes for a vim string literal.
    local_ps="${PASTE_STACK//\'/\'\'}"
    cat <<EOF
nnoremap <silent> <C-p> :let __vc_ps=tempname() \\| execute 'silent !${local_ps} pick > ' . shellescape(__vc_ps) \\| if filereadable(__vc_ps) && getfsize(__vc_ps) > 0 \\| execute 'read' __vc_ps \\| endif \\| call delete(__vc_ps)<CR>
EOF
  fi
} >"$vimrc"

if [[ -n "${VC_COMPOSER:-}" ]]; then
  # Operator override is a full command line (e.g. pensieve --wait).
  # shellcheck disable=SC2086
  ${VC_COMPOSER} "$f"
else
  editor="${EDITOR:-vim}"
  # -u: only our profile. -N: nocompatible when -u is used. No extra -c.
  if [[ "$(basename -- "$editor")" == nvim || "$editor" == *nvim* ]]; then
    "$editor" -u "$vimrc" "$f"
  else
    "$editor" -N -u "$vimrc" "$f"
  fi
fi

if [[ -s "$f" ]]; then
  if [[ -n "$PASTE_STACK" ]]; then
    "$PASTE_STACK" push "$f" || true
  fi
  vc-frame action toggle-floating-panes || true
  vc-frame action write-chars "$(cat -- "$f")"
fi
