use ansi_term::AnsiStrings;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{ARROW_SEPARATOR, LinePart, TabRenderData};
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Fixed character-column budgets for the compact-bar — Vibecrafted Column
/// Guard Contract (`v0.47.2-contract`). Every zone is either fixed-width or a
/// single controlled flex (tabs + spacer). Mode switches, tab open/close, and
/// metric updates must never shift Z0/Z1/Z3 by even one cell.
///
/// Grid (Row 0 chrome), anchored to the Sessions-rail partition datum `⎮`:
/// ```text
///   [left_inset][Z0 brand 14][gap 4][⎮][gap 1][Z1 mode 5][Z2 tabs flex][Z3 toolbar 48]
/// ```
/// With the default operator layout (`left_inset=6`, rail `size=24`):
/// brand ends at col 20, 4-col gap, datum at col 24 (= rail width), mode at 26.
/// Growing Z3 by [`VOC_CHIP_COLS`] steals only from the Z2 flex; Z0/Z1/datum
/// must not shift by one cell.
pub const BRAND_ZONE_COLS: usize = 14;
/// Columns between brand right edge and the datum partition line.
pub const BRAND_DATUM_GAP_COLS: usize = 4;
/// Canonical partition glyph between Sessions rail and workspace (1 col).
pub const DATUM_PARTITION: &str = "⎮";
pub const DATUM_PARTITION_COLS: usize = 1;
/// Columns between datum and the mode chip.
pub const MODE_LEAD_GAP_COLS: usize = 1;
/// Mode chip body — always exactly 5 display columns (fully inverted chip).
pub const MODE_ZONE_COLS: usize = 5;
/// Fixed prefix after brand: gap + datum + lead + mode.
pub const AFTER_BRAND_FIXED_COLS: usize =
    BRAND_DATUM_GAP_COLS + DATUM_PARTITION_COLS + MODE_LEAD_GAP_COLS + MODE_ZONE_COLS;
/// `✍ Composer` padded to 11 grid cells (Z3 left half). Its shortcut lives
/// permanently in the bottom status bar, never in the clickable chrome.
pub const COMPOSER_CHIP_COLS: usize = 11;
/// Leading seam + `❯_ Quick cmd` padded to 16 grid cells.
/// Its shortcut lives permanently in the bottom status bar as well.
pub const QUICK_CMD_CHIP_COLS: usize = 16;
/// Counted Panels chip left of Quick cmd (` · Panels 12` / ` · Panels 99+`).
pub const PANELS_CHIP_COLS: usize = 13;
/// Theme state/action glyph (`☾` dark, `☼` light) with a leading seam and a
/// one-cell trailing inset, so borderless windows never pin it to the edge.
pub const THEME_CHIP_COLS: usize = 3;
/// Fixed-width Voc host-console chip immediately left of Composer (` Voc `
/// with a one-cell seam on each side). Mode switches must not change this
/// budget. Founder 2026-09-19: the chip text is `Voc`, never `voc`.
pub const VOC_CHIP_COLS: usize = 5;
/// Protected right toolbar total — Voc + Composer + Panels + Quick cmd + theme.
/// Raised from 43 to 48 so the Voc chip sits in Z3 without shifting Z0/Z1
/// or the datum `⎮` at column 24.
pub const ENTRY_ZONE_COLS: usize =
    COMPOSER_CHIP_COLS + PANELS_CHIP_COLS + QUICK_CMD_CHIP_COLS + THEME_CHIP_COLS + VOC_CHIP_COLS;

const _: () = assert!(
    ENTRY_ZONE_COLS
        == COMPOSER_CHIP_COLS
            + PANELS_CHIP_COLS
            + QUICK_CMD_CHIP_COLS
            + THEME_CHIP_COLS
            + VOC_CHIP_COLS
);

/// Canonical guest organ names (D2: exact tab-name convention). C6 enforces
/// these names in the guest template. Missing organs are absent — never invented.
pub const GUEST_ORGAN_NAMES: [&str; 3] = ["Overview", "Agents", "Shell"];

/// Reorder guest tabs so organs render first in canonical order
/// (`Overview`, `Agents`, `Shell`), then every remaining tab unchanged.
/// Comparison is exact and case-sensitive: `agents` is not an organ.
pub fn project_guest_organs(tabs: &[TabInfo]) -> Vec<TabInfo> {
    let mut claimed = vec![false; tabs.len()];
    let mut projected = Vec::with_capacity(tabs.len());
    for organ in GUEST_ORGAN_NAMES {
        if let Some(index) = tabs.iter().position(|tab| tab.name == organ)
            && !claimed[index]
        {
            claimed[index] = true;
            projected.push(tabs[index].clone());
        }
    }
    for (index, tab) in tabs.iter().enumerate() {
        if !claimed[index] {
            projected.push(tab.clone());
        }
    }
    projected
}

pub fn tab_line(
    mode_info: &ModeInfo,
    tab_data: TabRenderData,
    cols: usize,
    config: TabLineConfig,
) -> Vec<LinePart> {
    let builder = TabLineBuilder::new(config, mode_info.style.colors, mode_info.capabilities, cols);
    builder.build(tab_data.tabs, tab_data.active_tab_index)
}

#[derive(Debug, Clone)]
pub struct TabLineConfig {
    pub mode: InputMode,
    pub toggle_tooltip_key: Option<String>,
    pub tooltip_is_active: bool,
    pub brand_text: Option<String>,
    pub brand_text_short: Option<String>,
    pub left_inset: usize,
    pub theme_indicator: String,
    pub pane_count: usize,
    pub panels_pager: Option<(usize, usize)>,
}

fn calculate_total_length(parts: &[LinePart]) -> usize {
    parts.iter().map(|p| p.len).sum()
}

/// Display width of a string as the sum of grapheme cluster widths (wcwidth).
pub fn display_width(text: &str) -> usize {
    text.graphemes(true).map(grapheme_width).sum()
}

fn grapheme_width(g: &str) -> usize {
    // unicode-width: most symbols 0/1/2. Zero-width joiners contribute 0.
    UnicodeWidthStr::width(g)
}

/// Pad or hard-trim `text` so its display width equals `cols` exactly.
/// Slices on grapheme-cluster boundaries; wide EAW glyphs count as 2;
/// padding uses ASCII spaces (width 1).
pub fn pad_to_cols(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for g in text.graphemes(true) {
        let g_w = grapheme_width(g);
        if width + g_w > cols {
            break;
        }
        out.push_str(g);
        width += g_w;
    }
    while width < cols {
        out.push(' ');
        width += 1;
    }
    out
}

