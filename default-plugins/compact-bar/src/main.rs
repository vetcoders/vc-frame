mod action_types;
mod clipboard_utils;
mod keybind_utils;
mod line;
mod tab;
mod tooltip;

use std::cmp::{max, min};
use std::collections::BTreeMap;
use std::convert::TryInto;

use tab::get_tab_to_focus;
use zellij_tile::prelude::*;

use crate::clipboard_utils::{system_clipboard_error, text_copied_hint};
use crate::line::tab_line;
use crate::tab::tab_style;
use crate::tooltip::TooltipRenderer;

static ARROW_SEPARATOR: &str = "";

const CONFIG_IS_TOOLTIP: &str = "is_tooltip";
const CONFIG_TOGGLE_TOOLTIP_KEY: &str = "tooltip";
const CONFIG_BRAND_TEXT: &str = "brand_text";
const CONFIG_BRAND_TEXT_SHORT: &str = "brand_text_short";
/// Columns of blank bar before the brand chip — the 🚥 zone. In the native
/// transparent window (Alacritty preset) the macOS traffic lights float over
/// the first row; the inset shifts the whole bar clear of them. 9 columns
/// ≈ 65–70px at a 13pt monospace.
const CONFIG_LEFT_INSET: &str = "left_inset";
const MSG_TOGGLE_TOOLTIP: &str = "toggle_tooltip";
// the status-bar shows up in the pane manifest as "vc-frame:status-bar" when
// loaded by url and as "status-bar" when loaded through its config alias
const STATUS_BAR_PLUGIN_URLS: [&str; 3] =
    ["vc-frame:status-bar", "zellij:status-bar", "status-bar"];
/// How long the clipboard notification ("Text copied...") stays on the bar
/// before dismissing itself without requiring user input.
const CLIPBOARD_HINT_TTL_SECONDS: f64 = 2.0;
const MSG_TOGGLE_PERSISTED_TOOLTIP: &str = "toggle_persisted_tooltip";
const MSG_LAUNCH_TOOLTIP: &str = "launch_tooltip_if_not_launched";
/// Sentinel tab_index marking the clickable Composer chip on the tab line —
/// a real tab can never occupy this index. Checked before tab resolution so
/// it never reaches switch_tab_to.
pub const COMPOSER_CLICK_SENTINEL: usize = usize::MAX;
/// Sentinel tab_index for the ䷅ LIVE fleet chip — click opens the Gallery.
pub const GALLERY_CLICK_SENTINEL: usize = usize::MAX - 1;
/// Sentinel tab_index for the ⌬ Agents chip — click lands in the Agents
/// station (the NOW-view of the fleet; ䷅ LIVE → Gallery is the WAS-view)
/// and floats the Dispatcher shell over it.
pub const AGENTS_CLICK_SENTINEL: usize = usize::MAX - 2;
/// The per-session station tab where dispatched agents stack up. One tab,
/// many stacked panes: a collapsed title-bar is a list row, an expanded
/// pane is the agent's full terminal.
const AGENTS_TAB_NAME: &str = "Agents";
/// The Dispatcher is a bare login shell in a named floating pane — dispatch
/// is typing vibecrafted CLI commands (`vc-init codex`) into it. Deliberate
/// zsh fallback: the server may run without SHELL in its env, and /bin/sh
/// with no profile is exactly the trap we refuse to spawn.
const DISPATCHER_COMMAND: &str = r#"exec "${SHELL:-/bin/zsh}" -l"#;
/// Same drafting contract as the Alt+e keybind in the default config: draft
/// in $VC_COMPOSER (or $EDITOR), land the text in the underlying pane
/// unexecuted, clean up. VC_COMPOSER expands unquoted on purpose — it is a
/// command line ("open -W -n -a Pensieve", "pensieve --wait"), not a path.
const COMPOSER_COMMAND: &str = r#"f=$(mktemp "${TMPDIR:-/tmp}/vc-composer.XXXXXX") || exit 1; ${VC_COMPOSER:-${EDITOR:-vim}} "$f"; if [ -s "$f" ]; then vc-frame action toggle-floating-panes; vc-frame action write-chars "$(cat "$f")"; fi; rm -f -- "$f""#;
/// Gallery v0: the aicx wizard is the cross-agent session gallery — one
/// timeline over claude/codex/grok/gemini histories. The chip opens it in a
/// floating pane; a missing aicx leaves an explanatory line instead of a
/// silently vanishing pane.
const GALLERY_COMMAND: &str = r#"command -v aicx >/dev/null 2>&1 && exec aicx wizard; echo "aicx not found on PATH — the Gallery rides on aicx (Loctree/aicx)"; read -r _"#;
/// Status-bucket session names — wire contract with the triage reaper, kept
/// identical to `run_triage.rs` (see the session-manager's matching pin).
/// Bucket sessions never count toward the fleet pulse.
const BUCKET_SESSION_NAMES: [&str; 3] = ["Finalized runs", "Failed runs", "Needs attention"];

