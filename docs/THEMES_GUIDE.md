# vc-frame Themes Guide

How the vc-frame chrome consumes a theme, and how to author one that keeps
the Vibecrafted semantics intact.

A theme in vc-frame is a set of *style declarations* — it does not decide
**what** is highlighted, only **with which ink**. The chrome (session rail,
compact-bar, status-bar, pane frames) assigns meaning to a fixed set of
palette slots. Change the colors freely; the meanings below are the contract.

## Theme file anatomy

Themes are KDL. Bundled themes live in `zellij-utils/assets/themes/*.kdl`;
user themes go into the `themes/` subdirectory of your config directory, and
are selected with the `theme "<name>"` option in `config.kdl`. The config
watcher picks up edits to the active theme file — editing it is live
feedback.

```kdl
themes {
    my-theme {
        text_unselected {
            base       255 255 255   // RGB triple
            background 0 0 0
            emphasis_0 255 184 108
            emphasis_1 139 233 253
            emphasis_2 80 250 123
            emphasis_3 255 121 198
        }
        text_selected { /* same six slots */ }
        ribbon_selected { /* ... */ }
        ribbon_unselected { /* ... */ }
        table_title { /* ... */ }
        table_cell_selected { /* ... */ }
        table_cell_unselected { /* ... */ }
        list_selected { /* ... */ }
        list_unselected { /* ... */ }
        frame_selected { /* ... */ }
        frame_highlight { /* ... */ }
        exit_code_success { /* ... */ }
        exit_code_error { /* ... */ }
        multiplayer_user_colors { player_1 255 121 198 /* ... player_10 */ }
    }
}
```

A single value instead of a triple (`background 0`) is an 8-bit terminal
color index. Components rendered through the `Text` API resolve
`color_range(N, ..)` to `emphasis_N` of the row's current declaration
(`text_unselected` normally, `text_selected` when the row is `.selected()`).

## The semantic contract — what vc-frame chrome reads from each slot

### `text_unselected` — the chrome ground

| Slot | Meaning in vc-frame |
|---|---|
| `base` | Primary ink: session names, rail tab names, bar text |
| `background` | Chrome background: compact-bar, rail, status-bar |
| `emphasis_0` | Reserved (free for plugin-specific accents) |
| `emphasis_1` | **The accent — "you are here".** Rail current-session name, `⚿ LOCKED` chip background |
| `emphasis_2` | Dim chrome: rail ordinals, `-` session markers, `·` separators, resource cockpit line, `⌁ NORMAL` |
| `emphasis_3` | Alarm: bell flash |

### `text_selected` — selection and the block highlight

`background` is the full-width bar behind: rail hover, keyboard selection,
and the current-session **block tint** (the whole block of the session you
are in, header plus its process rows). Emphasis slots mirror
`text_unselected` so accents survive selection.

### `ribbon_selected` — "this is armed / this is where you are"

`background`/`base` paint the armed-mode chip (PANE, TAB, SESSION, …), the
inverted LOCK chip, and the **active tab chip** (`◉`, bold) — the same
hard inversion the bottom bar's selected ribbon uses, so the whole chrome
answers "where am I" with one surface.

### `ribbon_unselected` — inactive ribbons

`background`/`base` for inactive tab chips (dim ink); `emphasis_1` is the
alternate ribbon shade — pure visual rhythm, so keep it one close step
from `background`: the ink is shared across all tabs, and a shade that
drifts toward the ink luminance breaks contrast (see doctrine below);
`emphasis_3` is the bell flash on an inactive tab.

### Frames and the rest

| Declaration | Meaning in vc-frame |
|---|---|
| `frame_selected` | Focused pane frame |
| `frame_highlight` | **The bilecik**: frame override for `$EDITOR` panes opened from chrome (file-open flow) — must pop against `frame_selected` |
| `exit_code_success` / `exit_code_error` | Status-bar command exit reporting |
| `multiplayer_user_colors` | Other clients' cursors (rail `[ ]` section, shared panes) |
| `table_*`, `list_*` | Component defaults for plugin UIs (session-manager full view, pickers) |

## Vibecrafted doctrine

The bundled look follows four rules. A theme may bend them; the chrome
never will.

1. **Single accent over grayscale.** One accent color (`emphasis_1`) means
   "you are here", everywhere. Everything else is ink, dim ink, or ground.
2. **State is a glyph and a contrast, not a shade.** The tab zone speaks
   the exact chip language of the bottom status-bar: active tab = `◉`
   (fisheye) with `ribbon_selected` ink on the `ribbon_selected`
   background; inactive tabs = `○` with `ribbon_unselected` ink on the
   `ribbon_unselected` background — everything bold. Alternating ribbon
   shades (`ribbon_unselected.emphasis_1`) are rhythm — they carry no
   state, so recoloring them can never lie about focus. Chips are
   separated by one cell of bar ground on each side: the seam is
   breathing room, never a painted-on rule and never a half-block.
   The same `◉`/`○` pair marks the rail: current session `◉`, every
   other session and bucket row `○` — one "you are here" glyph across
   the whole chrome. Locked: `⊝`. Normal: `▷`.
3. **Three highlight levels.** Ground < block tint (`text_selected`
   background) < inversion (accent background). The rail uses all three:
   plain rows, the current-session block, the active tab row inside it.
4. **Text-presentation glyphs only.** `⚿` (U+26BF, "parental lock") instead
   of the emoji padlock: no color-font override, exactly one column wide.
   Every chrome glyph must be width-1 under `unicode-width`, or click maps
   and column math drift.

