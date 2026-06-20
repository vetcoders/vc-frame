# How to release vc-frame

`vc-frame` is a standalone Rust binary (a Vibecrafted-tuned zellij fork). A
release ships prebuilt, checksummed, GPG-signed binaries plus a `manifest.json`,
and is installable from a single `curl … | sh`. This document is the runbook for
cutting one.

There is **no undo button** for a real release: it is triggered by pushing a
`vX.Y.Z` tag, which fires `.github/workflows/release.yml`. Read the whole
runbook, do the dry-run, and only then cut the tag.

---

## What a release publishes

For each supported target, `release.yml` uploads to the GitHub release:

| Asset | Purpose |
|---|---|
| `vc-frame-<target>.tar.gz` (`.zip` on Windows) | the binary archive (bare `vc-frame` at the archive root) |
| `vc-frame-<target>.tar.gz.sha256` | SHA256 of the **archive** (verify-before-extract) |
| `vc-frame-<target>.tar.gz.sig` | detached GPG signature of the archive (Unix targets) |
| `vc-frame-<target>-installer.msi` + `.sha256` | Windows MSI (built from `wix/main.wxs` + `wix/VcFrame.wxl`) |

Once per release, a `publish-manifest` job also uploads:

| Asset | Purpose |
|---|---|
| `manifest.json` | source of truth the installer reads to resolve the per-target archive name |
| `vc-frame-signing.asc` | the **public** half of the release key (GPG trust root for the installer) |

A `-no-web` variant of each archive is built in parallel for environments
without the web/control-plane assets. The canonical installer resolves the full
(web) build; `-no-web` is a secondary channel.

### Supported targets

The build matrix (`release.yml`) covers:

- `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` — **musl-static**.
  vc-frame is standalone (no native embedder), so musl is the maximally-portable
  choice. The installer's `target_triple()` resolves Linux to `-musl`; these
  names **must** stay in lockstep with what `release.yml` uploads.
- `x86_64-apple-darwin`, `aarch64-apple-darwin`
- `x86_64-pc-windows-msvc`

---

## Signing (operator-provided key material)

GPG is the release trust root. The **private** key never lives in this repo or
in any agent-authored file — only the signing *step* and the public-key publish
are wired in `release.yml`. The key is provided at release time via repository
secrets:

- `GPG_PRIVATE_KEY` — ASCII-armored private key, imported on each build runner.
- `GPG_PASSPHRASE` — passphrase for that key (loopback pinentry).
- `GPG_PUBLIC_KEY` — ASCII-armored public key, published as `vc-frame-signing.asc`.

The operator's keys live in the `~/.keys` vault. When the secrets are absent
(e.g. a `workflow_dispatch` dry-run), the build still succeeds but emits
**unsigned** artifacts and a loud `::warning::`. A real `vX.Y.Z` release must run
with the secrets present.

Once the release key is minted, record its fingerprint and set it as the
installer default (`VCFRAME_GPG_FINGERPRINT`) so foreign installs pin the key
instead of trusting-on-publish.

---

## The canonical installer

`tools/install.sh` is the script `https://vibecrafted.io/install.sh` serves
(operator turf — see *Operator buttons* below). It:

1. resolves `target_triple()` (Linux → `-musl`),
2. fetches `manifest.json` → the per-target archive name,
3. downloads the archive + `.sha256` + `.sig`,
4. verifies the SHA256 (always) and the GPG signature (trust root under
   `VCFRAME_REQUIRE_GPG=1`, the default),
5. extracts the bare `vc-frame` binary into `INSTALL_DIR` (default `~/.local/bin`),
6. ensures `INSTALL_DIR` is on `PATH`,
7. hard-checks `vc-frame --version` — the exact contract the Vibecrafted
   foundations gate enforces (`binary_runs vc-frame`).