#[derive(Debug, Default)]
pub struct LinePart {
    part: String,
    len: usize,
    tab_index: Option<usize>,
}

#[derive(Default)]
struct State {
    // Tab state
    tabs: Vec<TabInfo>,
    active_tab_idx: usize,

    // Display state
    mode_info: ModeInfo,
    tab_line: Vec<LinePart>,
    display_area_rows: usize,
    display_area_cols: usize,

    // Clipboard state
    text_copy_destination: Option<CopyDestination>,
    display_system_clipboard_failure: bool,
    pending_clipboard_hint_timers: usize,
    // when a status-bar is also present in the layout it owns the clipboard
    // hint line, so we defer to it instead of showing the hint twice
    status_bar_is_present: bool,

    // Plugin configuration
    config: BTreeMap<String, String>,
    own_plugin_id: Option<u32>,
    toggle_tooltip_key: Option<String>,
    brand_text: Option<String>,
    brand_text_short: Option<String>,
    left_inset: usize,

    // Tooltip state
    is_tooltip: bool,
    tooltip_is_active: bool,
    persist: bool,
    is_first_run: bool,
    own_tab_index: Option<usize>,
    own_client_id: u16,

    // Keybinding cache
    cached_keybinds: KeybindsVec,

    // Fleet pulse: live agent-process count across every session (the rail's
    // LIVE number promoted to the bar). Fed by SessionUpdate.
    live_count: usize,
}

struct TabRenderData {
    tabs: Vec<LinePart>,
    active_tab_index: usize,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        let plugin_ids = get_plugin_ids();
        self.own_plugin_id = Some(plugin_ids.plugin_id);
        self.own_client_id = plugin_ids.client_id;
        self.initialize_configuration(configuration);
        // The fleet pulse needs the cross-session snapshot (SessionUpdate).
        request_permission(&[PermissionType::ReadApplicationState]);
        self.setup_subscriptions();
        self.configure_keybinds();
    }

    fn update(&mut self, event: Event) -> bool {
        self.is_first_run = false;

        match event {
            Event::InitialKeybinds(keybinds) => {
                self.cached_keybinds = keybinds;
                if !self.cached_keybinds.is_empty() {
                    self.mode_info.keybinds = self.cached_keybinds.clone();
                }
                true
            },
            Event::ModeUpdate(mut mode_info) => {
                if mode_info.keybinds.is_empty() && !self.cached_keybinds.is_empty() {
                    mode_info.keybinds = self.cached_keybinds.clone();
                } else if !mode_info.keybinds.is_empty() {
                    self.cached_keybinds = mode_info.keybinds.clone();
                }
                self.handle_mode_update(mode_info)
            },
            Event::TabUpdate(tabs) => self.handle_tab_update(tabs),
            Event::PaneUpdate(pane_manifest) => self.handle_pane_update(pane_manifest),
            Event::Mouse(mouse_event) => {
                self.handle_mouse_event(mouse_event);
                false
            },
            Event::CopyToClipboard(copy_destination) => {
                self.handle_clipboard_copy(copy_destination)
            },
            Event::SystemClipboardFailure => self.handle_clipboard_failure(),
            Event::Timer(_) => self.handle_clipboard_hint_timeout(),
            Event::InputReceived => self.handle_input_received(),
            Event::SessionUpdate(sessions, _) => {
                let live_count = fleet_live_count(&sessions);
                let should_render = self.live_count != live_count;
                self.live_count = live_count;
                should_render
            },
            Event::PermissionRequestResult(_) => true,
            _ => false,
        }
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if self.is_tooltip && message.is_private {
            self.handle_tooltip_pipe(message);
        } else if message.name == MSG_TOGGLE_TOOLTIP
            && message.is_private
            && self.toggle_tooltip_key.is_some()
            // only launch once per plugin instance
            && self.own_tab_index == Some(self.active_tab_idx.saturating_sub(1))
            // only launch once per client of plugin instance
            && Some(format!("{}", self.own_client_id)) == message.payload
        {
            self.toggle_persisted_tooltip(self.mode_info.mode);
        }
        false
    }

    fn render(&mut self, rows: usize, cols: usize) {
        if self.is_tooltip {
            self.render_tooltip(rows, cols);
        } else {
            self.render_tab_line(cols);
        }
    }
}

