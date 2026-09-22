mod action_types;
mod clipboard_utils;
mod keybind_utils;
mod line;
mod panel_drawer;
mod tab;
mod tooltip;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;

use tab::get_tab_to_focus;
use zellij_tile::prelude::*;

use crate::clipboard_utils::{system_clipboard_error, text_copied_hint};
use crate::line::{project_guest_organs, tab_line};
use crate::panel_drawer::{
    CONFIG_IS_PANEL_DRAWER, DrawerCommand, MSG_TOGGLE_PANEL_DRAWER, PANEL_DRAWER_TITLE,
    PanelDrawer, current_tab_position, detect_panel_drawer, floating_panes_visible,
    inventory_for_tab, panel_drawer_coordinates, render_drawer,
};
use crate::tab::tab_style;
use crate::tooltip::TooltipRenderer;

static ARROW_SEPARATOR: &str = "";

const CONFIG_IS_TOOLTIP: &str = "is_tooltip";
const CONFIG_TOGGLE_TOOLTIP_KEY: &str = "tooltip";
const CONFIG_BRAND_TEXT: &str = "brand_text";
const CONFIG_BRAND_TEXT_SHORT: &str = "brand_text_short";
/// Columns of blank bar before the brand chip — the 🚥 zone. In the native
/// transparent window (Alacritty preset) the macOS traffic lights float over
/// the first row; the inset shifts the whole bar clear of them. Default
/// layouts use 6 columns at standard monospace (~13pt); large fonts may want
/// 9–12 via layout config.
const CONFIG_LEFT_INSET: &str = "left_inset";
const MSG_TOGGLE_TOOLTIP: &str = "toggle_tooltip";
const MSG_OPEN_QUICK_CMD: &str = "vc_quick_cmd";
/// Context key stamped on the `ToggleTheme` action the ☾/☼ chip dispatches,
/// so the originating plugin is identifiable in server logs.
const THEME_ACTION_CONTEXT_KEY: &str = "vc_frame_theme";
// the status-bar shows up in the pane manifest as "vc-frame:status-bar" when
// loaded by url and as "status-bar" when loaded through its config alias
const STATUS_BAR_PLUGIN_URLS: [&str; 3] =
    ["vc-frame:status-bar", "zellij:status-bar", "status-bar"];
/// How long the clipboard notification ("Text copied...") stays on the bar
/// before dismissing itself without requiring user input.
const CLIPBOARD_HINT_TTL_SECONDS: f64 = 2.0;
const VC_CHROME_VISIBILITY_MESSAGE: &str = "vc.status-bar-visibility.v1";
const VC_CHROME_HEARTBEAT_MESSAGE: &str = "vc.fleet-live-count.v1";
const MSG_TOGGLE_PERSISTED_TOOLTIP: &str = "toggle_persisted_tooltip";
const MSG_LAUNCH_TOOLTIP: &str = "launch_tooltip_if_not_launched";
/// Sentinel tab_index marking the clickable Composer chip on the tab line —
/// a real tab can never occupy this index. Checked before tab resolution so
/// it never reaches switch_tab_to.
pub const COMPOSER_CLICK_SENTINEL: usize = usize::MAX;
/// Sentinel tab_index for the Quick cmd chip — click opens a non-ephemeral
/// mini console (interactive terminal) over the current tab. LIVE pulse
/// lives on the bottom status-bar — no tool rides on it.
pub const AGENTS_CLICK_SENTINEL: usize = usize::MAX - 2;
/// Sentinel for the frame theme switcher (☾/☼) at the far-right edge of the bar.
pub const THEME_CLICK_SENTINEL: usize = usize::MAX - 3;
/// Sentinel for the counted Panels chip — opens the right-edge drawer.
pub const PANELS_CLICK_SENTINEL: usize = usize::MAX - 4;
/// Sentinel for the Voc host-console chip immediately left of Composer.
/// Click logs one receipt line and does nothing else until C5 wires the pane.
pub const VOC_CLICK_SENTINEL: usize = usize::MAX - 5;
/// One-line plugin-log receipt for a Voc chip click (no pane, no pipe).
const VOC_CLICK_RECEIPT: &str = "compact-bar: Voc chip click receipt (host pane deferred to C5)";
/// Pane title for the Quick cmd mini console (matches the bar chip glyph).
const QUICK_CMD_PANE_NAME: &str = "❯_ Quick cmd";
/// Pane title for the Composer atelier — header carries the Paste stack affordance.
const COMPOSER_PANE_NAME: &str = "✍ Composer · ⧉ Paste stack";

/// The frame's live theme mode as the server announces it
/// (`Event::HostTerminalThemeChanged`). The name of that event is historical:
/// since the frame owns the theme, the mode it carries is vc-frame's canonical
/// choice, seeded from the host terminal only until the user picks one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FrameTheme {
    #[default]
    Dark,
    Light,
}

impl From<HostTerminalThemeMode> for FrameTheme {
    fn from(mode: HostTerminalThemeMode) -> Self {
        match mode {
            HostTerminalThemeMode::Dark => FrameTheme::Dark,
            HostTerminalThemeMode::Light => FrameTheme::Light,
        }
    }
}

