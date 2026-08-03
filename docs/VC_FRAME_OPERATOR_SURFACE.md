# vc-frame Operator Surface

vc-frame is the Vetcoders terminal frame for Vibecrafted operator work. It is a
Zellij-core fork with a fork-owned default surface: grayscale chrome, sessions on
the left, and runs/tabs on top.

![vc-frame dual rail terminal schematic](assets/vc-frame-dual-rail-terminal.svg)

## Product Promise

Fresh `vc-frame` starts with no theme configured should make the operator state
obvious before any command is typed:

- sessions are listed in the left rail
- runs and tabs stay in the top bar
- ordinary frame chrome is grayscale by default
- color is reserved for state that needs attention
- Vibecrafted fleet liveness output remains machine-parseable

## Default Layout

The default layout uses the existing plugins rather than a new parallel session
system:

- `tab-bar` renders the top run/tab rail
- `session-manager` renders the left rail with `rail true`
- the main pane starts to the right of the 24-column rail
- `status-bar` stays at the bottom

The `session-rail` plugin alias in `zellij-utils/assets/config/default.kdl`
points at `zellij:session-manager` with `rail true`.

The operator entrypoint layout (`vibecrafted`) replaces the top `tab-bar` with
the redesigned `compact-bar`: brand chip, inverted mode chip, fisheye tab
ribbons, the Quick cmd chip, and the Command Composer chip (`Cmd+E`). Zones
follow the Fixed Character Grid Model — brand 14 cols, mode 8 cols, entry chips
12+18 — so mode switches never shift the tab zone. `left_inset` (default **6**
at standard monospace; raise to 9–12 for large fonts) clears the macOS traffic
lights in a decoration-free host window. Quick cmd floats a login shell over
the current tab; Composer drafts via `$VC_COMPOSER`/`$EDITOR`, seeds from and
pushes to the Paste Stack (`~/.cache/vc-frame/paste-stack.json`), then
`write-chars` into the pane beneath (Enter stays human). The bottom
`status-bar` owns pure status: the fleet `LIVE` count, host CPU/memory/disk
cockpit (fixed-width fields), health, and layout state.
The diodes live in the resting mode only (LOCK when the base mode is locked,
NORMAL otherwise) — action modes hand every column to the shortcut hints and
keep just the swap-layout chip as arrangement context. On a narrow bar the
segment degrades block by block (DISK, then MEM, then CPU, then the swap
chip, then HEALTH; the fleet pulse goes last) instead of vanishing whole,
and a two-cell seam always separates hints from statuses.

`LIVE` has a bounded background-cost contract. The server derives the count
once from the session snapshot it already owns and sends a small scalar message
only to the status-bar plugin/client pairs viewing active tabs. When a client
switches tabs, the server sends an exact plugin/client deactivation signal to
the status bar it left; sampling does not rely on the tab-global `Visible`
event, which cannot distinguish multiple clients in one session. Per-tab
status bars never subscribe to the full cross-session `SessionUpdate`, and
unrelated `CustomMessage` consumers are not awakened. Host resource sampling
also runs only in active status-bar instances, and clipboard timers cannot
create extra sampling cadences. These lifecycle messages describe refreshable
current state, so they bypass the shared pending-plugin event cache: an already
ready exact target receives them immediately even while another client instance
is loading. A not-yet-ready status bar starts idle and requests a fresh server
snapshot after it loads successfully. A failed or disconnected attach therefore
cannot leave a cached lifecycle signal blocking healthy status-bar instances.
The lifecycle recognizer covers the canonical `vc-frame:status-bar`, the legacy
`zellij:status-bar`, the default `status-bar` alias, and renamed aliases that
resolve to that built-in plugin. An unrelated `file:` or remote plugin is not
treated as the built-in status bar merely because its filename looks similar;
custom replacements must implement and wire their own sampling lifecycle.

## Key Contract

The shipped defaults promise one navigation language — one modifier per
owner (contract v3):

- `Cmd+←/→` — previous/next tab, **every mode including LOCK**
- `Cmd+↑/↓` — previous/next session, **every mode including LOCK**
- `Cmd+E` — Command Composer
- `Ctrl+arrows` — the same switcher outside LOCK; inside LOCK they pass
  through to the pane (the shell owns Ctrl there)
- `Alt` — belongs entirely to the writer: diacritics layer and host-side
  word-jump; vc-frame binds no Alt+arrow keys

If declared keys do nothing, the usual culprit is a frozen
`keybinds clear-defaults=true` dump in the user config shadowing the shipped
contract. `vc-frame doctor` diagnoses it (read-only, `--json`, exit 0/1/2) and
`vc-frame repair key-bindings` fixes it with a backup and an explicit report
of any personal binds lost. Full runbook: [DOCTOR.md](DOCTOR.md). Host-side
requirements (macOS `option_as_alt`, hints, glyph width) live in
[ALACRITTY_INTEGRATION.md](ALACRITTY_INTEGRATION.md).

## Try It From Source

```bash
make install
vc-frame setup --check
vc-frame setup --dump-layout default
vc-frame setup --dump-config
vc-frame
```

Public packages and executables named `zellij` belong to upstream Zellij, not
the Vetcoders vc-frame runtime.

## Release Channel

The release-grade path is documented in [RELEASE.md](RELEASE.md). A real public
release should provide prebuilt `vc-frame-*` artifacts, checksums, signatures,
`manifest.json`, and a served installer:

```bash
VCFRAME_GPG_FINGERPRINT=<pinned-fingerprint> \
  sh -c "$(curl -fsSL https://github.com/vetcoders/vc-frame/releases/latest/download/install.sh)"
vc-frame --version
```

Until those artifacts are published for a tag, this repository should be
described as source-build preview for outside users.

## Verified Runtime Evidence

The July 2026 lifecycle verified the default surface against the real debug
binary:

- `cargo check --workspace` passed
- `cargo test --workspace` passed in polarize/DoU
- `make clippy` passed as the repo-native lint gate
- `setup --dump-layout default` showed the left rail plus top `tab-bar`
- live PTY marbles runs rendered `SESSIONS 2` with `Tab #1`
- rail ordinal switching moved a client between live sessions
- locked mode kept the `Sessions` rail visible and unsuppressed
- `list-sessions --no-formatting` preserved `[Created ...]` rows and `(current)`

The raw strict command `cargo clippy --workspace --all-targets -- -D warnings`
still fails on inherited baseline lint debt outside the redesign. Do not claim
that gate is green until it is fixed or explicitly scoped by the operator.

## Upstream Boundary

vc-frame intentionally keeps Zellij compatibility for configuration, layouts,
plugins, and many docs concepts. Upstream documentation is useful for inherited
features, but vc-frame-specific claims live in this repository:

- default grayscale styling
- left session rail
- top run/tab rail
- Vibecrafted layout installation
- `VC_FRAME_*` env contracts
- release installer and artifact names