impl State {
    fn initialize_configuration(&mut self, configuration: BTreeMap<String, String>) {
        self.config = configuration.clone();
        self.is_tooltip = self.parse_bool_config(CONFIG_IS_TOOLTIP, false);

        if !self.is_tooltip {
            if let Some(tooltip_toggle_key) = configuration.get(CONFIG_TOGGLE_TOOLTIP_KEY) {
                self.toggle_tooltip_key = Some(tooltip_toggle_key.clone());
            }
            self.brand_text = configuration.get(CONFIG_BRAND_TEXT).cloned();
            self.brand_text_short = configuration.get(CONFIG_BRAND_TEXT_SHORT).cloned();
            self.left_inset = configuration
                .get(CONFIG_LEFT_INSET)
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }

        if self.is_tooltip {
            self.is_first_run = true;
        }
    }

    fn setup_subscriptions(&self) {
        set_selectable(false);

        let events = if self.is_tooltip {
            vec![
                EventType::ModeUpdate,
                EventType::TabUpdate,
                EventType::InitialKeybinds,
            ]
        } else {
            vec![
                EventType::TabUpdate,
                EventType::PaneUpdate,
                EventType::ModeUpdate,
                EventType::Mouse,
                EventType::CopyToClipboard,
                EventType::InputReceived,
                EventType::SystemClipboardFailure,
                EventType::InitialKeybinds,
                EventType::Timer,
                EventType::SessionUpdate,
                EventType::PermissionRequestResult,
            ]
        };

        subscribe(&events);
    }

    fn configure_keybinds(&self) {
        if !self.is_tooltip
            && self.toggle_tooltip_key.is_some()
            && let Some(toggle_key) = &self.toggle_tooltip_key
        {
            reconfigure(
                bind_toggle_key_config(toggle_key, self.own_client_id),
                false,
            );
        }
    }

