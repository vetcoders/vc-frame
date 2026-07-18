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

## Local installer smoke

An unsigned local fixture may exercise archive resolution and SHA verification:

```sh
REL=/tmp/vcframe-test/v0.45.4
mkdir -p "$REL" /tmp/vcframe-stage
cp "$(command -v vc-frame)" /tmp/vcframe-stage/vc-frame
target="$(uname -m)-apple-darwin" # use *-unknown-linux-musl on Linux
tar czf "$REL/vc-frame-$target.tar.gz" -C /tmp/vcframe-stage vc-frame
(cd "$REL" && shasum -a 256 "vc-frame-$target.tar.gz" > "vc-frame-$target.tar.gz.sha256")
printf '{"artifacts":{"%s":"vc-frame-%s.tar.gz"}}\n' "$target" "$target" > "$REL/manifest.json"

VCFRAME_VERSION=0.45.4 VCFRAME_BASE_URL=file:///tmp/vcframe-test \
VCFRAME_REQUIRE_GPG=0 INSTALL_DIR=/tmp/vcframe-bin \
VCFRAME_NO_PROFILE_UPDATE=1 sh tools/install.sh
/tmp/vcframe-bin/vc-frame --version
```

The candidate and final release must additionally prove strict GPG
verification against the selected pinned fingerprint.

## Rollback

Release assets are immutable. Do not replace files underneath an existing tag.
If a candidate is wrong, delete its draft. If a published release is wrong,
mark it non-latest, document the defect, and publish a fixed patch version. A
rollback is complete only when the canonical installer resolves to a verified
good release and a clean-machine smoke passes.
