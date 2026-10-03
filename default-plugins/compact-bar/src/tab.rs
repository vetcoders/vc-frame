use std::collections::BTreeSet;

use crate::LinePart;
use crate::line::truncate_display_width;
use ansi_term::{AnsiString, AnsiStrings};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Soft max for a single tab label before the chip wastes Z2 budget.
/// Overflow across many tabs is still handled by the `+N` compact badge.
const TAB_LABEL_MAX_COLS: usize = 16;

/// Fisheye tab markers: the focused tab carries ◉ (fisheye, alive center),
/// every inactive tab carries ○. The marker carries state together with the
/// chip contrast — shade alone is never the signal. Guest organ chips
/// (Overview / Agents / Shell) reuse this pair: the active organ is the
/// fisheye, never a second glyph.
const ACTIVE_TAB_MARKER: &str = "◉";
const INACTIVE_TAB_MARKER: &str = "○";

/// ASCII/box multiplication sign. Width 1. Not the emoji close mark.
pub const CLOSE_GLYPH: &str = "×";
/// separator + glyph + separator, on the right edge of a closable chip.
pub const CLOSE_ZONE_COLS: usize = 3;
/// First click arms a live tab. A later timer at this delay disarms it.
pub const CLOSE_ARM_TIMEOUT_SECS: f64 = 3.0;

/// Close affordance lives in session state. The tab object does not carry it.
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

/// Live work arms, then confirms. A dead tab closes on the first click.
/// A different tab (or the other surface) never confirms the previous arm.
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

/// Clipboard hints and close-arm timers share `Event::Timer`. Classify by
/// which configured delay the elapsed value is closer to. A tie stays with
/// the other timer so a slow clipboard hint cannot disarm a close.
pub fn timer_is_close_arm(elapsed: f64, arm_secs: f64, other_secs: f64) -> bool {
    (elapsed - arm_secs).abs() < (elapsed - other_secs).abs()
}

/// Layout-contract tabs have no close glyph. Guest organs come from
/// `GUEST_ORGAN_NAMES`. Host names are the frame layouts (`vibecrafted-host`
/// Home/Workspace and standalone `vibecrafted.kdl`), matched exactly.
pub fn tab_is_contractual(name: &str, guest_projection: bool) -> bool {
    if guest_projection {
        GUEST_ORGAN_NAMES.contains(&name)
    } else {
        name == VC_HOME_TAB_NAME
            || name == VC_SHARED_WORKSPACE_TAB_NAME
            || matches!(name, "Start here" | "Agents" | "Shell" | "Voc")
    }
}