    fn parse_bool_config(&self, key: &str, default: bool) -> bool {
        self.config
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    // Event handlers
    fn handle_mode_update(&mut self, mode_info: ModeInfo) -> bool {
        let should_render = self.mode_info != mode_info;
        let old_mode = self.mode_info.mode;
        let new_mode = mode_info.mode;
        let base_mode = mode_info.base_mode.unwrap_or(InputMode::Normal);

        self.mode_info = mode_info;

        if self.is_tooltip {
            self.handle_tooltip_mode_update(old_mode, new_mode, base_mode);
        } else {
            self.handle_main_mode_update(new_mode, base_mode);
        }

        should_render
    }

    fn handle_main_mode_update(&self, new_mode: InputMode, base_mode: InputMode) {
        if self.toggle_tooltip_key.is_some()
            && new_mode != base_mode
            && !self.is_restricted_mode(new_mode)
        {
            self.launch_tooltip_if_not_launched(new_mode);
        }
    }

    fn handle_tooltip_mode_update(
        &mut self,
        old_mode: InputMode,
        new_mode: InputMode,
        base_mode: InputMode,
    ) {
        if !self.persist && (new_mode == base_mode || self.is_restricted_mode(new_mode)) {
            close_self();
        } else if new_mode != old_mode || self.persist {
            self.update_tooltip_for_mode_change(new_mode);
        }
    }

    fn handle_tab_update(&mut self, tabs: Vec<TabInfo>) -> bool {
        self.update_display_area(&tabs);

        if let Some(active_tab_index) = tabs.iter().position(|t| t.active) {
            let active_tab_idx = active_tab_index + 1; // Convert to 1-based indexing
            let should_render = self.active_tab_idx != active_tab_idx || self.tabs != tabs;

            if self.is_tooltip && self.active_tab_idx != active_tab_idx {
                self.move_tooltip_to_new_tab(active_tab_idx);
            }

            self.active_tab_idx = active_tab_idx;
            self.tabs = tabs;
            should_render
        } else {
            false
        }
    }

    fn handle_pane_update(&mut self, pane_manifest: PaneManifest) -> bool {
        self.status_bar_is_present = self.detect_status_bar_presence(&pane_manifest);
        if self.toggle_tooltip_key.is_some() {
            let previous_tooltip_state = self.tooltip_is_active;
            self.tooltip_is_active = self.detect_tooltip_presence(&pane_manifest);
            self.own_tab_index = self.find_own_tab_index(&pane_manifest);
            previous_tooltip_state != self.tooltip_is_active
        } else {
            false
        }
    }

    fn handle_mouse_event(&mut self, mouse_event: Mouse) {
        if self.is_tooltip {
            return;
        }

        match mouse_event {
            Mouse::LeftClick(_, col) => self.handle_tab_click(col),
            Mouse::ScrollUp(_) => self.scroll_tab_up(),
            Mouse::ScrollDown(_) => self.scroll_tab_down(),
            _ => {},
        }
    }

    fn handle_clipboard_copy(&mut self, copy_destination: CopyDestination) -> bool {
        if self.is_tooltip || self.status_bar_is_present {
            return false;
        }

        let should_render = match self.text_copy_destination {
            Some(current) => current != copy_destination,
            None => true,
        };

        self.text_copy_destination = Some(copy_destination);
        self.pending_clipboard_hint_timers += 1;
        set_timeout(CLIPBOARD_HINT_TTL_SECONDS);
        should_render
    }

    fn handle_clipboard_failure(&mut self) -> bool {
        if self.is_tooltip || self.status_bar_is_present {
            return false;
        }

        self.display_system_clipboard_failure = true;
        self.pending_clipboard_hint_timers += 1;
        set_timeout(CLIPBOARD_HINT_TTL_SECONDS);
        true
    }

    fn handle_clipboard_hint_timeout(&mut self) -> bool {
        // only the timer set by the most recent notification may dismiss it -
        // earlier timers are stale (the TTL restarted)
        self.pending_clipboard_hint_timers = self.pending_clipboard_hint_timers.saturating_sub(1);
        if self.pending_clipboard_hint_timers == 0
            && (self.text_copy_destination.is_some() || self.display_system_clipboard_failure)
        {
            self.clear_clipboard_state();
            true
        } else {
            false
        }
    }

    fn handle_input_received(&mut self) -> bool {
        if self.is_tooltip {
            return false;
        }

        let should_render =
            self.text_copy_destination.is_some() || self.display_system_clipboard_failure;
        self.clear_clipboard_state();
        should_render
    }

    fn handle_tooltip_pipe(&mut self, message: PipeMessage) {
        if message.name == MSG_TOGGLE_PERSISTED_TOOLTIP {
            if self.is_first_run {
                self.persist = true;
            } else {
                #[cfg(target_family = "wasm")]
                close_self();
            }
        }
    }

    // Helper methods
    fn update_display_area(&mut self, tabs: &[TabInfo]) {
        for tab in tabs {
            if tab.active {
                self.display_area_rows = tab.display_area_rows;
                self.display_area_cols = tab.display_area_columns;
                break;
            }
        }
    }

    fn detect_status_bar_presence(&self, pane_manifest: &PaneManifest) -> bool {
        pane_manifest.panes.values().flatten().any(|pane| {
            pane.plugin_url
                .as_deref()
                .is_some_and(|url| STATUS_BAR_PLUGIN_URLS.contains(&url))
        })
    }

    fn detect_tooltip_presence(&self, pane_manifest: &PaneManifest) -> bool {
        for panes in pane_manifest.panes.values() {
            for pane in panes {
                if (pane.plugin_url.as_deref() == Some("vc-frame:compact-bar")
                    || pane.plugin_url.as_deref() == Some("zellij:compact-bar"))
                    && pane.pane_x != pane.pane_content_x
                {
                    return true;
                }
            }
        }
        false
    }

    fn find_own_tab_index(&self, pane_manifest: &PaneManifest) -> Option<usize> {
        for (tab_index, panes) in &pane_manifest.panes {
            for pane in panes {
                if pane.is_plugin && Some(pane.id) == self.own_plugin_id {
                    return Some(*tab_index);
                }
            }
        }
        None
    }

    fn handle_tab_click(&self, col: usize) {
        if self.sentinel_clicked(col, COMPOSER_CLICK_SENTINEL) {
            open_composer();
            return;
        }
        if self.sentinel_clicked(col, GALLERY_CLICK_SENTINEL) {
            open_gallery();
            return;
        }
        if self.sentinel_clicked(col, AGENTS_CLICK_SENTINEL) {
            self.open_agents_station();
            return;
        }
        if let Some(tab_idx) = get_tab_to_focus(&self.tab_line, self.active_tab_idx, col) {
            switch_tab_to(tab_idx.try_into().unwrap());
        }
    }

    /// Click path of the ⌬ Agents chip: land in the Agents station tab
    /// (creating it on first use) and float the Dispatcher shell over it —
    /// "klikasz i masz gdzie pisać". Known v0 wrinkle: tab creation and the
    /// floating pane race through separate server paths, so on the very
    /// first click the Dispatcher may float over the tab you came from.
    fn open_agents_station(&self) {
        if self.tabs.iter().any(|tab| tab.name == AGENTS_TAB_NAME) {
            go_to_tab_name(AGENTS_TAB_NAME);
        } else {
            new_tab(Some(AGENTS_TAB_NAME), None);
        }
        open_dispatcher();
    }

    fn sentinel_clicked(&self, col: usize, sentinel: usize) -> bool {
        let mut offset = 0;
        for part in &self.tab_line {
            if part.tab_index == Some(sentinel) && col >= offset && col < offset + part.len {
                return true;
            }
            offset += part.len;
        }
        false
    }

    fn scroll_tab_up(&self) {
        let next_tab = min(self.active_tab_idx + 1, self.tabs.len());
        switch_tab_to(next_tab as u32);
    }
}

/// The Dispatcher: a named floating login shell — the operator's dispatch
/// console (vibecrafted CLI is the dispatch language, the pane is the desk).
fn open_dispatcher() {
    let command = CommandToRun::new_with_args("sh", vec!["-c", DISPATCHER_COMMAND]);
    if let Some(PaneId::Terminal(terminal_pane_id)) =
        open_command_pane_floating(command, None, BTreeMap::new())
    {
        rename_terminal_pane(terminal_pane_id, "Dispatcher");
    }
}

/// Click path of the Composer chip — identical contract to the Alt+e bind.
fn open_composer() {
    let command = CommandToRun::new_with_args("sh", vec!["-c", COMPOSER_COMMAND]);
    if let Some(PaneId::Terminal(terminal_pane_id)) =
        open_command_pane_floating(command, None, BTreeMap::new())
    {
        rename_terminal_pane(terminal_pane_id, "Composer");
    }
}

/// Click path of the ䷅ LIVE chip: the Gallery — cross-agent session history
/// in a floating pane (aicx wizard, Gallery v0).
fn open_gallery() {
    let command = CommandToRun::new_with_args("sh", vec!["-c", GALLERY_COMMAND]);
    if let Some(PaneId::Terminal(terminal_pane_id)) =
        open_command_pane_floating(command, None, BTreeMap::new())
    {
        rename_terminal_pane(terminal_pane_id, "Gallery");
    }
}

/// The rail's LIVE number, recomputed from the same predicate the rail uses:
/// a tab is "live" when it holds at least one non-plugin pane that has not
/// exited and is not held. Bucket sessions are pinned drawers, not fleet.
fn fleet_live_count(sessions: &[SessionInfo]) -> usize {
    sessions
        .iter()
        .filter(|session| !BUCKET_SESSION_NAMES.contains(&session.name.as_str()))
        .map(|session| {
            session
                .tabs
                .iter()
                .filter(|tab| {
                    session.panes.panes.get(&tab.position).is_some_and(|panes| {
                        panes
                            .iter()
                            .any(|pane| !pane.is_plugin && !pane.exited && !pane.is_held)
                    })
                })
                .count()
        })
        .sum()
}

impl State {
    fn scroll_tab_down(&self) {
        let prev_tab = max(self.active_tab_idx.saturating_sub(1), 1);
        switch_tab_to(prev_tab as u32);
    }

