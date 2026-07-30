use crate::LinePart;
use ansi_term::{AnsiString, AnsiStrings};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Fisheye tab markers. The alternating ribbon shades stay, but shade alone
/// used to double as state — a lighter alternate ribbon read as "active".
/// Now every inactive tab carries ○ and only the focused tab carries ●, so
/// the marker is the state and the shade is just rhythm.
const ACTIVE_TAB_MARKER: &str = "●";
const INACTIVE_TAB_MARKER: &str = "○";

/// Tab chip edges: each chip owns exactly half of the seam cell on either
/// side (fg = own background, bg = bar ground), so the boundary between two
/// chips is split 50|50 on the border line — no painted-on rules.
const LEFT_EDGE: &str = "▐";
const RIGHT_EDGE: &str = "▌";

fn cursors<'a>(
    focused_clients: &'a [ClientId],
    colors: MultiplayerColors,
) -> (Vec<AnsiString<'a>>, usize) {
    // cursor section, text length
    let mut len = 0;
    let mut cursors = vec![];
    for client_id in focused_clients.iter() {
        if let Some(color) = client_id_to_colors(*client_id, colors) {
            cursors.push(style!(color.1, color.0).paint(" "));
            len += 1;
        }
    }
    len += 2; // 2 for the brackets: [ and ]
    (cursors, len)
}

pub fn render_tab(
    text: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
) -> LinePart {
    let focused_clients = tab.other_focused_clients.as_slice();
    // One letter color across the whole tab zone: state is carried by
    // dim/bold and the ○/● marker, never by switching ink — that is what
    // used to produce near-white ink on the near-white alternate shade.
    // The active chip is the soft-selected surface (text_selected), one
    // step lighter than the rhythm shades; the hard accent inversion
    // belongs to the mode chip alone.
    let background_color = if tab.active {
        palette.text_selected.background
    } else if is_alternate_tab {
        palette.ribbon_unselected.emphasis_1
    } else {
        palette.ribbon_unselected.background
    };
    let foreground_color = if tab.is_flashing_bell {
        palette.ribbon_unselected.emphasis_3
    } else if tab.active {
        palette.text_selected.base
    } else {
        palette.ribbon_unselected.base
    };
    let marker = if tab.active {
        ACTIVE_TAB_MARKER
    } else {
        INACTIVE_TAB_MARKER
    };
    let ground = palette.text_unselected.background;
    let text_style = if tab.active {
        style!(foreground_color, background_color).bold()
    } else {
        style!(foreground_color, background_color).dimmed()
    };
    let padded_text = format!(" {} {} ", marker, text);
    let left_edge = style!(background_color, ground).paint(LEFT_EDGE);
    let right_edge = style!(background_color, ground).paint(RIGHT_EDGE);
    let mut tab_text_len = padded_text.width() + 2; // half-block edge cells

    let tab_styled_text = text_style.paint(padded_text);

    let tab_styled_text = if !focused_clients.is_empty() {
        let (cursor_section, extra_length) =
            cursors(focused_clients, palette.multiplayer_user_colors);
        tab_text_len += extra_length;
        let mut s = String::new();
        let cursor_beginning = text_style.paint("[").to_string();
        let cursor_section = AnsiStrings(&cursor_section).to_string();
        let cursor_end = text_style.paint("]").to_string();
        s.push_str(&left_edge.to_string());
        s.push_str(&tab_styled_text.to_string());
        s.push_str(&cursor_beginning);
        s.push_str(&cursor_section);
        s.push_str(&cursor_end);
        s.push_str(&right_edge.to_string());
        s
    } else {
        AnsiStrings(&[left_edge, tab_styled_text, right_edge]).to_string()
    };

    LinePart {
        part: tab_styled_text,
        len: tab_text_len,
        tab_index: Some(tab.position),
    }
}

pub fn tab_style(
    mut tabname: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
    _capabilities: PluginCapabilities,
) -> LinePart {
    if tab.is_fullscreen_active {
        tabname.push_str(" (FULLSCREEN)");
    } else if tab.is_sync_panes_active {
        tabname.push_str(" (SYNC)");
    }
    if tab.has_bell_notification || tab.is_flashing_bell {
        tabname.push_str(" [!]");
    }
    // The alternating shade is pure rhythm and no longer depends on host
    // font capabilities — the half-block edges separate chips everywhere.
    render_tab(tabname, tab, is_alternate_tab, palette)
}

pub(crate) fn get_tab_to_focus(
    tab_line: &[LinePart],
    active_tab_idx: usize,
    mouse_click_col: usize,
) -> Option<usize> {
    let clicked_line_part = get_clicked_line_part(tab_line, mouse_click_col)?;
    let clicked_tab_idx = clicked_line_part.tab_index?;
    // tabs are indexed starting from 1 so we need to add 1
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