/// Dead for one click: at least one terminal pane, and every terminal pane
/// has exited. Plugin-only chrome and a fresh tab with no terminal yet stay
/// on the two-phase path. Failure (`exit_status`) is a separate marker.
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
    has_failed_pane: bool,
    close: TabCloseAffordance,
) -> LinePart {
    let focused_clients = tab.other_focused_clients.as_slice();
    // The tab zone speaks the exact chip language of the bottom status-bar
    // (`color_elements()` in status-bar): selected = ribbon_selected base on
    // its background, unselected = ribbon_unselected base on its background,
    // alternate rows shift the background one step for countable rhythm —
    // everything bold. Chips are separated by bar ground, not by drawn rules.
    let background_color = if tab.active {
        palette.ribbon_selected.background
    } else if is_alternate_tab {
        palette.ribbon_unselected.emphasis_1
    } else {
        palette.ribbon_unselected.background
    };
    let foreground_color = if has_failed_pane || tab.is_flashing_bell {
        palette.ribbon_unselected.emphasis_3
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
    // The colored chip keeps its trailing space. The 3-cell close zone
    // replaces only the 1-column ground gap on the right, so a closable
    // chip is two columns wider than the same unclosable chip.
    let chip = format!(" {marker} {text} ");
    let chip_width = chip.width();
    let gap = style!(ground, ground);
    let (cursor_block, cursor_extra) = if focused_clients.is_empty() {
        (String::new(), 0)
    } else {
        let (cursor_section, extra_length) =
            cursors(focused_clients, palette.multiplayer_user_colors);
        let mut block = String::new();
        block.push_str(&text_style.paint("[").to_string());
        block.push_str(&AnsiStrings(&cursor_section).to_string());
        block.push_str(&text_style.paint("]").to_string());
        (block, extra_length)
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
    has_failed_pane: bool,
) -> LinePart {
    let _ = capabilities;
    tab_style_with_close(
        tabname,
        tab,
        is_alternate_tab,
        palette,
        has_failed_pane,
        TabCloseAffordance::default(),
        false,
    )
}

pub fn tab_style_with_close(
    mut tabname: String,
    tab: &TabInfo,
    is_alternate_tab: bool,
    palette: Styling,
    has_failed_pane: bool,
    mut close: TabCloseAffordance,
    guest_projection: bool,
) -> LinePart {
    // Contract wins over a caller that marked the tab closable. The check
    // uses the name before truncation and before FULLSCREEN / SYNC / ⚠.
    if tab_is_contractual(&tabname, guest_projection) {
        close.closable = false;
        close.close_id = None;
        close.armed = false;
        close.dead = false;
    }
    // Grapheme-safe soft truncate so long tab titles never explode Z2 width.
    tabname = truncate_display_width(&tabname, TAB_LABEL_MAX_COLS);
    if tab.is_fullscreen_active {
        tabname.push_str(" (FULLSCREEN)");
    } else if tab.is_sync_panes_active {
        tabname.push_str(" (SYNC)");
    }
    if tab.has_bell_notification || tab.is_flashing_bell {
        tabname.push_str(" [!]");
    }
    if has_failed_pane {
        tabname.push_str(" ⚠");
    }
    render_tab(
        tabname,
        tab,
        is_alternate_tab,
        palette,
        has_failed_pane,
        close,
    )
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

/// Left click closes only inside the 3-cell zone. The label never closes.
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

/// Middle click anywhere on a closable chip is the same two-phase machine.
/// Unclosable chips have no `close_id` and do nothing.
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
            dead: false,
            armed: false,
            close_id: Some(id),
        }
    }

    fn styled(name: &str, close: TabCloseAffordance, guest: bool) -> LinePart {
        tab_style_with_close(
            name.to_owned(),
            &TabInfo::default(),
            false,
            Styling::default(),
            false,
            close,
            guest,
        )
    }

    #[test]
    fn failed_pane_marker_is_visible_only_for_failed_tabs() {
        let tab = TabInfo::default();
        let warning = tab_style(
            "resume-codex".to_owned(),
            &tab,
            false,
            Styling::default(),
            PluginCapabilities::default(),
            true,
        );
        let healthy = tab_style(
            "resume-codex".to_owned(),
            &tab,
            false,
            Styling::default(),
            PluginCapabilities::default(),
            false,
        );

        assert!(warning.part.contains('⚠'));
        assert!(!healthy.part.contains('⚠'));
        assert!(!warning.part.contains('×'));
    }

    #[test]
    fn close_zone_is_three_cells_and_the_label_does_not_close() {
        let chip = styled("codex", closable(7), false);
        let start = chip.close_start.expect("closable chip publishes the zone");
        assert_eq!(chip.close_id, Some(7));
        assert_eq!(chip.len, start + CLOSE_ZONE_COLS);
        assert!(chip.part.contains('×'));

        let line = [chip];
        assert_eq!(close_hit(&line, 0), None, "left ground is not the glyph");
        assert_eq!(close_hit(&line, start.saturating_sub(1)), None);
        assert_eq!(close_hit(&line, start), Some(7));
        assert_eq!(close_hit(&line, start + 1), Some(7));
        assert_eq!(close_hit(&line, start + 2), Some(7));
        assert_eq!(close_hit(&line, start + CLOSE_ZONE_COLS), None);
    }

    #[test]
    fn contractual_names_hide_the_glyph_even_when_marked_closable() {
        for name in ["Home", "Workspace", "Start here", "Agents", "Shell", "Voc"] {
            let chip = styled(name, closable(1), false);
            assert!(
                !chip.part.contains('×'),
                "{name} is a host contract tab and must not draw ×"
            );
            assert_eq!(chip.close_start, None);
            assert_eq!(chip.close_id, None);
        }
        for name in ["Overview", "Agents", "Shell"] {
            let chip = styled(name, closable(1), true);
            assert!(
                !chip.part.contains('×'),
                "{name} is a guest organ and must not draw ×"
            );
        }
        let user = styled("agents", closable(4), true);
        assert!(user.part.contains('×'), "lowercase agents is not an organ");
        let overview_on_host = styled("Overview", closable(4), false);
        assert!(
            overview_on_host.part.contains('×'),
            "Overview is not a host layout contract"
        );
    }

    #[test]
    fn closable_chip_is_two_columns_wider_than_the_unclosable_chip() {
        let open = styled("codex", closable(3), false);
        let shut = styled("codex", TabCloseAffordance::default(), false);
        assert_eq!(open.len, shut.len + 2);
    }

    #[test]
    fn dead_and_armed_glyphs_change_only_the_close_cell() {
        let live = styled("codex", closable(3), false);
        let mut dead_close = closable(3);
        dead_close.dead = true;
        let dead = styled("codex", dead_close, false);
        let mut armed_close = closable(3);
        armed_close.armed = true;
        let armed = styled("codex", armed_close, false);
        assert!(live.part.contains('×') && dead.part.contains('×') && armed.part.contains('×'));
        assert_ne!(live.part, dead.part);
        assert_ne!(live.part, armed.part);
        assert_eq!(live.len, dead.len);
        assert_eq!(live.close_start, armed.close_start);
    }

    #[test]
    fn active_tab_close_zone_still_hits_when_focus_is_filtered() {
        let mut tab = TabInfo::default();
        tab.position = 2;
        tab.active = true;
        tab.tab_id = 9;
        let chip = tab_style_with_close(
            "codex".to_owned(),
            &tab,
            false,
            Styling::default(),
            false,
            closable(9),
            false,
        );
        let start = chip.close_start.expect("zone");
        let line = [chip];
        assert_eq!(close_hit(&line, start + 1), Some(9));
        assert_eq!(get_tab_to_focus(&line, 3, start + 1), None);
        assert_eq!(get_tab_to_focus(&line, 1, 1), Some(3));
    }

    #[test]
    fn middle_click_on_the_label_aliases_the_armed_machine() {
        let chip = styled("codex", closable(5), false);
        let start = chip.close_start.expect("zone");
        let line = [chip];
        assert_eq!(middle_close_hit(&line, 1), Some(5));
        assert_eq!(close_hit(&line, 1), None);
        assert_eq!(
            decide_close(None, 5, false, false),
            CloseDecision::Arm {
                tab_id: 5,
                guest: false
            }
        );
        assert_eq!(
            decide_close(Some((5, false)), 5, false, false),
            CloseDecision::Confirm {
                tab_id: 5,
                guest: false
            }
        );
        assert_eq!(middle_close_hit(&line, start), Some(5));
        let unclosable = styled("Home", closable(5), false);
        assert_eq!(middle_close_hit(&[unclosable], 1), None);
    }

    #[test]
    fn decide_close_does_not_confirm_across_tabs_or_surfaces() {
        assert_eq!(
            decide_close(Some((1, false)), 2, false, false),
            CloseDecision::Arm {
                tab_id: 2,
                guest: false
            }
        );
        assert_eq!(
            decide_close(Some((1, false)), 1, true, false),
            CloseDecision::Arm {
                tab_id: 1,
                guest: true
            }
        );
        assert_eq!(
            decide_close(Some((1, false)), 1, false, true),
            CloseDecision::CloseImmediately {
                tab_id: 1,
                guest: false
            }
        );
    }

    #[test]
    fn timer_classification_prefers_the_nearer_constant() {
        assert!(timer_is_close_arm(3.0, 3.0, 2.0));
        assert!(timer_is_close_arm(2.6, 3.0, 2.0));
        assert!(!timer_is_close_arm(2.0, 3.0, 2.0));
        assert!(!timer_is_close_arm(2.4, 3.0, 2.0));
        assert!(!timer_is_close_arm(2.5, 3.0, 2.0));
    }

    #[test]
    fn dead_positions_require_every_terminal_to_have_exited() {
        let mut manifest = PaneManifest::default();
        manifest.panes.insert(
            0,
            vec![PaneInfo {
                is_plugin: true,
                ..PaneInfo::default()
            }],
        );
        manifest.panes.insert(
            1,
            vec![PaneInfo {
                exited: false,
                ..PaneInfo::default()
            }],
        );
        manifest.panes.insert(
            2,
            vec![
                PaneInfo {
                    exited: true,
                    exit_status: Some(1),
                    ..PaneInfo::default()
                },
                PaneInfo {
                    is_plugin: true,
                    ..PaneInfo::default()
                },
            ],
        );
        manifest.panes.insert(
            3,
            vec![
                PaneInfo {
                    exited: true,
                    ..PaneInfo::default()
                },
                PaneInfo {
                    exited: false,
                    ..PaneInfo::default()
                },
            ],
        );
        let dead = dead_tab_positions(&manifest);
        assert!(!dead.contains(&0), "plugin-only chrome is not a dead session");
        assert!(!dead.contains(&1), "a live terminal stays two-phase");
        assert!(dead.contains(&2), "an exited terminal is one-click");
        assert!(!dead.contains(&3), "one live pane keeps the tab two-phase");
    }
}
