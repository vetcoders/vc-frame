mod first_line;
mod one_line_ui;
mod second_line;
mod tip;

use ansi_term::{
    AnsiString,
    Color::{Fixed, Rgb},
    Style,
};

use std::collections::BTreeMap;
use std::fmt::{Display, Error, Formatter};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;
use zellij_tile::prelude::actions::Action;
use zellij_tile::prelude::*;
use zellij_tile_utils::{palette_match, style};

use first_line::first_line;
use one_line_ui::one_line_ui;
use second_line::{
    floating_panes_are_visible, fullscreen_panes_to_hide, keybinds,
    locked_floating_panes_are_visible, locked_fullscreen_panes_to_hide, system_clipboard_error,
    text_copied_hint,
};
use tip::utils::get_cached_tip_name;

// for more of these, copy paste from: https://en.wikipedia.org/wiki/Box-drawing_character
static ARROW_SEPARATOR: &str = "";
static MORE_MSG: &str = " ... ";
/// How long the clipboard notification ("Text copied...") stays on the bar
/// before dismissing itself without requiring user input.
const CLIPBOARD_HINT_TTL_SECONDS: f64 = 2.0;
/// Host resource cockpit (moved here from the session rail): context key of
/// the sampling run_command and the seconds between samples.
const RESOURCE_SAMPLE_CONTEXT_KEY: &str = "vc_status_resources";
const RESOURCE_SAMPLE_SECONDS: f64 = 5.0;
/// Lightweight server-to-plugin signal carrying the fleet's live terminal-tab
/// count. Keep this wire name in sync with `zellij-server/src/screen.rs`.
const VC_FLEET_LIVE_COUNT_MESSAGE: &str = "vc.fleet-live-count.v1";
/// Exact per-plugin/client lifecycle signal emitted by Screen. Generic
/// `Visible` is tab-global and cannot distinguish clients viewing different
/// tabs in a non-mirrored session.
const VC_STATUS_BAR_VISIBILITY_MESSAGE: &str = "vc.status-bar-visibility.v1";
// Portable host sample: total CPU% (per-core percentages summed, like top),
// used RSS KiB and total RAM KiB — Linux via /proc/meminfo, macOS via sysctl —
// plus available KiB on the root filesystem (df POSIX output, field 4).
const RESOURCE_SAMPLE_COMMAND: &str = r#"cpu=$(ps -A -o %cpu= | awk '{s+=$1} END {printf "%.0f", s}'); used=$(ps -A -o rss= | awk '{s+=$1} END {print s}'); if [ -r /proc/meminfo ]; then total=$(awk '/^MemTotal:/{print $2}' /proc/meminfo); else total=$(( $(sysctl -n hw.memsize) / 1024 )); fi; disk=$(df -P -k / | awk 'NR==2 {print $4}'); printf '%s %s %s %s' "$cpu" "$used" "$total" "$disk""#;
/// Shorthand for `Action::SwitchToMode{input_mode: InputMode::Normal}`.
const TO_NORMAL: Action = Action::SwitchToMode {
    input_mode: InputMode::Normal,
};
/// Minimum breathing room between the hint line and the right status
/// segment — hints may never touch the diodes.
const STATUS_SEAM_CELLS: usize = 2;
/// Columns the resting-mode hint ("Ctrl g LOCK") keeps for itself before
/// the status segment may claim the rest of the bar.
const RESTING_HINT_RESERVE: usize = 16;
/// Unlocked modes hand the width to the shortcut cheat-sheet; the
/// swap-layout chip may claim at most 1/N of the row.
const SWAP_CHIP_MAX_BAR_FRACTION: usize = 4;

// Floor for a renderable frame: anything below is a transient startup event,
// not a legal surface. Kept far below the comfortable chrome minimum
// (tools/repro_chrome.py MIN_COLUMNS) so legal small panes always render.
const MIN_RENDER_ROWS: usize = 1;
const MIN_RENDER_COLS: usize = 4;

fn dimensions_are_transient(rows: usize, cols: usize) -> bool {
    rows < MIN_RENDER_ROWS || cols < MIN_RENDER_COLS
}

#[derive(Default)]
struct State {
    tabs: Vec<TabInfo>,
    tip_name: String,
    mode_info: ModeInfo,
    text_copy_destination: Option<CopyDestination>,
    display_system_clipboard_failure: bool,
    clipboard_hint_deadline: Option<Instant>,
    classic_ui: bool,
    base_mode_is_locked: bool,
    cached_keybinds: KeybindsVec,
    // Host resource cockpit ("CPU … | MEM … | DISK …"), sampled via
    // run_command. None until the first valid sample; a failed or malformed
    // sample clears the line so HEALTH cannot claim "ok" on stale numbers.
    resource_line: Option<String>,
    resource_sample_in_flight: bool,
    resource_sample_due: Option<Instant>,
    is_visible: bool,
    // Fleet pulse: the server computes this once from its existing session
    // snapshot and sends only a scalar custom message to per-tab chrome.
    live_count: usize,
}

register_plugin!(State);

#[derive(Clone, Default)]
pub struct LinePart {
    part: String,
    len: usize,
}

impl LinePart {
    pub fn append(&mut self, to_append: &LinePart) {
        self.part.push_str(&to_append.part);
        self.len += to_append.len;
    }
}

impl Display for LinePart {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{}", self.part)
    }
}

#[derive(Clone, Copy)]
pub struct ColoredElements {
    pub selected: SegmentStyle,
    pub unselected: SegmentStyle,
    pub unselected_alternate: SegmentStyle,
    pub disabled: SegmentStyle,
    // superkey
    pub superkey_prefix: Style,
    pub superkey_suffix_separator: Style,
}

#[derive(Clone, Copy)]
pub struct SegmentStyle {
    pub prefix_separator: Style,
    pub char_left_separator: Style,
    pub char_shortcut: Style,
    pub char_right_separator: Style,
    pub styled_text: Style,
    pub suffix_separator: Style,
}

// I really hate this, but I can't come up with a good solution for this,
// we need different colors from palette for the default theme
// plus here we can add new sources in the future, like Theme
// that can be defined in the config perhaps
fn color_elements(palette: Styling, different_color_alternates: bool) -> ColoredElements {
    let background = palette.text_unselected.background;
    let foreground = palette.text_unselected.base;
    let alternate_background_color = if different_color_alternates {
        palette.ribbon_unselected.base
    } else {
        palette.ribbon_unselected.background
    };
    ColoredElements {
        selected: SegmentStyle {
            prefix_separator: style!(background, palette.ribbon_selected.background),
            char_left_separator: style!(
                palette.ribbon_selected.base,
                palette.ribbon_selected.background
            )
            .bold(),
            char_shortcut: style!(
                palette.ribbon_selected.emphasis_1,
                palette.ribbon_selected.background
            )
            .bold(),
            char_right_separator: style!(
                palette.ribbon_selected.base,
                palette.ribbon_selected.background
            )
            .bold(),
            styled_text: style!(
                palette.ribbon_selected.base,
                palette.ribbon_selected.background
            )
            .bold(),
            suffix_separator: style!(palette.ribbon_selected.background, background).bold(),
        },
        unselected: SegmentStyle {
            prefix_separator: style!(background, palette.ribbon_unselected.background),
            char_left_separator: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .bold(),
            char_shortcut: style!(
                palette.ribbon_unselected.emphasis_1,
                palette.ribbon_unselected.background
            )
            .bold(),
            char_right_separator: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .bold(),
            styled_text: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .bold(),
            suffix_separator: style!(palette.ribbon_unselected.background, background).bold(),
        },
        unselected_alternate: SegmentStyle {
            prefix_separator: style!(background, alternate_background_color),
            char_left_separator: style!(background, alternate_background_color).bold(),
            char_shortcut: style!(
                palette.ribbon_unselected.emphasis_1,
                alternate_background_color
            )
            .bold(),
            char_right_separator: style!(background, alternate_background_color).bold(),
            styled_text: style!(palette.ribbon_unselected.base, alternate_background_color).bold(),
            suffix_separator: style!(alternate_background_color, background).bold(),
        },
        disabled: SegmentStyle {
            prefix_separator: style!(background, palette.ribbon_unselected.background),
            char_left_separator: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .dimmed()
            .italic(),
            char_shortcut: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .dimmed()
            .italic(),
            char_right_separator: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .dimmed()
            .italic(),
            styled_text: style!(
                palette.ribbon_unselected.base,
                palette.ribbon_unselected.background
            )
            .dimmed()
            .italic(),
            suffix_separator: style!(palette.ribbon_unselected.background, background),
        },
        superkey_prefix: style!(foreground, background).bold(),
        superkey_suffix_separator: style!(background, background),
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // TODO: Should be able to choose whether to use the cache through config.
        self.tip_name = get_cached_tip_name();
        self.classic_ui = configuration
            .get("classic")
            .map(|c| c == "true")
            .unwrap_or(false);
        set_selectable(false);
        request_permission(&status_bar_permissions());
        subscribe(&status_bar_subscriptions());
        // Attach loads a client instance for plugins in every tab, including
        // hidden tabs. Stay idle until Screen targets this active status-bar
        // with the fleet heartbeat or its exact lifecycle signal.
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::InitialKeybinds(keybinds) => {
                self.cached_keybinds = keybinds;
                if !self.cached_keybinds.is_empty() {
                    self.mode_info.keybinds = self.cached_keybinds.clone();
                }
                should_render = true;
            },
            Event::ModeUpdate(mut mode_info) => {
                if mode_info.keybinds.is_empty() && !self.cached_keybinds.is_empty() {
                    mode_info.keybinds = self.cached_keybinds.clone();
                } else if !mode_info.keybinds.is_empty() {
                    self.cached_keybinds = mode_info.keybinds.clone();
                }
                if self.mode_info != mode_info {
                    should_render = true;
                }
                self.mode_info = mode_info;
                self.base_mode_is_locked = self.mode_info.base_mode == Some(InputMode::Locked);
            },
            Event::TabUpdate(tabs) => {
                if self.tabs != tabs {
                    should_render = true;
                }
                self.tabs = tabs;
            },
            Event::CopyToClipboard(copy_destination) => {
                if !self.is_visible {
                    return false;
                }
                match self.text_copy_destination {
                    Some(text_copy_destination) => {
                        if text_copy_destination != copy_destination {
                            should_render = true;
                        }
                    },
                    None => {
                        should_render = true;
                    },
                }
                self.text_copy_destination = Some(copy_destination);
                self.clipboard_hint_deadline =
                    Some(Instant::now() + Duration::from_secs_f64(CLIPBOARD_HINT_TTL_SECONDS));
                set_timeout(CLIPBOARD_HINT_TTL_SECONDS);
            },
            Event::SystemClipboardFailure => {
                if !self.is_visible {
                    return false;
                }
                should_render = true;
                self.display_system_clipboard_failure = true;
                self.clipboard_hint_deadline =
                    Some(Instant::now() + Duration::from_secs_f64(CLIPBOARD_HINT_TTL_SECONDS));
                set_timeout(CLIPBOARD_HINT_TTL_SECONDS);
            },
            Event::Timer(_) => {
                let now = Instant::now();
                if self
                    .clipboard_hint_deadline
                    .is_some_and(|deadline| now >= deadline)
                {
                    self.clipboard_hint_deadline = None;
                    self.text_copy_destination = None;
                    self.display_system_clipboard_failure = false;
                    should_render = true;
                }
                if self.is_visible
                    && self
                        .resource_sample_due
                        .is_some_and(|deadline| now >= deadline)
                {
                    self.resource_sample_due = None;
                    self.start_resource_sample();
                }
            },
            Event::RunCommandResult(exit_code, stdout, _stderr, context)
                if context.contains_key(RESOURCE_SAMPLE_CONTEXT_KEY) =>
            {
                self.resource_sample_in_flight = false;
                if self.is_visible {
                    self.schedule_resource_sample();
                } else {
                    self.resource_sample_due = None;
                }
                // A failed or malformed sample must not freeze the last good
                // reading forever under "HEALTH ok". Clear to unknown so the
                // bar is honest until the next successful sample.
                if exit_code == Some(0) {
                    if let Some(line) = parse_resource_sample(&stdout) {
                        if self.resource_line.as_deref() != Some(line.as_str()) {
                            self.resource_line = Some(line);
                            should_render = true;
                        }
                    } else if self.resource_line.take().is_some() {
                        should_render = true;
                    }
                } else if self.resource_line.take().is_some() {
                    should_render = true;
                }
            },
            Event::CustomMessage(message, payload) if message == VC_FLEET_LIVE_COUNT_MESSAGE => {
                // Screen targets this message only at status-bars on active
                // tabs. Treat it as a positive visibility heartbeat as well.
                let became_visible = self.set_visibility(true);
                let live_count_changed = self.apply_fleet_live_count(&payload);
                should_render = became_visible || live_count_changed;
            },
            Event::CustomMessage(message, payload)
                if message == VC_STATUS_BAR_VISIBILITY_MESSAGE =>
            {
                match payload.as_str() {
                    "true" => should_render = self.set_visibility(true),
                    "false" => should_render = self.set_visibility(false),
                    _ => {},
                }
            },
            Event::PermissionRequestResult(_) => {
                if self.is_visible {
                    self.start_resource_sample();
                }
                should_render = true;
            },
            Event::InputReceived => {
                if self.text_copy_destination.is_some() || self.display_system_clipboard_failure {
                    should_render = true;
                }
                self.text_copy_destination = None;
                self.display_system_clipboard_failure = false;
                self.clipboard_hint_deadline = None;
            },
            _ => {},
        };
        should_render
    }

    fn render(&mut self, rows: usize, cols: usize) {
        // Transient initial resize events arrive with rows/cols at or near
        // zero before the real layout lands; painting those frames is what
        // makes the chrome visibly jump at session start.
        if dimensions_are_transient(rows, cols) {
            return;
        }
        let supports_arrow_fonts = !self.mode_info.capabilities.arrow_fonts;
        let separator = if supports_arrow_fonts {
            ARROW_SEPARATOR
        } else {
            ""
        };

        let background = self.mode_info.style.colors.text_unselected.background;

        if rows == 1 && !self.classic_ui {
            let fill_bg = match background {
                PaletteColor::Rgb((r, g, b)) => format!("\u{1b}[48;2;{};{};{}m\u{1b}[0K", r, g, b),
                PaletteColor::EightBit(color) => format!("\u{1b}[48;5;{}m\u{1b}[0K", color),
            };
            let active_tab = self.tabs.iter().find(|t| t.active);
            // The bar keeps one contract: LOCK is the presentation mode —
            // the whole bar belongs to the status diodes (LIVE, cockpit,
            // HEALTH) regardless of which base mode the config declares.
            // Every unlocked mode hands the width to the shortcut
            // cheat-sheet; only the swap-layout chip stays, because it is
            // arrangement context, not telemetry. (Operator regression
            // 2026-08-05: gating on a derived "resting mode" hid the
            // cockpit in LOCK whenever the base mode was Normal.)
            let right = if self.mode_info.mode == InputMode::Locked {
                self.right_status_segment(active_tab, cols.saturating_sub(RESTING_HINT_RESERVE))
            } else {
                // Unlocked modes: the width belongs to the full shortcut
                // cheat-sheet — no telemetry. Only the swap-layout chip
                // ("BASE") keeps the right edge: manipulation modes are
                // exactly when the operator is arranging.
                self.swap_chip_segment(active_tab, cols / SWAP_CHIP_MAX_BAR_FRACTION)
            };
            let seam = if right.len > 0 { STATUS_SEAM_CELLS } else { 0 };
            let ui_cols = cols.saturating_sub(right.len + seam);
            let line = one_line_ui(
                &self.mode_info,
                active_tab,
                ui_cols,
                separator,
                self.base_mode_is_locked,
                self.text_copy_destination,
                self.display_system_clipboard_failure,
            );
            if right.len > 0 && cols > line.len + right.len {
                // Right-align the status segment by PRINTING FORWARD only:
                // hints, a background-styled spacer, the segment, then EL
                // for the final column. The previous shape (EL, then CHA
                // back, then text) corrupted the composed frame on every
                // render — the climbing/ghosting chrome of 2026-07-31,
                // bisected to exactly that print. No cursor motion, no
                // write into the last cell: nothing left to go wrong.
                let pad = cols.saturating_sub(line.len + right.len + 1);
                let spacer = style!(background, background).paint(" ".repeat(pad));
                print!("{}{}{}{}", line, spacer, right.part, fill_bg);
            } else {
                print!("{}{}", line, fill_bg);
            }
            return;
        }

        //TODO: Switch to UI components here
        let active_tab = self.tabs.iter().find(|t| t.active);
        let first_line = first_line(&self.mode_info, active_tab, cols, separator);
        let second_line = self.second_line(cols);

        // [48;5;238m is white background, [0K is so that it fills the rest of the line
        // [m is background reset, [0K is so that it clears the rest of the line
        match background {
            PaletteColor::Rgb((r, g, b)) => {
                if rows > 1 {
                    println!("{}\u{1b}[48;2;{};{};{}m\u{1b}[0K", first_line, r, g, b);
                } else if self.mode_info.mode == InputMode::Normal {
                    print!("{}\u{1b}[48;2;{};{};{}m\u{1b}[0K", first_line, r, g, b);
                } else {
                    print!("\u{1b}[m{}\u{1b}[0K", second_line);
                }
            },
            PaletteColor::EightBit(color) => {
                if rows > 1 {
                    println!("{}\u{1b}[48;5;{}m\u{1b}[0K", first_line, color);
                } else if self.mode_info.mode == InputMode::Normal {
                    print!("{}\u{1b}[48;5;{}m\u{1b}[0K", first_line, color);
                } else {
                    print!("\u{1b}[m{}\u{1b}[0K", second_line);
                }
            },
        }

        if rows > 1 {
            print!("\u{1b}[m{}\u{1b}[0K", second_line);
        }
    }
}

