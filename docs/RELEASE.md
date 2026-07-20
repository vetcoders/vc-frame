# Releasing vc-frame

vc-frame uses a **single-repository, single-tag release**: one `vX.Y.Z` tag in
`vetcoders/vc-frame` creates one draft GitHub Release, builds the complete
platform matrix, verifies the asset contract, and only then publishes it. The
shape is intentionally Zellij-like; the trust and verification gates are
Loctree-grade.

GitHub Releases is the canonical artifact and installer owner. A future
`vibecrafted.io` endpoint may mirror or redirect to it, but it is not on the
critical path.

## Why this shape

| Concern | Zellij model | Loctree suite model | vc-frame decision |
|---|---|---|---|
| Ownership | One source repo and one GitHub Release | Suite orchestrates crates, thin repos, npm, bundles and taps | One source repo and one GitHub Release |
| Trigger | Version tag or manual draft | Several manual/tag workflows and downstream releases | Version tag or manual candidate draft |
| Platform builds | One normal/no-web matrix | Several matrices across binary, package and bundle channels | One normal/no-web matrix |
| Trust | Checksums; limited signing policy | GPG sidecars, Apple signing/notarization, cross-channel gates | SHA256 plus mandatory GPG for Unix archives; no unsigned publish |
| Atomicity | Release is assembled directly | Cascade is powerful but has more partial-failure boundaries | Draft first, publish only after every required asset exists |
| Installer | Release assets are primary | Multiple package/install channels | Versionless `install.sh` asset points to the tagged assets |
| Complexity | Low | High, justified by a product suite | Low until vc-frame has real downstream channels |

Do not copy Loctree's multi-repository cascade into vc-frame until vc-frame
actually has crates, npm platform packages, taps, or thin distribution repos to
coordinate. Adding those boundaries early would create failure modes without
creating user value.

## Release asset contract

Every supported target publishes a full build and a `no-web` build:

- `vc-frame-<target>.tar.gz` on Unix, or `.zip` on Windows
- an archive-level `.sha256` sidecar
- a detached `.sig` for every Unix archive
- Windows MSI installers and checksum sidecars

The release also contains:

- `manifest.json`, the installer's target-to-archive map
- `vc-frame-signing.asc`, the public release key
- `install.sh`, the versionless canonical installer

Supported targets are x86_64/aarch64 Linux musl, x86_64/aarch64 macOS, and
x86_64 Windows MSVC.

## Trust root

The workflow accepts three repository secrets:

- `GPG_PRIVATE_KEY`: ASCII-armored private signing key
- `GPG_PASSPHRASE`: optional passphrase for that key
- `GPG_PUBLIC_KEY`: ASCII-armored public key published with the release

Candidates and tags fail before building if the private or public key is
missing. `tools/install.sh` defaults to strict GPG verification and also
requires a pinned `VCFRAME_GPG_FINGERPRINT`; downloading a key and signature
from the same untrusted location is not treated as proof of identity.

The operator vault's `vibecrafted-signing.key/.pub` pair is RSA/PEM, not GPG,
so it cannot be dropped into this contract without changing the signature
format and installer. Loctree's release path can use a GPG identity already in
the runner keyring. Reusing that GPG identity is technically valid if VetCoders
intentionally wants one product-family trust root, but a dedicated vc-frame
key or signing subkey gives cleaner revocation and blast-radius boundaries. In
either case, never commit private key material.

Before the first public release:

1. Configure the three repository secrets.
2. Record the selected public key fingerprint as the installer's default or
   require callers to pass `VCFRAME_GPG_FINGERPRINT`.
3. Run the candidate workflow and prove the signed installer path.

## Candidate release

Run **Actions → Release → Run workflow**. The workflow:

1. verifies Cargo and installer versions match;
2. requires signing inputs;
3. runs `make semgrep` and `make ci`;
4. creates draft `candidate-<run-id>`;
5. builds and signs the full matrix;
6. verifies the complete asset list;
7. leaves the candidate as a draft.

Candidate runs never become `latest` and never publish a production release.

## Real release

