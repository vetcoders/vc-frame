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

### 2. Never bind bare Shift+click for *app* handlers

Terminals — Alacritty included — reserve bare `Shift+click` to **bypass
mouse reporting** (so users can select text natively even when an app owns
the mouse). A plain `Shift+click` therefore cannot reach vc-frame, ever —
and that is intentional. The preset enables Alacritty **hints** with
`mouse.enabled = true` so `Shift+click` on a URL / OSC 8 hyperlink opens
via the host (`open` on macOS). vc-frame still owns bare Click and
`Alt+Shift` / `Ctrl+Shift` for paths without OSC 8. Do not try to "fix"
Shift into an app binding; it is terminal physics that becomes the host
browser gesture.

### 3. Do not steal Alt+arrows in the host

**Product key-contract:** `Alt+←/→` previous/next tab, `Alt+↑/↓`
previous/next session (unlocked modes). LOCK is typing mode: `Alt+←/→`
word-jump inside vc-frame; Ctrl+arrows remain an optional bonus when the
OS does not claim them. The preset must **not** bind `Alt+Left/Right` to
host `chars` — that would steal the product contract. Mission Control may
keep its default Ctrl+arrow Spaces shortcuts; operators no longer need to
disable them for the declared contract to work.

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
- **The native-window block** (preset default, operator-tuned live
  2026-07-29): `decorations = "Transparent"` + `title = "𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍."` +
  `startup_mode = "Maximized"` + `blur`/`opacity 0.9` + zero padding. The OS
  titlebar dissolves and the compact-bar becomes the de-facto window chrome —
  vc-frame reads as a native app. The traffic-light zone is handled bar-side,
  not with padding: the operator layouts pass `left_inset "9"` to the
  compact-bar, which starts the bar 9 blank columns in (≈ 65–70px at a 13pt
  monospace) so the brand chip clears the macOS window buttons. Padding would
  shift every row and both edges; the inset costs only the first row's
  corner. Tune the number to your font size, or set 0 in a decorated window.
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
| `Alt+←/→` types word-jump / does nothing for tabs | host still binds Alt+arrows as `chars` | remove host Alt+Left/Right binds; use the shipped preset |
| `Shift+click` on a URL does nothing | hints missing / mouse disabled | import preset `[hints]` with `mouse.enabled = true` |
| `Ctrl+←/→` switches macOS Spaces | Mission Control owns the shortcut | expected; product contract is Alt+arrows — leave MC alone |
| Chrome glyphs show as boxes | font lacks Misc Symbols coverage | pick a fuller monospace / Nerd Font |
| Thin colored border around the chrome | host padding + opaque mismatched background | keep the preset's `blur`/`opacity`, or match theme ground / zero the padding |
| Chrome keys from the guide do nothing / `Ctrl+q` kills the whole session | frozen `clear-defaults` keybinds in the user config shadow the shipped contract | `vc-frame doctor`, then `vc-frame repair key-bindings` (see [DOCTOR.md](DOCTOR.md)) |

---

𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
