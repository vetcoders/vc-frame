# Config ownership — single source of truth

**Release rule (2026-08):** one owner per layer. Dual-written configs are a
product bug and a flicker class.

## Ownership matrix

| Layer | Owner | Canonical path |
| ----- | ----- | -------------- |
| Runtime binary, KDL schema, built-in layouts/themes, doctor/repair | **vc-frame** | this repo (`zellij-utils/assets/`, `default-plugins/`) |
| Product packaging, install wire, operator presets (keys, themes, layouts for Vetcoders) | **vibecrafted** | `vibecrafted/config/vc-frame/` |
| Active user view on disk | **vibecrafted install** (wires) | `~/.config/vc-frame` → frontier tools store |
| Frontier live copy | **vibecrafted install** | `~/.config/vetcoders/frontier/vc-frame/` |

## What this means in practice

1. **Do not hand-edit** both a checkout `config/vc-frame` and a live
   `~/.config/vc-frame` tree and expect them to stay friends. Drift paints as
   “random Main sessions”, missing keys, and chrome that migoce.
2. **vc-frame** owns *what the binary understands* (schema, defaults in
   assets, `vc-frame doctor` / `repair`).
3. **vibecrafted** owns *how the product is installed and which preset is
   live* (`vibecrafted install` / doctor `vc-frame:truth`).
4. Operator scripts for Composer / paste-stack / quick-cmd ship from
   **vc-frame** `assets/operator-scripts/` and are installed next to the
   preset by vibecrafted install (or copied to frontier on release).

## Historical note

Pre-0.47 dual ownership (YAML converter + ad-hoc user dumps + package
overlays) was the root of config shadowing. The YAML converter was dropped
(`one config format, one owner`). This document closes the remaining
product-layer ambiguity between the two repos.

## Release checklist

- [ ] `vc-frame doctor` green against the wired view (next-session config)
- [ ] vibecrafted doctor `vc-frame:truth` — checkout ↔ store ↔ frontier agree
- [ ] No second `config.kdl` authored in-tree under both repos for the same
      concern without an explicit install projection
- [ ] PR to `develop` includes asset wasm + SHA256SUMS when chrome plugins change
