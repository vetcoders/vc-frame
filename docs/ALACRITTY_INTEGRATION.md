# Alacritty Integration

Alacritty is the reference host terminal of the Vibecrafted stack — the
maintenance input. The division of labor is strict: **vc-frame owns the
chrome** (session rail, compact-bar, status-bar, themes, keybindings), **the
terminal stays a transparent host**. Everything in this document serves that
one goal: no host feature may eat an input before vc-frame sees it, and no
host pixel may fight the chrome.

The shipped preset lives at `tools/alacritty/vc-frame.toml`.

## Quick start

```toml
# ~/.config/alacritty/alacritty.toml
[general]
import = ["~/.config/alacritty/vc-frame.toml"]
```

Copy the preset next to your config (or point `import` into the repo), keep
your personal settings below the import — later values win. Verify with
`alacritty migrate` after Alacritty upgrades.

## The four hard requirements

These are not taste — each one was a debugged failure mode. Skipping any of
them breaks a shipped vc-frame feature.

### 1. `option_as_alt = "Both"` (macOS)

Without it the Option key composes locale characters (`ę`, `€`, …) instead
of sending Alt. Casualties: `Alt+e` (Command Composer) and the
`Alt+Shift+click` external-open gesture — they simply never reach vc-frame.
This is the single most common "Composer does not work" cause.

### 2. Never bind bare Shift+click

Terminals — Alacritty included — reserve bare `Shift+click` to **bypass
mouse reporting** (so users can select text natively even when an app owns
the mouse). A plain `Shift+click` therefore cannot reach vc-frame, ever.
This is why the external-open gesture exists twice: `Alt+Shift+click` and
`Ctrl+Shift+click` are equivalent. Do not try to "fix" Shift in host
bindings; it is terminal physics.

### 3. Free Ctrl+Arrows from the OS

vc-frame LOCK-mode navigation: `Ctrl+←/→` previous/next tab, `Ctrl+↑/↓`
previous/next session. On macOS, Mission Control claims `Ctrl+←/→` at the
OS level — System Settings → Keyboard → Keyboard Shortcuts → Mission
Control, disable "Move left/right a space" (or remap them). Until then the
events never reach any terminal, and no Alacritty setting can help.

### 4. Width-1 glyph rendering

The chrome speaks in text-presentation Unicode: `◉ ○ ⚿ ⌁ │ · *`. All are
single-column under `unicode-width`, and Alacritty renders ambiguous-width
characters single-column by default — do not override that. Fonts: any
complete monospace works; a Nerd Font is a safe superset. Emoji-presentation
glyphs are deliberately absent from the chrome (see `docs/THEMES_GUIDE.md`),
so no color-font surprises.

## Recommended, not required

- **`shell = { program = "vc-frame", args = ["attach", "--create", "main"] }`**
  ([terminal] table) — opening Alacritty lands you in the operator
  workspace; closing the window detaches, the session lives on. Prefer a
  plain shell? Comment it out and run `vc-frame` on demand.
- **`padding = 0` + background matching the theme ground** — the chrome
  paints its own background; padding shows the host color as a seam around
  it. Either zero the padding (preset default) or match the color.
- **`live_config_reload = true`** — same feedback loop the vc-frame config
  watcher gives: edit, see, iterate.
- **`hide_when_typing = true`** — the pointer hides while typing; hover
  highlights on the rail return the moment the mouse moves.

## Integration levels

| Level | What | Status |
|---|---|---|
| L0 | Any terminal, no setup — vc-frame works, minus Alt gestures on macOS | works |
| L1 | Imported preset (`tools/alacritty/vc-frame.toml`) | **shipped** |
| L2 | `tools/install.sh` offers to install the preset + import line | planned |
| L3 | `workspace-designer` exports paired themes: `themes/<name>.kdl` + an Alacritty color TOML from the same palette | roadmap |

L3 closes the last seam: one palette source generating both the vc-frame
theme and the host colors, so the terminal background, selection colors and
chrome ground can never drift apart.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Alt+e` types `ę` / opens nothing | Option not sending Alt | `option_as_alt = "Both"` |
| `Shift+click` on a link does nothing | terminal-level mouse-reporting bypass | use `Alt+Shift` or `Ctrl+Shift` click |
| `Ctrl+←/→` switches macOS Spaces | Mission Control owns the shortcut | disable in System Settings |
| Chrome glyphs show as boxes | font lacks Misc Symbols coverage | pick a fuller monospace / Nerd Font |
| Thin colored border around the chrome | host padding + mismatched background | `padding = 0` or match theme ground |

---

𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