    fn clear_clipboard_state(&mut self) {
        self.text_copy_destination = None;
        self.display_system_clipboard_failure = false;
    }

    fn is_restricted_mode(&self, mode: InputMode) -> bool {
        matches!(
            mode,
            InputMode::Locked
                | InputMode::EnterSearch
                | InputMode::RenameTab
                | InputMode::RenamePane
                | InputMode::Prompt
                | InputMode::Tmux
        )
    }

    // Tooltip operations
    fn toggle_persisted_tooltip(&self, new_mode: InputMode) {
        #[allow(unused_variables)]
        let message = self
            .create_tooltip_message(MSG_TOGGLE_PERSISTED_TOOLTIP, new_mode)
            .with_args(self.create_persist_args());

        #[cfg(target_family = "wasm")]
        pipe_message_to_plugin(message);
    }

    fn launch_tooltip_if_not_launched(&self, new_mode: InputMode) {
        let message = self.create_tooltip_message(MSG_LAUNCH_TOOLTIP, new_mode);
        pipe_message_to_plugin(message);
    }

    fn create_tooltip_message(&self, name: &str, mode: InputMode) -> MessageToPlugin {
        let mut tooltip_config = self.config.clone();
        tooltip_config.insert(CONFIG_IS_TOOLTIP.to_string(), "true".to_string());

        MessageToPlugin::new(name)
            .with_plugin_url("vc-frame:OWN_URL")
            .with_plugin_config(tooltip_config)
            .with_floating_pane_coordinates(self.calculate_tooltip_coordinates())
            .new_plugin_instance_should_have_pane_title(format!("{:?}", mode))
    }

