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
| `emphasis_1` | **The accent — "you are here".** Rail current-session name, `⚿ LOCKED` chip background, active `●` tab dot |
| `emphasis_2` | Dim chrome: rail ordinals, `-` session markers, `·` separators, resource cockpit line, `⌁ NORMAL` |
| `emphasis_3` | Alarm: bell flash |

### `text_selected` — selection and the block highlight

`background` is the full-width bar behind: rail hover, keyboard selection,
and the current-session **block tint** (the whole block of the session you
are in, header plus its process rows). Emphasis slots mirror
`text_unselected` so accents survive selection.

### `ribbon_selected` — "this is active"

`background`/`base` paint the active tab chip on the compact-bar and the
armed-mode chip (PANE, TAB, SESSION, …). This must contrast with **both**
`ribbon_unselected` shades, or the active tab becomes guesswork.

### `ribbon_unselected` — inactive ribbons

`background`/`base` for inactive tab chips; `emphasis_1` is the alternate
ribbon shade (visual rhythm only — see doctrine below); `emphasis_3` is the
bell flash on an inactive tab.

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
2. **State is a glyph, not a shade.** Active tab: `◉`. Inactive: `○`.
   Current session: `*`. Others: `-`. Locked: `⚿`. Normal: `⌁`. Alternating
   ribbon shades are rhythm — they carry no state, so recoloring them can
   never lie about focus.
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
   `ribbon_selected.background`: one hue for "you are here" and "this tab
   is active" keeps the whole chrome speaking one language.
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

## Roadmap: the workspace designer

A planned `workspace-designer` plugin turns this guide into a tool: a
floating atelier pane that previews the rail, tab line, and mode chips with
live styling, lets you edit the **semantic** slots (Ground / Ink / Accent /
Alarm / Selection) instead of raw declarations, and exports a ready
`themes/<name>.kdl`. Editing the active theme file is already live-reloaded,
so the feedback loop exists today — the plugin removes the hand-mapping.

---

𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI
