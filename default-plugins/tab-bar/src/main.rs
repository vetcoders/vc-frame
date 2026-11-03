mod line;
mod tab;

use std::cmp::{max, min};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;

use tab::{close_hit, dead_tab_positions, get_tab_to_focus, middle_close_hit};
use zellij_tile::prelude::*;

use crate::line::tab_line;
use crate::tab::{
    CLOSE_ARM_TIMEOUT_SECS, CloseDecision, TabCloseAffordance, decide_close, tab_is_contractual,
    tab_style, tab_style_with_close,
};

#[derive(Debug, Default)]
pub struct LinePart {
    part: String,
    len: usize,
    tab_index: Option<usize>,
    /// Display column of the 3-cell close zone inside this part.
    close_start: Option<usize>,
    /// Stable tab id the close zone acts on.
    close_id: Option<usize>,
}

impl LinePart {
    pub fn append(&mut self, to_append: &LinePart) {
        // A zone already on the left keeps its columns. A zone arriving on
        // the right shifts by everything already painted.
        if self.close_start.is_none()
            && let Some(start) = to_append.close_start
        {
            self.close_start = Some(self.len + start);
            self.close_id = to_append.close_id;
        }
        self.part.push_str(&to_append.part);
        self.len += to_append.len;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CloseArm {
    tab_id: usize,
}

#[derive(Default, Debug)]
struct State {
    tabs: Vec<TabInfo>,
    active_tab_idx: usize,
    mode_info: ModeInfo,
    tab_line: Vec<LinePart>,
    hide_swap_layout_indication: bool,
    cached_keybinds: KeybindsVec,
    left_inset: usize,
    dead_tab_positions: BTreeSet<usize>,
    armed_close: Option<CloseArm>,
    stale_close_arm_timers: u64,
}

static ARROW_SEPARATOR: &str = "";

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        self.hide_swap_layout_indication = configuration
            .get("hide_swap_layout_indication")
            .map(|s| s == "true")
            .unwrap_or(false);
        // 🚥 zone: blank columns before the prefix so the bar clears the
        // macOS traffic lights — the same knob compact-bar honours.
        self.left_inset = configuration
            .get("left_inset")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(0);
        set_selectable(false);
        subscribe(&[
            EventType::TabUpdate,
            EventType::ModeUpdate,
            EventType::Mouse,
            EventType::InitialKeybinds,
            EventType::PaneUpdate,
            EventType::Timer,
        ]);
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
            },
            Event::TabUpdate(tabs) => {
                if let Some(active_tab_index) = tabs.iter().position(|t| t.active) {
                    // tabs are indexed starting from 1 so we need to add 1
                    let active_tab_idx = active_tab_index + 1;

                    if self.active_tab_idx != active_tab_idx || self.tabs != tabs {
                        should_render = true;
                    }
                    self.active_tab_idx = active_tab_idx;
                    self.tabs = tabs;
                } else {
                    eprintln!("Could not find active tab.");
                }
            },
            Event::PaneUpdate(manifest) => {
                let dead = dead_tab_positions(&manifest);
                if self.dead_tab_positions != dead {
                    self.dead_tab_positions = dead;
                    should_render = true;
                }
            },
            Event::Timer(_) => {
                if self.stale_close_arm_timers > 0 {
                    self.stale_close_arm_timers -= 1;
                } else if self.armed_close.take().is_some() {
                    should_render = true;
                }
            },
            Event::Mouse(me) => match me {
                Mouse::LeftClick(_, col) => {
                    if let Some(tab_id) = close_hit(&self.tab_line, col) {
                        should_render = self.request_close(tab_id);
                    } else if let Some(idx) =
                        get_tab_to_focus(&self.tab_line, self.active_tab_idx, col)
                    {
                        self.disarm_close();
                        switch_tab_to(idx.try_into().unwrap());
                        should_render = true;
                    }
                },
                Mouse::MiddleClick(_, col) => {
                    if let Some(tab_id) = middle_close_hit(&self.tab_line, col) {
                        should_render = self.request_close(tab_id);
                    }
                },
                Mouse::ScrollUp(_) => {
                    switch_tab_to(min(self.active_tab_idx + 1, self.tabs.len()) as u32);
                },
                Mouse::ScrollDown(_) => {
                    switch_tab_to(max(self.active_tab_idx.saturating_sub(1), 1) as u32);
                },
                _ => {},
            },
            _ => {
                eprintln!("Got unrecognized event: {:?}", event);
            },
        }
        if self.tabs.is_empty() {
            // no need to render if we have no tabs, this can sometimes happen on startup before we
            // get the tab update and then we definitely don't want to render
            should_render = false;
        }
        should_render
    }

    fn render(&mut self, _rows: usize, cols: usize) {
        if self.tabs.is_empty() {
            return;
        }
        let mut all_tabs: Vec<LinePart> = vec![];
        let mut active_tab_index = 0;
        let mut is_alternate_tab = false;
        for t in &mut self.tabs {
            let mut tabname = t.name.clone();
            if t.active && self.mode_info.mode == InputMode::RenameTab {
                if tabname.is_empty() {
                    tabname = String::from("Enter name...");
                }
                active_tab_index = t.position;
            } else if t.active {
                active_tab_index = t.position;
            }
            let close_id = Some(t.tab_id);
            let affordance = TabCloseAffordance {
                closable: !tab_is_contractual(&t.name),
                dead: self.dead_tab_positions.contains(&t.position),
                armed: self.armed_close.is_some_and(|arm| arm.tab_id == t.tab_id),
                close_id,
            };
            let colors = self.mode_info.style.colors;
            let capabilities = self.mode_info.capabilities;
            let tab = if tab_is_contractual(&t.name) {
                tab_style(tabname, t, is_alternate_tab, colors, capabilities)
            } else {
                tab_style_with_close(
                    tabname,
                    t,
                    is_alternate_tab,
                    colors,
                    capabilities,
                    affordance,
                )
            };
            is_alternate_tab = !is_alternate_tab;
            all_tabs.push(tab);
        }

        let background = self.mode_info.style.colors.text_unselected.background;

        self.tab_line = tab_line(crate::line::TabLineParams {
            session_name: self.mode_info.session_name.as_deref(),
            all_tabs,
            active_tab_index,
            cols: cols.saturating_sub(1),
            palette: self.mode_info.style.colors,
            capabilities: self.mode_info.capabilities,
            hide_session_name: self.mode_info.style.hide_session_name,
            tab_info: self.tabs.iter().find(|t| t.active),
            mode_info: &self.mode_info,
            hide_swap_layout_indicator: self.hide_swap_layout_indication,
            background: &background,
            left_inset: self.left_inset,
        });

        let output = self
            .tab_line
            .iter()
            .fold(String::new(), |output, part| output + &part.part);

        match background {
            PaletteColor::Rgb((r, g, b)) => {
                print!("{}\u{1b}[48;2;{};{};{}m\u{1b}[0K", output, r, g, b);
            },
            PaletteColor::EightBit(color) => {
                print!("{}\u{1b}[48;5;{}m\u{1b}[0K", output, color);
            },
        }
    }
}