    fn create_persist_args(&self) -> BTreeMap<String, String> {
        let mut args = BTreeMap::new();
        args.insert("persist".to_string(), String::new());
        args
    }

    fn update_tooltip_for_mode_change(&self, new_mode: InputMode) {
        if let Some(plugin_id) = self.own_plugin_id {
            let coordinates = self.calculate_tooltip_coordinates();
            change_floating_panes_coordinates(vec![(PaneId::Plugin(plugin_id), coordinates)]);
            rename_plugin_pane(plugin_id, format!("{:?}", new_mode));
        }
    }

    fn move_tooltip_to_new_tab(&self, new_tab_index: usize) {
        if let Some(plugin_id) = self.own_plugin_id {
            break_panes_to_tab_with_index(
                &[PaneId::Plugin(plugin_id)],
                new_tab_index.saturating_sub(1), // Convert to 0-based indexing
                false,
            );
        }
    }

    fn calculate_tooltip_coordinates(&self) -> FloatingPaneCoordinates {
        let tooltip_renderer = TooltipRenderer::new(&self.mode_info);
        let (tooltip_rows, tooltip_cols) =
            tooltip_renderer.calculate_dimensions(self.mode_info.mode);

        let width = tooltip_cols + 4; // 2 for borders, 2 for padding
        let height = tooltip_rows + 2; // 2 for borders
        let x_position = 2;
        let y_position = self.display_area_rows.saturating_sub(height + 2);

        FloatingPaneCoordinates::new(
            Some(x_position.to_string()),
            Some(y_position.to_string()),
            Some(width.to_string()),
            Some(height.to_string()),
            Some(true),
            Some(false),
        )
        .unwrap_or_default()
    }