Only cut a tag from the reviewed commit on `main`:

```sh
version=0.45.4
git switch main
git pull --ff-only origin main
git tag -a "v$version" -m "vc-frame v$version"
git push origin "v$version"
```

The tag must match `[workspace.package].version` in `Cargo.toml` and the default
version in `tools/install.sh`. The workflow creates a draft, uploads all assets,
checks the contract through the GitHub API, and publishes only after all jobs
succeed.

The canonical cold install is:

```sh
VCFRAME_GPG_FINGERPRINT=<pinned-fingerprint> \
  sh -c "$(curl -fsSL https://github.com/vetcoders/vc-frame/releases/latest/download/install.sh)"
vc-frame --version
```

## Build provenance

Every binary carries its own identity, embedded at build time by
`zellij-utils/build.rs` and owned by `zellij-utils/src/build_info.rs`. That
module is the **single owner**: clap's `--version`, `--build-info`, and the
`setup --check` dump all read from it, so they cannot disagree. Nothing invokes
git at runtime — an installed binary knows what it is with no repository in
sight.

```console
$ vc-frame --version
vc-frame 0.45.4+gbcd9e175

$ vc-frame --build-info
{
  "product": "vc-frame",
  "version": "0.45.4",
  "human_version": "0.45.4+gbcd9e175",
  "git_sha": "bcd9e175b5267fb0f0bdcbd12d657072db351999",
  "git_sha_short": "bcd9e175",
  "git_dirty": false,
  "build_time_utc": "2026-07-20T17:24:11Z",
  "profile": "release"
}
```

A build from a modified tree reports `0.45.4+gbcd9e175.dirty`, and packaging
refuses to proceed at all — see below.

The commit identity resolves in this order:

1. `VC_FRAME_GIT_SHA` / `VC_FRAME_GIT_DIRTY` from the environment. The release
   workflow sets these from `github.sha` so every matrix runner embeds the same
   commit regardless of local git availability.
2. `git` in the checkout.
3. Nothing — debug builds record `unknown`, and **release builds fail closed**.
   A release binary must never claim an identity it does not have. Building a
   release outside a git checkout is supported only by passing the values in:

   ```sh
   VC_FRAME_GIT_SHA=<40-hex-sha> VC_FRAME_GIT_DIRTY=0 cargo build --release
   ```

`VC_FRAME_BUILD_TIME_UTC` can be pinned the same way for reproducible builds.

## Packaging provenance

```sh
make release-guard   # refuse to package a dirty worktree
make package         # guard → release build → archive → checksum → receipt
```

`make package` writes `target/dist/RECEIPT.json`, which names the exact bytes:
archive and binary SHA256 plus sizes, the source commit, the toolchain, and the
provenance the packaged binary reports **about itself**. Packaging aborts if
that self-report disagrees with the commit being packaged, so the receipt cannot
describe a binary other than the one produced.

The same guard runs first in the release workflow's `verify-release` job.

## Local installer smoke

The installer is fail-closed, and `make install-test` proves it:

```sh
make install-test
```

It builds synthetic release trees, serves them over `file://`, and asserts the
installer **rejects** every one of: missing manifest, malformed manifest,
foreign-product manifest, version-mismatched manifest, manifest without this
target, missing archive, missing checksum sidecar, checksum mismatch, strict
mode without a pinned fingerprint, strict mode with no published key, and a
binary that fails any part of the post-install smoke contract (`--version`,
`--build-info`, `setup --check`, or a session command). The positive control
additionally asserts the resulting prefix holds `vc-frame` and no `zellij`
alias.

`manifest.json` is **mandatory**. There is no guessed-filename fallback: a
guessed name can only agree with the release by luck, and luck is not
provenance.

The candidate and final release must additionally prove strict GPG
verification against the selected pinned fingerprint.

## Rollback

Release assets are immutable. Do not replace files underneath an existing tag.
If a candidate is wrong, delete its draft. If a published release is wrong,
mark it non-latest, document the defect, and publish a fixed patch version. A
rollback is complete only when the canonical installer resolves to a verified
good release and a clean-machine smoke passes.
