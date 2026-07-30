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
the redesigned `compact-bar`: brand chip, inverted mode chip, session anchor,
fisheye tab ribbons, fleet-pulse chip, the Agents station chip, and the
Command Composer chip (`Cmd+E`). Its `left_inset` option clears the macOS
traffic lights in a decoration-free host window.

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
