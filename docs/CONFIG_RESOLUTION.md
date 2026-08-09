# Configuration resolution contract

vc-frame has one effective configuration. Resolution is deterministic and the
first readable `config.kdl` wins:

1. `VC_FRAME_CONFIG_DIR`, when explicitly set.
2. The platform user config directory (`~/.config/vc-frame` on Unix-like hosts).
3. The Frontier compatibility directory, when present.
4. Remaining platform/XDG candidates returned by the shared config resolver.
5. Embedded shipped assets when no file wins.

`vc-frame doctor --json` is the authority for the current process. Its config
section prints every candidate in search order, a final `WINNER` line, and the
SHA-256 content hash of the winning file. A path without its content hash is not
enough evidence: the file may have changed between two sessions.

`vc-frame repair key-bindings` edits only the winning user config, creates a
timestamped backup first, and never modifies embedded assets. Repair is not a
runtime reload promise: verify the backup and diff, then recreate the affected
session and run `vc-frame doctor --json` again. Old servers retain the config
they started with until migrated or restarted.

The package installer may place config assets, but it does not own resolution.
Schema, ordering, diagnostics, and runtime behavior remain vc-frame authority.