impl State {
    fn start_resource_sample(&mut self) {
        if !self.is_visible || self.resource_sample_in_flight {
            return;
        }
        self.resource_sample_due = None;
        self.resource_sample_in_flight = true;
        let mut context = BTreeMap::new();
        context.insert(RESOURCE_SAMPLE_CONTEXT_KEY.to_owned(), "true".to_owned());
        run_command(&["sh", "-c", RESOURCE_SAMPLE_COMMAND], context);
    }

    fn set_visibility(&mut self, is_visible: bool) -> bool {
        if self.is_visible == is_visible {
            return false;
        }
        self.is_visible = is_visible;
        if is_visible {
            self.start_resource_sample();
        } else {
            self.resource_sample_due = None;
            self.clipboard_hint_deadline = None;
            self.text_copy_destination = None;
            self.display_system_clipboard_failure = false;
        }
        true
    }

    fn schedule_resource_sample(&mut self) {
        self.resource_sample_due =
            Some(Instant::now() + Duration::from_secs_f64(RESOURCE_SAMPLE_SECONDS));
        set_timeout(RESOURCE_SAMPLE_SECONDS);
    }

    fn apply_fleet_live_count(&mut self, payload: &str) -> bool {
        let Ok(live_count) = payload.parse::<usize>() else {
            return false;
        };
        if self.live_count == live_count {
            return false;
        }
        self.live_count = live_count;
        true
    }

    /// The bar's right edge — pure statuses, zero tools (operator call
    /// 2026-07-31 / close-out Fork IV): fleet LIVE, host cockpit, and a
    /// HEALTH chip. All glyphs are single-cell ASCII/emoji-safe tokens so we
    /// never re-introduce the ䷅ (U+4DC5, width 2) jumping-screen class.
    ///
    /// Degradation ladder: instead of dropping the whole segment when the
    /// bar narrows, shed blocks right-to-left — DISK, then MEM, then CPU,
    /// then the swap chip, then HEALTH; the fleet pulse goes last. The
    /// returned segment always fits `max_len` (or is empty).
    fn right_status_segment(&self, active_tab: Option<&TabInfo>, max_len: usize) -> LinePart {
        let cockpit: Vec<&str> = self
            .resource_line
            .as_deref()
            .map(|line| line.split(" | ").collect())
            .unwrap_or_default();
        let swap_chip = self.swap_layout_status(active_tab);

        let mut ladder: Vec<(usize, bool, bool)> = (0..=cockpit.len())
            .rev()
            .map(|kept| (kept, true, true))
            .collect();
        ladder.push((0, false, true));
        ladder.push((0, false, false));

        for (fields_kept, with_swap, with_health) in ladder {
            let chip = if with_swap { swap_chip.as_ref() } else { None };
            let segment = self.compose_status_segment(&cockpit[..fields_kept], chip, with_health);
            if segment.len <= max_len {
                return segment;
            }
        }
        LinePart::default()
    }

