# Artifact ledger — bundled plugins & distribution residue

Owner wave: **W0-C** (Make bundled artifacts truthful).  
Canonical producer: `make plugins-assets` → `cargo xtask build --release --plugins-only`  
Parity validator: `make plugins-parity` → `scripts/plugins-parity.sh` (universal bash/zsh)  
Hash receipt: `zellij-utils/assets/plugins/SHA256SUMS`

## Tracked plugin source → artifact ownership

| Source crate | Bundled artifact | Runtime embedded (ASSET_MAP) |
| --- | --- | --- |
| `default-plugins/about` | `zellij-utils/assets/plugins/about.wasm` | yes |
| `default-plugins/compact-bar` | `…/compact-bar.wasm` | yes |
| `default-plugins/configuration` | `…/configuration.wasm` | yes |
| `default-plugins/fixture-plugin-for-tests` | `…/fixture-plugin-for-tests.wasm` | no (tests only) |
| `default-plugins/layout-manager` | `…/layout-manager.wasm` | yes |
| `default-plugins/link` | `…/link.wasm` | yes |
| `default-plugins/multiple-select` | `…/multiple-select.wasm` | yes |
| `default-plugins/plugin-manager` | `…/plugin-manager.wasm` | yes |
| `default-plugins/session-manager` | `…/session-manager.wasm` | yes |
| `default-plugins/share` | `…/share.wasm` | yes |
| `default-plugins/status-bar` | `…/status-bar.wasm` | yes |
| `default-plugins/strider` | `…/strider.wasm` | yes |
| `default-plugins/tab-bar` | `…/tab-bar.wasm` | yes |

Copy path is owned by `xtask/src/build.rs` (`move_plugin_to_assets`, release-only).  
Runtime identity is owned by `zellij-utils/src/consts.rs` (`ASSET_MAP` + `include_bytes!`).

## Residue decisions (W0-C)

| Path | State at audit | Consumers (packaging / code) | Decision | Evidence |
| --- | --- | --- | --- | --- |
| `assets/zellij.desktop` | untracked legacy twin | **None.** Dist copies `assets/vc-frame.desktop` (`xtask/src/pipelines.rs`). | **REMOVE** | Diff vs `vc-frame.desktop` is name/Exec/Icon only (`zellij` brand). No code path references `zellij.desktop`. Fixture snapshots only list the historical filename as terminal text. |
| `assets/zellij.rc` | untracked twin of `vc-frame.rc` | **None.** `src/build.rs` embeds `assets/vc-frame.rc`. | **REMOVE** | Byte-identical icon resource; unused path. |
| `wix/Zellij.wxl` | untracked legacy localization | **None.** `wix/main.wxs` builds with `wix/VcFrame.wxl`. | **REMOVE** | Diff is "Zellij" → "Vc-Frame" string localization only. |
| `build/` (`build/Vibecrafted.app/…`) | untracked packager output | Produced by `scripts/package-vibecrafted-app.zsh`; not a source surface. | **REMOVE + gitignore `/build/`** | Local assembly dir; must never be committed. |

## Keep (canonical distribution sources)

| Path | Owner |
| --- | --- |
| `assets/vc-frame.desktop` | `xtask` dist pipeline |
| `assets/vc-frame.rc` | Windows resource embed (`src/build.rs`) |
| `wix/VcFrame.wxl` + `wix/main.wxs` | Windows MSI localization / installer |
| `zellij-utils/assets/plugins/*.wasm` | Release-bundled plugins (tracked this cycle) |
| `zellij-utils/assets/plugins/SHA256SUMS` | Parity receipt for double-rebuild truth |

## Gates

```bash
make plugins-parity            # assets == SHA256SUMS
make plugins-parity-self-test  # perturb fails, restore passes
make plugins-parity-double     # two isolated rebuilds, identical hashes
cargo test -p zellij-utils asset_map_matches_bundled_plugin_files -- --nocapture
```

Out of scope for this ledger: untracking all WASM, upstream feature sync, release publication.
