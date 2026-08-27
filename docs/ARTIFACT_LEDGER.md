# Artifact ledger — build-derived bundled plugins

Plugin source under `default-plugins/` is the only source of truth. WASM files
and their SHA-256 receipt are derived outputs and never belong in Git.

## Ownership

| Surface | Owner | Contract |
| --- | --- | --- |
| Plugin sources | `default-plugins/*` | Reviewed and committed product input |
| Debug WASM | `target/vc-frame-plugins/wasm32-wasip1/debug/*.wasm` | Built automatically before every native debug embed |
| Release WASM | `target/vc-frame-plugins/wasm32-wasip1/release/*.wasm` | Built automatically before every native release embed |
| Build receipt | `zellij-utils` build-script `OUT_DIR/plugin-SHA256SUMS` | Generated from the exact target bytes embedded by `ASSET_MAP` |
| Runtime bytes | `zellij-utils/src/consts.rs::ASSET_MAP` | `include_bytes!` reads only the build-script-selected target directory |

`zellij-utils/build.rs` is the one writer. It runs the complete plugin build in
a lock-isolated Cargo target, emits the profile-specific path, generates the
receipt, and fails with `rustup target add wasm32-wasip1` when the target is
missing. `xtask` uses the same derived target and no longer copies anything
into the source tree.

The runtime fleet contains 13 plugins. `fixture-plugin-for-tests.wasm` is the
fourteenth built artifact and is receipt-only, never embedded in `ASSET_MAP`.

## Distribution

Vibecrafted Runtime Pack consumes the compiled `vc-frame` helper only. It does
not copy `zellij-utils/assets/plugins/`; therefore removing tracked blobs does
not remove plugins from the DMG or Runtime Pack. They remain inside the binary.

The Debian metadata likewise no longer installs a second loose-plugin fleet.
Automatic asset installation can still dump the embedded `ASSET_MAP` bytes at
runtime, preserving one binary-owned generation.

## Gates

```bash
make plugins-parity
make plugins-parity-self-test
make plugins-parity-double
scripts/plugins-parity.zsh receipt-json
scripts/plugins-parity.zsh clean-clone
cargo test -p zellij-utils asset_map_matches_current_source_plugin_build -- --nocapture
```

`clean-clone` builds debug and release in a local clone, runs the byte-match
test, and requires `git status --porcelain` to remain empty.