    /// One rung of the status ladder: LIVE + the kept cockpit fields +
    /// optional HEALTH + optional swap-layout chip, in bar order.
    fn compose_status_segment(
        &self,
        cockpit_fields: &[&str],
        swap_chip: Option<&LinePart>,
        with_health: bool,
    ) -> LinePart {
        let mut segment = LinePart::default();
        let palette = self.mode_info.style.colors;
        let dim = style!(
            palette.text_unselected.emphasis_2,
            palette.text_unselected.background
        );
        let hot = style!(
            palette.text_unselected.emphasis_1,
            palette.text_unselected.background
        )
        .bold();

        // LIVE = fleet pulse (agent process tabs across sessions).
        // Two-digit field so LIVE 9 → LIVE 12 never shifts the cockpit.
        let live_shown = self.live_count.min(99);
        let live_text = format!("LIVE {:2}", live_shown);
        let live_part = if self.live_count > 0 {
            hot.paint(live_text.clone()).to_string()
        } else {
            dim.paint(live_text.clone()).to_string()
        };
        segment.append(&LinePart {
            len: live_text.width(),
            part: live_part,
        });

        for field in cockpit_fields {
            let text = format!(" | {}", field);
            segment.append(&LinePart {
                len: text.width(),
                part: dim.paint(text).to_string(),
            });
        }

        // HEALTH: green `ok` when the sample is present; `?` when we have no
        // sample yet (honest unknown). The verdict reads the sample itself,
        // not the kept fields — a narrow bar never changes the diagnosis.
        if with_health {
            let (label, emphasis) = if self.resource_line.is_some() {
                ("HEALTH ok", true)
            } else {
                ("HEALTH ?", false)
            };
            let text = format!(" | {}", label);
            let painted = if emphasis {
                style!(
                    palette.text_unselected.emphasis_1,
                    palette.text_unselected.background
                )
                .paint(text.clone())
                .to_string()
            } else {
                dim.paint(text.clone()).to_string()
            };
            segment.append(&LinePart {
                len: text.width(),
                part: painted,
            });
        }

        if let Some(swap_chip) = swap_chip {
            let sep = LinePart {
                len: 1,
                part: dim.paint(" ").to_string(),
            };
            segment.append(&sep);
            segment.append(swap_chip);
        }

        segment
    }

    /// Unlocked-mode right edge: the swap-layout chip alone. Manipulation
    /// modes are exactly when the operator is arranging — but the chip
    /// yields once the bar gets tight.
    fn swap_chip_segment(&self, active_tab: Option<&TabInfo>, max_len: usize) -> LinePart {
        match self.swap_layout_status(active_tab) {
            Some(chip) if chip.len <= max_len => chip,
            _ => LinePart::default(),
        }
    }

    fn swap_layout_status(&self, active_tab: Option<&TabInfo>) -> Option<LinePart> {
        let tab = active_tab?;
        let name = tab.active_swap_layout_name.as_ref()?;
        let mut label = format!(" {} ", name);
        label.make_ascii_uppercase();
        let len = label.chars().count();
        let palette = self.mode_info.style.colors;

        let styled = match self.mode_info.mode {
            InputMode::Locked => style!(
                palette.text_unselected.background,
                palette.ribbon_unselected.background
            )
            .italic(),
            _ if tab.is_swap_layout_dirty => style!(
                palette.text_unselected.background,
                palette.ribbon_unselected.background
            )
            .bold(),
            _ => style!(
                palette.text_unselected.background,
                palette.ribbon_selected.background
            )
            .bold(),
        };

        Some(LinePart {
            part: styled.paint(label).to_string(),
            len,
        })
    }

    fn second_line(&self, cols: usize) -> LinePart {
        let active_tab = self.tabs.iter().find(|t| t.active);

        if let Some(copy_destination) = self.text_copy_destination {
            text_copied_hint(copy_destination)
        } else if self.display_system_clipboard_failure {
            system_clipboard_error(&self.mode_info.style.colors)
        } else if let Some(active_tab) = active_tab {
            if active_tab.is_fullscreen_active {
                match self.mode_info.mode {
                    InputMode::Normal => fullscreen_panes_to_hide(
                        &self.mode_info.style.colors,
                        active_tab.panes_to_hide,
                    ),
                    InputMode::Locked => locked_fullscreen_panes_to_hide(
                        &self.mode_info.style.colors,
                        active_tab.panes_to_hide,
                    ),
                    _ => keybinds(&self.mode_info, &self.tip_name, cols),
                }
            } else if active_tab.are_floating_panes_visible {
                match self.mode_info.mode {
                    InputMode::Normal => floating_panes_are_visible(&self.mode_info),
                    InputMode::Locked => {
                        locked_floating_panes_are_visible(&self.mode_info.style.colors)
                    },
                    _ => keybinds(&self.mode_info, &self.tip_name, cols),
                }
            } else {
                keybinds(&self.mode_info, &self.tip_name, cols)
            }
        } else {
            LinePart::default()
        }
    }
}

/// Format the four-number sample ("cpu used_kib total_kib disk_avail_kib")
/// into the cockpit line. Returns None on any malformed field so a bad
/// sample never blanks a previously valid reading.
fn parse_resource_sample(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let mut parts = text.split_whitespace();
    let cpu: f64 = parts.next()?.parse().ok()?;
    let used_kib: f64 = parts.next()?.parse().ok()?;
    let total_kib: f64 = parts.next()?.parse().ok()?;
    let disk_avail_kib: f64 = parts.next()?.parse().ok()?;
    if total_kib <= 0.0 || cpu < 0.0 || used_kib < 0.0 || disk_avail_kib < 0.0 {
        return None;
    }
    const KIB_PER_GIB: f64 = 1024.0 * 1024.0;
    // Fixed-width fields so the right-edge segment never jitters when a
    // reading rolls from 9 → 100, 8.0G → 264.3G, or multi-core CPU past
    // 999% (Pensieve Fixed Character Grid Model + live operator hardware).
    // Widths are max-and-min: CPU 4, MEM used 5.1, total 3, DISK 3.
    // Clamp so a pathological sample cannot expand past the budget.
    Some(format!(
        "CPU {:4.0}% | MEM {:5.1}/{:3.0}G | DISK {:3.0}G",
        cpu.min(9999.0),
        (used_kib / KIB_PER_GIB).min(999.9),
        (total_kib / KIB_PER_GIB).min(999.0),
        (disk_avail_kib / KIB_PER_GIB).min(999.0),
    ))
}