## Authoring walkthrough

Four decisions produce a coherent theme; everything else derives:

1. **Ground** — `text_unselected.background` (and `ribbon_unselected`
   shades near it).
2. **Ink** — `text_unselected.base`, readable on ground.
3. **Accent** — `text_unselected.emphasis_1` *and*
   `ribbon_selected.background`: one hue for "you are here", the armed
   MODE chip and the active tab chip — the top bar, the rail and the
   bottom shortcut bar all invert on the same ribbon accent, so the whole
   chrome speaks one language.
4. **Alarm** — `emphasis_3`, reserved for bells; nothing else may use it.

Then set `text_selected.background` to a step between ground and accent
(the block tint), dim `emphasis_2` toward the ground, and give
`frame_highlight` a hue distinct from `frame_selected`.

Checklist before shipping a theme:

- [ ] Active tab obvious with **zero** color vision (the `◉` helps, but
      contrast should not depend on it)
- [ ] `⚿ LOCKED` chip readable (accent background, ground-colored text)
- [ ] Block tint visible but calmer than the active-row inversion
- [ ] Bell flash distinguishable from the accent
- [ ] Bilecik (`frame_highlight`) ≠ focused frame (`frame_selected`)

## Live theme owner — dark/light without the host terminal

Since 2026-09-08 (`FRAME-theme`) **vc-frame is the single owner of the live
theme**. The ☾/☼ chip in the top bar, `vc-frame action toggle-theme` /
`set-dark-theme` / `set-light-theme`, and a keybinding all end in the same
place: `Screen::apply_theme_mode` in the server. Nothing in that path touches
the host terminal, its palette files, or any third-party TUI's colors.

### The gate

Both keys must be set in `config.kdl`:

```kdl
theme       "monochrome"          // static fallback when the owner is off
theme_dark  "monochrome"
theme_light "vibecrafted-ivory"
```

With only one (or none) vc-frame behaves like upstream zellij: static `theme`,
host-default passthrough for pane cells, manual switch refused with a clear
CLI error. With both, the **theme owner is engaged**, which means:

1. **Chrome + canvas swap together.** Every tab, every pane, every attached
   client receives the mode's palette live — no restart, no pane recreation.
   `Screen.style` is kept in sync, so tabs and panes created *after* a switch
   are born with the current choice.
2. **Default-colored pane cells are painted by the frame.** Cells an
   application left at the default foreground/background (SGR reset, never
   styled) render with `text_unselected.base` / `text_unselected.background`
   of the live palette instead of falling through to whatever the host
   terminal paints as its default. This is `Style::theme_owns_pane_defaults`
   and it is resolved at render time in `Grid::render`.
   Precedence per cell slot: app-provided **OSC 10/11** default → live theme →
   host passthrough (owner off). **Explicit ANSI/RGB colors an app sets are
   never touched** — no blanket recolor.
3. **An explicit choice pins the frame.** Until the user picks, the host
   terminal's CSI 2031 / DSR 997 report seeds the mode (VC Terminal supports
   it; most other engines never report, so the static theme stays). After the
   first click/action the host no longer gets a vote: later host reports are
   ignored, not forwarded. Pinning is session-wide and survives `config.kdl`
   reloads (a reconfigure re-applies the live mode's palette, not the static
   `theme`). Only a server restart clears it.
4. **One session, one theme.** All clients attached to a session share the
   mode; there is no per-client theme. A toggle flips based on the current
   canonical mode (unknown → light), any number of times.

### What the switcher plugin does (and does not)

`compact-bar` dispatches `Action::ToggleTheme` through the plugin
`run_action` API and paints ☾/☼ from `Event::HostTerminalThemeChanged`,
which the server fans out on every switch **and replays after each plugin
(re)load** (`RequestStateUpdateForPlugins`), so a freshly loaded bar never
guesses. The event name is historical — what it carries is vc-frame's
canonical mode. The bar no longer runs `vc-theme` and no longer publishes a
host palette; switching the outer terminal app's own theme is that app's
business.

### Propagation contract for panes and TUIs (e.g. `vc-start-here.py`)

- Paint with **default colors** (`curses.use_default_colors()`, plain
  `\e[0m` text) and you get the live frame palette for free, live, on every
  switch — the frame repaints every line of every pane.
- Set **explicit** colors and they are yours; the frame never overrides them.
  Pick colors that read on both grounds, or…
- …ask for the mode: `CSI ? 2031 h` subscribes, `CSI ? 996 n` queries, the
  reply/notification is `CSI ? 997 ; 1 n` (dark) / `CSI ? 997 ; 2 n`
  (light). The frame answers from its **canonical** mode, so a TUI that
  follows this protocol follows the ☾/☼ chip, not the host terminal.
- Per-pane `OSC 10` / `OSC 11` defaults still win over the theme for the
  slot they claim (`OSC 110/111` hand it back).

## Roadmap: the workspace designer

A planned `workspace-designer` plugin turns this guide into a tool: a
floating atelier pane that previews the rail, tab line, and mode chips with
live styling, lets you edit the **semantic** slots (Ground / Ink / Accent /
Alarm / Selection) instead of raw declarations, and exports a ready
`themes/<name>.kdl`. Editing the active theme file is already live-reloaded,
so the feedback loop exists today — the plugin removes the hand-mapping.

---

𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI
