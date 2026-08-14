# vc-frame donor release contract

`vc-frame` is the session interior of Vibecrafted, not a separately installed
product. **Vibecrafted.app owns** the application bundle, the
single `Vibecrafted.dmg`, Developer ID signing, Apple notarization, installation,
updates, runtime generation and user-facing release notes.

This repository owns one release-facing operation:

```sh
make release
```

That target builds the deterministic `vc-frame` donor binary and bundled WASM
assets. The sibling `vibecrafted` release builder records this checkout's exact
Git revision, embeds the resulting binary under `Vibecrafted.app`, signs the
whole containment boundary and verifies it as one artifact.

There is deliberately no `vc-frame` installer, app bundle, DMG, MSI, archive
publisher, tag-triggered GitHub Release or update channel. Adding any of those
would recreate a second owner and is rejected by:

```sh
make release-contract-test
```

For local development use `make build`, `make run`, `make test` and
`make precheck`. For an end-user install or upgrade, download the
`Vibecrafted.dmg` published by
[`vetcoders/vibecrafted`](https://github.com/vetcoders/vibecrafted/releases).