/// Right-align `text` into exactly `cols` display columns (left-pad spaces).
/// Truncates on grapheme boundaries when over-budget.
pub fn right_align_to_cols(text: &str, cols: usize) -> String {
    let trimmed = text.trim_end();
    let mut kept = String::new();
    let mut width = 0usize;
    for g in trimmed.graphemes(true) {
        let g_w = grapheme_width(g);
        if width + g_w > cols {
            break;
        }
        kept.push_str(g);
        width += g_w;
    }
    let pad = cols.saturating_sub(width);
    format!("{}{}", " ".repeat(pad), kept)
}

/// Truncate `text` to at most `max_cols` display columns on grapheme
/// boundaries. When truncation is required, the last column is the ellipsis
/// `…` (1 display col) — never mid-grapheme or mid-UTF-8.
pub fn truncate_display_width(text: &str, max_cols: usize) -> String {
    if max_cols == 0 {
        return String::new();
    }
    let full_w: usize = text.graphemes(true).map(grapheme_width).sum();
    if full_w <= max_cols {
        return text.to_owned();
    }
    let mut current_width = 0usize;
    let mut result = String::new();
    for grapheme in text.graphemes(true) {
        let g_width = grapheme_width(grapheme);
        // Reserve 1 col for the ellipsis when this grapheme would not fit.
        if current_width + g_width + 1 > max_cols {
            result.push('…');
            break;
        }
        result.push_str(grapheme);
        current_width += g_width;
    }
    if result.is_empty() && max_cols > 0 {
        result.push('…');
    }
    result
}

/// Mode zone text — always exactly [`MODE_ZONE_COLS`] display columns.
/// Column Guard: mode starts one col right of the datum (lead gap is a
/// separate LinePart). Body is glyph + short code, space-padded — no trailing
/// frame bar (the datum `⎮` is the partition, not a chip separator).
pub fn format_mode_zone(mode: InputMode) -> String {
    let (glyph, code) = mode_chip(mode);
    let body = format!(" {} {}", glyph, code);
    pad_to_cols(&body, MODE_ZONE_COLS)
}

/// Brand zone text — always exactly [`BRAND_ZONE_COLS`] display columns,
/// right-aligned so its right edge sits flush against the 4-col datum gap.
pub fn format_brand_zone(brand_text: Option<&str>, brand_text_short: Option<&str>) -> String {
    let selected = select_brand_text(brand_text, brand_text_short);
    right_align_to_cols(&selected, BRAND_ZONE_COLS)
}

fn select_brand_text(brand_text: Option<&str>, brand_text_short: Option<&str>) -> String {
    // Bare wordmark (12 cols under unicode-width); right_align pads to 14.
    let default_brand = "𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍.".to_owned();
    match (brand_text, brand_text_short) {
        (Some(long_brand), Some(short_brand))
            if long_brand.width() <= BRAND_ZONE_COLS
                && long_brand.width() >= short_brand.width() =>
        {
            long_brand.to_owned()
        },
        (Some(_long_brand), Some(short_brand)) if short_brand.width() <= BRAND_ZONE_COLS => {
            short_brand.to_owned()
        },
        (Some(long_brand), _) if long_brand.width() <= BRAND_ZONE_COLS => long_brand.to_owned(),
        (Some(long_brand), _) => long_brand.to_owned(), // right_align/truncate will fit
        _ => default_brand,
    }
}

struct TabLinePopulator {
    cols: usize,
    palette: Styling,
    capabilities: PluginCapabilities,
}

impl TabLinePopulator {
    fn new(cols: usize, palette: Styling, capabilities: PluginCapabilities) -> Self {
        Self {
            cols,
            palette,
            capabilities,
        }
    }

    fn populate_tabs(
        &self,
        tabs_before_active: &mut Vec<LinePart>,
        tabs_after_active: &mut Vec<LinePart>,
        tabs_to_render: &mut Vec<LinePart>,
    ) {
        let mut middle_size = calculate_total_length(tabs_to_render);
        let mut total_left = 0;
        let mut total_right = 0;

        loop {
            let left_count = tabs_before_active.len();
            let right_count = tabs_after_active.len();

            let collapsed_indicators =
                self.create_collapsed_indicators(tabs_before_active, tabs_after_active);

            let total_size =
                collapsed_indicators.left.len + middle_size + collapsed_indicators.right.len;

            if total_size > self.cols {
                break;
            }

            let tab_sizes = TabSizes {
                left: tabs_before_active.last().map_or(usize::MAX, |tab| tab.len),
                right: tabs_after_active.first().map_or(usize::MAX, |tab| tab.len),
            };

            let fit_analysis = self.analyze_tab_fit(
                &tab_sizes,
                total_size,
                left_count,
                right_count,
                &collapsed_indicators,
            );

            match self.decide_next_action(&fit_analysis, total_left, total_right) {
                TabAction::AddLeft => {
                    if let Some(tab) = tabs_before_active.pop() {
                        middle_size += tab.len;
                        total_left += tab.len;
                        tabs_to_render.insert(0, tab);
                    }
                },
                TabAction::AddRight => {
                    if !tabs_after_active.is_empty() {
                        let tab = tabs_after_active.remove(0);
                        middle_size += tab.len;
                        total_right += tab.len;
                        tabs_to_render.push(tab);
                    }
                },
                TabAction::Finish => {
                    tabs_to_render.insert(0, collapsed_indicators.left);
                    tabs_to_render.push(collapsed_indicators.right);
                    break;
                },
            }
        }
    }

    fn create_collapsed_indicators(
        &self,
        tabs_before_active: &[LinePart],
        tabs_after_active: &[LinePart],
    ) -> CollapsedIndicators {
        // Overflow identity: a `+N` badge must point at the HIDDEN tab's own
        // LinePart.tab_index (last hidden before / first hidden after the
        // rendered window). The organ projection reorders tabs, so a position
        // in the reordered list is not an identity — clicking `+N` through
        // get_tab_to_focus must select the actual hidden tab.
        let left = tabs_before_active
            .last()
            .and_then(|tab| tab.tab_index.map(|index| (tabs_before_active.len(), index)));
        let right = tabs_after_active
            .first()
            .and_then(|tab| tab.tab_index.map(|index| (tabs_after_active.len(), index)));

        CollapsedIndicators {
            left: left.map_or_else(LinePart::default, |(count, index)| {
                self.create_left_indicator(count, index)
            }),
            right: right.map_or_else(LinePart::default, |(count, index)| {
                self.create_right_indicator(count, index)
            }),
        }
    }

