use std::collections::BTreeSet;

use crate::LinePart;
use ansi_term::{AnsiString, AnsiStrings};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Fisheye tab markers — the same state language as compact-bar and the
/// bottom status-bar chips: the focused tab carries ◉, every other tab ○.
const ACTIVE_TAB_MARKER: &str = "◉";
const INACTIVE_TAB_MARKER: &str = "○";

pub const CLOSE_GLYPH: &str = "×";
pub const CLOSE_ZONE_COLS: usize = 3;
pub const CLOSE_ARM_TIMEOUT_SECS: f64 = 3.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TabCloseAffordance {
    pub closable: bool,
    pub dead: bool,
    pub armed: bool,
    pub close_id: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseDecision {
    Arm { tab_id: usize, guest: bool },
    Confirm { tab_id: usize, guest: bool },
    CloseImmediately { tab_id: usize, guest: bool },
}

pub fn decide_close(
    armed: Option<(usize, bool)>,
    tab_id: usize,
    guest: bool,
    dead: bool,
) -> CloseDecision {
    if dead {
        return CloseDecision::CloseImmediately { tab_id, guest };
    }
    if armed == Some((tab_id, guest)) {
        CloseDecision::Confirm { tab_id, guest }
    } else {
        CloseDecision::Arm { tab_id, guest }
    }
}

pub fn tab_is_contractual(name: &str) -> bool {
    name == VC_HOME_TAB_NAME
        || name == VC_SHARED_WORKSPACE_TAB_NAME
        || matches!(name, "Start here" | "Agents" | "Shell" | "Voc")
        || GUEST_ORGAN_NAMES.contains(&name)
}

pub fn dead_tab_positions(manifest: &PaneManifest) -> BTreeSet<usize> {
    manifest
        .panes
        .iter()
        .filter_map(|(position, panes)| {
            let mut saw_terminal = false;
            for pane in panes {
                if pane.is_plugin {
                    continue;
                }
                saw_terminal = true;
                if !pane.exited {
                    return None;
                }
            }
            saw_terminal.then_some(*position)
        })
        .collect()
}

fn cursors<'a>(
    focused_clients: &'a [ClientId],
    multiplayer_colors: MultiplayerColors,
) -> (Vec<AnsiString<'a>>, usize) {
    let mut len = 0;
    let mut cursors = vec![];
    for client_id in focused_clients.iter() {
        if let Some(color) = client_id_to_colors(*client_id, multiplayer_colors) {
            cursors.push(style!(color.1, color.0).paint(" "));
            len += 1;
        }
    }
    (cursors, len)
}

pub fn render_tab(
    text: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
    close: TabCloseAffordance,
) -> LinePart {
    let focused_clients = tab.other_focused_clients.as_slice();
    let background_color = if tab.active {
        palette.ribbon_selected.background
    } else if is_alternate_tab {
        palette.ribbon_unselected.emphasis_1
    } else {
        palette.ribbon_unselected.background
    };
    let foreground_color = if tab.is_flashing_bell {
        if tab.active {
            palette.ribbon_selected.emphasis_3
        } else {
            palette.ribbon_unselected.emphasis_3
        }
    } else if tab.active {
        palette.ribbon_selected.base
    } else {
        palette.ribbon_unselected.base
    };
    let marker = if tab.active {
        ACTIVE_TAB_MARKER
    } else {
        INACTIVE_TAB_MARKER
    };
    let ground = palette.text_unselected.background;
    let text_style = style!(foreground_color, background_color).bold();
    let show_close = close.closable && close.close_id.is_some();
    // Colored chip keeps its trailing space. " × " replaces the right ground
    // gap, so the closable chip is two columns wider.
    let chip = format!(" {marker} {text} ");
    let chip_width = chip.width();
    let gap = style!(ground, ground);
    let (cursor_block, cursor_extra) = if focused_clients.is_empty() {
        (String::new(), 0)
    } else {
        let (cursor_section, extra_length) =
            cursors(focused_clients, palette.multiplayer_user_colors);
        let mut block = String::new();
        block.push_str(&text_style.bold().paint("[").to_string());
        block.push_str(&AnsiStrings(&cursor_section).to_string());
        block.push_str(&text_style.bold().paint("]").to_string());
        (block, extra_length + 2)
    };
    let close_start = show_close.then_some(1 + chip_width + cursor_extra);
    let tab_text_len = match close_start {
        Some(start) => start + CLOSE_ZONE_COLS,
        None => chip_width + 2 + cursor_extra,
    };

    let mut part = String::new();
    part.push_str(&gap.paint(" ").to_string());
    part.push_str(&text_style.paint(chip).to_string());
    part.push_str(&cursor_block);
    if show_close {
        let glyph_style = if close.armed {
            style!(background_color, foreground_color).bold()
        } else if close.dead {
            text_style.dimmed()
        } else {
            text_style
        };
        part.push_str(&gap.paint(" ").to_string());
        part.push_str(&glyph_style.paint(CLOSE_GLYPH).to_string());
        part.push_str(&gap.paint(" ").to_string());
    } else {
        part.push_str(&gap.paint(" ").to_string());
    }

    LinePart {
        part,
        len: tab_text_len,
        tab_index: Some(tab.position),
        close_start,
        close_id: if show_close { close.close_id } else { None },
    }
}

