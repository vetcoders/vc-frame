# Keyboard contract

The effective key contract is the merge of the shipped defaults and the single
winning user config described in [CONFIG_RESOLUTION.md](CONFIG_RESOLUTION.md).
Rendered help is descriptive; the parsed effective keymap is authoritative.

Safety rules:

- Host-terminal bindings fire before vc-frame and can shadow the contract.
- `clear-defaults=true` replaces a mode; it is not an overlay.
- LOCK must retain an escape path and the navigation needed by the session rail.
- `Ctrl q` means close focus in the shipped contract; a frozen config that maps
  it to quit is a destructive divergence.
- Width-sensitive chrome uses Unicode display width, never byte or character
  count. Ambiguous-width behavior is fixed to the shipped narrow-cell contract;
  a host configured to render ambiguous glyphs wide is unsupported and must be
  reported by the reproduction harness.

Use `vc-frame doctor` before repair. `vc-frame repair key-bindings` is honest
about its boundary: it backs up and rewrites the winning config, but cannot
reload an already-running server or rewrite the host terminal's shortcuts.
Acceptance is repair, deliberate session recreation, then a second doctor run.