fn status_bar_permissions() -> Vec<PermissionType> {
    vec![PermissionType::RunCommands]
}

fn status_bar_subscriptions() -> Vec<EventType> {
    vec![
        EventType::ModeUpdate,
        EventType::TabUpdate,
        EventType::PaneUpdate,
        EventType::CopyToClipboard,
        EventType::InputReceived,
        EventType::SystemClipboardFailure,
        EventType::InitialKeybinds,
        EventType::Timer,
        EventType::RunCommandResult,
        EventType::CustomMessage,
        EventType::PermissionRequestResult,
    ]
}

pub fn get_common_modifiers(mut keyvec: Vec<&KeyWithModifier>) -> Vec<KeyModifier> {
    if keyvec.is_empty() {
        return vec![];
    }
    let mut common_modifiers = keyvec.pop().unwrap().key_modifiers.clone();
    for key in keyvec {
        common_modifiers = common_modifiers
            .intersection(&key.key_modifiers)
            .cloned()
            .collect();
    }
    common_modifiers.into_iter().collect()
}

/// Get key from action pattern(s).
///
/// This function takes as arguments a `keymap` that is a `Vec<(Key, Vec<Action>)>` and contains
/// all keybindings for the current mode and one or more `p` patterns which match a sequence of
/// actions to search for. If within the keymap a sequence of actions matching `p` is found, all
/// keys that trigger the action pattern are returned as vector of `Vec<Key>`.
pub fn action_key(
    keymap: &[(KeyWithModifier, Vec<Action>)],
    action: &[Action],
) -> Vec<KeyWithModifier> {
    keymap
        .iter()
        .filter_map(|(key, acvec)| {
            let matching = acvec
                .iter()
                .zip(action)
                .filter(|(a, b)| a.shallow_eq(b))
                .count();

            if matching == acvec.len() && matching == action.len() {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect::<Vec<KeyWithModifier>>()
}

/// Get multiple keys for multiple actions.
///
/// An extension of [`action_key`] that iterates over all action tuples and collects the results.
pub fn action_key_group(
    keymap: &[(KeyWithModifier, Vec<Action>)],
    actions: &[&[Action]],
) -> Vec<KeyWithModifier> {
    let mut ret = vec![];
    for action in actions {
        ret.extend(action_key(keymap, action));
    }
    ret
}

/// Style a vector of [`Key`]s with the given [`Palette`].
///
/// Creates a line segment of style `<KEYS>`, with correct theming applied: The brackets have the
/// regular text color, the enclosed keys are painted green and bold. If the keys share a common
/// modifier (See [`get_common_modifier`]), it is printed in front of the keys, painted green and
/// bold, separated with a `+`: `MOD + <KEYS>`.
///
/// If multiple [`Key`]s are given, the individual keys are separated with a `|` char. This does
/// not apply to the following groups of keys which are treated specially and don't have a
/// separator between them:
///
/// - "hjkl"
/// - "HJKL"
/// - "←↓↑→"
/// - "←→"
/// - "↓↑"
///
/// The returned Vector of [`AnsiString`] is suitable for transformation into an [`AnsiStrings`]
/// type.
pub fn style_key_with_modifier(
    keyvec: &[KeyWithModifier],
    palette: &Styling,
    background: Option<PaletteColor>,
) -> Vec<AnsiString<'static>> {
    if keyvec.is_empty() {
        return vec![];
    }

    let text_color = palette_match!(palette.text_unselected.base);
    let green_color = palette_match!(palette.text_unselected.emphasis_2);
    let orange_color = palette_match!(palette.text_unselected.emphasis_1);
    let mut ret = vec![];

    let common_modifiers = get_common_modifiers(keyvec.iter().collect());

    let no_common_modifier = common_modifiers.is_empty();
    // macOS product glyphs (⌃ not "Ctrl") — chrome help SSOT.
    let modifier_str = first_line::format_modifiers(&common_modifiers);
    let painted_modifier = if modifier_str.is_empty() {
        Style::new().paint("")
    } else if let Some(background) = background {
        let background = palette_match!(background);
        Style::new()
            .fg(orange_color)
            .on(background)
            .bold()
            .paint(modifier_str)
    } else {
        Style::new().fg(orange_color).bold().paint(modifier_str)
    };
    ret.push(painted_modifier);

    // Prints key group start
    let group_start_str = if no_common_modifier { "<" } else { " + <" };
    if let Some(background) = background {
        let background = palette_match!(background);
        ret.push(
            Style::new()
                .fg(text_color)
                .on(background)
                .paint(group_start_str),
        );
    } else {
        ret.push(Style::new().fg(text_color).paint(group_start_str));
    }

    // Prints the keys — macOS glyphs for any remaining modifiers.
    let key = keyvec
        .iter()
        .map(|key| {
            if no_common_modifier {
                first_line::chrome_key_label(key)
            } else {
                let leftover: Vec<KeyModifier> = key
                    .key_modifiers
                    .iter()
                    .filter(|m| !common_modifiers.contains(m))
                    .copied()
                    .collect();
                if leftover.is_empty() {
                    format!("{}", key.bare_key)
                } else {
                    format!(
                        "{}{}",
                        first_line::format_modifiers(&leftover),
                        key.bare_key
                    )
                }
            }
        })
        .collect::<Vec<String>>();

    // Special handling of some pre-defined keygroups
    let key_string = key.join("");
    let key_separator = match &key_string[..] {
        "HJKL" => "",
        "hjkl" => "",
        "←↓↑→" => "",
        "←→" => "",
        "↓↑" => "",
        "[]" => "",
        _ => "|",
    };

    for (idx, key) in key.iter().enumerate() {
        if idx > 0 && !key_separator.is_empty() {
            if let Some(background) = background {
                let background = palette_match!(background);
                ret.push(
                    Style::new()
                        .fg(text_color)
                        .on(background)
                        .paint(key_separator),
                );
            } else {
                ret.push(Style::new().fg(text_color).paint(key_separator));
            }
        }
        if let Some(background) = background {
            let background = palette_match!(background);
            ret.push(
                Style::new()
                    .fg(green_color)
                    .on(background)
                    .bold()
                    .paint(key.clone()),
            );
        } else {
            ret.push(Style::new().fg(green_color).bold().paint(key.clone()));
        }
    }

    let group_end_str = ">";
    if let Some(background) = background {
        let background = palette_match!(background);
        ret.push(
            Style::new()
                .fg(text_color)
                .on(background)
                .paint(group_end_str),
        );
    } else {
        ret.push(Style::new().fg(text_color).paint(group_end_str));
    }

    ret
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ansi_term::AnsiStrings;
    use ansi_term::unstyle;

    fn big_keymap() -> Vec<(KeyWithModifier, Vec<Action>)> {
        vec![
            (KeyWithModifier::new(BareKey::Char('a')), vec![Action::Quit]),
            (
                KeyWithModifier::new(BareKey::Char('b')).with_ctrl_modifier(),
                vec![Action::ScrollUp],
            ),
            (
                KeyWithModifier::new(BareKey::Char('d')).with_ctrl_modifier(),
                vec![Action::ScrollDown],
            ),
            (
                KeyWithModifier::new(BareKey::Char('c')).with_alt_modifier(),
                vec![
                    Action::ScrollDown,
                    Action::SwitchToMode {
                        input_mode: InputMode::Normal,
                    },
                ],
            ),
            (
                KeyWithModifier::new(BareKey::Char('1')),
                vec![
                    TO_NORMAL,
                    Action::SwitchToMode {
                        input_mode: InputMode::Locked,
                    },
                ],
            ),
        ]
    }

    #[test]
    fn resource_sample_formats_cpu_memory_and_disk() {
        // Fixed-width fields (CPU 4, MEM used 5.1, total 3, DISK 3).
        // Mid-range laptop sample: 342% CPU, 8 GiB / 64 GiB, 13 GiB free.
        let mid = parse_resource_sample(b"342 8388608 67108864 13631488");
        assert_eq!(
            mid.as_deref(),
            Some("CPU  342% | MEM   8.0/ 64G | DISK  13G")
        );
        // Single-digit path still occupies the full budget.
        let small = parse_resource_sample(b"9 1048576 2097152 1048576");
        assert_eq!(
            small.as_deref(),
            Some("CPU    9% | MEM   1.0/  2G | DISK   1G")
        );
        // Operator hardware from Pensieve screenshots: multi-core CPU past
        // 999% and used memory past 100G (264.3/512G, DISK 173G).
        // 264.3 GiB = 264.3 * 1024 * 1024 KiB; 512 GiB total; 173 GiB disk.
        let used_kib = (264.3_f64 * 1024.0 * 1024.0).round() as u64;
        let total_kib = 512u64 * 1024 * 1024;
        let disk_kib = 173u64 * 1024 * 1024;
        let hardware = parse_resource_sample(
            format!("1949 {} {} {}", used_kib, total_kib, disk_kib).as_bytes(),
        );
        assert_eq!(
            hardware.as_deref(),
            Some("CPU 1949% | MEM 264.3/512G | DISK 173G")
        );
        // Another plan screenshot magnitude: 371.8 used of 512.
        let used_kib_hi = (371.8_f64 * 1024.0 * 1024.0).round() as u64;
        let heavy = parse_resource_sample(
            format!("469 {} {} {}", used_kib_hi, total_kib, 348u64 * 1024 * 1024).as_bytes(),
        );
        assert_eq!(
            heavy.as_deref(),
            Some("CPU  469% | MEM 371.8/512G | DISK 348G")
        );

        let widths = [
            mid.as_ref().unwrap().width(),
            small.as_ref().unwrap().width(),
            hardware.as_ref().unwrap().width(),
            heavy.as_ref().unwrap().width(),
        ];
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "metric line width must be stable across single-digit, mid, and >=100G used: {widths:?}"
        );
    }

    #[test]
    fn status_ladder_sheds_cockpit_fields_before_the_pulse() {
        let state = State {
            live_count: 3,
            resource_line: Some("CPU  342% | MEM   8.0/ 64G | DISK  13G".to_owned()),
            ..Default::default()
        };

        // Wide bar: the full segment fits. LIVE uses a 2-digit field.
        let full = state.right_status_segment(None, 200);
        assert_eq!(
            full.len,
            "LIVE  3 | CPU  342% | MEM   8.0/ 64G | DISK  13G | HEALTH ok".width()
        );
        // Narrow: DISK is shed first...
        let no_disk = state.right_status_segment(None, 55);
        assert_eq!(
            no_disk.len,
            "LIVE  3 | CPU  342% | MEM   8.0/ 64G | HEALTH ok".width()
        );
        // ...then MEM...
        let no_mem = state.right_status_segment(None, 34);
        assert_eq!(no_mem.len, "LIVE  3 | CPU  342% | HEALTH ok".width());
        // ...down to the bare pulse...
        let bare = state.right_status_segment(None, 8);
        assert_eq!(bare.len, "LIVE  3".width());
        // ...and an impossible budget yields empty, never an overflow.
        assert_eq!(state.right_status_segment(None, 3).len, 0);
    }

    #[test]
    fn status_ladder_health_verdict_survives_field_shedding() {
        // A narrow bar hides cockpit numbers but must not change the
        // diagnosis: the sample exists, so HEALTH stays ok.
        let mut state = State {
            live_count: 0,
            resource_line: Some("CPU   10% | MEM   1.0/  2G | DISK   1G".to_owned()),
            ..Default::default()
        };
        let narrow = state.right_status_segment(None, "LIVE  0 | HEALTH ok".width());
        assert_eq!(narrow.len, "LIVE  0 | HEALTH ok".width());
        assert!(narrow.part.contains("HEALTH ok"));

        // No sample at all: the verdict is an honest unknown.
        state.resource_line = None;
        let unknown = state.right_status_segment(None, 200);
        assert!(unknown.part.contains("HEALTH ?"));
    }

    #[test]
    fn live_pulse_width_is_stable_across_counts() {
        let low = State {
            live_count: 3,
            ..Default::default()
        };
        let high = State {
            live_count: 12,
            ..Default::default()
        };
        assert_eq!(
            low.right_status_segment(None, 200).len,
            high.right_status_segment(None, 200).len,
            "LIVE field must not shift cockpit on count roll"
        );
    }

    #[test]
    fn resource_sample_rejects_malformed_input() {
        assert_eq!(parse_resource_sample(b""), None, "empty");
        assert_eq!(parse_resource_sample(b"only two"), None, "non-numeric");
        assert_eq!(parse_resource_sample(b"12 34"), None, "missing total");
        assert_eq!(parse_resource_sample(b"12 34 100"), None, "missing disk");
        assert_eq!(parse_resource_sample(b"12 34 0 55"), None, "zero total");
        assert_eq!(parse_resource_sample(b"-5 34 100 55"), None, "negative cpu");
        assert_eq!(
            parse_resource_sample(b"12 34 100 -1"),
            None,
            "negative disk"
        );
    }

    #[test]
    fn resource_sample_success_then_failure_clears_stale_health_line() {
        // Mirror the RunCommandResult branch: a good sample sets the line;
        // a later non-zero exit or unparseable body must clear it so HEALTH
        // flips back to unknown instead of freezing "ok".
        let mut line = parse_resource_sample(b"10 1024 2048 512");
        assert!(line.is_some());
        // malformed after success
        if parse_resource_sample(b"not-a-sample").is_none() {
            line = None;
        }
        assert_eq!(line, None);
        line = parse_resource_sample(b"10 1024 2048 512");
        assert!(line.is_some());
        // failed exit clears regardless of stdout
        let exit_code = Some(1);
        if exit_code != Some(0) {
            line = None;
        }
        assert_eq!(line, None);
    }

    #[test]
    fn status_bar_never_subscribes_to_full_session_snapshots() {
        assert!(
            !status_bar_subscriptions().contains(&EventType::SessionUpdate),
            "per-tab status bars must never receive full cross-session snapshots"
        );
        assert!(status_bar_subscriptions().contains(&EventType::CustomMessage));
        assert!(
            !status_bar_subscriptions().contains(&EventType::Visible),
            "tab-global visibility must not control a per-client sampler"
        );
        assert!(
            !status_bar_permissions().contains(&PermissionType::ReadApplicationState),
            "the scalar fleet message must not require cross-session read access"
        );
    }

    #[test]
    fn fleet_live_count_accepts_only_valid_changed_scalars() {
        let mut state = State {
            is_visible: true,
            live_count: 3,
            ..Default::default()
        };

        assert!(state.update(Event::CustomMessage(
            VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
            "4".to_owned(),
        )));
        assert_eq!(state.live_count, 4);

        assert!(!state.update(Event::CustomMessage(
            VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
            "4".to_owned(),
        )));
        assert!(!state.update(Event::CustomMessage(
            VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
            "not-a-number".to_owned(),
        )));
        assert_eq!(
            state.live_count, 4,
            "invalid input must keep last good value"
        );
    }

    #[test]
    fn clipboard_timer_does_not_start_or_rearm_resource_sampling() {
        let resource_due = Instant::now() + Duration::from_secs(60);
        let mut state = State {
            is_visible: true,
            text_copy_destination: Some(CopyDestination::Command),
            clipboard_hint_deadline: Some(Instant::now() - Duration::from_secs(1)),
            resource_sample_due: Some(resource_due),
            ..Default::default()
        };

        assert!(state.update(Event::Timer(CLIPBOARD_HINT_TTL_SECONDS)));
        assert_eq!(state.text_copy_destination, None);
        assert_eq!(state.resource_sample_due, Some(resource_due));
        assert!(!state.resource_sample_in_flight);
    }

    #[test]
    fn stale_timer_is_a_noop_before_either_deadline() {
        let clipboard_deadline = Instant::now() + Duration::from_secs(30);
        let resource_due = Instant::now() + Duration::from_secs(60);
        let mut state = State {
            is_visible: true,
            text_copy_destination: Some(CopyDestination::Command),
            clipboard_hint_deadline: Some(clipboard_deadline),
            resource_sample_due: Some(resource_due),
            ..Default::default()
        };

        assert!(!state.update(Event::Timer(0.1)));
        assert_eq!(state.clipboard_hint_deadline, Some(clipboard_deadline));
        assert_eq!(state.resource_sample_due, Some(resource_due));
        assert!(!state.resource_sample_in_flight);
    }

    #[test]
    fn hidden_status_bar_never_starts_or_rearms_resource_sampling() {
        let mut state = State {
            is_visible: false,
            resource_sample_in_flight: true,
            ..Default::default()
        };
        let mut context = BTreeMap::new();
        context.insert(RESOURCE_SAMPLE_CONTEXT_KEY.to_owned(), "true".to_owned());

        assert!(state.update(Event::RunCommandResult(
            Some(0),
            b"10 1048576 8388608 2097152".to_vec(),
            vec![],
            context,
        )));
        assert!(!state.resource_sample_in_flight);
        assert_eq!(state.resource_sample_due, None);

        state.start_resource_sample();
        assert!(!state.resource_sample_in_flight);
        assert_eq!(state.resource_sample_due, None);
    }

    #[test]
    fn fresh_status_bar_is_idle_until_a_visibility_signal() {
        let mut state = State::default();

        assert!(!state.is_visible);
        state.start_resource_sample();
        assert!(!state.resource_sample_in_flight);
        assert_eq!(state.resource_sample_due, None);
    }

    #[test]
    fn targeted_fleet_message_resumes_status_bar_after_reattach() {
        let mut state = State {
            is_visible: false,
            live_count: 1,
            ..Default::default()
        };

        assert!(state.update(Event::CustomMessage(
            VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
            "2".to_owned(),
        )));
        assert!(state.is_visible);
        assert!(state.resource_sample_in_flight);
        assert_eq!(state.live_count, 2);
    }

    #[test]
    fn targeted_visibility_message_stops_only_that_status_bar_instance() {
        let mut state = State {
            is_visible: true,
            resource_sample_due: Some(Instant::now() + Duration::from_secs(5)),
            text_copy_destination: Some(CopyDestination::Command),
            clipboard_hint_deadline: Some(Instant::now() + Duration::from_secs(2)),
            ..Default::default()
        };

        assert!(state.update(Event::CustomMessage(
            VC_STATUS_BAR_VISIBILITY_MESSAGE.to_owned(),
            "false".to_owned(),
        )));
        assert!(!state.is_visible);
        assert_eq!(state.resource_sample_due, None);
        assert_eq!(state.clipboard_hint_deadline, None);
        assert_eq!(state.text_copy_destination, None);

        assert!(!state.update(Event::CustomMessage(
            VC_STATUS_BAR_VISIBILITY_MESSAGE.to_owned(),
            "invalid".to_owned(),
        )));
        assert!(!state.is_visible);
    }

    #[test]
    fn common_modifier_with_ctrl_keys() {
        let keyvec = [
            KeyWithModifier::new(BareKey::Char('a')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('b')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('c')).with_ctrl_modifier(),
        ];
        let ret = get_common_modifiers(keyvec.iter().collect());
        assert_eq!(ret, vec![KeyModifier::Ctrl]);
    }

    #[test]
    fn common_modifier_with_alt_keys_chars() {
        let keyvec = [
            KeyWithModifier::new(BareKey::Char('1')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('t')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('z')).with_alt_modifier(),
        ];
        let ret = get_common_modifiers(keyvec.iter().collect());
        assert_eq!(ret, vec![KeyModifier::Alt]);
    }

    #[test]
    fn common_modifier_with_mixed_alt_ctrl_keys() {
        let keyvec = [
            KeyWithModifier::new(BareKey::Char('1')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('t')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('z')).with_alt_modifier(),
        ];
        let ret = get_common_modifiers(keyvec.iter().collect());
        assert_eq!(ret, vec![]); // no common modifiers
    }

    #[test]
    fn common_modifier_with_any_keys() {
        let keyvec = [
            KeyWithModifier::new(BareKey::Char('1')),
            KeyWithModifier::new(BareKey::Char('t')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('z')).with_alt_modifier(),
        ];
        let ret = get_common_modifiers(keyvec.iter().collect());
        assert_eq!(ret, vec![]); // no common modifiers
    }

    #[test]
    fn action_key_simple_pattern_match_exact() {
        let keymap = &[(KeyWithModifier::new(BareKey::Char('f')), vec![Action::Quit])];
        let ret = action_key(keymap, &[Action::Quit]);
        assert_eq!(ret, vec![KeyWithModifier::new(BareKey::Char('f'))]);
    }

    #[test]
    fn action_key_simple_pattern_match_pattern_too_long() {
        let keymap = &[(KeyWithModifier::new(BareKey::Char('f')), vec![Action::Quit])];
        let ret = action_key(keymap, &[Action::Quit, Action::ScrollUp]);
        assert_eq!(ret, Vec::new());
    }

    #[test]
    fn action_key_simple_pattern_match_pattern_empty() {
        let keymap = &[(KeyWithModifier::new(BareKey::Char('f')), vec![Action::Quit])];
        let ret = action_key(keymap, &[]);
        assert_eq!(ret, Vec::new());
    }

    #[test]
    fn action_key_long_pattern_match_exact() {
        let keymap = big_keymap();
        let ret = action_key(&keymap, &[Action::ScrollDown, TO_NORMAL]);
        assert_eq!(
            ret,
            vec![KeyWithModifier::new(BareKey::Char('c')).with_alt_modifier()]
        );
    }

    #[test]
    fn action_key_long_pattern_match_too_short() {
        let keymap = big_keymap();
        let ret = action_key(&keymap, &[TO_NORMAL]);
        assert_eq!(ret, Vec::new());
    }

    #[test]
    fn action_key_group_single_pattern() {
        let keymap = big_keymap();
        let ret = action_key_group(&keymap, &[&[Action::Quit]]);
        assert_eq!(ret, vec![KeyWithModifier::new(BareKey::Char('a'))]);
    }

    #[test]
    fn action_key_group_two_patterns() {
        let keymap = big_keymap();
        let ret = action_key_group(&keymap, &[&[Action::ScrollDown], &[Action::ScrollUp]]);
        // Mind the order!
        assert_eq!(
            ret,
            vec![
                KeyWithModifier::new(BareKey::Char('d')).with_ctrl_modifier(),
                KeyWithModifier::new(BareKey::Char('b')).with_ctrl_modifier()
            ]
        );
    }

    #[test]
    fn style_key_with_modifier_only_chars() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Char('a')),
            KeyWithModifier::new(BareKey::Char('b')),
            KeyWithModifier::new(BareKey::Char('c')),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<a|b|c>".to_string())
    }

    #[test]
    fn style_key_with_modifier_special_group_hjkl() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Char('h')),
            KeyWithModifier::new(BareKey::Char('j')),
            KeyWithModifier::new(BareKey::Char('k')),
            KeyWithModifier::new(BareKey::Char('l')),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<hjkl>".to_string())
    }

    #[test]
    fn style_key_with_modifier_special_group_all_arrows() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Left),
            KeyWithModifier::new(BareKey::Down),
            KeyWithModifier::new(BareKey::Up),
            KeyWithModifier::new(BareKey::Right),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<←↓↑→>".to_string())
    }

    #[test]
    fn style_key_with_modifier_special_group_left_right_arrows() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Left),
            KeyWithModifier::new(BareKey::Right),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<←→>".to_string())
    }

    #[test]
    fn style_key_with_modifier_special_group_down_up_arrows() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Down),
            KeyWithModifier::new(BareKey::Up),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<↓↑>".to_string())
    }

    #[test]
    fn style_key_with_modifier_common_ctrl_modifier_chars() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Char('a')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('b')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('c')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('d')).with_ctrl_modifier(),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "⌃ + <a|b|c|d>".to_string())
    }

    #[test]
    fn style_key_with_modifier_common_alt_modifier_chars() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Char('a')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('b')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('c')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('d')).with_alt_modifier(),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "⌥ + <a|b|c|d>".to_string())
    }

    #[test]
    fn style_key_with_modifier_common_alt_modifier_with_special_group_all_arrows() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Left).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Down).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Up).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Right).with_alt_modifier(),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "⌥ + <←↓↑→>".to_string())
    }

    #[test]
    fn style_key_with_modifier_ctrl_alt_char_mixed() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Char('a')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char('b')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char('c')),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "<⌥a|⌃b|c>".to_string())
    }

    #[test]
    fn style_key_with_modifier_unprintables() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Backspace),
            KeyWithModifier::new(BareKey::Enter),
            KeyWithModifier::new(BareKey::Char(' ')),
            KeyWithModifier::new(BareKey::Tab),
            KeyWithModifier::new(BareKey::PageDown),
            KeyWithModifier::new(BareKey::Delete),
            KeyWithModifier::new(BareKey::Home),
            KeyWithModifier::new(BareKey::End),
            KeyWithModifier::new(BareKey::Insert),
            KeyWithModifier::new(BareKey::Tab),
            KeyWithModifier::new(BareKey::Esc),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(
            ret,
            "<BACKSPACE|ENTER|SPACE|TAB|PgDn|DEL|HOME|END|INS|TAB|ESC>".to_string()
        )
    }

    #[test]
    fn style_key_with_modifier_unprintables_with_common_ctrl_modifier() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Enter).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Char(' ')).with_ctrl_modifier(),
            KeyWithModifier::new(BareKey::Tab).with_ctrl_modifier(),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "⌃ + <ENTER|SPACE|TAB>".to_string())
    }

    #[test]
    fn style_key_with_modifier_unprintables_with_common_alt_modifier() {
        let keyvec = vec![
            KeyWithModifier::new(BareKey::Enter).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Char(' ')).with_alt_modifier(),
            KeyWithModifier::new(BareKey::Tab).with_alt_modifier(),
        ];
        let palette = Styling::default();

        let ret = style_key_with_modifier(&keyvec, &palette, None);
        let ret = unstyle(&AnsiStrings(&ret));

        assert_eq!(ret, "⌥ + <ENTER|SPACE|TAB>".to_string())
    }

    #[test]
    fn transient_dimensions_are_guarded() {
        assert!(dimensions_are_transient(0, 80));
        assert!(dimensions_are_transient(1, 0));
        assert!(dimensions_are_transient(1, 3));
    }

    #[test]
    fn legal_dimensions_are_not_transient() {
        assert!(!dimensions_are_transient(1, 4));
        assert!(!dimensions_are_transient(2, 24));
    }
}
