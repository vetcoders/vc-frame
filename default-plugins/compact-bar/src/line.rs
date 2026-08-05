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
///   [left_inset][Z0 brand 14][gap 4][⎮][gap 1][Z1 mode 8][Z2 tabs flex][Z3 toolbar 36]
/// ```
/// With the default operator layout (`left_inset=6`, rail `size=24`):
/// brand ends at col 20, 4-col gap, datum at col 24 (= rail width), mode at 26.
pub const BRAND_ZONE_COLS: usize = 14;
/// Columns between brand right edge and the datum partition line.
pub const BRAND_DATUM_GAP_COLS: usize = 4;
/// Canonical partition glyph between Sessions rail and workspace (1 col).
pub const DATUM_PARTITION: &str = "⎮";
pub const DATUM_PARTITION_COLS: usize = 1;
/// Columns between datum and the mode chip.
pub const MODE_LEAD_GAP_COLS: usize = 1;
/// Mode chip body — always exactly 8 display columns (no trailing frame bar).
pub const MODE_ZONE_COLS: usize = 8;
/// Fixed prefix after brand: gap + datum + lead + mode.
pub const AFTER_BRAND_FIXED_COLS: usize =
    BRAND_DATUM_GAP_COLS + DATUM_PARTITION_COLS + MODE_LEAD_GAP_COLS + MODE_ZONE_COLS;
/// `✍ Composer` padded to 14 grid cells (Z3 left half).
pub const COMPOSER_CHIP_COLS: usize = 14;
/// Leading seam + `❯_ Quick cmd` padded to 22 grid cells (Z3 right half).
pub const QUICK_CMD_CHIP_COLS: usize = 22;
/// Protected right toolbar total — immutable position; tabs never push it out.
pub const ENTRY_ZONE_COLS: usize = COMPOSER_CHIP_COLS + QUICK_CMD_CHIP_COLS; // 36

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
    let body = format!("{} {}", glyph, code);
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
                self.create_collapsed_indicators(left_count, right_count, tabs_to_render.len());

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
        left_count: usize,
        right_count: usize,
        rendered_count: usize,
    ) -> CollapsedIndicators {
        let left_more_tab_index = left_count.saturating_sub(1);
        let right_more_tab_index = left_count + rendered_count;

        CollapsedIndicators {
            left: self.create_left_indicator(left_count, left_more_tab_index),
            right: self.create_right_indicator(right_count, right_more_tab_index),
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
            format!("{}", format_str.replace("{}", &count.to_string()))
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
}

impl RightSideElementsBuilder {
    fn new(palette: Styling) -> Self {
        Self { palette }
    }

    /// Protected Z3 only — Composer + Quick cmd. Never includes optional chrome.
    fn build_protected_zone(&self) -> Vec<LinePart> {
        let elements = vec![self.create_composer_chip(), self.create_quick_cmd_chip()];
        debug_assert_eq!(
            elements[0].len + elements[1].len,
            ENTRY_ZONE_COLS,
            "composer+quick entry zone must be exactly {ENTRY_ZONE_COLS} cols"
        );
        elements
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

    /// Always-visible Composer entry point, clickable via the sentinel
    /// tab_index. Fixed [`COMPOSER_CHIP_COLS`]. ✍ (text-presentation) says
    /// "drafting" — onboarding and the tooltip teach Cmd+E / Alt+e.
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
        // Composer + Quick cmd never shift or fall off when tabs overflow.
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
        // Right Guard: Z3 (Composer + Quick cmd) is always placed. Optional
        // tooltip may follow only when free columns remain after Z3.
        let right_builder = RightSideElementsBuilder::new(self.palette);
        let mut right_elements = right_builder.build_protected_zone();
        let z3_len = calculate_total_length(&right_elements);
        debug_assert_eq!(z3_len, ENTRY_ZONE_COLS);

        let current_len = calculate_total_length(prefix);
        let remaining_space = self.cols.saturating_sub(current_len).saturating_sub(z3_len);
        if remaining_space > 0 {
            prefix.push(self.create_spacer(remaining_space));
        }
        prefix.append(&mut right_elements);

        // Tooltip is optional chrome — never steals columns from Z3.
        if let Some(ref tooltip_key) = self.config.toggle_tooltip_key {
            let tip = right_builder.create_tooltip_indicator(
                tooltip_key,
                self.config.tooltip_is_active,
            );
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
        InputMode::Locked => ("⊝", "L"),
        InputMode::Pane => ("◫", "P"),
        InputMode::Tab => ("𝌁", "T"),
        InputMode::Resize => ("⤢", "R"),
        InputMode::Move => ("⟷", "M"),
        InputMode::Scroll => ("⇅", "S"),
        InputMode::Search => ("⌕", "F"),
        InputMode::EnterSearch => ("↵", "F"),
        InputMode::RenameTab => ("✎", "RNT"),
        InputMode::RenamePane => ("✎", "RNP"),
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
    fn mode_zone_has_no_trailing_frame_bar() {
        // Datum `⎮` is a separate partition part; mode body must not re-draw │.
        for mode in ALL_MODES {
            let zone = format_mode_zone(mode);
            assert!(
                !zone.contains('│') && !zone.contains('⎮'),
                "mode {:?} must not embed partition glyphs (got {:?})",
                mode,
                zone
            );
        }
        // Rename codes are three letters; still fit the fixed budget.
        assert!(format_mode_zone(InputMode::RenameTab).contains("RNT"));
        assert!(format_mode_zone(InputMode::RenamePane).contains("RNP"));
    }

    #[test]
    fn brand_zone_is_fixed_width_and_right_aligned() {
        assert_eq!(display_width(&format_brand_zone(None, None)), BRAND_ZONE_COLS);
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
        assert!(short.starts_with(' '), "short brand must left-pad: {:?}", short);
    }

    #[test]
    fn entry_chips_sum_to_protected_z3_36() {
        assert_eq!(ENTRY_ZONE_COLS, 36);
        assert_eq!(COMPOSER_CHIP_COLS + QUICK_CMD_CHIP_COLS, ENTRY_ZONE_COLS);
        assert_eq!(
            display_width(&pad_to_cols("✍ Composer", COMPOSER_CHIP_COLS)),
            COMPOSER_CHIP_COLS
        );
        assert_eq!(
            display_width(&pad_to_cols(" · ❯_ Quick cmd", QUICK_CMD_CHIP_COLS)),
            QUICK_CMD_CHIP_COLS
        );
    }

    #[test]
    fn after_brand_fixed_cols_match_column_guard() {
        // gap(4) + datum(1) + lead(1) + mode(8) = 14
        assert_eq!(AFTER_BRAND_FIXED_COLS, 14);
        assert_eq!(DATUM_PARTITION.width(), DATUM_PARTITION_COLS);
    }

    #[test]
    fn pad_to_cols_handles_wide_eaw_glyphs() {
        // 𝌆 is EAW wide (2). Budget of 4 must absorb it without overshoot.
        let s = pad_to_cols("𝌆", 4);
        assert_eq!(display_width(&s), 4);
        let s2 = pad_to_cols("│𝌆", 3);
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
        // Spec: Protected Toolbar Fixed 36 cols.
        assert_eq!(ENTRY_ZONE_COLS, 36);
        assert_eq!(BRAND_ZONE_COLS, 14);
        assert_eq!(MODE_ZONE_COLS, 8);
        assert_eq!(BRAND_DATUM_GAP_COLS, 4);
        assert_eq!(MODE_LEAD_GAP_COLS, 1);
    }
}