    fn analyze_tab_fit(
        &self,
        tab_sizes: &TabSizes,
        total_size: usize,
        left_count: usize,
        right_count: usize,
        collapsed_indicators: &CollapsedIndicators,
    ) -> TabFitAnalysis {
        let size_by_adding_left =
            tab_sizes
                .left
                .saturating_add(total_size)
                .saturating_sub(if left_count == 1 {
                    collapsed_indicators.left.len
                } else {
                    0
                });

        let size_by_adding_right =
            tab_sizes
                .right
                .saturating_add(total_size)
                .saturating_sub(if right_count == 1 {
                    collapsed_indicators.right.len
                } else {
                    0
                });

        TabFitAnalysis {
            left_fits: size_by_adding_left <= self.cols,
            right_fits: size_by_adding_right <= self.cols,
        }
    }

    fn decide_next_action(
        &self,
        fit_analysis: &TabFitAnalysis,
        total_left: usize,
        total_right: usize,
    ) -> TabAction {
        if (total_left <= total_right || !fit_analysis.right_fits) && fit_analysis.left_fits {
            TabAction::AddLeft
        } else if fit_analysis.right_fits {
            TabAction::AddRight
        } else {
            TabAction::Finish
        }
    }

    fn create_left_indicator(&self, tab_count: usize, tab_index: usize) -> LinePart {
        if tab_count == 0 {
            return LinePart::default();
        }
        // Compact contract: `+N` badge (no arrows that eat Z2 width).
        let more_text = self.format_count_text(tab_count, "+{}", "+many");
        self.create_styled_indicator(more_text, tab_index)
    }

    fn create_right_indicator(&self, tab_count: usize, tab_index: usize) -> LinePart {
        if tab_count == 0 {
            return LinePart::default();
        }
        // Compact contract (`○ name+N`): overflow count as a tight `+N` badge.
        // Never `+N →` — arrows pushed Z3 off-screen in the legacy layout.
        let more_text = self.format_count_text(tab_count, "+{}", "+many");
        self.create_styled_indicator(more_text, tab_index)
    }

    fn format_count_text(&self, count: usize, format_str: &str, fallback: &str) -> String {
        if count < 10000 {
            format_str.replace("{}", &count.to_string())
        } else {
            fallback.to_string()
        }
    }

    fn create_styled_indicator(&self, text: String, tab_index: usize) -> LinePart {
        let separator = tab_separator(self.capabilities);
        let text_len = text.width() + 2 * separator.width();

        let colors = IndicatorColors {
            text: self.palette.ribbon_unselected.base,
            separator: self.palette.text_unselected.background,
            background: self.palette.text_selected.emphasis_0,
        };

        let styled_parts = [
            style!(colors.separator, colors.background).paint(separator),
            style!(colors.text, colors.background).bold().paint(text),
            style!(colors.background, colors.separator).paint(separator),
        ];

        LinePart {
            part: AnsiStrings(&styled_parts).to_string(),
            len: text_len,
            tab_index: Some(tab_index),
        }
    }
}

#[derive(Debug)]
struct CollapsedIndicators {
    left: LinePart,
    right: LinePart,
}

#[derive(Debug)]
struct TabSizes {
    left: usize,
    right: usize,
}

#[derive(Debug)]
struct TabFitAnalysis {
    left_fits: bool,
    right_fits: bool,
}

#[derive(Debug)]
struct IndicatorColors {
    text: PaletteColor,
    separator: PaletteColor,
    background: PaletteColor,
}

#[derive(Debug)]
enum TabAction {
    AddLeft,
    AddRight,
    Finish,
}

struct TabLinePrefixBuilder {
    palette: Styling,
    cols: usize,
}

impl TabLinePrefixBuilder {
    fn new(palette: Styling, cols: usize) -> Self {
        Self { palette, cols }
    }

    fn build(
        &self,
        mode: InputMode,
        brand_text: Option<&str>,
        brand_text_short: Option<&str>,
    ) -> Vec<LinePart> {
        // Column Guard order: Z0 brand · 4-col gap · datum `⎮` · 1-col lead · Z1 mode · Z2 tabs.
        // The session anchor lives in the rail header (`SESSIONS N · name`), not here.
        let mut parts = vec![self.create_brand_part(brand_text, brand_text_short)];
        parts.push(self.create_gap_part(BRAND_DATUM_GAP_COLS));
        parts.push(self.create_datum_part());
        parts.push(self.create_gap_part(MODE_LEAD_GAP_COLS));
        let used_len = calculate_total_length(&parts);
        if let Some(mode_part) = self.create_mode_part(mode, used_len) {
            parts.push(mode_part);
        }
        // Brand is always present as parts[0]; fixed suffix is gap+datum+lead[+mode].
        let brand_len = parts.first().map_or(0, |p| p.len);
        let after_brand = calculate_total_length(&parts).saturating_sub(brand_len);
        debug_assert!(
            after_brand == AFTER_BRAND_FIXED_COLS
                || after_brand == AFTER_BRAND_FIXED_COLS.saturating_sub(MODE_ZONE_COLS),
            "after-brand fixed width drifted: {after_brand} (want {AFTER_BRAND_FIXED_COLS} or gap-only)"
        );
        parts
    }

    fn create_gap_part(&self, cols: usize) -> LinePart {
        if cols == 0 {
            return LinePart::default();
        }
        let colors = self.get_text_colors();
        LinePart {
            part: style!(colors.text, colors.background)
                .paint(" ".repeat(cols))
                .to_string(),
            len: cols,
            tab_index: None,
        }
    }

    fn create_datum_part(&self) -> LinePart {
        let colors = self.get_text_colors();
        // Partition mark — aligns with the Sessions-rail / workspace split.
        // Painted in base ink so the datum is visible against the bar ground.
        LinePart {
            part: style!(colors.text, colors.background)
                .paint(DATUM_PARTITION)
                .to_string(),
            len: DATUM_PARTITION_COLS,
            tab_index: None,
        }
    }

    fn create_brand_part(
        &self,
        brand_text: Option<&str>,
        brand_text_short: Option<&str>,
    ) -> LinePart {
        // Fixed BRAND_ZONE_COLS — brand text never moves the mode chip.
        let prefix_text = format_brand_zone(brand_text, brand_text_short);
        // The brand sits bare on the bar ground (operator call 2026-07-30):
        // the inverted chip belongs to the MODE, not the wordmark.
        let colors = self.get_text_colors();

        LinePart {
            part: style!(colors.text, colors.background)
                .bold()
                .paint(prefix_text.clone())
                .to_string(),
            len: BRAND_ZONE_COLS,
            tab_index: None,
        }
    }