impl FrameTheme {
    fn indicator(self) -> &'static str {
        match self {
            Self::Dark => "☾",
            Self::Light => "☼",
        }
    }
}
/// Same drafting contract as Super+e (Cmd+E) in the default config — the
/// single product key. Prefer installed paste-stack-aware `vc-composer.sh`
/// (vim profile: number, laststatus=0, Ctrl+p paste-stack pick).
/// The fallback speaks the same caret language as the installed script
/// (caret-semantics.md, one contract, two roads) via a mktemp mini-vimrc:
/// insert=beam 6 / replace=blink-underline 3 / normal=underline 4 through
/// termcaps, DECSCUSR 0 handed back after the editor exits. Named
/// degradation: visual/cmdline/operator-pending states live only in the
/// installed script — the one-liner budget stops at the three termcaps.
const COMPOSER_COMMAND: &str = r#"if [ -x "${HOME}/.config/vetcoders/frontier/vc-frame/vc-composer.sh" ]; then "${HOME}/.config/vetcoders/frontier/vc-frame/vc-composer.sh"; elif [ -x "${HOME}/.config/vc-frame/vc-composer.sh" ]; then "${HOME}/.config/vc-frame/vc-composer.sh"; else f=$(mktemp "${TMPDIR:-/tmp}/vc-composer.XXXXXX") || exit 1; rc=$(mktemp "${TMPDIR:-/tmp}/vc-composer-vimrc.XXXXXX") || exit 1; printf '%s\n' 'set number laststatus=0 nowrap textwidth=0' > "$rc"; if [ "${VC_COMPOSER_CARET:-1}" != "0" ]; then printf '%s\n' 'let &t_SI = "\e[6 q"' 'let &t_SR = "\e[3 q"' 'let &t_EI = "\e[4 q"' >> "$rc"; fi; ${EDITOR:-vim} -N -u "$rc" "$f"; if [ "${VC_COMPOSER_CARET:-1}" != "0" ]; then printf '\033[0 q'; fi; if [ -s "$f" ]; then vc-frame action toggle-floating-panes; vc-frame action write-chars "$(cat "$f")"; fi; rm -f -- "$f" "$rc"; fi"#;
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
    failed_tab_positions: BTreeSet<usize>,

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
    frame_theme: FrameTheme,

    // Tooltip state
    is_tooltip: bool,
    tooltip_is_active: bool,
    persist: bool,
    is_first_run: bool,
    own_tab_index: Option<usize>,
    own_client_id: u16,
    is_visible: bool,
    // Last (mode, coordinates) actually sent to the server. Repositioning is
    // idempotent against this: a persistent tooltip receives ModeUpdate
    // broadcasts that its own coordinate/rename calls trigger, and resending
    // on every echo made a self-sustaining ~12/s reposition storm (the
    // jumping screen of 2026-07-31).
    last_sent_tooltip_state: Option<(InputMode, FloatingPaneCoordinates)>,

    // Keybinding cache
    cached_keybinds: KeybindsVec,
    guest_projection_session: Option<String>,
    host_plugin_id: Option<u32>,

    // Panel drawer — server PaneManifest is the inventory; this is a view.
    is_panel_drawer: bool,
    pane_manifest: Option<PaneManifest>,
    panel_count: usize,
    panel_drawer_plugin_id: Option<u32>,
    panel_drawer_is_visible: bool,
    panel_drawer: PanelDrawer,
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
        self.setup_subscriptions();
        self.configure_keybinds();
        // No theme query here: the server replays the live mode as
        // `Event::HostTerminalThemeChanged` right after every plugin load
        // (RequestStateUpdateForPlugins), so the chip starts truthful.
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
            Event::TabUpdate(tabs) => {
                if self.guest_projection_session.is_some() {
                    false
                } else {
                    self.handle_tab_update(tabs)
                }
            },
            Event::PaneUpdate(pane_manifest) => self.handle_pane_update(pane_manifest),
            Event::Key(key) => self.handle_drawer_key(key),
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
            Event::PermissionRequestResult(_) => true,
            Event::HostTerminalThemeChanged(mode) => self.handle_frame_theme_changed(mode),
            Event::CustomMessage(message, payload) if message == VC_GUEST_SURFACE_MESSAGE => {
                self.handle_guest_surface_payload(&payload)
            },
            Event::CustomMessage(message, payload) if message == VC_CHROME_VISIBILITY_MESSAGE => {
                let was_visible = self.is_visible;
                match payload.as_str() {
                    "true" => self.is_visible = true,
                    "false" => self.is_visible = false,
                    _ => {},
                }
                self.is_visible && !was_visible
            },
            Event::CustomMessage(message, _) if message == VC_CHROME_HEARTBEAT_MESSAGE => {
                let was_visible = self.is_visible;
                self.is_visible = true;
                !was_visible
            },
            Event::Visible(is_visible) => {
                let was_visible = self.is_visible;
                self.is_visible = is_visible;
                is_visible && !was_visible
            },
            _ => false,
        }
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if message.name == VC_GUEST_SURFACE_MESSAGE {
            return message
                .payload
                .as_deref()
                .map(|payload| self.handle_guest_surface_payload(payload))
                .unwrap_or(false);
        }
        if self.is_tooltip && message.is_private {
            self.handle_tooltip_pipe(message);
        } else if self.quick_cmd_message_targets_active_bar(&message) {
            // Keep keyboard and mouse on one runtime path: both end in the
            // same runner, geometry and pane-title contract.
            open_quick_cmd();
        } else if message.name == MSG_TOGGLE_PANEL_DRAWER
            && message.is_private
            && !self.is_panel_drawer
            && !self.is_tooltip
        {
            self.toggle_panel_drawer();
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
        // Transient initial resize events arrive with rows/cols at or near
        // zero before the real layout lands; painting those frames is what
        // makes the chrome visibly jump at session start.
        if dimensions_are_transient(rows, cols) {
            return;
        }
        if self.is_tooltip {
            self.render_tooltip(rows, cols);
        } else if self.is_panel_drawer {
            render_drawer(rows, cols, &self.panel_drawer);
        } else {
            self.render_tab_line(cols);
        }
    }
}

// Floor for a renderable frame: anything below is a transient startup event,
// not a legal surface. Kept far below the comfortable chrome minimum
// (tools/repro_chrome.py MIN_COLUMNS) so legal small panes — the tooltip
// floating pane included — always render.
const MIN_RENDER_ROWS: usize = 1;
const MIN_RENDER_COLS: usize = 4;