impl State {
    /// Two-phase close. A dead tab closes on the first click. A live tab
    /// arms, and only a second click on the same id confirms.
    fn request_close(&mut self, tab_id: usize) -> bool {
        let dead = self
            .tabs
            .iter()
            .any(|tab| tab.tab_id == tab_id && self.dead_tab_positions.contains(&tab.position));
        let armed = self.armed_close.map(|arm| (arm.tab_id, false));
        match decide_close(armed, tab_id, false, dead) {
            CloseDecision::Arm { tab_id, .. } => {
                self.arm_close(tab_id);
                true
            },
            CloseDecision::Confirm { tab_id, .. }
            | CloseDecision::CloseImmediately { tab_id, .. } => {
                self.disarm_close();
                self.commit_close(tab_id);
                true
            },
        }
    }

    fn arm_close(&mut self, tab_id: usize) {
        if self.armed_close.is_some() {
            self.stale_close_arm_timers = self.stale_close_arm_timers.saturating_add(1);
        }
        self.armed_close = Some(CloseArm { tab_id });
        set_timeout(CLOSE_ARM_TIMEOUT_SECS);
    }

    fn disarm_close(&mut self) {
        if self.armed_close.take().is_some() {
            self.stale_close_arm_timers = self.stale_close_arm_timers.saturating_add(1);
        }
    }

    fn commit_close(&self, tab_id: usize) {
        close_tab_with_id(tab_id as u64);
    }
}