pub fn tab_style(
    tabname: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
    capabilities: PluginCapabilities,
) -> LinePart {
    tab_style_with_close(
        tabname,
        tab,
        is_alternate_tab,
        palette,
        capabilities,
        TabCloseAffordance::default(),
    )
}

pub fn tab_style_with_close(
    mut tabname: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
    _capabilities: PluginCapabilities,
    mut close: TabCloseAffordance,
) -> LinePart {
    if tab_is_contractual(&tabname) {
        close.closable = false;
        close.close_id = None;
        close.armed = false;
        close.dead = false;
    }
    if tab.is_fullscreen_active {
        tabname.push_str(" (FULLSCREEN)");
    } else if tab.is_sync_panes_active {
        tabname.push_str(" (SYNC)");
    }
    if tab.has_bell_notification || tab.is_flashing_bell {
        tabname.push_str(" [!]");
    }
    render_tab(tabname, tab, is_alternate_tab, palette, close)
}

pub(crate) fn get_tab_to_focus(
    tab_line: &[LinePart],
    active_tab_idx: usize,
    mouse_click_col: usize,
) -> Option<usize> {
    let clicked_line_part = get_clicked_line_part(tab_line, mouse_click_col)?;
    let clicked_tab_idx = clicked_line_part.tab_index?;
    let clicked_tab_idx = clicked_tab_idx + 1;
    if clicked_tab_idx != active_tab_idx {
        return Some(clicked_tab_idx);
    }
    None
}

pub(crate) fn get_clicked_line_part(
    tab_line: &[LinePart],
    mouse_click_col: usize,
) -> Option<&LinePart> {
    let mut len = 0;
    for tab_line_part in tab_line {
        if mouse_click_col >= len && mouse_click_col < len + tab_line_part.len {
            return Some(tab_line_part);
        }
        len += tab_line_part.len;
    }
    None
}

pub fn close_hit(tab_line: &[LinePart], mouse_click_col: usize) -> Option<usize> {
    let mut len = 0;
    for part in tab_line {
        if mouse_click_col >= len && mouse_click_col < len + part.len {
            let (Some(start), Some(id)) = (part.close_start, part.close_id) else {
                return None;
            };
            let local = mouse_click_col - len;
            if local >= start && local < start + CLOSE_ZONE_COLS {
                return Some(id);
            }
            return None;
        }
        len += part.len;
    }
    None
}

pub fn middle_close_hit(tab_line: &[LinePart], mouse_click_col: usize) -> Option<usize> {
    let mut len = 0;
    for part in tab_line {
        if mouse_click_col >= len && mouse_click_col < len + part.len {
            return part.close_id;
        }
        len += part.len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closable(id: usize) -> TabCloseAffordance {
        TabCloseAffordance {
            closable: true,
            close_id: Some(id),
            ..TabCloseAffordance::default()
        }
    }

    fn styled(name: &str, close: TabCloseAffordance) -> LinePart {
        tab_style_with_close(
            name.to_owned(),
            &TabInfo::default(),
            false,
            Styling::default(),
            PluginCapabilities::default(),
            close,
        )
    }

    #[test]
    fn close_zone_is_three_cells_and_the_label_does_not_close() {
        let chip = styled("codex", closable(7));
        let start = chip.close_start.expect("zone");
        let line = [chip];
        assert_eq!(close_hit(&line, start.saturating_sub(1)), None);
        assert_eq!(close_hit(&line, start), Some(7));
        assert_eq!(close_hit(&line, start + 2), Some(7));
        assert_eq!(middle_close_hit(&line, 1), Some(7));
        assert_eq!(
            decide_close(None, 7, false, false),
            CloseDecision::Arm {
                tab_id: 7,
                guest: false
            }
        );
        assert_eq!(
            decide_close(Some((7, false)), 7, false, false),
            CloseDecision::Confirm {
                tab_id: 7,
                guest: false
            }
        );
    }

    #[test]
    fn contractual_classic_tabs_have_no_glyph() {
        for name in ["Home", "Workspace", "Start here", "Agents", "Shell", "Voc", "Overview"] {
            let chip = styled(name, closable(1));
            assert!(!chip.part.contains('×'), "{name}");
        }
        assert!(styled("codex", closable(2)).part.contains('×'));
    }
}