fn dimensions_are_transient(rows: usize, cols: usize) -> bool {
    rows < MIN_RENDER_ROWS || cols < MIN_RENDER_COLS
}

impl State {
    fn quick_cmd_message_targets_active_bar(&self, message: &PipeMessage) -> bool {
        message.name == MSG_OPEN_QUICK_CMD
            && message.is_private
            && message.source == PipeSource::Keybind
            // A session canvas is a singleton runtime projected into every
            // tab, so its runtime plugin id is deliberately absent from the
            // per-tab PaneManifest. It is already selected by the server as
            // the canvas authority; asking it for a tab index would reject
            // every keyboard Quick cmd. Legacy bars remain tab-scoped.
            && (self.parse_bool_config("session_canvas", false)
                || self.own_tab_index == Some(self.active_tab_idx.saturating_sub(1)))
    }

    fn initialize_configuration(&mut self, configuration: BTreeMap<String, String>) {
        self.config = configuration.clone();
        self.is_tooltip = self.parse_bool_config(CONFIG_IS_TOOLTIP, false);
        self.is_panel_drawer = self.parse_bool_config(CONFIG_IS_PANEL_DRAWER, false);

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
        set_selectable(self.is_panel_drawer);

        let events = if self.is_tooltip {
            vec![
                EventType::ModeUpdate,
                EventType::TabUpdate,
                EventType::InitialKeybinds,
            ]
        } else if self.is_panel_drawer {
            vec![
                EventType::PaneUpdate,
                EventType::TabUpdate,
                EventType::Key,
                EventType::Mouse,
                EventType::ModeUpdate,
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
                EventType::PermissionRequestResult,
                EventType::CustomMessage,
                EventType::Visible,
                EventType::HostTerminalThemeChanged,
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
        if self.guest_projection_session.is_some() {
            return false;
        }
        self.apply_tabs(tabs)
    }

    fn apply_tabs(&mut self, tabs: Vec<TabInfo>) -> bool {
        self.update_display_area(&tabs);

        if let Some(active_tab_index) = tabs.iter().position(|t| t.active) {
            let active_tab_idx = active_tab_index + 1; // Convert to 1-based indexing
            let should_render = self.active_tab_idx != active_tab_idx || self.tabs != tabs;

            if self.is_tooltip && self.active_tab_idx != active_tab_idx {
                self.move_tooltip_to_new_tab(active_tab_idx);
            }

            self.active_tab_idx = active_tab_idx;
            self.tabs = tabs;
            let inventory_changed = self.refresh_panel_inventory();

            should_render || inventory_changed
        } else {
            false
        }
    }

    fn handle_pane_update(&mut self, pane_manifest: PaneManifest) -> bool {
        self.status_bar_is_present = self.detect_status_bar_presence(&pane_manifest);
        let failed_tab_positions = pane_manifest
            .panes
            .iter()
            .filter_map(|(tab_position, panes)| {
                panes
                    .iter()
                    .any(|pane| !pane.is_plugin && pane.exited && pane.exit_status != Some(0))
                    .then_some(*tab_position)
            })
            .collect();
        let failures_changed = self.failed_tab_positions != failed_tab_positions;
        self.failed_tab_positions = failed_tab_positions;

        let tooltip_changed = if self.toggle_tooltip_key.is_some() {
            let previous_tooltip_state = self.tooltip_is_active;
            self.tooltip_is_active = self.detect_tooltip_presence(&pane_manifest);
            self.own_tab_index = self.find_own_tab_index(&pane_manifest);
            previous_tooltip_state != self.tooltip_is_active
        } else {
            self.own_tab_index = self.find_own_tab_index(&pane_manifest);
            false
        };

        let floating_visible = floating_panes_visible(&self.tabs);
        let (drawer_id, drawer_visible) = detect_panel_drawer(&pane_manifest, floating_visible);
        let drawer_changed = self.panel_drawer_plugin_id != drawer_id
            || self.panel_drawer_is_visible != drawer_visible;
        self.panel_drawer_plugin_id = drawer_id;
        self.panel_drawer_is_visible = drawer_visible;

        let rows = inventory_for_tab(
            &pane_manifest,
            current_tab_position(self.active_tab_idx),
            self.own_plugin_id,
            floating_visible,
        );
        let count_changed = self.panel_count != rows.len();
        self.panel_count = rows.len();
        let drawer_rows_changed = if self.is_panel_drawer {
            self.panel_drawer.replace_rows(rows)
        } else {
            false
        };
        self.pane_manifest = Some(pane_manifest);

        failures_changed
            || tooltip_changed
            || count_changed
            || drawer_changed
            || drawer_rows_changed
    }

    fn handle_mouse_event(&mut self, mouse_event: Mouse) {
        if self.is_panel_drawer {
            if let Mouse::LeftClick(line, _) = mouse_event {
                let command = self.panel_drawer.handle_click(line);
                self.apply_drawer_command(command);
            }
            return;
        }
        if self.is_tooltip {
            return;
        }

        match mouse_event {
            Mouse::LeftClick(_, col) => self.handle_tab_click(col),
            Mouse::ScrollUp(lines) => self.forward_scroll_to_focused_pane(true, lines),
            Mouse::ScrollDown(lines) => self.forward_scroll_to_focused_pane(false, lines),
            _ => {},
        }
    }

    fn handle_clipboard_copy(&mut self, copy_destination: CopyDestination) -> bool {
        if self.is_tooltip || self.is_panel_drawer || self.status_bar_is_present {
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
        if self.is_tooltip || self.is_panel_drawer || self.status_bar_is_present {
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
        if self.is_tooltip || self.is_panel_drawer {
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
                    && pane.title != PANEL_DRAWER_TITLE
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

    fn refresh_panel_inventory(&mut self) -> bool {
        let Some(manifest) = self.pane_manifest.as_ref() else {
            return false;
        };
        let floating_visible = floating_panes_visible(&self.tabs);
        let rows = inventory_for_tab(
            manifest,
            current_tab_position(self.active_tab_idx),
            self.own_plugin_id,
            floating_visible,
        );
        let count_changed = self.panel_count != rows.len();
        self.panel_count = rows.len();
        let drawer_rows_changed = if self.is_panel_drawer {
            self.panel_drawer.replace_rows(rows)
        } else {
            false
        };
        count_changed || drawer_rows_changed
    }

    fn handle_drawer_key(&mut self, key: KeyWithModifier) -> bool {
        if !self.is_panel_drawer {
            return false;
        }
        let command = self.panel_drawer.handle_key(&key);
        let redraw = matches!(command, DrawerCommand::Redraw);
        self.apply_drawer_command(command);
        redraw
    }

    fn apply_drawer_command(&self, command: DrawerCommand) {
        match command {
            DrawerCommand::Hide => {
                #[cfg(target_family = "wasm")]
                hide_self();
            },
            DrawerCommand::Focus(pane_id) => {
                #[cfg(target_family = "wasm")]
                {
                    show_pane_with_id(pane_id, true, true);
                    hide_self();
                }
                #[cfg(not(target_family = "wasm"))]
                let _ = pane_id;
            },
            DrawerCommand::Redraw | DrawerCommand::None => {},
        }
    }

    fn toggle_panel_drawer(&self) {
        if self.is_panel_drawer {
            #[cfg(target_family = "wasm")]
            hide_self();
            return;
        }
        if let Some(plugin_id) = self.panel_drawer_plugin_id {
            #[cfg(target_family = "wasm")]
            if self.panel_drawer_is_visible {
                hide_pane_with_id(PaneId::Plugin(plugin_id));
            } else {
                show_pane_with_id(PaneId::Plugin(plugin_id), true, true);
            }
            #[cfg(not(target_family = "wasm"))]
            let _ = plugin_id;
            return;
        }
        let Some(message) = self.panel_drawer_launch_message() else {
            return;
        };
        #[cfg(target_family = "wasm")]
        pipe_message_to_plugin(message);
        #[cfg(not(target_family = "wasm"))]
        let _ = message;
    }

    fn panel_drawer_launch_message(&self) -> Option<MessageToPlugin> {
        let coordinates = panel_drawer_coordinates()?;
        let mut config = self.config.clone();
        config.insert(CONFIG_IS_PANEL_DRAWER.to_string(), "true".to_string());
        Some(
            MessageToPlugin::new("launch_panel_drawer")
                .with_plugin_url("vc-frame:OWN_URL")
                .with_plugin_config(config)
                .with_floating_pane_coordinates(coordinates)
                .new_plugin_instance_should_float(true)
                .new_plugin_instance_should_be_focused()
                .new_plugin_instance_should_have_pane_title(PANEL_DRAWER_TITLE),
        )
    }

    fn handle_guest_surface_payload(&mut self, payload: &str) -> bool {
        match parse_guest_surface_payload(payload) {
            Some(GuestSurfaceRequest::Surface {
                session,
                host_plugin_id,
                tabs,
            }) => {
                self.guest_projection_session = Some(session);
                self.host_plugin_id = host_plugin_id;
                let projected: Vec<TabInfo> = tabs
                    .into_iter()
                    .enumerate()
                    .map(|(index, tab)| TabInfo {
                        position: tab.position,
                        name: tab.name,
                        active: tab.active,
                        tab_id: index,
                        ..TabInfo::default()
                    })
                    .collect();
                self.apply_tabs(projected)
            },
            _ => false,
        }
    }

    fn handle_tab_click(&mut self, col: usize) {
        if self.sentinel_clicked(col, THEME_CLICK_SENTINEL) {
            toggle_frame_theme();
            return;
        }
        if self.sentinel_clicked(col, PANELS_CLICK_SENTINEL) {
            self.toggle_panel_drawer();
            return;
        }
        if self.sentinel_clicked(col, COMPOSER_CLICK_SENTINEL) {
            open_composer();
            return;
        }
        if self.sentinel_clicked(col, AGENTS_CLICK_SENTINEL) {
            // Quick cmd floats over the *current* tab — no Agents detour, no
            // deferred spawn race, no "Process will run…" over the wrong pane.
            open_quick_cmd();
            return;
        }
        if self.sentinel_clicked(col, VOC_CLICK_SENTINEL) {
            emit_voc_click_receipt();
            return;
        }
        if let Some(tab_idx) = get_tab_to_focus(&self.tab_line, self.active_tab_idx, col) {
            if let Some(session) = self.guest_projection_session.clone() {
                let message = guest_tab_activation_message(
                    &session,
                    tab_idx.saturating_sub(1),
                    self.host_plugin_id,
                );
                #[cfg(target_family = "wasm")]
                pipe_message_to_plugin(message);
                #[cfg(not(target_family = "wasm"))]
                let _ = message;
            } else {
                switch_tab_to(tab_idx.try_into().unwrap());
            }
        }
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

    /// The server announced the frame's live theme mode. Rerender only when
    /// the chip actually flips — replays after plugin (re)loads and duplicate
    /// reports are idempotent.
    fn handle_frame_theme_changed(&mut self, mode: HostTerminalThemeMode) -> bool {
        let theme = FrameTheme::from(mode);
        let changed = self.frame_theme != theme;
        self.frame_theme = theme;
        changed
    }

    fn forward_scroll_to_focused_pane(&self, scroll_up: bool, lines: usize) {
        let Ok((_, focused_pane_id)) = get_focused_pane_info() else {
            return;
        };
        let Some(focused_pane) = get_pane_info(focused_pane_id) else {
            return;
        };
        let Some((pane_id, position)) =
            focused_terminal_scroll_target(focused_pane_id, &focused_pane)
        else {
            return;
        };
        let lines = bounded_mouse_scroll_lines(lines);
        if scroll_up {
            mouse_scroll_up_in_pane_id(pane_id, position, lines);
        } else {
            mouse_scroll_down_in_pane_id(pane_id, position, lines);
        }
    }
}

fn bounded_mouse_scroll_lines(lines: usize) -> usize {
    lines.min(plugin_api::plugin_command::MAX_MOUSE_SCROLL_LINES_IN_PANE_ID)
}

fn focused_terminal_scroll_target(
    focused_pane_id: PaneId,
    focused_pane: &PaneInfo,
) -> Option<(PaneId, Position)> {
    let pane_id = if focused_pane.is_plugin {
        PaneId::Plugin(focused_pane.id)
    } else {
        PaneId::Terminal(focused_pane.id)
    };
    if focused_pane.is_plugin || pane_id != focused_pane_id {
        return None;
    }
    if focused_pane.pane_content_rows == 0 || focused_pane.pane_content_columns == 0 {
        return None;
    }

    let content_offset_column = focused_pane
        .pane_content_x
        .saturating_sub(focused_pane.pane_x);
    let content_offset_line = focused_pane
        .pane_content_y
        .saturating_sub(focused_pane.pane_y);
    let (column, line) = focused_pane
        .cursor_coordinates_in_pane
        .and_then(|(column, line)| {
            Some((
                column.checked_sub(content_offset_column)?,
                line.checked_sub(content_offset_line)?,
            ))
        })
        .filter(|(column, line)| {
            *column < focused_pane.pane_content_columns && *line < focused_pane.pane_content_rows
        })
        .unwrap_or((
            focused_pane.pane_content_columns / 2,
            focused_pane.pane_content_rows / 2,
        ));
    Some((
        focused_pane_id,
        Position::new(line.try_into().ok()?, column.try_into().ok()?),
    ))
}

/// Quick cmd mini console: shallow, wide, upper-center — non-ephemeral
/// interactive terminal (not a command-pane "Process will run…" ticket).
/// Commands run in-pane; the operator inspects output without the float dying.
fn quick_cmd_coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("18%".to_owned()),
        Some("8%".to_owned()),
        Some("64%".to_owned()),
        Some("28%".to_owned()),
        Some(false),
        None,
    )
}

/// The Composer atelier: large, centered writing surface — same footprint
/// every time so the writing layer always opens where the hands remember it.
fn composer_coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("15%".to_owned()),
        Some("10%".to_owned()),
        Some("70%".to_owned()),
        Some("72%".to_owned()),
        Some(false),
        None,
    )
}

/// The ☾/☼ chip: flip the frame's live theme through the server-owned
/// `ToggleTheme` action. The server (Screen) is the single theme owner — it
/// swaps chrome + canvas palettes for every client and tab, pins the choice
/// against host-terminal reports, and announces the result back as
/// `Event::HostTerminalThemeChanged`, which is what repaints this chip. No
/// external command, no host-terminal palette file: other terminal engines
/// see exactly what VC Terminal sees.
fn toggle_frame_theme() {
    let mut context = BTreeMap::new();
    context.insert(THEME_ACTION_CONTEXT_KEY.to_owned(), "toggle".to_owned());
    run_action(actions::Action::ToggleTheme, context);
}

/// Quick cmd: non-ephemeral floating *terminal* at a fixed upper-center
/// footprint (spec 1.2 §C). Interactive terminal — not a command-pane ticket —
/// so there is no "Process will run in separated pane" chrome and the pane
/// survives after each command. Prefer the installed `vc-quick-cmd.sh` banner
/// wrapper when present; otherwise open a plain login shell on `.`.
///
/// The fallback runner is **POSIX `sh` only** (no bashisms). Debian/Ubuntu
/// `sh` is dash — `${PWD/#$HOME/~}` is a bash-only rewrite and aborts with
/// `sh: 1: Bad substitution` / exit 2 (the EXIT CODE strip the operator saw).
/// Voc chip click: one plugin-log receipt, no pane, no pipe. C5 owns the
/// host-console action seam.
#[derive(Debug)]
#[allow(dead_code)]
struct VocClickOutcome {
    receipt_line: &'static str,
    opened_pane: bool,
    piped_message: bool,
}

fn emit_voc_click_receipt() -> VocClickOutcome {
    eprintln!("{VOC_CLICK_RECEIPT}");
    VocClickOutcome {
        receipt_line: VOC_CLICK_RECEIPT,
        opened_pane: false,
        piped_message: false,
    }
}

fn guest_tab_activation_message(
    session: &str,
    tab: usize,
    host_plugin_id: Option<u32>,
) -> MessageToPlugin {
    let message = MessageToPlugin::new(VC_GUEST_SURFACE_MESSAGE)
        .with_payload(activate_guest_tab_payload(session, tab));
    if let Some(host_plugin_id) = host_plugin_id {
        message.with_destination_plugin_id(host_plugin_id)
    } else {
        message
            .with_plugin_url(VC_FRAME_HOST_PLUGIN_ALIAS)
            .with_plugin_config(host_session_manager_configuration())
    }
}

fn open_quick_cmd() {
    // Keep this string dash-clean: ${var:-def} and ${var#prefix} are POSIX;
    // ${var/pat/repl} and ${var/#pat/repl} are not.
    let quick_cmd_runner = quick_cmd_runner_script();
    // open_command_pane_floating + exec keeps one long-lived process (the
    // login shell). We accept command-pane chrome only when the wrapper is
    // missing; preferred path is still a real shell via the wrapper script.
    let command = CommandToRun::new_with_args("sh", vec!["-c", quick_cmd_runner.as_str()]);
    if let Some(PaneId::Terminal(terminal_pane_id)) =
        open_command_pane_floating(command, quick_cmd_coordinates(), BTreeMap::new())
    {
        // The host binds this SDK action to this plugin instance's client.
        // Open first: a rejected/unavailable command must not change modes.
        // Both the chip and keybind use this path, including from TAB/LOCK.
        switch_to_input_mode(&InputMode::Normal);
        rename_terminal_pane(terminal_pane_id, QUICK_CMD_PANE_NAME);
    }
}

/// Click path of the Composer chip — identical contract to Super+e (Cmd+E).
/// Alt+e is deliberately free for Polish `ę` (spec 1.2 §A).
fn open_composer() {
    let command = CommandToRun::new_with_args("sh", vec!["-c", COMPOSER_COMMAND]);
    if let Some(PaneId::Terminal(terminal_pane_id)) =
        open_command_pane_floating(command, composer_coordinates(), BTreeMap::new())
    {
        rename_terminal_pane(terminal_pane_id, COMPOSER_PANE_NAME);
    }
}

impl State {
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
        // `message` is consumed only by the wasm-gated pipe below; native builds
        // still type-check the construction but never send it.
        #[cfg_attr(not(target_family = "wasm"), allow(unused_variables))]
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

    fn update_tooltip_for_mode_change(&mut self, new_mode: InputMode) {
        if let Some(plugin_id) = self.own_plugin_id {
            let coordinates = self.calculate_tooltip_coordinates();
            let next_state = (new_mode, coordinates.clone());
            if self.last_sent_tooltip_state.as_ref() == Some(&next_state) {
                return;
            }
            change_floating_panes_coordinates(vec![(PaneId::Plugin(plugin_id), coordinates)]);
            rename_plugin_pane(plugin_id, format!("{:?}", new_mode));
            self.last_sent_tooltip_state = Some(next_state);
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
            mode: self.mode_info.mode,
            toggle_tooltip_key: self.toggle_tooltip_key.clone(),
            tooltip_is_active: self.tooltip_is_active,
            brand_text: self.brand_text.clone(),
            brand_text_short: self.brand_text_short.clone(),
            left_inset: self.left_inset,
            theme_indicator: self.frame_theme.indicator().to_owned(),
            pane_count: self.panel_count,
        };
        self.tab_line = tab_line(&self.mode_info, tab_data, cols, config);

        let output = self
            .tab_line
            .iter()
            .fold(String::new(), |acc, part| acc + &part.part);

        self.render_background_with_text(&output);
    }

    fn prepare_tab_data(&self) -> TabRenderData {
        let projected = project_guest_organs(&self.tabs);
        let mut all_tabs = Vec::new();
        let mut active_tab_index = 0;
        let mut is_alternate_tab = false;

        for (index, tab) in projected.iter().enumerate() {
            let tab_name = self.get_tab_display_name(tab);

            if tab.active {
                // Index in the projected Z2 row — not the original tab.position —
                // so split_tabs keeps the fisheye on the active organ after reorder.
                active_tab_index = index;
            }

            let styled_tab = tab_style(
                tab_name,
                tab,
                is_alternate_tab,
                self.mode_info.style.colors,
                self.mode_info.capabilities,
                self.failed_tab_positions.contains(&tab.position),
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

#[cfg(test)]
mod transient_dimension_guard_tests {
    use super::*;

    #[test]
    fn zero_dimensions_are_transient() {
        assert!(dimensions_are_transient(0, 80));
        assert!(dimensions_are_transient(1, 0));
        assert!(dimensions_are_transient(0, 0));
    }

    #[test]
    fn sub_minimum_columns_are_transient() {
        assert!(dimensions_are_transient(1, 3));
    }

    #[test]
    fn legal_small_surfaces_still_render() {
        // The tooltip lives in a small floating pane; the guard must not
        // eat it.
        assert!(!dimensions_are_transient(1, 4));
        assert!(!dimensions_are_transient(1, 8));
        assert!(!dimensions_are_transient(10, 40));
    }

    #[test]
    fn quick_cmd_keybind_targets_only_the_active_bar() {
        let mut state = State {
            active_tab_idx: 2,
            own_tab_index: Some(1),
            ..Default::default()
        };
        let message = PipeMessage::new(PipeSource::Keybind, MSG_OPEN_QUICK_CMD, &None, &None, true);

        assert!(state.quick_cmd_message_targets_active_bar(&message));

        state.own_tab_index = Some(0);
        assert!(!state.quick_cmd_message_targets_active_bar(&message));

        let public_message =
            PipeMessage::new(PipeSource::Keybind, MSG_OPEN_QUICK_CMD, &None, &None, false);
        assert!(!state.quick_cmd_message_targets_active_bar(&public_message));
    }

    #[test]
    fn quick_cmd_keybind_accepts_the_projected_session_canvas_without_a_tab_manifest_entry() {
        let mut state = State {
            active_tab_idx: 2,
            ..Default::default()
        };
        state
            .config
            .insert("session_canvas".to_owned(), "true".to_owned());
        let message = PipeMessage::new(PipeSource::Keybind, MSG_OPEN_QUICK_CMD, &None, &None, true);

        assert!(state.quick_cmd_message_targets_active_bar(&message));

        let public_message =
            PipeMessage::new(PipeSource::Keybind, MSG_OPEN_QUICK_CMD, &None, &None, false);
        assert!(!state.quick_cmd_message_targets_active_bar(&public_message));
    }

    #[test]
    fn wheel_actions_target_the_focused_terminal_cursor() {
        let focused_terminal = PaneInfo {
            is_focused: true,
            pane_x: 23,
            pane_y: 1,
            pane_content_x: 24,
            pane_content_y: 2,
            pane_content_columns: 80,
            pane_content_rows: 20,
            cursor_coordinates_in_pane: Some((8, 4)),
            ..Default::default()
        };
        let target =
            focused_terminal_scroll_target(PaneId::Terminal(0), &focused_terminal).unwrap();
        assert_eq!(target, (PaneId::Terminal(0), Position::new(3, 7)));
    }

    #[test]
    fn wheel_actions_fall_back_to_the_content_center() {
        let focused_terminal = PaneInfo {
            id: 4,
            pane_content_x: 24,
            pane_content_y: 2,
            pane_content_columns: 80,
            pane_content_rows: 20,
            ..Default::default()
        };

        let target =
            focused_terminal_scroll_target(PaneId::Terminal(4), &focused_terminal).unwrap();
        assert_eq!(target, (PaneId::Terminal(4), Position::new(10, 40)));
    }

    #[test]
    fn wheel_forwarding_bounds_large_trackpad_deltas() {
        assert_eq!(bounded_mouse_scroll_lines(3), 3);
        assert_eq!(bounded_mouse_scroll_lines(100), 100);
        assert_eq!(bounded_mouse_scroll_lines(usize::MAX), 100);
    }

    #[test]
    fn wheel_forwarding_ignores_plugin_only_and_empty_content_surfaces() {
        let plugin_only = PaneInfo {
            is_focused: true,
            is_plugin: true,
            pane_content_columns: 80,
            pane_content_rows: 20,
            ..Default::default()
        };
        assert_eq!(
            focused_terminal_scroll_target(PaneId::Plugin(0), &plugin_only),
            None
        );

        let empty_terminal = PaneInfo {
            is_focused: true,
            pane_content_columns: 0,
            pane_content_rows: 20,
            ..Default::default()
        };
        assert_eq!(
            focused_terminal_scroll_target(PaneId::Terminal(0), &empty_terminal),
            None
        );
    }

    #[test]
    fn command_bridge_theme_chip_follows_server_not_host_guess() {
        frame_theme_event_flips_chip_only_on_real_change();
    }

    #[test]
    fn frame_theme_event_flips_chip_only_on_real_change() {
        let mut state = State::default();
        assert_eq!(
            state.frame_theme.indicator(),
            "☾",
            "dark until the server says otherwise"
        );

        // replay of the current (dark) mode after plugin load: no repaint
        assert!(!state.handle_frame_theme_changed(HostTerminalThemeMode::Dark));
        assert_eq!(state.frame_theme.indicator(), "☾");
        // first real switch repaints
        assert!(state.handle_frame_theme_changed(HostTerminalThemeMode::Light));
        assert_eq!(state.frame_theme.indicator(), "☼");
        // duplicate report is idempotent
        assert!(!state.handle_frame_theme_changed(HostTerminalThemeMode::Light));
        // repeated toggles keep tracking the server
        assert!(state.handle_frame_theme_changed(HostTerminalThemeMode::Dark));
        assert_eq!(state.frame_theme.indicator(), "☾");
        assert!(state.handle_frame_theme_changed(HostTerminalThemeMode::Light));
        assert_eq!(state.frame_theme.indicator(), "☼");
    }

    #[test]
    fn failed_command_panes_warn_only_their_tab_until_the_manifest_clears() {
        let mut state = State::default();
        let failed_pane = PaneInfo {
            exited: true,
            exit_status: Some(1),
            ..Default::default()
        };
        let failed_manifest = PaneManifest {
            panes: std::collections::HashMap::from([(2, vec![failed_pane])]),
        };

        assert!(state.handle_pane_update(failed_manifest.clone()));
        assert_eq!(state.failed_tab_positions, BTreeSet::from([2]));
        assert!(!state.handle_pane_update(failed_manifest));

        let successful_manifest = PaneManifest {
            panes: std::collections::HashMap::from([(
                2,
                vec![PaneInfo {
                    exited: true,
                    exit_status: Some(0),
                    ..Default::default()
                }],
            )]),
        };
        assert!(state.handle_pane_update(successful_manifest));
        assert!(state.failed_tab_positions.is_empty());
    }

    #[test]
    fn guest_surface_replaces_generic_workspace_tab() {
        let mut state = State::default();
        assert!(state.handle_tab_update(vec![TabInfo {
            name: "Workspace".to_owned(),
            active: true,
            position: 0,
            ..TabInfo::default()
        }]));
        assert_eq!(state.tabs.len(), 1);
        assert_eq!(state.tabs[0].name, "Workspace");

        let payload = r#"{"session":"workspace-b","tabs":[{"name":"Start here","active":true,"position":0},{"name":"Agents","active":false,"position":1}]}"#;
        assert!(state.handle_guest_surface_payload(payload));
        assert_eq!(
            state.guest_projection_session.as_deref(),
            Some("workspace-b")
        );
        let names: Vec<&str> = state.tabs.iter().map(|tab| tab.name.as_str()).collect();
        assert_eq!(names, vec!["Start here", "Agents"]);
        assert!(state.tabs[0].active);
        assert!(!state.handle_tab_update(vec![TabInfo {
            name: "Workspace".to_owned(),
            active: true,
            ..TabInfo::default()
        }]));
        assert_eq!(state.tabs[0].name, "Start here");
    }

    #[test]
    fn guest_tab_activation_targets_host_plugin_id_exclusively() {
        let message = guest_tab_activation_message("workspace-a", 1, Some(11));
        assert_eq!(message.destination_plugin_id, Some(11));
        assert!(message.plugin_url.is_none());
        assert_eq!(message.message_name, VC_GUEST_SURFACE_MESSAGE);
    }

    #[test]
    fn guest_tab_activation_falls_back_to_frame_host_alias() {
        let message = guest_tab_activation_message("workspace-b", 0, None);
        assert_eq!(
            message.plugin_url.as_deref(),
            Some(VC_FRAME_HOST_PLUGIN_ALIAS)
        );
        assert_eq!(
            message.plugin_config.get("frame_host").map(String::as_str),
            Some("true")
        );
        assert!(message.destination_plugin_id.is_none());
    }

    #[test]
    fn guest_surface_stores_host_plugin_id_for_exclusive_routing() {
        let mut state = State::default();
        let payload = r#"{"session":"workspace-a","host_plugin_id":4,"status":"workspace-a","tabs":[{"name":"Start here","active":true,"position":0}]}"#;
        assert!(state.handle_guest_surface_payload(payload));
        assert_eq!(state.host_plugin_id, Some(4));
        assert_eq!(
            state.guest_projection_session.as_deref(),
            Some("workspace-a")
        );
    }

    #[test]
    fn pane_update_chip_count_is_tab_scoped_and_change_driven() {
        use std::collections::HashMap;
        let mut state = State {
            active_tab_idx: 1,
            ..Default::default()
        };
        let visible = PaneInfo {
            id: 4,
            title: "shell".to_owned(),
            is_selectable: true,
            ..PaneInfo::default()
        };
        let other_tab = PaneInfo {
            id: 9,
            title: "other".to_owned(),
            is_selectable: true,
            ..PaneInfo::default()
        };
        let mut panes = HashMap::new();
        panes.insert(0, vec![visible.clone()]);
        panes.insert(1, vec![other_tab]);
        let first = PaneManifest {
            panes: panes.clone(),
        };
        assert!(state.handle_pane_update(first.clone()));
        assert_eq!(state.panel_count, 1);
        assert!(
            !state.handle_pane_update(first),
            "identical PaneManifest must not rerender the chip"
        );

        let extra = PaneInfo {
            id: 5,
            title: "❯_ Quick cmd".to_owned(),
            is_selectable: true,
            is_floating: true,
            is_suppressed: true,
            ..PaneInfo::default()
        };
        panes.insert(0, vec![visible, extra]);
        let two = PaneManifest { panes };
        assert!(state.handle_pane_update(two));
        assert_eq!(
            state.panel_count, 2,
            "hidden Quick cmd stays in the current-tab count"
        );
    }

    #[test]
    fn drawer_key_escape_is_hide_not_focus() {
        let mut state = State {
            is_panel_drawer: true,
            ..Default::default()
        };
        let mut pane = PaneInfo {
            id: 3,
            title: "hidden-term".to_owned(),
            is_selectable: true,
            is_suppressed: true,
            ..PaneInfo::default()
        };
        pane.is_suppressed = true;
        let mut panes = std::collections::HashMap::new();
        panes.insert(0, vec![pane]);
        state.active_tab_idx = 1;
        assert!(state.handle_pane_update(PaneManifest { panes }));
        let hide = state
            .panel_drawer
            .handle_key(&KeyWithModifier::new(BareKey::Esc));
        assert_eq!(hide, crate::panel_drawer::DrawerCommand::Hide);
        let enter = state
            .panel_drawer
            .handle_key(&KeyWithModifier::new(BareKey::Enter));
        assert_eq!(
            enter,
            crate::panel_drawer::DrawerCommand::Focus(PaneId::Terminal(3))
        );
    }

    #[test]
    fn panel_drawer_launch_message_is_a_new_floating_panels_instance() {
        let state = State::default();
        let message = state
            .panel_drawer_launch_message()
            .expect("right-edge coordinates must parse");
        assert_eq!(message.plugin_url.as_deref(), Some("vc-frame:OWN_URL"));
        assert_eq!(
            message
                .plugin_config
                .get(CONFIG_IS_PANEL_DRAWER)
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            message
                .new_plugin_args
                .as_ref()
                .and_then(|args| args.pane_title.as_deref()),
            Some(PANEL_DRAWER_TITLE)
        );
        assert!(message.floating_pane_coordinates.is_some());
    }

    #[test]
    fn voc_click_emits_receipt_only() {
        let mut state = State::default();
        state.tab_line = vec![LinePart {
            part: " Voc ".to_owned(),
            len: crate::line::VOC_CHIP_COLS,
            tab_index: Some(VOC_CLICK_SENTINEL),
        }];
        assert!(
            state.sentinel_clicked(0, VOC_CLICK_SENTINEL),
            "column 0 of the Voc chip must hit the sentinel"
        );
        assert!(state.sentinel_clicked(crate::line::VOC_CHIP_COLS - 1, VOC_CLICK_SENTINEL));
        assert!(!state.sentinel_clicked(crate::line::VOC_CHIP_COLS, VOC_CLICK_SENTINEL));

        let outcome = emit_voc_click_receipt();
        assert_eq!(
            outcome
                .receipt_line
                .lines()
                .filter(|line| !line.is_empty())
                .count(),
            1
        );
        assert!(
            outcome.receipt_line.contains("Voc"),
            "receipt must say Voc, not voc: {}",
            outcome.receipt_line
        );
        assert!(
            !outcome.opened_pane,
            "Voc click must not open a pane until C5"
        );
        assert!(
            !outcome.piped_message,
            "Voc click must not pipe a plugin message until C5"
        );
        assert_eq!(outcome.receipt_line, VOC_CLICK_RECEIPT);

        // Production dispatch seam: a click inside the Voc chip must be
        // intercepted by handle_tab_click's sentinel branch before the tab
        // route. Without the branch the same column would fall through to
        // get_tab_to_focus and try to switch to the sentinel-as-tab-index —
        // a clean return with untouched state IS the side-effect proof.
        let mut production = State::default();
        production.tab_line = vec![
            LinePart {
                part: " Voc ".to_owned(),
                len: crate::line::VOC_CHIP_COLS,
                tab_index: Some(VOC_CLICK_SENTINEL),
            },
            LinePart {
                part: " Agents ".to_owned(),
                len: 8,
                tab_index: Some(0),
            },
        ];
        production.active_tab_idx = 2;
        production.handle_tab_click(1);
        assert_eq!(production.active_tab_idx, 2);
        assert_eq!(production.tab_line.len(), 2);
        // A click on the real tab still routes to the tab (sentinel branch
        // did not swallow the row).
        assert_eq!(
            crate::tab::get_tab_to_focus(&production.tab_line, 2, crate::line::VOC_CHIP_COLS + 1),
            Some(1)
        );
    }
}