    /// Mode chip: glyph + short code, always exactly [`MODE_ZONE_COLS`]
    /// columns so mode switches never shift tabs or entry chips.
    fn create_mode_part(&self, mode: InputMode, used_len: usize) -> Option<LinePart> {
        // The mode chip carries the bar's inversion (operator call
        // 2026-07-30): always inverse video, with the ground telling the
        // state apart — neutral base for Normal, the emphasis_1 accent for
        // Locked, the ribbon accent for every armed mode.
        let style = match mode {
            InputMode::Locked => style!(
                self.palette.text_unselected.background,
                self.palette.text_unselected.emphasis_1
            ),
            InputMode::Normal => style!(
                self.palette.text_unselected.background,
                self.palette.text_unselected.base
            ),
            _ => style!(
                self.palette.ribbon_selected.base,
                self.palette.ribbon_selected.background
            ),
        };
        let mode_text = format_mode_zone(mode);
        let mode_len = MODE_ZONE_COLS;

        if self.cols.saturating_sub(used_len) >= mode_len {
            Some(LinePart {
                part: style.bold().paint(mode_text).to_string(),
                len: mode_len,
                tab_index: None,
            })
        } else {
            None
        }
    }

    fn get_text_colors(&self) -> IndicatorColors {
        IndicatorColors {
            text: self.palette.text_unselected.base,
            background: self.palette.text_unselected.background,
            separator: self.palette.text_unselected.background,
        }
    }
}

struct RightSideElementsBuilder {
    palette: Styling,
    theme_indicator: String,
    pane_count: usize,
    panels_pager: Option<(usize, usize)>,
}

impl RightSideElementsBuilder {
    fn new(
        palette: Styling,
        theme_indicator: String,
        pane_count: usize,
        panels_pager: Option<(usize, usize)>,
    ) -> Self {
        Self {
            palette,
            theme_indicator,
            pane_count,
            panels_pager,
        }
    }

    /// Protected Z3 — Voc + Composer + Panels + Quick cmd + terminal theme.
    fn build_protected_zone(&self) -> Vec<LinePart> {
        let elements = vec![
            self.create_voc_chip(),
            self.create_composer_chip(),
            self.create_panels_chip(),
            self.create_quick_cmd_chip(),
            self.create_theme_chip(),
        ];
        debug_assert_eq!(
            elements.iter().map(|element| element.len).sum::<usize>(),
            ENTRY_ZONE_COLS,
            "voc+composer+panels+quick+theme entry zone must be exactly {ENTRY_ZONE_COLS} cols"
        );
        elements
    }

    fn create_panels_chip(&self) -> LinePart {
        let raw_label = if let Some((index, total)) = self.panels_pager {
            let pager = format!("{index}/{total}");
            if 7 + pager.len() <= 10 {
                format!("Panels {pager}")
            } else if 4 + pager.len() <= 10 {
                format!("Pnl {pager}")
            } else {
                pager
            }
        } else {
            let count = if self.pane_count > 99 {
                "99+".to_owned()
            } else {
                format!("{:>2}", self.pane_count)
            };
            format!("Panels {count}")
        };
        let plain = pad_to_cols(&format!(" · {raw_label}"), PANELS_CHIP_COLS);
        let seam = " · ";
        let label = pad_to_cols(&raw_label, 10);
        let pad_tail = " ".repeat(
            display_width(&plain).saturating_sub(display_width(seam) + display_width(&label)),
        );
        let styled_parts = [
            style!(
                self.palette.text_unselected.emphasis_2,
                self.palette.text_unselected.background
            )
            .paint(seam),
            style!(
                self.palette.text_unselected.base,
                self.palette.text_unselected.background
            )
            .bold()
            .paint(label),
            style!(
                self.palette.text_unselected.base,
                self.palette.text_unselected.background
            )
            .paint(pad_tail),
        ];

        LinePart {
            part: AnsiStrings(&styled_parts).to_string(),
            len: PANELS_CHIP_COLS,
            tab_index: Some(crate::PANELS_CLICK_SENTINEL),
        }
    }

    fn create_theme_chip(&self) -> LinePart {
        let text = pad_to_cols(&format!(" {}", self.theme_indicator), THEME_CHIP_COLS);
        let styled = style!(
            self.palette.text_unselected.emphasis_2,
            self.palette.text_unselected.background
        )
        .bold()
        .paint(text);

        LinePart {
            part: styled.to_string(),
            len: THEME_CHIP_COLS,
            tab_index: Some(crate::THEME_CLICK_SENTINEL),
        }
    }

    /// The Quick cmd chip — floating dispatch shell to type into, not an
    /// agents dashboard (operator call 2026-07-31). Fixed
    /// [`QUICK_CMD_CHIP_COLS`] so the entry zone never breathes. LIVE pulse
    /// lives on the bottom status-bar.
    fn create_quick_cmd_chip(&self) -> LinePart {
        let plain = pad_to_cols(" · ❯_ Quick cmd", QUICK_CMD_CHIP_COLS);
        // Style the visible label; trailing pad spaces inherit the bar ground.
        let label = "❯_ Quick cmd";
        let seam = " · ";
        let pad_tail = " ".repeat(
            display_width(&plain).saturating_sub(display_width(seam) + display_width(label)),
        );
        let styled_parts = [
            style!(
                self.palette.text_unselected.emphasis_2,
                self.palette.text_unselected.background
            )
            .paint(seam),
            style!(
                self.palette.text_unselected.base,
                self.palette.text_unselected.background
            )
            .bold()
            .paint(label),
            style!(
                self.palette.text_unselected.base,
                self.palette.text_unselected.background
            )
            .paint(pad_tail),
        ];

        LinePart {
            part: AnsiStrings(&styled_parts).to_string(),
            len: QUICK_CMD_CHIP_COLS,
            tab_index: Some(crate::AGENTS_CLICK_SENTINEL),
        }
    }