Env overrides (full list in the script header): `VCFRAME_VERSION`,
`VCFRAME_BASE_URL`, `VCFRAME_GPG_KEY_URL`, `VCFRAME_GPG_FINGERPRINT`,
`VCFRAME_REQUIRE_GPG`, `INSTALL_DIR`, `VCFRAME_NO_PROFILE_UPDATE`.

---

## Dry-run before cutting a tag

A real release is irreversible. Validate first.

### 1. Build artifacts without releasing

Trigger the workflow via **`workflow_dispatch`** (Actions → Release → Run
workflow). This produces a *draft* release with all artifacts under the `main`
tag name and never publishes a `vX.Y.Z`. Inspect the asset names — every asset
must be `vc-frame-`prefixed and each archive's checksum/signature name must match
its archive.

### 2. Smoke-test the installer locally

Point the installer at a local `file://` test release built from any `vc-frame`
binary (the layout exactly mirrors what `release.yml` uploads):

```sh
REL=/tmp/vcframe-test/0.45.4; mkdir -p "$REL" /tmp/stage
cp "$(command -v vc-frame)" /tmp/stage/vc-frame
target="$(uname -m)-apple-darwin"            # or *-unknown-linux-musl on Linux
tar czf "$REL/vc-frame-$target.tar.gz" -C /tmp/stage vc-frame
( cd "$REL" && shasum -a 256 "vc-frame-$target.tar.gz" > "vc-frame-$target.tar.gz.sha256" )
printf '{ "artifacts": { "%s": "vc-frame-%s.tar.gz" } }\n' "$target" "$target" > "$REL/manifest.json"

VCFRAME_VERSION=0.45.4 VCFRAME_BASE_URL="file:///tmp/vcframe-test" \
VCFRAME_REQUIRE_GPG=0 INSTALL_DIR=/tmp/vcframe-bin VCFRAME_NO_PROFILE_UPDATE=1 \
sh tools/install.sh
/tmp/vcframe-bin/vc-frame --version    # must print: vc-frame X.Y.Z
```

When a signed release exists, drop `VCFRAME_REQUIRE_GPG=0` to exercise the full
GPG path against the published `vc-frame-signing.asc`.

---

## Cutting the real release

> ⛔ **Operator button.** The steps below trigger an irreversible release and
> must be performed by the operator, not an agent.

1. Bump `version` in the workspace `[workspace.package]` of the root `Cargo.toml`.
2. Confirm `GPG_PRIVATE_KEY` / `GPG_PASSPHRASE` / `GPG_PUBLIC_KEY` secrets are set.
3. Commit, then tag and push:
   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```
4. `release.yml` builds the matrix, signs each Unix archive, and publishes the
   artifacts + `manifest.json` + `vc-frame-signing.asc`.
5. The operator's `vibecrafted-io` (`make site*` / `make release*`) serves
   `https://vibecrafted.io/install.sh` (this script) and the release artifacts.

---

## The non-fakeable proof (foundations gate)

A green tag-build is not a shipped release. The only proof is a **clean foreign
machine** completing the Vibecrafted foundations gate:

```sh
curl -fsSL https://vibecrafted.io/install.sh | sh   # installs vc-frame
vc-frame --version                                   # must run
# then, in a vibecrafted checkout:
make install                                         # foundations gate must pass
```

The gate (`scripts/install-foundations.sh` → `install_vcframe`) runs
`binary_runs vc-frame` after `curl $VCFRAME_INSTALL_URL | sh`. GPG is **not**
required at the gate — only that `vc-frame --version`/`--help` succeeds. Before
this runbook existed, that gate failed with `vc-frame: MISSING` and aborted
every foreign `make install`. A release is "done" only when this line is green
on a machine that never built vc-frame.

---

## Operator buttons (never an agent)

- Cutting the `vX.Y.Z` tag (triggers the irreversible release).
- Wiring/serving `vibecrafted.io/install.sh` + the artifacts via `vibecrafted-io`.
- The GPG **private** key material in `~/.keys` (the runner/secret provides it at
  release time; an agent only wires the signing step and publishes the public key).
