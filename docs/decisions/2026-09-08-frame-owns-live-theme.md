# FRAME-theme decision — vc-frame owns the live theme

Date: 2026-09-08

Founder instruction (Maciej, 2026-09-08): "uzależnić jednak zmianę theme nie
od vc-terminal themes tylko osadzić w vc-frame" — the theme switch must not
depend on VC Terminal's theme files; it lives in vc-frame.

Decision: the server (`Screen`) is the one theme owner. The compact-bar ☾/☼
chip runs `Action::ToggleTheme` through the plugin `run_action` API instead of
executing the external `vc-theme` command, and reads its state from
`Event::HostTerminalThemeChanged`, replayed after every plugin load. A manual
choice pins the frame against host CSI 2031 reports. When `theme_dark` and
`theme_light` are both configured, the frame also paints default-colored pane
cells with its palette (`Style::theme_owns_pane_defaults`), so Frame-rendered
surfaces look the same in VC Terminal, Ghostty, Alacritty or a raw protocol
client, independent of the host palette. Explicit app colors and per-pane OSC
10/11 defaults are never overridden.

Consequences: the chip no longer flips the host terminal palette; `vc-theme`
(vibecrafted) remains a standalone host-palette tool and may still call
`vc-frame action set-*-theme`, which now pins. Reconfigure re-applies the live
mode's palette rather than the static `theme`. New tabs/panes inherit the live
choice because `Screen.style` is now kept in sync on every switch (this was a
latent bug: the upstream auto-switch updated tabs but not `Screen.style`).

Not decided here: renaming `HostTerminalThemeMode` / `HostTerminalThemeChanged`
(protobuf-visible plugin API), and whether the pin should be persisted across
server restarts.