    /// Always-visible Voc host-console chip, clickable via the sentinel
    /// tab_index. Fixed [`VOC_CHIP_COLS`]. Click is a plugin-log receipt
    /// only until C5 opens the unsinkable host pane. Text is `Voc`.
    fn create_voc_chip(&self) -> LinePart {
        let text = pad_to_cols(" Voc ", VOC_CHIP_COLS);
        let styled = style!(
            self.palette.text_unselected.base,
            self.palette.text_unselected.background
        )
        .bold()
        .paint(text);

        LinePart {
            part: styled.to_string(),
            len: VOC_CHIP_COLS,
            tab_index: Some(crate::VOC_CLICK_SENTINEL),
        }
    }

    /// Always-visible Composer entry point, clickable via the sentinel
    /// tab_index. Fixed [`COMPOSER_CHIP_COLS`]. ✍ (text-presentation) says
    /// "drafting" — the persistent bottom status bar teaches Cmd+E.
    fn create_composer_chip(&self) -> LinePart {
        let text = pad_to_cols("✍ Composer", COMPOSER_CHIP_COLS);
        let styled = style!(
            self.palette.text_unselected.base,
            self.palette.text_unselected.background
        )
        .bold()
        .paint(text);

        LinePart {
            part: styled.to_string(),
            len: COMPOSER_CHIP_COLS,
            tab_index: Some(crate::COMPOSER_CLICK_SENTINEL),
        }
    }

    fn create_tooltip_indicator(&self, toggle_key: &str, is_active: bool) -> LinePart {
        let key_text = toggle_key;
        let key = Text::new(key_text).color_all(3).opaque();
        let ribbon_text = "Tooltip";
        let mut ribbon = Text::new(ribbon_text);

        if is_active {
            ribbon = ribbon.selected();
        }

        LinePart {
            part: format!("{} {}", serialize_text(&key), serialize_ribbon(&ribbon)),
            len: key_text.chars().count() + ribbon_text.chars().count() + 6,
            tab_index: None,
        }
    }
}

pub struct TabLineBuilder {
    config: TabLineConfig,
    palette: Styling,
    capabilities: PluginCapabilities,
    cols: usize,
}

impl TabLineBuilder {
    pub fn new(
        config: TabLineConfig,
        palette: Styling,
        capabilities: PluginCapabilities,
        cols: usize,
    ) -> Self {
        Self {
            config,
            palette,
            capabilities,
            cols,
        }
    }

    pub fn build(self, all_tabs: Vec<LinePart>, active_tab_index: usize) -> Vec<LinePart> {
        let (tabs_before_active, active_tab, tabs_after_active) =
            self.split_tabs(all_tabs, active_tab_index);

        let prefix_builder = TabLinePrefixBuilder::new(self.palette, self.cols);
        let mut prefix = prefix_builder.build(
            self.config.mode,
            self.config.brand_text.as_deref(),
            self.config.brand_text_short.as_deref(),
        );
        // The 🚥 zone: blank columns before the brand so the bar clears the
        // macOS traffic lights in the native transparent window. A LinePart
        // with tab_index None keeps the click map honest — both the tab and
        // sentinel resolvers walk cumulative lens.
        //
        // With rail size=24 and left_inset=6: brand (14) ends at col 20, the
        // 4-col gap + datum land on the Sessions/workspace partition (col 24).
        let left_inset = self.config.left_inset.min(self.cols / 2);
        if left_inset > 0 {
            let colors = self.palette.text_unselected;
            prefix.insert(
                0,
                LinePart {
                    part: style!(colors.base, colors.background)
                        .paint(" ".repeat(left_inset))
                        .to_string(),
                    len: left_inset,
                    tab_index: None,
                },
            );
        }
        let prefix_len = calculate_total_length(&prefix);

        // Protected Right Action Zone (Z3): always reserve ENTRY_ZONE_COLS so
        // Voc + Composer + Panels + Quick cmd never shift or fall off when tabs overflow.
        let reserved_right = ENTRY_ZONE_COLS.min(self.cols.saturating_sub(prefix_len));
        let tabs_budget = self
            .cols
            .saturating_sub(prefix_len)
            .saturating_sub(reserved_right);

        if active_tab.len > tabs_budget {
            // Even the active tab alone is too wide — still pin Z3.
            self.add_right_side_elements(&mut prefix);
            return prefix;
        }

        let mut tabs_to_render = vec![active_tab];
        let populator = TabLinePopulator::new(tabs_budget, self.palette, self.capabilities);

        let mut tabs_before = tabs_before_active;
        let mut tabs_after = tabs_after_active;
        populator.populate_tabs(&mut tabs_before, &mut tabs_after, &mut tabs_to_render);

        prefix.append(&mut tabs_to_render);

        self.add_right_side_elements(&mut prefix);
        prefix
    }

    fn split_tabs(
        &self,
        mut all_tabs: Vec<LinePart>,
        active_tab_index: usize,
    ) -> (Vec<LinePart>, LinePart, Vec<LinePart>) {
        let mut tabs_after_active = all_tabs.split_off(active_tab_index);
        let mut tabs_before_active = all_tabs;

        let active_tab = if !tabs_after_active.is_empty() {
            tabs_after_active.remove(0)
        } else {
            tabs_before_active.pop().unwrap_or_default()
        };

        (tabs_before_active, active_tab, tabs_after_active)
    }

    fn add_right_side_elements(&self, prefix: &mut Vec<LinePart>) {
        // Right Guard: Z3 (Voc + Composer + Panels + Quick cmd) is always placed.
        // Optional tooltip may follow only when free columns remain after Z3.
        let right_builder = RightSideElementsBuilder::new(
            self.palette,
            self.config.theme_indicator.clone(),
            self.config.pane_count,
            self.config.panels_pager,
        );
        let mut right_elements = right_builder.build_protected_zone();
        let z3_len = calculate_total_length(&right_elements);
        debug_assert_eq!(z3_len, ENTRY_ZONE_COLS);

        let current_len = calculate_total_length(prefix);
        let available = self.cols.saturating_sub(current_len);
        // Narrow-width contract: when the bar cannot hold prefix + full Z3,
        // chips shed in reverse criticality — theme, Quick cmd, Panels,
        // Composer — and the Voc host-console chip stays longest. The emitted
        // line NEVER exceeds cols; a clipped reservation that still appends
        // the full toolbar is a lie (regression range was 74–78 cols).
        while calculate_total_length(&right_elements) > available && !right_elements.is_empty() {
            right_elements.pop();
        }
        let z3_len = calculate_total_length(&right_elements);

        let remaining_space = self.cols.saturating_sub(current_len).saturating_sub(z3_len);
        if remaining_space > 0 && z3_len > 0 {
            prefix.push(self.create_spacer(remaining_space));
        }
        prefix.append(&mut right_elements);

        // Tooltip is optional chrome — never steals columns from Z3.
        if let Some(ref tooltip_key) = self.config.toggle_tooltip_key {
            let tip =
                right_builder.create_tooltip_indicator(tooltip_key, self.config.tooltip_is_active);
            let after_z3 = calculate_total_length(prefix);
            if after_z3 + tip.len <= self.cols {
                prefix.push(tip);
            }
        }
    }

