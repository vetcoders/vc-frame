use crate::LinePart;
use ansi_term::{AnsiString, AnsiStrings};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Fisheye tab markers — the same state language as compact-bar and the
/// bottom status-bar chips: the focused tab carries ◉, every other tab ○.
const ACTIVE_TAB_MARKER: &str = "◉";
const INACTIVE_TAB_MARKER: &str = "○";

fn cursors<'a>(
    focused_clients: &'a [ClientId],
    multiplayer_colors: MultiplayerColors,
) -> (Vec<AnsiString<'a>>, usize) {
    // cursor section, text length
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
) -> LinePart {
    let focused_clients = tab.other_focused_clients.as_slice();

    // The same chip recipe as compact-bar and the bottom status-bar
    // (`color_elements()`): active = ribbon_selected base on its background,
    // inactive = ribbon_unselected base on its background, alternate rows on
    // emphasis_1 — everything bold, state carried by contrast + marker.
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

    // One ground cell on each side — chips separated by the bar itself,
    // never by drawn rules or powerline arrows.
    let ground = palette.text_unselected.background;
    let gap = style!(ground, ground);
    let left_separator = gap.paint(" ");
    let padded_text = format!(" {} {} ", marker, text);
    let mut tab_text_len = padded_text.width() + 2; // ground gap cells
    let tab_styled_text = style!(foreground_color, background_color)
        .bold()
        .paint(padded_text);

    let right_separator = gap.paint(" ");
    let tab_styled_text = if !focused_clients.is_empty() {
        let (cursor_section, extra_length) =
            cursors(focused_clients, palette.multiplayer_user_colors);
        tab_text_len += extra_length + 2; // 2 for cursor_beginning and cursor_end
        let mut s = String::new();
        let cursor_beginning = style!(foreground_color, background_color)
            .bold()
            .paint("[")
            .to_string();
        let cursor_section = AnsiStrings(&cursor_section).to_string();
        let cursor_end = style!(foreground_color, background_color)
            .bold()
            .paint("]")
            .to_string();
        s.push_str(&left_separator.to_string());
        s.push_str(&tab_styled_text.to_string());
        s.push_str(&cursor_beginning);
        s.push_str(&cursor_section);
        s.push_str(&cursor_end);
        s.push_str(&right_separator.to_string());
        s
    } else {
        AnsiStrings(&[left_separator, tab_styled_text, right_separator]).to_string()
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
    // The alternating shade is rhythm and no longer depends on host font
    // capabilities — the ground seams separate chips everywhere.
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