    // Rendering
    fn render_tooltip(&self, rows: usize, cols: usize) {
        let tooltip_renderer = TooltipRenderer::new(&self.mode_info);
        tooltip_renderer.render(rows, cols);
    }

    fn render_tab_line(&mut self, cols: usize) {
        if let Some(copy_destination) = self.text_copy_destination {
            self.render_clipboard_hint(copy_destination);
        } else if self.display_system_clipboard_failure {
            self.render_clipboard_error();
        } else {
            self.render_tabs(cols);
        }
    }

    fn render_clipboard_hint(&self, copy_destination: CopyDestination) {
        let hint = text_copied_hint(copy_destination).part;
        self.render_background_with_text(&hint);
    }

    fn render_clipboard_error(&self) {
        let hint = system_clipboard_error().part;
        self.render_background_with_text(&hint);
    }

    fn render_background_with_text(&self, text: &str) {
        let background = self.mode_info.style.colors.text_unselected.background;
        match background {
            PaletteColor::Rgb((r, g, b)) => {
                print!("{}\u{1b}[48;2;{};{};{}m\u{1b}[0K", text, r, g, b);
            },
            PaletteColor::EightBit(color) => {
                print!("{}\u{1b}[48;5;{}m\u{1b}[0K", text, color);
            },
        }
    }

    fn render_tabs(&mut self, cols: usize) {
        if self.tabs.is_empty() {
            return;
        }

        let tab_data = self.prepare_tab_data();
        let config = crate::line::TabLineConfig {
            session_name: self.mode_info.session_name.to_owned(),
            hide_session_name: self.mode_info.style.hide_session_name,
            mode: self.mode_info.mode,
            toggle_tooltip_key: self.toggle_tooltip_key.clone(),
            tooltip_is_active: self.tooltip_is_active,
            brand_text: self.brand_text.clone(),
            brand_text_short: self.brand_text_short.clone(),
            live_count: self.live_count,
            left_inset: self.left_inset,
        };
        self.tab_line = tab_line(&self.mode_info, tab_data, cols, config);

        let output = self
            .tab_line
            .iter()
            .fold(String::new(), |acc, part| acc + &part.part);

        self.render_background_with_text(&output);
    }

    fn prepare_tab_data(&self) -> TabRenderData {
        let mut all_tabs = Vec::new();
        let mut active_tab_index = 0;
        let mut is_alternate_tab = false;

        for tab in &self.tabs {
            let tab_name = self.get_tab_display_name(tab);

            if tab.active {
                active_tab_index = tab.position;
            }

            let styled_tab = tab_style(
                tab_name,
                tab,
                is_alternate_tab,
                self.mode_info.style.colors,
                self.mode_info.capabilities,
            );

            is_alternate_tab = !is_alternate_tab;
            all_tabs.push(styled_tab);
        }

        TabRenderData {
            tabs: all_tabs,
            active_tab_index,
        }
    }

    fn get_tab_display_name(&self, tab: &TabInfo) -> String {
        let mut tab_name = tab.name.clone();
        if tab.active && self.mode_info.mode == InputMode::RenameTab && tab_name.is_empty() {
            tab_name = "Enter name...".to_string();
        }
        tab_name
    }
}

fn bind_toggle_key_config(toggle_key: &str, client_id: u16) -> String {
    format!(
        r#"
        keybinds {{
            shared {{
                bind "{}" {{
                  MessagePlugin "compact-bar" {{
                      name "toggle_tooltip"
                      tooltip "{}"
                      payload "{}"
                  }}
                }}
            }}
        }}
    "#,
        toggle_key, toggle_key, client_id
    )
}