    fn create_spacer(&self, space: usize) -> LinePart {
        let bg = self.palette.text_unselected.background;
        let buffer = (0..space)
            .map(|_| style!(bg, bg).paint(" ").to_string())
            .collect::<String>();

        LinePart {
            part: buffer,
            len: space,
            tab_index: None,
        }
    }
}

pub fn tab_separator(capabilities: PluginCapabilities) -> &'static str {
    if !capabilities.arrow_fonts {
        ARROW_SEPARATOR
    } else {
        ""
    }
}

/// The operator-tuned mode chip set (glyph, short code) — one visual language
/// for all fourteen input modes. Width is enforced by [`format_mode_zone`].
/// Rename codes are RNT/RNP; Prompt uses `⟩` so it never collides with the
/// Quick cmd prompt glyph `❯_`.
pub fn mode_chip(mode: InputMode) -> (&'static str, &'static str) {
    match mode {
        InputMode::Normal => ("▷", "N"),
        InputMode::Locked => ("⚿", "L"),
        InputMode::Pane => ("◫", "P"),
        InputMode::Tab => ("𝌁", "T"),
        InputMode::Resize => ("⤢", "R"),
        InputMode::Move => ("⟷", "M"),
        InputMode::Scroll => ("⇅", "S"),
        InputMode::Search => ("⌕", "F"),
        InputMode::EnterSearch => ("↵", "F"),
        InputMode::RenameTab => ("✎", "t"),
        InputMode::RenamePane => ("✎", "p"),
        InputMode::Session => ("𝌆", "S"),
        InputMode::Prompt => ("⟩", "P"),
        InputMode::Tmux => ("ⓣ", "T"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_MODES: [InputMode; 14] = [
        InputMode::Normal,
        InputMode::Locked,
        InputMode::Pane,
        InputMode::Tab,
        InputMode::Resize,
        InputMode::Move,
        InputMode::Scroll,
        InputMode::Search,
        InputMode::EnterSearch,
        InputMode::RenameTab,
        InputMode::RenamePane,
        InputMode::Session,
        InputMode::Prompt,
        InputMode::Tmux,
    ];

    #[test]
    fn mode_zone_is_fixed_width_for_every_mode() {
        let widths: Vec<usize> = ALL_MODES
            .iter()
            .map(|m| display_width(&format_mode_zone(*m)))
            .collect();
        for (mode, w) in ALL_MODES.iter().zip(widths.iter()) {
            assert_eq!(
                *w, MODE_ZONE_COLS,
                "mode {:?} zone width {} != {}",
                mode, w, MODE_ZONE_COLS
            );
        }
        // Stronger contract: every mode produces the same width (zero jitter).
        assert!(widths.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn mode_zone_has_leading_space_and_no_bar() {
        // Mode chip starts with a space and contains no leading vertical line.
        for mode in ALL_MODES {
            let zone = format_mode_zone(mode);
            assert!(
                zone.starts_with(' '),
                "mode {:?} must start with space (got {:?})",
                mode,
                zone
            );
            assert!(
                !zone.contains('│'),
                "mode {:?} must not contain vertical bar (got {:?})",
                mode,
                zone
            );
        }
        // Single-letter rename codes fit the exact 5-col budget.
        assert!(format_mode_zone(InputMode::RenameTab).contains("t"));
        assert!(format_mode_zone(InputMode::RenamePane).contains("p"));
    }

    #[test]
    fn brand_zone_is_fixed_width_and_right_aligned() {
        assert_eq!(
            display_width(&format_brand_zone(None, None)),
            BRAND_ZONE_COLS
        );
        assert_eq!(
            display_width(&format_brand_zone(Some("𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍."), None)),
            BRAND_ZONE_COLS
        );
        assert_eq!(
            display_width(&format_brand_zone(Some("SHORT"), Some("S"))),
            BRAND_ZONE_COLS
        );
        // Over-long brand is hard-trimmed, never expands the zone.
        assert_eq!(
            display_width(&format_brand_zone(Some("XXXXXXXXXXXXXXXXXXXX"), None)),
            BRAND_ZONE_COLS
        );
        // Right-aligned: short brand ends at the zone edge (leading spaces).
        let short = format_brand_zone(Some("VC"), None);
        assert!(short.ends_with("VC"), "got {:?}", short);
        assert!(
            short.starts_with(' '),
            "short brand must left-pad: {:?}",
            short
        );
    }

    #[test]
    fn entry_chips_sum_to_protected_z3_43() {
        // Historical name freezes the sum identity; the budget is now 48.
        assert_eq!(ENTRY_ZONE_COLS, 48);
        assert_eq!(
            COMPOSER_CHIP_COLS
                + PANELS_CHIP_COLS
                + QUICK_CMD_CHIP_COLS
                + THEME_CHIP_COLS
                + VOC_CHIP_COLS,
            ENTRY_ZONE_COLS
        );
        assert_eq!(VOC_CHIP_COLS, 5);
        assert_eq!(
            display_width(&pad_to_cols(" Voc ", VOC_CHIP_COLS)),
            VOC_CHIP_COLS
        );
        assert_eq!(
            display_width(&pad_to_cols("✍ Composer", COMPOSER_CHIP_COLS)),
            COMPOSER_CHIP_COLS
        );
        assert_eq!(
            display_width(&pad_to_cols(" · Panels 12", PANELS_CHIP_COLS)),
            PANELS_CHIP_COLS
        );
        assert_eq!(
            display_width(&pad_to_cols(" · Panels 99+", PANELS_CHIP_COLS)),
            PANELS_CHIP_COLS
        );
        assert_eq!(
            display_width(&pad_to_cols(" · ❯_ Quick cmd", QUICK_CMD_CHIP_COLS)),
            QUICK_CMD_CHIP_COLS
        );
        assert_eq!(display_width(&pad_to_cols(" ☾", THEME_CHIP_COLS)), 3);
        assert_eq!(display_width(&pad_to_cols(" ☼", THEME_CHIP_COLS)), 3);
    }

    #[test]
    fn after_brand_fixed_cols_match_column_guard() {
        // gap(4) + datum(1) + lead(1) + mode(5) = 11
        assert_eq!(AFTER_BRAND_FIXED_COLS, 11);
        assert_eq!(DATUM_PARTITION.width(), DATUM_PARTITION_COLS);
        // left_inset=6 + brand 14 + gap 4 → datum `⎮` starts at column 24.
        assert_eq!(6 + BRAND_ZONE_COLS + BRAND_DATUM_GAP_COLS, 24);
    }

    #[test]
    fn pad_to_cols_handles_wide_eaw_glyphs() {
        // 𝌆 is EAW wide (2). Budget of 4 must absorb it without overshoot.
        let s = pad_to_cols("𝌆", 4);
        assert_eq!(display_width(&s), 4);
        let s2 = pad_to_cols(" 𝌆", 3);
        assert_eq!(display_width(&s2), 3);
    }

    #[test]
    fn truncate_display_width_uses_ellipsis_on_grapheme_boundary() {
        let t = truncate_display_width("HelloWorld", 6);
        assert_eq!(display_width(&t), 6);
        assert!(t.ends_with('…'), "got {:?}", t);
        // Already fits — no ellipsis.
        assert_eq!(truncate_display_width("Hi", 5), "Hi");
        // Empty budget.
        assert_eq!(truncate_display_width("Hi", 0), "");
    }

    #[test]
    fn reserved_z3_constant_matches_toolbar_budget() {
        // Spec: Protected Toolbar Fixed 48 cols (Voc chip added left of Composer).
        assert_eq!(ENTRY_ZONE_COLS, 48);
        assert_eq!(BRAND_ZONE_COLS, 14);
        // 5 since the mode chip was tightened from the original 8-col budget
        // (f5b8dff65); this freeze-test guards against accidental drift, so
        // it must track deliberate budget changes.
        assert_eq!(MODE_ZONE_COLS, 5);
        assert_eq!(BRAND_DATUM_GAP_COLS, 4);
        assert_eq!(MODE_LEAD_GAP_COLS, 1);
    }

    fn tab_named(name: &str, position: usize, active: bool) -> TabInfo {
        TabInfo {
            name: name.to_owned(),
            position,
            active,
            ..TabInfo::default()
        }
    }

    #[test]
    fn organs_render_in_canonical_order_and_keep_fisheye() {
        let tabs = vec![
            tab_named("Shell", 0, false),
            tab_named("Agents", 1, true),
            tab_named("Foo", 2, false),
            tab_named("agents", 3, false),
        ];
        let projected = project_guest_organs(&tabs);
        let names: Vec<&str> = projected.iter().map(|tab| tab.name.as_str()).collect();
        assert_eq!(names, ["Agents", "Shell", "Foo", "agents"]);
        assert!(
            !names.contains(&"Overview"),
            "missing organs must not be invented"
        );

        let rendered: Vec<LinePart> = projected
            .iter()
            .map(|tab| {
                crate::tab::tab_style(
                    tab.name.clone(),
                    tab,
                    false,
                    Styling::default(),
                    PluginCapabilities::default(),
                    false,
                )
            })
            .collect();

        assert!(
            rendered[0].part.contains("◉"),
            "active Agents organ must keep the fisheye: {}",
            rendered[0].part
        );
        assert!(rendered[0].part.contains("Agents"));
        assert_eq!(
            rendered[0].tab_index,
            Some(1),
            "organ click must map to the underlying guest tab position, not the organ index"
        );

        assert!(rendered[1].part.contains("○"));
        assert!(rendered[1].part.contains("Shell"));
        assert_eq!(rendered[1].tab_index, Some(0));

        assert!(rendered[2].part.contains("Foo"));
        assert_eq!(rendered[2].tab_index, Some(2));

        assert!(
            rendered[3].part.contains("agents"),
            "lowercase agents is not an organ and stays after: {}",
            rendered[3].part
        );
        assert!(!rendered[3].part.contains("◉"));
        assert_eq!(rendered[3].tab_index, Some(3));
    }

    #[test]
    fn voc_chip_width_is_constant_across_modes() {
        let modes = [
            InputMode::Normal,
            InputMode::Locked,
            InputMode::Pane,
            InputMode::Tab,
        ];
        let mut lens = Vec::new();
        for mode in modes {
            let builder = RightSideElementsBuilder::new(Styling::default(), "☾".to_owned(), 0, None);
            let chip = builder.create_voc_chip();
            assert_eq!(
                chip.len, VOC_CHIP_COLS,
                "Voc chip len must equal VOC_CHIP_COLS in {:?}",
                mode
            );
            assert!(
                chip.part.contains("Voc"),
                "chip text is Voc (Founder 2026-09-19), mode {:?}: {}",
                mode,
                chip.part
            );
            assert_eq!(chip.tab_index, Some(crate::VOC_CLICK_SENTINEL));
            lens.push(chip.len);

            let zone = builder.build_protected_zone();
            assert_eq!(zone.len(), 5, "protected zone has five chips in {:?}", mode);
            assert_eq!(zone[0].tab_index, Some(crate::VOC_CLICK_SENTINEL));
            assert_eq!(zone[1].tab_index, Some(crate::COMPOSER_CLICK_SENTINEL));
            assert_eq!(
                zone.iter().map(|element| element.len).sum::<usize>(),
                ENTRY_ZONE_COLS
            );
        }
        assert!(
            lens.windows(2).all(|pair| pair[0] == pair[1]),
            "Voc chip width must not jitter across InputMode"
        );
        assert_eq!(lens[0], VOC_CHIP_COLS);
    }

    #[test]
    fn panels_chip_shows_active_pager() {
        let builder_1 = RightSideElementsBuilder::new(
            Styling::default(),
            "☾".to_owned(),
            3,
            Some((1, 3)),
        );
        let chip_1 = builder_1.create_panels_chip();
        assert_eq!(chip_1.len, PANELS_CHIP_COLS);
        assert!(chip_1.part.contains("Panels 1/3"));

        let builder_3 = RightSideElementsBuilder::new(
            Styling::default(),
            "☾".to_owned(),
            3,
            Some((3, 3)),
        );
        let chip_3 = builder_3.create_panels_chip();
        assert_eq!(chip_3.len, PANELS_CHIP_COLS);
        assert!(chip_3.part.contains("Panels 3/3"));
    }

    fn bare_part(tab_index: usize, len: usize) -> LinePart {
        LinePart {
            part: "x".repeat(len),
            len,
            tab_index: Some(tab_index),
        }
    }

    fn test_config(mode: InputMode, left_inset: usize) -> TabLineConfig {
        TabLineConfig {
            mode,
            toggle_tooltip_key: None,
            tooltip_is_active: false,
            brand_text: None,
            brand_text_short: None,
            left_inset,
            theme_indicator: "☾".to_owned(),
            pane_count: 0,
            panels_pager: None,
        }
    }

    #[test]
    fn overflow_badges_carry_the_hidden_tabs_identity() {
        // Right overflow — projected organ order [Agents(pos 1, active),
        // Shell(pos 0), Foo(pos 2)]: the `+2` badge must target Shell's
        // underlying position 0 (first hidden after the window), never the
        // reordered-list offset 1 (that is the active Agents — a no-op).
        let populator =
            TabLinePopulator::new(12, Styling::default(), PluginCapabilities::default());
        let mut before: Vec<LinePart> = vec![];
        let mut after = vec![bare_part(0, 10), bare_part(2, 10)];
        let mut rendered = vec![bare_part(1, 8)];
        populator.populate_tabs(&mut before, &mut after, &mut rendered);
        let badge = rendered.last().expect("right overflow badge present");
        assert!(badge.part.contains("+2"), "two hidden tabs: {}", badge.part);
        assert_eq!(
            badge.tab_index,
            Some(0),
            "badge must carry the hidden tab's LinePart.tab_index"
        );
        // Through the real click route: the badge column selects Shell.
        let badge_start: usize = rendered
            .iter()
            .take(rendered.len() - 1)
            .map(|p| p.len)
            .sum();
        assert_eq!(
            crate::tab::get_tab_to_focus(&rendered, 2, badge_start),
            Some(1),
            "clicking +2 must resolve to tab position 0 + 1"
        );

        // Left overflow — active renders last; the badge targets the LAST
        // hidden tab before the window (nearest neighbour), not index 0 of
        // the hidden count.
        let populator =
            TabLinePopulator::new(16, Styling::default(), PluginCapabilities::default());
        let mut before = vec![bare_part(0, 10), bare_part(2, 10)];
        let mut after: Vec<LinePart> = vec![];
        let mut rendered = vec![bare_part(1, 8)];
        populator.populate_tabs(&mut before, &mut after, &mut rendered);
        let badge = rendered.first().expect("left overflow badge present");
        assert!(badge.part.contains("+2"), "two hidden tabs: {}", badge.part);
        assert_eq!(badge.tab_index, Some(2));
        assert_eq!(crate::tab::get_tab_to_focus(&rendered, 2, 0), Some(3));
    }

    #[test]
    fn narrow_width_bar_never_exceeds_cols_and_sheds_z3_in_reverse_criticality() {
        // Regression range was 74–78: the builder reserved a clipped Z3
        // budget but still appended all 48 toolbar columns (75 emitted 79).
        for cols in [50usize, 60, 70, 74, 75, 78, 79, 80, 100] {
            let data = TabRenderData {
                tabs: vec![bare_part(0, 10)],
                active_tab_index: 0,
            };
            let line = tab_line(
                &ModeInfo::default(),
                data,
                cols,
                test_config(InputMode::Normal, 6),
            );
            let total = calculate_total_length(&line);
            assert!(
                total <= cols,
                "cols={cols}: emitted {total} columns — the line must never exceed the bar"
            );
        }
        // Shed order is reverse criticality: theme first, Voc (host console)
        // last. Chips keep their relative order and never straddle.
        let has = |cols: usize, sentinel: usize| {
            let data = TabRenderData {
                tabs: vec![bare_part(0, 10)],
                active_tab_index: 0,
            };
            tab_line(
                &ModeInfo::default(),
                data,
                cols,
                test_config(InputMode::Normal, 6),
            )
            .iter()
            .any(|part| part.tab_index == Some(sentinel))
        };
        // 79 = left_inset 6 + prefix 25 + full Z3 48: everything fits.
        assert!(has(79, crate::THEME_CLICK_SENTINEL));
        assert!(has(79, crate::AGENTS_CLICK_SENTINEL));
        // 78: theme sheds first, the rest stays.
        assert!(!has(78, crate::THEME_CLICK_SENTINEL));
        assert!(has(78, crate::AGENTS_CLICK_SENTINEL));
        assert!(has(78, crate::VOC_CLICK_SENTINEL));
        // 75: theme + Quick cmd shed; Panels, Composer, Voc stay.
        assert!(!has(75, crate::AGENTS_CLICK_SENTINEL));
        assert!(has(75, crate::PANELS_CLICK_SENTINEL));
        assert!(has(75, crate::COMPOSER_CLICK_SENTINEL));
        assert!(has(75, crate::VOC_CLICK_SENTINEL));
    }

    #[test]
    fn datum_voc_and_composer_hold_position_across_real_mode_switches() {
        // The mode must actually flow through TabLineConfig.mode into the
        // prefix — not just decorate assertion messages.
        for mode in [
            InputMode::Normal,
            InputMode::Locked,
            InputMode::Pane,
            InputMode::Tab,
        ] {
            let data = TabRenderData {
                tabs: vec![bare_part(0, 10)],
                active_tab_index: 0,
            };
            let line = tab_line(&ModeInfo::default(), data, 100, test_config(mode, 6));
            assert_eq!(
                calculate_total_length(&line),
                100,
                "mode {mode:?}: the bar fills exactly its columns"
            );
            let mut offset = 0;
            let mut datum_at = None;
            let mut voc_at = None;
            let mut composer_at = None;
            for part in &line {
                if part.part.contains(DATUM_PARTITION) {
                    datum_at = Some(offset);
                }
                if part.tab_index == Some(crate::VOC_CLICK_SENTINEL) {
                    voc_at = Some(offset);
                }
                if part.tab_index == Some(crate::COMPOSER_CLICK_SENTINEL) {
                    composer_at = Some(offset);
                }
                offset += part.len;
            }
            assert_eq!(datum_at, Some(24), "mode {mode:?}: datum `⎮` at col 24");
            assert_eq!(
                voc_at,
                Some(100 - ENTRY_ZONE_COLS),
                "mode {mode:?}: Voc opens Z3"
            );
            assert_eq!(
                composer_at,
                Some(100 - ENTRY_ZONE_COLS + VOC_CHIP_COLS),
                "mode {mode:?}: Voc sits immediately left of Composer"
            );
        }
    }
}
