use ansi_term::AnsiStrings;
use unicode_width::UnicodeWidthStr;

use crate::{ARROW_SEPARATOR, LinePart, TabRenderData};
use zellij_tile::prelude::*;
use zellij_tile_utils::style;

/// Fixed character-column budgets for the compact-bar (Pensieve Fixed Character
/// Grid Model). Every zone is either fixed-width or a single controlled flex
/// (tabs + spacer). Mode switches and metric updates must never shift the bar
/// by even one cell.
pub const BRAND_ZONE_COLS: usize = 14;
pub const MODE_ZONE_COLS: usize = 8;
/// `✍ Composer` padded to 12 grid cells.
pub const COMPOSER_CHIP_COLS: usize = 12;
/// Leading seam + `❯_ Quick cmd` padded to 18 grid cells.
pub const QUICK_CMD_CHIP_COLS: usize = 18;
/// Right entry zone total: Composer + Quick cmd (no free-form growth).
pub const ENTRY_ZONE_COLS: usize = COMPOSER_CHIP_COLS + QUICK_CMD_CHIP_COLS;

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

/// Pad or hard-trim `text` so its display width equals `cols` exactly.
/// Wide EAW glyphs count as 2; padding uses ASCII spaces (width 1).
pub fn pad_to_cols(text: &str, cols: usize) -> String {
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let ch_w = ch.to_string().width().max(1);
        if width + ch_w > cols {
            break;
        }
        out.push(ch);
        width += ch_w;
    }
    while width < cols {
        out.push(' ');
        width += 1;
    }
    out
}

/// Mode zone text — always exactly [`MODE_ZONE_COLS`] display columns.
pub fn format_mode_zone(mode: InputMode) -> String {
    let (glyph, code) = mode_chip(mode);
    // Leading seam, glyph, code. Wide glyphs (𝌁/𝌆) sit flush so the budget
    // still closes at MODE_ZONE_COLS for every mode — no horizontal shift.
    let raw = format!(" {} {} ", glyph, code);
    pad_to_cols(&raw, MODE_ZONE_COLS)
}

/// Brand zone text — always exactly [`BRAND_ZONE_COLS`] display columns.
pub fn format_brand_zone(brand_text: Option<&str>, brand_text_short: Option<&str>) -> String {
    let selected = select_brand_text(brand_text, brand_text_short);
    pad_to_cols(&selected, BRAND_ZONE_COLS)
}

fn select_brand_text(brand_text: Option<&str>, brand_text_short: Option<&str>) -> String {
    let default_brand = " 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. ".to_owned();
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
        (Some(long_brand), _) => long_brand.to_owned(), // pad_to_cols will trim
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

        let more_text = self.format_count_text(tab_count, "← +{}", " ← +many ");
        self.create_styled_indicator(more_text, tab_index)
    }

    fn create_right_indicator(&self, tab_count: usize, tab_index: usize) -> LinePart {
        if tab_count == 0 {
            return LinePart::default();
        }

        let more_text = self.format_count_text(tab_count, "+{} →", " +many → ");
        self.create_styled_indicator(more_text, tab_index)
    }

    fn format_count_text(&self, count: usize, format_str: &str, fallback: &str) -> String {
        if count < 10000 {
            format!(" {} ", format_str.replace("{}", &count.to_string()))
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
        let mut parts = vec![self.create_brand_part(brand_text, brand_text_short)];
        let used_len = parts.first().map_or(0, |p| p.len);

        // Operator's zone order: brand │ mode │ tabs — the mode chip sits
        // directly after the brand so it never leaves the eye's home
        // position. The session anchor lives in the rail header
        // (`SESSIONS N · name`), not on this bar (operator call 2026-07-30).
        if let Some(mode_part) = self.create_mode_part(mode, used_len) {
            parts.push(mode_part);
        }

        parts
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

    fn build(&self, config: &TabLineConfig) -> Vec<LinePart> {
        let mut elements = Vec::new();

        elements.push(self.create_composer_chip());
        elements.push(self.create_quick_cmd_chip());

        if let Some(ref tooltip_key) = config.toggle_tooltip_key {
            elements.push(self.create_tooltip_indicator(tooltip_key, config.tooltip_is_active));
        }

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
        let pad_tail = " ".repeat(plain.width().saturating_sub(seam.width() + label.width()));
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

        if prefix_len + active_tab.len > self.cols {
            return prefix;
        }

        let mut tabs_to_render = vec![active_tab];
        let populator = TabLinePopulator::new(
            self.cols.saturating_sub(prefix_len),
            self.palette,
            self.capabilities,
        );

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
        let current_len = calculate_total_length(prefix);

        if current_len < self.cols {
            let right_builder = RightSideElementsBuilder::new(self.palette);
            let mut right_elements = right_builder.build(&self.config);

            let right_len = calculate_total_length(&right_elements);

            if current_len + right_len <= self.cols {
                let remaining_space = self
                    .cols
                    .saturating_sub(current_len)
                    .saturating_sub(right_len);

                if remaining_space > 0 {
                    prefix.push(self.create_spacer(remaining_space));
                }

                prefix.append(&mut right_elements);
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
/// for all fourteen input modes. Glyphs are plain text-presentation Unicode
/// (no emoji VS16). EnterSearch shares F with Search on purpose: the extra ↵
/// marks the typing phase. Wide EAW glyphs (𝌁/𝌆) stay; the zone budget in
/// [`format_mode_zone`] absorbs them so the rest of the bar never shifts.
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
        InputMode::RenameTab => ("✎", "RT"),
        InputMode::RenamePane => ("✎", "RP"),
        InputMode::Session => ("𝌆", "S"),
        InputMode::Prompt => ("❯", "P"),
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
            .map(|m| format_mode_zone(*m).width())
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
    fn brand_zone_is_fixed_width() {
        assert_eq!(format_brand_zone(None, None).width(), BRAND_ZONE_COLS);
        assert_eq!(
            format_brand_zone(Some(" 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. "), None).width(),
            BRAND_ZONE_COLS
        );
        assert_eq!(
            format_brand_zone(Some("SHORT"), Some("S")).width(),
            BRAND_ZONE_COLS
        );
        // Over-long brand is hard-trimmed, never expands the zone.
        assert_eq!(
            format_brand_zone(Some("XXXXXXXXXXXXXXXXXXXX"), None).width(),
            BRAND_ZONE_COLS
        );
    }

    #[test]
    fn entry_chips_sum_to_fixed_entry_zone() {
        assert_eq!(COMPOSER_CHIP_COLS + QUICK_CMD_CHIP_COLS, ENTRY_ZONE_COLS);
        assert_eq!(pad_to_cols("✍ Composer", COMPOSER_CHIP_COLS).width(), COMPOSER_CHIP_COLS);
        assert_eq!(
            pad_to_cols(" · ❯_ Quick cmd", QUICK_CMD_CHIP_COLS).width(),
            QUICK_CMD_CHIP_COLS
        );
    }

    #[test]
    fn pad_to_cols_handles_wide_eaw_glyphs() {
        // 𝌆 is EAW wide (2). Budget of 4 must absorb it without overshoot.
        let s = pad_to_cols("𝌆", 4);
        assert_eq!(s.width(), 4);
        let s2 = pad_to_cols("│𝌆", 3);
        assert_eq!(s2.width(), 3);
    }
}
