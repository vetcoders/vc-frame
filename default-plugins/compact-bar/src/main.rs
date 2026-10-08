mod action_types;
mod clipboard_utils;
mod context_layers;
mod keybind_utils;
mod line;
mod panel_drawer;
mod tab;
mod tooltip;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;

use tab::{
    close_hit, dead_tab_positions as exited_terminal_tabs, get_tab_to_focus, middle_close_hit,
};
use zellij_tile::prelude::*;

use crate::action_types::VocClickOutcome;
use crate::clipboard_utils::{system_clipboard_error, text_copied_hint};
use crate::context_layers::{ContextLayer, competing_layers, contextual_panes_to_hide};
use crate::line::tab_line;
use crate::panel_drawer::{
    CONFIG_IS_PANEL_DRAWER, DrawerCommand, DrawerScope, MSG_TOGGLE_PANEL_DRAWER,
    PANEL_DRAWER_TITLE, PanelDrawer, PanelRow, active_pager, current_tab_position,
    detect_panel_drawer, floating_panes_visible, inventory_across_sessions, inventory_for_scope,
    inventory_for_tab, panel_drawer_coordinates, render_drawer,
};
use crate::tab::{
    CLOSE_ARM_TIMEOUT_SECS, CloseDecision, TabCloseAffordance, decide_close, tab_is_contractual,
    tab_style_with_pane_count, timer_is_close_arm,
};
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
const MSG_OPEN_VOC: &str = "vc_voc";
const MSG_TAB_NAVIGATION: &str = "vc_tab_navigation";

#[derive(Debug, PartialEq, Eq)]
enum TabNavigation {
    HostNext,
    HostPrevious,
}

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
pub const VOC_CLICK_SENTINEL: usize = usize::MAX - 5;
/// Sentinel for the `[+]` control on the tab line. A real tab index cannot
/// reach this value. Click opens a shell in a new tab of the current session.
pub const NEW_TAB_CLICK_SENTINEL: usize = usize::MAX - 1;
/// One-line prefix for the consumed Voc activation outcome.
const VOC_CLICK_RECEIPT: &str = "compact-bar: Voc host console";
const DISPLAY_LABEL_MESSAGE: &str = "vc_tab_command_label";
#[cfg(target_family = "wasm")]
const DISPLAY_REGISTER_MESSAGE: &str = "vc_tab_command_labels_register";
const VOC_PANE_NAME: &str = "Voc · Host console";
/// Delegate binary selection to the public deck contract. `vibecrafted tui`
/// owns `_resolve_voc_binary`; vc-frame must not grow a second resolver.
/// A failed command pane is held by vc-frame, so the diagnosis remains visible
/// instead of flashing away.
const VOC_COMMAND: &str = r#"if command -v vibecrafted >/dev/null 2>&1; then exec vibecrafted tui; fi; printf '\nVoc console is unavailable. Install or repair the Vibecrafted Runtime Pack (missing `vibecrafted tui`).\n' >&2; exit 127"#;
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
    /// Display column of the trailing close zone inside this part.
    /// `None` means the part cannot close (brand, sentinels, contract tabs).
    close_start: Option<usize>,
    /// Stable tab id the close zone acts on. Host id 0 is valid; absence is `None`.
    close_id: Option<usize>,
}

/// Armed close lives on the bar, keyed by stable tab id, not on `TabInfo`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CloseArm {
    tab_id: usize,
    guest: bool,
}

#[derive(Default)]
struct State {
    // Concise projections from the existing command observer, keyed by stable tab id.
    command_labels: BTreeMap<usize, (String, String)>,
    title_plugin_id: Option<u32>,
    // Tab state
    tabs: Vec<TabInfo>,
    active_tab_idx: usize,
    failed_tab_positions: BTreeSet<usize>,
    dead_tab_positions: BTreeSet<usize>,
    armed_close: Option<CloseArm>,
    /// Timers whose arm was replaced or confirmed before they fired.
    stale_close_arm_timers: u64,

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
    host_tabs: Vec<TabInfo>,

    // Host Voc console — the immediate id closes the open/update race; the
    // manifest makes the singleton recoverable after plugin reloads.
    voc_pane_id: Option<u32>,
    voc_pane_seen: bool,

    // Quick cmd — one per tab. The immediate id closes the open/update race (a
    // second press before the server's NewPane manifest); the manifest finds
    // the pane again after a plugin reload.
    quick_cmd_pane: Option<TrackedQuickCmd>,

    // Panel drawer — server PaneManifest is the inventory; this is a view.
    is_panel_drawer: bool,
    pane_manifest: Option<PaneManifest>,
    panel_count: usize,
    panels_pager: Option<(usize, usize)>,
    panel_drawer_plugin_id: Option<u32>,
    // Cross-session truth for the drawer's Global scope, from SessionUpdate:
    // (session name, is_current, its pane manifest).
    drawer_sessions: Vec<(String, bool, PaneManifest)>,
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
            Event::TabUpdate(tabs) => self.handle_tab_update(tabs),
            Event::PaneUpdate(pane_manifest) => self.handle_pane_update(pane_manifest),
            Event::SessionUpdate(session_infos, _) => self.handle_session_update(session_infos),
            Event::Key(key) => self.handle_drawer_key(key),
            Event::Mouse(mouse_event) => self.handle_mouse_event(mouse_event),
            Event::CopyToClipboard(copy_destination) => {
                self.handle_clipboard_copy(copy_destination)
            },
            Event::SystemClipboardFailure => self.handle_clipboard_failure(),
            Event::Timer(elapsed) => self.handle_timer(elapsed),
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
            Event::Visible(is_visible) => {
                let was_visible = self.is_visible;
                self.is_visible = is_visible;
                is_visible && !was_visible
            },
            _ => false,
        }
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if message.name == DISPLAY_LABEL_MESSAGE
            && self
                .title_plugin_id
                .is_some_and(|id| message.source == PipeSource::Plugin(id))
        {
            return message
                .payload
                .as_deref()
                .is_some_and(|payload| self.receive_command_label(payload));
        }
        if message.name == VC_GUEST_SURFACE_MESSAGE {
            return message
                .payload
                .as_deref()
                .map(|payload| self.handle_guest_surface_payload(payload))
                .unwrap_or(false);
        }
        if self.is_tooltip && message.is_private {
            self.handle_tooltip_pipe(message);
        } else if self.tab_navigation_message_targets_active_bar(&message) {
            match self.tab_navigation(message.payload.as_deref() == Some("next")) {
                TabNavigation::HostNext => go_to_next_tab(),
                TabNavigation::HostPrevious => go_to_previous_tab(),
            }
        } else if self.voc_message_targets_active_bar(&message) {
            let mut host = ZellijVocPaneHost;
            let outcome = self.open_or_focus_voc(&mut host, true);
            consume_voc_click_outcome(&outcome);
        } else if self.quick_cmd_message_targets_active_bar(&message) {
            // Keep keyboard and mouse on one runtime path: both end in the
            // same runner, geometry and pane-title contract.
            let mut host = ZellijVocPaneHost;
            self.open_or_focus_quick_cmd(&mut host);
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
            render_drawer(rows, cols, &mut self.panel_drawer);
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
    fn voc_message_targets_active_bar(&self, message: &PipeMessage) -> bool {
        message.name == MSG_OPEN_VOC
            && message.is_private
            && message.source == PipeSource::Keybind
            && (self.parse_bool_config("session_canvas", false)
                || self.own_tab_index == Some(self.active_tab_idx.saturating_sub(1)))
    }

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
                EventType::SessionUpdate,
                EventType::Key,
                EventType::Mouse,
                EventType::ModeUpdate,
            ]
        } else {
            vec![
                EventType::TabUpdate,
                EventType::PaneUpdate,
                EventType::SessionUpdate,
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
        if !self.is_tooltip && !self.is_panel_drawer {
            reconfigure(
                bind_compact_bar_keys_config(
                    self.toggle_tooltip_key.as_deref(),
                    self.own_client_id,
                ),
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
        self.host_tabs = tabs;
        self.apply_tabs(self.display_tabs())
    }

    fn display_tabs(&self) -> Vec<TabInfo> {
        self.host_tabs.clone()
    }

    fn tab_navigation_message_targets_active_bar(&self, message: &PipeMessage) -> bool {
        message.name == MSG_TAB_NAVIGATION
            && message.is_private
            && message.source == PipeSource::Keybind
            && matches!(message.payload.as_deref(), Some("next" | "previous"))
            && !self.is_tooltip
            && !self.is_panel_drawer
            && (self.parse_bool_config("session_canvas", false)
                || (self.own_tab_index.is_some()
                    && self.own_tab_index
                        == self
                            .host_tabs
                            .iter()
                            .find(|tab| tab.active)
                            .map(|tab| tab.position)))
    }

    fn tab_navigation(&self, next: bool) -> TabNavigation {
        if next {
            TabNavigation::HostNext
        } else {
            TabNavigation::HostPrevious
        }
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
        let dead_tab_positions = exited_terminal_tabs(&pane_manifest);
        let dead_changed = self.dead_tab_positions != dead_tab_positions;
        self.dead_tab_positions = dead_tab_positions;

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
        let (drawer_id, drawer_visible) = detect_panel_drawer(
            &pane_manifest,
            current_tab_position(self.active_tab_idx),
            floating_visible,
        );
        let drawer_changed = self.panel_drawer_plugin_id != drawer_id
            || self.panel_drawer_is_visible != drawer_visible;
        self.panel_drawer_plugin_id = drawer_id;
        self.panel_drawer_is_visible = drawer_visible;

        let discovered_voc_pane_id = voc_pane_id_in_manifest(&pane_manifest, self.voc_pane_id);
        let previous_voc_pane_id = self.voc_pane_id;
        match discovered_voc_pane_id {
            Some(pane_id) => {
                self.voc_pane_id = Some(pane_id);
                self.voc_pane_seen = true;
            },
            None if self.voc_pane_seen => {
                self.voc_pane_id = None;
                self.voc_pane_seen = false;
            },
            None => {},
        }
        let voc_pane_changed = self.voc_pane_id != previous_voc_pane_id;
        self.quick_cmd_pane = self
            .quick_cmd_pane
            .and_then(|tracked| track_quick_cmd(&pane_manifest, tracked));

        let rows = inventory_for_tab(
            &pane_manifest,
            current_tab_position(self.active_tab_idx),
            self.own_plugin_id,
            floating_visible,
        );
        let next_pager = active_pager(&rows);
        let pager_changed = self.panels_pager != next_pager;
        self.panels_pager = next_pager;
        let count_changed = self.panel_count != rows.len();
        self.panel_count = rows.len();
        let drawer_rows_changed = if self.is_panel_drawer {
            let drawer_rows = self.drawer_inventory(&pane_manifest, floating_visible);
            self.panel_drawer.replace_rows(drawer_rows)
        } else {
            false
        };
        if !self.is_panel_drawer
            && !self.is_tooltip
            && floating_visible
            && let Some(panes) = pane_manifest
                .panes
                .get(&current_tab_position(self.active_tab_idx))
        {
            suppress_context_layers(contextual_panes_to_hide(panes));
        }
        self.pane_manifest = Some(pane_manifest);

        failures_changed
            || dead_changed
            || tooltip_changed
            || count_changed
            || pager_changed
            || drawer_changed
            || voc_pane_changed
            || drawer_rows_changed
    }

    fn handle_mouse_event(&mut self, mouse_event: Mouse) -> bool {
        if self.is_panel_drawer {
            if let Mouse::LeftClick(line, col) = mouse_event {
                let command = self.panel_drawer.handle_click(line, col);
                return self.apply_drawer_command(command);
            }
            return false;
        }
        if self.is_tooltip {
            return false;
        }

        match mouse_event {
            Mouse::LeftClick(_, col) => self.handle_tab_click(col),
            Mouse::MiddleClick(_, col) => self.handle_middle_click(col),
            Mouse::ScrollUp(lines) => {
                self.forward_scroll_to_focused_pane(true, lines);
                false
            },
            Mouse::ScrollDown(lines) => {
                self.forward_scroll_to_focused_pane(false, lines);
                false
            },
            _ => false,
        }
    }

    fn handle_timer(&mut self, elapsed: f64) -> bool {
        if timer_is_close_arm(elapsed, CLOSE_ARM_TIMEOUT_SECS, CLIPBOARD_HINT_TTL_SECONDS) {
            if self.stale_close_arm_timers > 0 {
                self.stale_close_arm_timers -= 1;
                false
            } else {
                self.armed_close.take().is_some()
            }
        } else {
            self.handle_clipboard_hint_timeout()
        }
    }

    fn handle_middle_click(&mut self, col: usize) -> bool {
        match middle_close_hit(&self.tab_line, col) {
            Some(tab_id) => self.request_close(tab_id),
            None => false,
        }
    }

    fn request_close(&mut self, tab_id: usize) -> bool {
        let guest = false;
        if guest && tab_id == usize::MAX {
            return false;
        }
        let dead = self.close_target_is_dead(tab_id, guest);
        let armed = self.armed_close.map(|arm| (arm.tab_id, arm.guest));
        match decide_close(armed, tab_id, guest, dead) {
            CloseDecision::Arm { tab_id, guest } => {
                self.arm_close(tab_id, guest);
                true
            },
            CloseDecision::Confirm { tab_id, guest }
            | CloseDecision::CloseImmediately { tab_id, guest } => {
                self.disarm_close();
                self.commit_close(tab_id, guest);
                true
            },
        }
    }

    fn close_target_is_dead(&self, tab_id: usize, _guest: bool) -> bool {
        self.tabs
            .iter()
            .any(|tab| tab.tab_id == tab_id && self.dead_tab_positions.contains(&tab.position))
    }

    fn arm_close(&mut self, tab_id: usize, guest: bool) {
        if self.armed_close.is_some() {
            self.stale_close_arm_timers = self.stale_close_arm_timers.saturating_add(1);
        }
        self.armed_close = Some(CloseArm { tab_id, guest });
        set_timeout(CLOSE_ARM_TIMEOUT_SECS);
    }

    fn disarm_close(&mut self) {
        if self.armed_close.take().is_some() {
            self.stale_close_arm_timers = self.stale_close_arm_timers.saturating_add(1);
        }
    }

    fn commit_close(&self, tab_id: usize, _guest: bool) {
        close_tab_with_id(tab_id as u64);
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
        let next_pager = active_pager(&rows);
        let pager_changed = self.panels_pager != next_pager;
        self.panels_pager = next_pager;
        let count_changed = self.panel_count != rows.len();
        self.panel_count = rows.len();
        let drawer_rows_changed = if self.is_panel_drawer {
            let drawer_rows = self.drawer_inventory(manifest, floating_visible);
            self.panel_drawer.replace_rows(drawer_rows)
        } else {
            false
        };
        count_changed || pager_changed || drawer_rows_changed
    }

    /// The drawer lists its scope's inventory; the bar chip stays tab-scoped
    /// (its count answers "how many panels here").
    fn drawer_inventory(&self, manifest: &PaneManifest, floating_visible: bool) -> Vec<PanelRow> {
        if self.panel_drawer.scope == DrawerScope::Global && !self.drawer_sessions.is_empty() {
            // Global pokazuje panele wszystkich sesji (zgłoszenie Macieja
            // 2026-10-05); the SessionUpdate snapshot is the only
            // cross-session truth a plugin receives.
            return inventory_across_sessions(
                &self.drawer_sessions,
                self.own_plugin_id,
                floating_visible,
            );
        }
        inventory_for_scope(
            manifest,
            self.own_plugin_id,
            floating_visible,
            self.panel_drawer.scope,
        )
    }

    /// Store the cross-session snapshot and rebuild the drawer rows —
    /// Project pokazuje cały workspace, Global wszystkie sesje (zgłoszenie
    /// Macieja 2026-10-05).
    fn handle_session_update(&mut self, session_infos: Vec<SessionInfo>) -> bool {
        if !self.is_tooltip && !self.is_panel_drawer {
            let producer = session_infos
                .iter()
                .find(|s| s.is_current_session)
                .and_then(|s| {
                    s.plugins
                        .iter()
                        .find(|(_, p)| is_tab_title_producer(&p.location))
                        .map(|(id, _)| *id)
                });
            if self.title_plugin_id != producer {
                self.title_plugin_id = producer;
                self.command_labels.clear();
                if let Some(id) = producer {
                    // Runtime plugin ids include the session canvas. A request
                    // identifies this actual renderer to the producer, with no
                    // broadcast or launch of another plugin instance.
                    #[cfg(target_family = "wasm")]
                    pipe_message_to_plugin(
                        MessageToPlugin::new(DISPLAY_REGISTER_MESSAGE)
                            .with_destination_plugin_id(id),
                    );
                    #[cfg(not(target_family = "wasm"))]
                    let _ = id;
                }
                return true;
            }
        }
        if !self.is_panel_drawer {
            return false;
        }
        self.drawer_sessions = session_infos
            .into_iter()
            .map(|session| (session.name, session.is_current_session, session.panes))
            .collect();
        if let Some(manifest) = self.pane_manifest.clone() {
            let floating_visible = floating_panes_visible(&self.tabs);
            let rows = self.drawer_inventory(&manifest, floating_visible);
            self.panel_drawer.replace_rows(rows);
        }
        true
    }

    fn handle_drawer_key(&mut self, key: KeyWithModifier) -> bool {
        if !self.is_panel_drawer {
            return false;
        }
        let command = self.panel_drawer.handle_key(&key);
        self.apply_drawer_command(command)
    }

    /// Returns whether the drawer must repaint (scope or selection changed).
    fn apply_drawer_command(&mut self, command: DrawerCommand) -> bool {
        match command {
            DrawerCommand::Hide => {
                #[cfg(target_family = "wasm")]
                hide_self();
                false
            },
            DrawerCommand::SetScope(scope) => {
                if self.panel_drawer.scope == scope {
                    return false;
                }
                self.panel_drawer.scope = scope;
                let Some(manifest) = self.pane_manifest.clone() else {
                    return true;
                };
                let floating_visible = floating_panes_visible(&self.tabs);
                let rows = self.drawer_inventory(&manifest, floating_visible);
                self.panel_drawer.replace_rows(rows);
                true
            },
            DrawerCommand::Focus(pane_id) => {
                #[cfg(target_family = "wasm")]
                {
                    show_pane_with_id(pane_id, true, true);
                    if let Some(pane) = self
                        .pane_manifest
                        .as_ref()
                        .and_then(|manifest| {
                            manifest
                                .panes
                                .get(&current_tab_position(self.active_tab_idx))
                        })
                        .and_then(|panes| {
                            panes.iter().find(|pane| {
                                (if pane.is_plugin {
                                    PaneId::Plugin(pane.id)
                                } else {
                                    PaneId::Terminal(pane.id)
                                }) == pane_id
                            })
                        })
                    {
                        let coordinates = match ContextLayer::of(pane) {
                            Some(ContextLayer::QuickCmd) => quick_cmd_coordinates(),
                            Some(ContextLayer::Composer) => composer_coordinates(),
                            _ => None,
                        };
                        if let Some(coordinates) = coordinates {
                            change_floating_panes_coordinates(vec![(pane_id, coordinates)]);
                        }
                    }
                    hide_self();
                }
                #[cfg(not(target_family = "wasm"))]
                let _ = pane_id;
                false
            },
            DrawerCommand::Teleport {
                session,
                tab_position,
            } => {
                // Panels is a switcher, never a follower (kanon 2026-09-23):
                // a Global row from another session teleports the operator.
                #[cfg(target_family = "wasm")]
                {
                    switch_session_with_focus(&session, Some(tab_position), None);
                    hide_self();
                }
                #[cfg(not(target_family = "wasm"))]
                let _ = (session, tab_position);
                false
            },
            DrawerCommand::Redraw => true,
            DrawerCommand::None => false,
        }
    }

    fn toggle_panel_drawer(&self) {
        if self.is_panel_drawer {
            #[cfg(target_family = "wasm")]
            hide_self();
            return;
        }
        if !self.panel_drawer_is_visible {
            self.prepare_context_layer(ContextLayer::Panels);
        }
        if let Some(plugin_id) = self.panel_drawer_plugin_id {
            #[cfg(target_family = "wasm")]
            if self.panel_drawer_is_visible {
                hide_pane_with_id(PaneId::Plugin(plugin_id));
            } else {
                show_pane_with_id(PaneId::Plugin(plugin_id), true, true);
                if let Some(coordinates) = panel_drawer_coordinates() {
                    change_floating_panes_coordinates(vec![(
                        PaneId::Plugin(plugin_id),
                        coordinates,
                    )]);
                }
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
        // A drawer is a content tool, not another session canvas projector.
        config.remove("session_canvas");
        config.remove("session_canvas_kind");
        let tab_id = self
            .tabs
            .iter()
            .find(|tab| tab.active)
            .map(|tab| tab.tab_id)
            .unwrap_or(self.active_tab_idx);
        config.insert("panel_drawer_tab_id".to_owned(), tab_id.to_string());
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

    fn handle_guest_surface_payload(&mut self, _payload: &str) -> bool {
        // Cross-session projection is retired. Only this server's TabUpdate
        // can replace the bar's tab inventory or its current-client selection.
        false
    }

    fn handle_tab_click(&mut self, col: usize) -> bool {
        if let Some(tab_id) = close_hit(&self.tab_line, col) {
            return self.request_close(tab_id);
        }
        let armed_before = self.armed_close;
        let mut host = ZellijVocPaneHost;
        let mut tabs = ZellijNewTabHost;
        self.dispatch_tab_click(col, &mut host, &mut tabs);
        self.armed_close != armed_before
    }

    fn dispatch_tab_click(
        &mut self,
        col: usize,
        host: &mut (impl VocPaneHost + QuickCmdPaneHost),
        tabs: &mut impl NewTabHost,
    ) -> Option<VocClickOutcome> {
        if self.sentinel_clicked(col, NEW_TAB_CLICK_SENTINEL) {
            // Public `new_tab` — a shell tab in the current session. Not a pane.
            tabs.open_shell_tab();
            return None;
        }
        if self.sentinel_clicked(col, THEME_CLICK_SENTINEL) {
            toggle_frame_theme();
            return None;
        }
        if self.sentinel_clicked(col, PANELS_CLICK_SENTINEL) {
            self.toggle_panel_drawer();
            return None;
        }
        if self.sentinel_clicked(col, COMPOSER_CLICK_SENTINEL) {
            self.prepare_context_layer(ContextLayer::Composer);
            open_composer();
            return None;
        }
        if self.sentinel_clicked(col, AGENTS_CLICK_SENTINEL) {
            // Quick cmd floats over the *current* tab — no Agents detour, no
            // deferred spawn race, no "Process will run…" over the wrong pane.
            self.open_or_focus_quick_cmd(host);
            return None;
        }
        if self.sentinel_clicked(col, VOC_CLICK_SENTINEL) {
            let outcome = self.open_or_focus_voc(host, false);
            consume_voc_click_outcome(&outcome);
            return Some(outcome);
        }
        if let Some(tab_idx) = get_tab_to_focus(&self.tab_line, self.active_tab_idx, col) {
            self.disarm_close();
            switch_tab_to(tab_idx.try_into().unwrap());
        }
        None
    }

    fn open_or_focus_voc(
        &mut self,
        host: &mut impl VocPaneHost,
        piped_message: bool,
    ) -> VocClickOutcome {
        let existing_pane_id = self.voc_pane_id.or_else(|| {
            self.pane_manifest
                .as_ref()
                .and_then(|manifest| voc_pane_id_in_manifest(manifest, None))
        });
        let opened_pane = if let Some(pane_id) = existing_pane_id {
            self.voc_pane_id = Some(pane_id);
            self.voc_pane_seen = true;
            // One entry summons and dismisses. Voc is pinned (Global), so the
            // floating-layer hide does not cover it; a press on the console
            // already in front hides it — process and scrollback stay.
            if self.voc_is_in_front(pane_id) {
                host.hide_voc_pane(pane_id);
            } else {
                host.focus_voc_pane(pane_id);
            }
            false
        } else if let Some(pane_id) = host.open_voc_pane() {
            self.voc_pane_id = Some(pane_id);
            // Do not clear this optimistic id on an unrelated manifest that
            // races the server's NewPane update.
            self.voc_pane_seen = false;
            true
        } else {
            false
        };

        VocClickOutcome {
            receipt_line: VOC_CLICK_RECEIPT,
            opened_pane,
            piped_message,
        }
    }

    fn voc_is_in_front(&self, pane_id: u32) -> bool {
        self.pane_manifest
            .as_ref()
            .and_then(|manifest| {
                manifest
                    .panes
                    .get(&current_tab_position(self.active_tab_idx))
            })
            .and_then(|panes| panes.iter().find(|p| !p.is_plugin && p.id == pane_id))
            .is_some_and(|pane| pane.is_focused && !pane.is_suppressed)
    }

    /// One Quick cmd per tab: focus the live one, open a shell only when the
    /// current tab has none. The shell itself stays after each command, so a
    /// second press means "take me back to it", never "stack another".
    fn open_or_focus_quick_cmd(&mut self, host: &mut impl QuickCmdPaneHost) -> bool {
        self.prepare_context_layer(ContextLayer::QuickCmd);
        let tab_position = current_tab_position(self.active_tab_idx);
        let existing_pane_id = self
            .quick_cmd_pane
            .filter(|tracked| tracked.tab_position == tab_position)
            .map(|tracked| tracked.pane_id)
            .or_else(|| {
                self.pane_manifest
                    .as_ref()
                    .and_then(|manifest| quick_cmd_pane_id_in_tab(manifest, tab_position))
            });
        if let Some(pane_id) = existing_pane_id {
            host.focus_quick_cmd_pane(pane_id);
            return false;
        }
        match host.open_quick_cmd_pane() {
            Some(pane_id) => {
                self.quick_cmd_pane = Some(TrackedQuickCmd {
                    tab_position,
                    pane_id,
                    seen: false,
                });
                true
            },
            None => false,
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

    fn prepare_context_layer(&self, entering: ContextLayer) {
        if let Some(panes) = self.pane_manifest.as_ref().and_then(|manifest| {
            manifest
                .panes
                .get(&current_tab_position(self.active_tab_idx))
        }) {
            suppress_context_layers(competing_layers(panes, entering));
        }
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

fn suppress_context_layers(panes: Vec<PaneId>) {
    for pane in panes {
        #[cfg(target_family = "wasm")]
        hide_pane_with_id(pane);
        #[cfg(not(target_family = "wasm"))]
        let _ = pane;
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

/// Quick cmd mini console: shallow, wide, below the content header — non-ephemeral
/// interactive terminal (not a command-pane "Process will run…" ticket).
/// Commands run in-pane; the operator inspects output without the float dying.
fn quick_cmd_coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("18%".to_owned()),
        Some("65%".to_owned()),
        Some("64%".to_owned()),
        Some("28%".to_owned()),
        Some(false),
        None,
    )
}

/// Voc is a host tool, not nested agent chrome: give its terminal a large,
/// stable work surface while leaving the host bar visible for repeat focus.
fn voc_coordinates() -> Option<FloatingPaneCoordinates> {
    FloatingPaneCoordinates::new(
        Some("10%".to_owned()),
        Some("7%".to_owned()),
        Some("80%".to_owned()),
        Some("78%".to_owned()),
        Some(true),
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

fn consume_voc_click_outcome(outcome: &VocClickOutcome) {
    eprintln!("{}", voc_click_outcome_report(outcome));
}

fn voc_click_outcome_report(outcome: &VocClickOutcome) -> String {
    format!(
        "{} opened_pane={} piped_message={}",
        outcome.receipt_line, outcome.opened_pane, outcome.piped_message
    )
}

/// The background `vc-tab-title` runtime, whatever spelling loaded it:
/// `vc-frame:vc-tab-title`, `zellij:vc-tab-title` or a `file:` wasm path.
/// Same leaf rule as the server's utility identity (`utility_location_key`).
fn is_tab_title_producer(location: &str) -> bool {
    let leaf = location
        .rsplit([':', '/'])
        .next()
        .unwrap_or(location)
        .trim_end_matches(".wasm");
    matches!(leaf, "vc-tab-title" | "vc_tab_title")
}

fn voc_pane_id_in_manifest(
    pane_manifest: &PaneManifest,
    tracked_pane_id: Option<u32>,
) -> Option<u32> {
    pane_manifest
        .panes
        .values()
        .flatten()
        .find(|pane| {
            !pane.is_plugin && (tracked_pane_id == Some(pane.id) || pane.title == VOC_PANE_NAME)
        })
        .map(|pane| pane.id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrackedQuickCmd {
    tab_position: usize,
    pane_id: u32,
    seen: bool,
}

fn live_quick_cmd(pane: &PaneInfo) -> bool {
    !pane.is_plugin && !pane.exited && pane.title == QUICK_CMD_PANE_NAME
}

fn quick_cmd_pane_id_in_tab(pane_manifest: &PaneManifest, tab_position: usize) -> Option<u32> {
    pane_manifest
        .panes
        .get(&tab_position)?
        .iter()
        .find(|pane| live_quick_cmd(pane))
        .map(|pane| pane.id)
}

/// Keep the optimistic id until the server has shown it once; after that the
/// manifest is the truth, so a closed Quick cmd stops being tracked.
fn track_quick_cmd(
    pane_manifest: &PaneManifest,
    tracked: TrackedQuickCmd,
) -> Option<TrackedQuickCmd> {
    let present = pane_manifest
        .panes
        .values()
        .flatten()
        .any(|pane| pane.id == tracked.pane_id && live_quick_cmd(pane));
    if present {
        Some(TrackedQuickCmd {
            seen: true,
            ..tracked
        })
    } else if tracked.seen {
        None
    } else {
        Some(tracked)
    }
}

trait QuickCmdPaneHost {
    fn open_quick_cmd_pane(&mut self) -> Option<u32>;
    fn focus_quick_cmd_pane(&mut self, pane_id: u32);
}

trait VocPaneHost {
    fn open_voc_pane(&mut self) -> Option<u32>;
    fn focus_voc_pane(&mut self, pane_id: u32);
    fn hide_voc_pane(&mut self, pane_id: u32);
}

struct ZellijVocPaneHost;

impl VocPaneHost for ZellijVocPaneHost {
    fn open_voc_pane(&mut self) -> Option<u32> {
        let command = CommandToRun::new_with_args("sh", vec!["-c", VOC_COMMAND]);
        let Some(PaneId::Terminal(terminal_pane_id)) =
            open_command_pane_floating(command, voc_coordinates(), BTreeMap::new())
        else {
            return None;
        };
        switch_to_input_mode(&InputMode::Normal);
        rename_terminal_pane(terminal_pane_id, VOC_PANE_NAME);
        set_floating_pane_pinned(PaneId::Terminal(terminal_pane_id), true);
        Some(terminal_pane_id)
    }

    fn focus_voc_pane(&mut self, pane_id: u32) {
        set_floating_pane_pinned(PaneId::Terminal(pane_id), true);
        show_pane_with_id(PaneId::Terminal(pane_id), true, true);
        switch_to_input_mode(&InputMode::Normal);
    }

    fn hide_voc_pane(&mut self, pane_id: u32) {
        hide_pane_with_id(PaneId::Terminal(pane_id));
    }
}

/// What the tab-line `[+]` does. One variant on purpose: a pane is not a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabBarShellAction {
    NewTab,
}

fn tab_bar_plus_action() -> TabBarShellAction {
    TabBarShellAction::NewTab
}

trait NewTabHost {
    fn open_shell_tab(&mut self);
}

struct ZellijNewTabHost;

impl NewTabHost for ZellijNewTabHost {
    fn open_shell_tab(&mut self) {
        // Public plugin command. Name and cwd stay unset so the session's
        // default shell opens in a new tab and focus follows it.
        if tab_bar_plus_action() == TabBarShellAction::NewTab {
            let _ = new_tab(None::<&str>, None::<&str>);
        }
    }
}

/// Quick cmd: non-ephemeral floating *terminal* at a fixed lower-center
/// footprint (spec 1.2 §C). Interactive terminal — not a command-pane ticket —
/// so there is no "Process will run in separated pane" chrome and the pane
/// survives after each command. Prefer the installed `vc-quick-cmd.sh` banner
/// wrapper when present; otherwise open a plain login shell on `.`.
///
/// The fallback runner is **POSIX `sh` only** (no bashisms). Debian/Ubuntu
/// `sh` is dash — `${PWD/#$HOME/~}` is a bash-only rewrite and aborts with
/// `sh: 1: Bad substitution` / exit 2 (the EXIT CODE strip the operator saw).
impl QuickCmdPaneHost for ZellijVocPaneHost {
    fn open_quick_cmd_pane(&mut self) -> Option<u32> {
        // Keep this string dash-clean: ${var:-def} and ${var#prefix} are POSIX;
        // ${var/pat/repl} and ${var/#pat/repl} are not.
        let quick_cmd_runner = quick_cmd_runner_script();
        // open_command_pane_floating + exec keeps one long-lived process (the
        // login shell). We accept command-pane chrome only when the wrapper is
        // missing; preferred path is still a real shell via the wrapper script.
        let command = CommandToRun::new_with_args("sh", vec!["-c", quick_cmd_runner.as_str()]);
        let Some(PaneId::Terminal(terminal_pane_id)) =
            open_command_pane_floating(command, quick_cmd_coordinates(), BTreeMap::new())
        else {
            return None;
        };
        // The host binds this SDK action to this plugin instance's client.
        // Open first: a rejected/unavailable command must not change modes.
        // Both the chip and keybind use this path, including from TAB/LOCK.
        switch_to_input_mode(&InputMode::Normal);
        rename_terminal_pane(terminal_pane_id, QUICK_CMD_PANE_NAME);
        Some(terminal_pane_id)
    }

    fn focus_quick_cmd_pane(&mut self, pane_id: u32) {
        show_pane_with_id(PaneId::Terminal(pane_id), true, true);
        // Unsuppression allocates default geometry in the host; restore the
        // tool's bounded footprint before the next draw.
        if let Some(coordinates) = quick_cmd_coordinates() {
            change_floating_panes_coordinates(vec![(PaneId::Terminal(pane_id), coordinates)]);
        }
        switch_to_input_mode(&InputMode::Normal);
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
        self.prepare_context_layer(ContextLayer::Help);
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
        // Automatic key hints must not displace an explicitly opened tool.
        if self
            .pane_manifest
            .as_ref()
            .and_then(|manifest| {
                manifest
                    .panes
                    .get(&current_tab_position(self.active_tab_idx))
            })
            .is_some_and(|panes| !competing_layers(panes, ContextLayer::Help).is_empty())
        {
            return;
        }
        let message = self.create_tooltip_message(MSG_LAUNCH_TOOLTIP, new_mode);
        pipe_message_to_plugin(message);
    }

    fn create_tooltip_message(&self, name: &str, mode: InputMode) -> MessageToPlugin {
        let mut tooltip_config = self.config.clone();
        tooltip_config.remove("session_canvas");
        tooltip_config.remove("session_canvas_kind");
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
            panels_pager: self.panels_pager,
        };
        self.tab_line = tab_line(&self.mode_info, tab_data, cols, config);

        let output = self
            .tab_line
            .iter()
            .fold(String::new(), |acc, part| acc + &part.part);

        self.render_background_with_text(&output);
    }

    fn prepare_tab_data(&self) -> TabRenderData {
        let projected = &self.tabs;
        let mut all_tabs = Vec::new();
        let mut active_tab_index = 0;
        let mut is_alternate_tab = false;

        for (index, tab) in projected.iter().enumerate() {
            let tab_name = self.get_tab_display_name(tab);

            if tab.active {
                // Index in the visible local row used by the overflow renderer.
                active_tab_index = index;
            }

            let guest = false;
            let close_id = Some(tab.tab_id);
            let dead = self.dead_tab_positions.contains(&tab.position);
            let armed = self
                .armed_close
                .is_some_and(|arm| arm.tab_id == tab.tab_id && arm.guest == guest);
            let affordance = TabCloseAffordance {
                closable: close_id.is_some() && !tab_is_contractual(&tab.name, guest),
                dead,
                armed,
                close_id,
            };
            let colors = self.mode_info.style.colors;
            let failed = self.failed_tab_positions.contains(&tab.position);
            let pane_count = Some(
                tab.selectable_tiled_panes_count
                    .saturating_add(tab.selectable_floating_panes_count),
            );
            // The renderer protects contractual chips and keeps count separate
            // from the +N badge for hidden neighbouring tabs.
            let styled_tab = tab_style_with_pane_count(
                (tab_name, pane_count),
                tab,
                is_alternate_tab,
                colors,
                failed,
                affordance,
                guest,
            );

            is_alternate_tab = !is_alternate_tab;
            all_tabs.push(styled_tab);
        }

        TabRenderData {
            tabs: all_tabs,
            active_tab_index,
        }
    }

    fn receive_command_label(&mut self, payload: &str) -> bool {
        let mut parts = payload.split('\n');
        let (Some(id), Some(base), Some(label), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let Ok(id) = id.parse::<usize>() else {
            return false;
        };
        if base.is_empty() && label.is_empty() {
            return self.command_labels.remove(&id).is_some();
        }
        if !matches!(base, "Shell" | "shell") && base != format!("Tab #{}", id + 1) {
            return false;
        }
        // `<native spinner frame> <label>` or `<label>`: the producer forwards
        // a program's braille frame, never its title text.
        let text = label
            .strip_prefix(|c: char| ('\u{2801}'..='\u{28ff}').contains(&c))
            .and_then(|rest| rest.strip_prefix(' '))
            .unwrap_or(label);
        if text.is_empty()
            || text.len() > 24
            || !text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        {
            return false;
        }
        let value = (base.to_owned(), label.to_owned());
        if self.command_labels.get(&id) == Some(&value) {
            return false;
        }
        self.command_labels.insert(id, value);
        true
    }

    fn get_tab_display_name(&self, tab: &TabInfo) -> String {
        if tab.active && self.mode_info.mode == InputMode::RenameTab {
            return if tab.name.is_empty() {
                "Enter name...".into()
            } else {
                tab.name.clone()
            };
        }
        // A label projects only over the stored name it was computed for; a
        // rename (manual or contractual) wins immediately.
        self.command_labels
            .get(&tab.tab_id)
            .filter(|(base, _)| base == &tab.name)
            .map(|(_, label)| label.clone())
            .unwrap_or_else(|| tab.name.clone())
    }
}

fn bind_compact_bar_keys_config(toggle_key: Option<&str>, client_id: u16) -> String {
    let mut config = r#"
        keybinds {
            shared {
                bind "Super Right" {
                    MessagePlugin "compact-bar" { name "vc_tab_navigation"; payload "next"; }
                }
                bind "Super Left" {
                    MessagePlugin "compact-bar" { name "vc_tab_navigation"; payload "previous"; }
                }
            }
            session {
                bind "v" {
                    MessagePlugin "compact-bar" {
                        name "vc_voc"
                    }
                    SwitchToMode "Normal"
                }
            }
    "#
    .to_owned();
    if let Some(toggle_key) = toggle_key {
        config.push_str(&format!(
            r#"
            shared {{
                bind "{}" {{
                  MessagePlugin "compact-bar" {{
                      name "toggle_tooltip"
                      tooltip "{}"
                      payload "{}"
                  }}
                }}
            }}
        "#,
            toggle_key, toggle_key, client_id
        ));
    }
    config.push_str("        }\n");
    config
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
    fn first_close_click_renders_armed_glyph_and_same_cell_confirms_same_tab() {
        let mut state = State {
            tabs: vec![TabInfo {
                name: "codex".into(),
                active: true,
                position: 0,
                tab_id: 9,
                ..TabInfo::default()
            }],
            ..Default::default()
        };
        state.render_tab_line(160);
        let mut offset = 0;
        let column = state
            .tab_line
            .iter()
            .find_map(|part| {
                let start = offset;
                offset += part.len;
                (part.close_id == Some(9)).then(|| start + part.close_start.unwrap() + 1)
            })
            .unwrap();
        assert!(state.handle_tab_click(column));
        assert_eq!(
            state.armed_close,
            Some(CloseArm {
                tab_id: 9,
                guest: false
            })
        );
        state.render_tab_line(160);
        let armed = state
            .tab_line
            .iter()
            .find(|part| part.close_id == Some(9))
            .unwrap();
        assert!(armed.part.contains("✖︎"));
        assert!(state.handle_tab_click(column));
        assert_eq!(state.armed_close, None);
        state.render_tab_line(160);
        assert!(!state.tab_line.iter().any(|part| part.part.contains("✖︎")));
    }

    #[test]
    fn armed_close_glyph_disappears_on_timeout_or_tab_switch() {
        let mut state = State {
            tabs: vec![
                TabInfo {
                    name: "codex".into(),
                    active: true,
                    position: 0,
                    tab_id: 9,
                    ..TabInfo::default()
                },
                TabInfo {
                    name: "claude".into(),
                    position: 1,
                    tab_id: 10,
                    ..TabInfo::default()
                },
            ],
            ..Default::default()
        };
        assert!(state.request_close(9));
        assert!(
            state
                .prepare_tab_data()
                .tabs
                .iter()
                .any(|part| part.part.contains("✖︎"))
        );
        assert!(state.handle_timer(CLOSE_ARM_TIMEOUT_SECS));
        assert!(
            !state
                .prepare_tab_data()
                .tabs
                .iter()
                .any(|part| part.part.contains("✖︎"))
        );
        assert!(state.request_close(9));
        state.render_tab_line(160);
        let mut offset = 0;
        let next_label = state
            .tab_line
            .iter()
            .find_map(|part| {
                let start = offset;
                offset += part.len;
                (part.close_id == Some(10)).then_some(start + 1)
            })
            .unwrap();
        state.handle_tab_click(next_label);
        assert_eq!(state.armed_close, None);
        assert!(
            !state
                .prepare_tab_data()
                .tabs
                .iter()
                .any(|part| part.part.contains("✖︎"))
        );
    }

    #[test]
    fn canonical_live_runs_feed_does_not_create_a_third_projection_or_wake_parked_chrome() {
        let mut state = State {
            is_visible: false,
            ..Default::default()
        };

        // The rail projection lives in session-manager and LIVE lives in the
        // status bar. Compact-bar must ignore vc.live-runs.v1 so a feed update
        // cannot override its targeted visibility lifecycle.
        assert!(!state.update(Event::CustomMessage(
            "vc.live-runs.v1".to_owned(),
            r#"{"schema":"vc.live-runs.v1","runs":[{"run_id":"r1"}]}"#.to_owned(),
        )));
        assert!(!state.is_visible, "the parked compact bar must stay parked");
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
    fn super_navigation_key_config_routes_both_modes_to_the_bar() {
        let config = zellij_utils::input::config::Config::from_kdl(
            &bind_compact_bar_keys_config(None, 7),
            None,
        )
        .unwrap();
        for mode in [InputMode::Locked, InputMode::Normal] {
            for (key, payload) in [(BareKey::Left, "previous"), (BareKey::Right, "next")] {
                let actions = config
                    .keybinds
                    .get_actions_for_key_in_mode(
                        &mode,
                        &KeyWithModifier::new(key).with_super_modifier(),
                    )
                    .unwrap();
                assert!(
                    matches!(actions.as_slice(), [zellij_utils::input::actions::Action::KeybindPipe {
                    name: Some(name), payload: Some(actual), plugin: Some(plugin), ..
                }] if name == MSG_TAB_NAVIGATION && actual == payload && plugin == "compact-bar")
                );
            }
        }
    }

    #[test]
    fn super_navigation_accepts_only_private_keybinds_for_the_active_bar() {
        let mut state = State::default();
        state.config.insert("session_canvas".into(), "true".into());
        let message = PipeMessage::new(
            PipeSource::Keybind,
            MSG_TAB_NAVIGATION,
            &Some("next".into()),
            &None,
            true,
        );
        assert!(state.tab_navigation_message_targets_active_bar(&message));
        state.is_tooltip = true;
        assert!(!state.tab_navigation_message_targets_active_bar(&message));
        state.is_tooltip = false;
        state.is_panel_drawer = true;
        assert!(!state.tab_navigation_message_targets_active_bar(&message));
        state.is_panel_drawer = false;
        let public = PipeMessage::new(
            PipeSource::Keybind,
            MSG_TAB_NAVIGATION,
            &Some("next".into()),
            &None,
            false,
        );
        assert!(!state.tab_navigation_message_targets_active_bar(&public));
        let cli = PipeMessage::new(
            PipeSource::Cli("pipe".into()),
            MSG_TAB_NAVIGATION,
            &Some("next".into()),
            &None,
            true,
        );
        assert!(!state.tab_navigation_message_targets_active_bar(&cli));
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
    fn drawer_scope_filters_the_list_while_the_chip_stays_tab_scoped() {
        use crate::panel_drawer::DrawerScope;
        use std::collections::HashMap;
        let mut state = State {
            is_panel_drawer: true,
            active_tab_idx: 1,
            ..Default::default()
        };
        state.mode_info.session_name = Some("workspace-a".into());
        state.tabs = vec![
            TabInfo {
                name: "Home".into(),
                active: true,
                position: 0,
                ..Default::default()
            },
            TabInfo {
                name: VC_SHARED_WORKSPACE_TAB_NAME.into(),
                active: false,
                position: 1,
                ..Default::default()
            },
        ];
        let mut panes = HashMap::new();
        panes.insert(
            0,
            vec![PaneInfo {
                id: 1,
                title: "shell".into(),
                is_selectable: true,
                ..Default::default()
            }],
        );
        panes.insert(
            1,
            vec![
                PaneInfo {
                    id: 2,
                    title: "VC Guest".into(),
                    is_selectable: true,
                    terminal_command: Some(
                        "vc-frame --workspace-projection {\"guest\":\"workspace-a\"} visit workspace-a"
                            .into(),
                    ),
                    ..Default::default()
                },
                PaneInfo {
                    id: 3,
                    title: "agent".into(),
                    is_selectable: true,
                    is_floating: true,
                    panel_scope: Some(PanelScope::Project("workspace-a".into())),
                    ..Default::default()
                },
            ],
        );
        assert!(state.handle_pane_update(PaneManifest { panes }));
        // Default scope: Global — the full switcher across tabs.
        assert_eq!(state.panel_drawer.rows.len(), 3);
        // The bar chip count is tab-scoped regardless (pinned behavior).
        assert_eq!(state.panel_count, 1);
        // Project pokazuje cały workspace (zgłoszenie Macieja 2026-10-05):
        // every tab of this session, not the current tab + guest rows. The
        // floating panel 3 is hidden here (floating layer off), so the
        // per-tab sort puts it after the visible visitor.
        assert!(state.apply_drawer_command(DrawerCommand::SetScope(DrawerScope::Project)));
        assert_eq!(
            state
                .panel_drawer
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        // Re-selecting the active scope is a no-op; switching back restores.
        assert!(!state.apply_drawer_command(DrawerCommand::SetScope(DrawerScope::Project)));
        assert!(state.apply_drawer_command(DrawerCommand::SetScope(DrawerScope::Global)));
        assert_eq!(state.panel_drawer.rows.len(), 3);
        // A SessionUpdate snapshot upgrades Global to the cross-session
        // truth: foreign rows arrive stamped with their origin session.
        let mut foreign_panes = HashMap::new();
        foreign_panes.insert(
            0,
            vec![PaneInfo {
                id: 9,
                title: "guest-shell".into(),
                is_selectable: true,
                ..Default::default()
            }],
        );
        let sessions = vec![
            SessionInfo {
                name: "workspace-a".into(),
                is_current_session: true,
                panes: state.pane_manifest.clone().unwrap(),
                ..Default::default()
            },
            SessionInfo {
                name: "loctree-suite".into(),
                is_current_session: false,
                panes: PaneManifest {
                    panes: foreign_panes,
                },
                ..Default::default()
            },
        ];
        assert!(state.handle_session_update(sessions));
        assert_eq!(state.panel_drawer.rows.len(), 4);
        let foreign = state
            .panel_drawer
            .rows
            .iter()
            .find(|row| row.id == 9)
            .expect("foreign session row present in Global");
        assert_eq!(foreign.session.as_deref(), Some("loctree-suite"));
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
    fn context_instances_do_not_inherit_canvas_authority_and_drawers_are_tab_scoped() {
        let mut state = State::default();
        state.config.insert("session_canvas".into(), "true".into());
        state
            .config
            .insert("session_canvas_kind".into(), "compact-bar".into());
        state.tabs = vec![TabInfo {
            active: true,
            tab_id: 42,
            ..Default::default()
        }];
        let first = state.panel_drawer_launch_message().unwrap();
        assert!(!first.plugin_config.contains_key("session_canvas"));
        assert!(!first.plugin_config.contains_key("session_canvas_kind"));
        state.tabs[0].tab_id = 43;
        let next = state.panel_drawer_launch_message().unwrap();
        assert_ne!(first.plugin_config, next.plugin_config);
        let tooltip = state.create_tooltip_message(MSG_LAUNCH_TOOLTIP, InputMode::Tab);
        assert!(!tooltip.plugin_config.contains_key("session_canvas"));
        assert!(!tooltip.plugin_config.contains_key("session_canvas_kind"));
    }

    #[derive(Default)]
    struct FakeVocPaneHost {
        open_result: Option<u32>,
        open_count: usize,
        focused: Vec<u32>,
        hidden: Vec<u32>,
        quick_open_result: Option<u32>,
        quick_open_count: usize,
        quick_focused: Vec<u32>,
    }

    impl VocPaneHost for FakeVocPaneHost {
        fn open_voc_pane(&mut self) -> Option<u32> {
            self.open_count += 1;
            self.open_result.take()
        }

        fn focus_voc_pane(&mut self, pane_id: u32) {
            self.focused.push(pane_id);
        }

        fn hide_voc_pane(&mut self, pane_id: u32) {
            self.hidden.push(pane_id);
        }
    }

    #[derive(Default)]
    struct FakeNewTabHost {
        opens: usize,
    }

    impl NewTabHost for FakeNewTabHost {
        fn open_shell_tab(&mut self) {
            self.opens += 1;
        }
    }

    #[test]
    fn plus_click_opens_a_new_tab_and_does_not_open_a_pane() {
        assert_eq!(tab_bar_plus_action(), TabBarShellAction::NewTab);

        let data = crate::line::tab_line(
            &ModeInfo::default(),
            TabRenderData {
                tabs: vec![LinePart {
                    part: " shell ".to_owned(),
                    len: 8,
                    tab_index: Some(0),
                    close_start: None,
                    close_id: None,
                }],
                active_tab_index: 0,
            },
            120,
            crate::line::TabLineConfig {
                mode: InputMode::Normal,
                toggle_tooltip_key: None,
                tooltip_is_active: false,
                brand_text: None,
                brand_text_short: None,
                left_inset: 6,
                theme_indicator: "☾".to_owned(),
                pane_count: 0,
                panels_pager: None,
            },
        );
        let mut offset = 0;
        let mut plus_col = None;
        for part in &data {
            if part.tab_index == Some(NEW_TAB_CLICK_SENTINEL) {
                assert!(
                    part.part.contains("[+]"),
                    "the clickable control must read as [+], got {}",
                    part.part
                );
                plus_col = Some(offset);
                break;
            }
            offset += part.len;
        }
        let plus_col = plus_col.expect("rendered tab line must include [+]");

        let mut state = State {
            tab_line: data,
            ..Default::default()
        };
        let mut panes = FakeVocPaneHost::default();
        let mut tabs = FakeNewTabHost::default();
        let outcome = state.dispatch_tab_click(plus_col, &mut panes, &mut tabs);
        assert!(outcome.is_none(), "[+] is not a Voc click");
        assert_eq!(tabs.opens, 1, "click must request one new shell tab");
        assert_eq!(panes.open_count, 0, "[+] must not open a pane");
        assert!(panes.focused.is_empty());

        // The leading seam of the same part is the same control.
        assert_eq!(tabs.opens, 1);
        let _ = state.dispatch_tab_click(plus_col + 1, &mut panes, &mut tabs);
        assert_eq!(tabs.opens, 2);
        assert_eq!(panes.open_count, 0);
    }

    impl QuickCmdPaneHost for FakeVocPaneHost {
        fn open_quick_cmd_pane(&mut self) -> Option<u32> {
            self.quick_open_count += 1;
            self.quick_open_result.take()
        }

        fn focus_quick_cmd_pane(&mut self, pane_id: u32) {
            self.quick_focused.push(pane_id);
        }
    }

    fn quick_cmd_pane(id: u32, exited: bool) -> PaneInfo {
        PaneInfo {
            id,
            title: QUICK_CMD_PANE_NAME.to_owned(),
            exited,
            ..Default::default()
        }
    }

    #[test]
    fn a_second_quick_cmd_press_focuses_the_shell_instead_of_stacking_another() {
        // 2026-09-24: the one-shot close was the old cure for piling up Quick
        // cmd panes. The shell now stays; the bar keeps it single.
        let mut state = State::default();
        let mut host = FakeVocPaneHost {
            quick_open_result: Some(7),
            ..Default::default()
        };

        assert!(state.open_or_focus_quick_cmd(&mut host));
        assert!(!state.open_or_focus_quick_cmd(&mut host));

        assert_eq!(host.quick_open_count, 1, "a repeat press must not spawn");
        assert_eq!(host.quick_focused, vec![7]);
    }

    #[test]
    fn a_live_quick_cmd_in_the_current_tab_is_found_after_a_plugin_reload() {
        let mut manifest = PaneManifest::default();
        manifest.panes.insert(0, vec![quick_cmd_pane(12, false)]);
        let mut state = State {
            pane_manifest: Some(manifest),
            ..Default::default()
        };
        let mut host = FakeVocPaneHost::default();

        assert!(!state.open_or_focus_quick_cmd(&mut host));

        assert_eq!(host.quick_open_count, 0);
        assert_eq!(host.quick_focused, vec![12]);
    }

    #[test]
    fn an_exited_or_other_tab_quick_cmd_does_not_stand_in_for_this_tab() {
        let mut manifest = PaneManifest::default();
        manifest.panes.insert(0, vec![quick_cmd_pane(3, true)]);
        manifest.panes.insert(1, vec![quick_cmd_pane(4, false)]);
        let mut state = State {
            pane_manifest: Some(manifest),
            ..Default::default()
        };
        let mut host = FakeVocPaneHost {
            quick_open_result: Some(9),
            ..Default::default()
        };

        assert!(state.open_or_focus_quick_cmd(&mut host));

        assert_eq!(host.quick_open_count, 1);
        assert!(host.quick_focused.is_empty());
    }

    #[test]
    fn a_closed_quick_cmd_stops_being_tracked_once_the_server_has_shown_it() {
        let tracked = TrackedQuickCmd {
            tab_position: 0,
            pane_id: 7,
            seen: false,
        };
        let mut with_pane = PaneManifest::default();
        with_pane.panes.insert(0, vec![quick_cmd_pane(7, false)]);
        let without_pane = PaneManifest::default();

        // Not shown yet: the open/update race keeps the optimistic id.
        assert_eq!(track_quick_cmd(&without_pane, tracked), Some(tracked));
        let seen = track_quick_cmd(&with_pane, tracked).unwrap();
        assert!(seen.seen);
        // Shown once, then gone: the operator closed it.
        assert_eq!(track_quick_cmd(&without_pane, seen), None);
    }

    #[test]
    fn voc_click_opens_once_then_focuses_the_existing_host_console() {
        let mut state = State {
            tab_line: vec![LinePart {
                part: " Voc ".to_owned(),
                len: crate::line::VOC_CHIP_COLS,
                tab_index: Some(VOC_CLICK_SENTINEL),
                close_start: None,
                close_id: None,
            }],
            ..Default::default()
        };
        assert!(
            state.sentinel_clicked(0, VOC_CLICK_SENTINEL),
            "column 0 of the Voc chip must hit the sentinel"
        );
        assert!(state.sentinel_clicked(crate::line::VOC_CHIP_COLS - 1, VOC_CLICK_SENTINEL));
        assert!(!state.sentinel_clicked(crate::line::VOC_CHIP_COLS, VOC_CLICK_SENTINEL));

        let mut host = FakeVocPaneHost {
            open_result: Some(41),
            ..Default::default()
        };
        let mut tabs = FakeNewTabHost::default();
        let opened = state.dispatch_tab_click(1, &mut host, &mut tabs).unwrap();
        assert!(opened.opened_pane);
        assert!(!opened.piped_message);
        assert_eq!(state.voc_pane_id, Some(41));
        assert_eq!(host.open_count, 1);
        assert!(host.focused.is_empty());
        assert_eq!(
            voc_click_outcome_report(&opened),
            "compact-bar: Voc host console opened_pane=true piped_message=false"
        );

        let focused = state.dispatch_tab_click(1, &mut host, &mut tabs).unwrap();
        assert!(!focused.opened_pane);
        assert!(!focused.piped_message);
        assert_eq!(host.open_count, 1, "repeat click must not spawn");
        assert_eq!(host.focused, vec![41]);
    }

    #[test]
    fn voc_keybind_is_private_active_bar_routing_and_reports_the_pipe() {
        let mut state = State::default();
        state
            .config
            .insert("session_canvas".to_owned(), "true".to_owned());
        let private_message =
            PipeMessage::new(PipeSource::Keybind, MSG_OPEN_VOC, &None, &None, true);
        assert!(state.voc_message_targets_active_bar(&private_message));
        let public_message =
            PipeMessage::new(PipeSource::Keybind, MSG_OPEN_VOC, &None, &None, false);
        assert!(!state.voc_message_targets_active_bar(&public_message));

        let mut host = FakeVocPaneHost {
            open_result: Some(9),
            ..Default::default()
        };
        let outcome = state.open_or_focus_voc(&mut host, true);
        assert!(outcome.opened_pane);
        assert!(outcome.piped_message);
        assert_eq!(host.open_count, 1);
    }

    #[test]
    fn voc_manifest_recovers_and_releases_the_singleton() {
        let mut state = State::default();
        let manifest = PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![PaneInfo {
                    id: 17,
                    title: VOC_PANE_NAME.to_owned(),
                    ..PaneInfo::default()
                }],
            )]),
        };
        assert!(state.handle_pane_update(manifest));
        assert_eq!(state.voc_pane_id, Some(17));

        let mut host = FakeVocPaneHost {
            open_result: Some(18),
            ..Default::default()
        };
        let focused = state.open_or_focus_voc(&mut host, false);
        assert!(!focused.opened_pane);
        assert_eq!(host.focused, vec![17]);
        assert_eq!(host.open_count, 0);

        assert!(state.handle_pane_update(PaneManifest::default()));
        assert_eq!(state.voc_pane_id, None);
        let reopened = state.open_or_focus_voc(&mut host, false);
        assert!(reopened.opened_pane);
        assert_eq!(state.voc_pane_id, Some(18));
    }

    #[test]
    fn voc_key_config_installs_session_v_without_dropping_tooltip_binding() {
        let config = bind_compact_bar_keys_config(Some("Ctrl y"), 7);
        assert!(config.contains("session"));
        assert!(config.contains("vc_voc"));
        assert!(config.contains("SwitchToMode \"Normal\""));
        assert!(config.contains("toggle_tooltip"));
        assert!(config.contains("payload \"7\""));
    }

    #[test]
    fn voc_runner_delegates_resolution_and_names_the_missing_launcher() {
        assert!(VOC_COMMAND.contains("vibecrafted tui"));
        assert!(!VOC_COMMAND.contains("command -v voc"));
        assert!(VOC_COMMAND.contains("Voc console is unavailable"));
    }
    #[test]
    fn native_tabs_keep_order_counts_close_and_reject_foreign_projection() {
        let mut state = State::default();
        state.handle_tab_update(vec![
            TabInfo {
                name: "Workspace".into(),
                active: true,
                tab_id: 9,
                selectable_tiled_panes_count: 2,
                selectable_floating_panes_count: 1,
                ..Default::default()
            },
            TabInfo {
                name: "Agents".into(),
                position: 1,
                tab_id: 10,
                ..Default::default()
            },
        ]);
        let foreign = r#"{"session":"other","host_plugin_id":4,"tabs":[{"name":"leaked","active":true,"position":0,"tab_id":99}]}"#;
        assert!(!state.update(Event::CustomMessage(
            VC_GUEST_SURFACE_MESSAGE.into(),
            foreign.into()
        )));
        assert_eq!(state.tabs[0].name, "Workspace");
        assert_eq!(state.tabs[1].name, "Agents");
        let rendered = state.prepare_tab_data();
        // Decyzja Macieja 2026-10-05: pane counts stay data-only — the chip
        // renders the bare name even though the host published 2+1 panes.
        assert!(rendered.tabs[0].part.contains("Workspace"));
        assert!(!rendered.tabs[0].part.contains("Workspace (3)"));
        assert!(rendered.tabs[0].part.contains("◉"));
        assert!(!rendered.tabs.iter().any(|tab| tab.part.contains("leaked")));
        assert_eq!(state.tab_navigation(true), TabNavigation::HostNext);
        assert_eq!(state.tab_navigation(false), TabNavigation::HostPrevious);
    }
    #[test]
    fn command_projection_preserves_names_and_native_spinner_frames() {
        let mut state = State::default();
        let tab = TabInfo {
            tab_id: 2,
            position: 2,
            name: "Shell".into(),
            ..Default::default()
        };
        assert!(state.receive_command_label("2\nShell\ncargo build"));
        assert_eq!(state.get_tab_display_name(&tab), "cargo build");
        // The producer samples the program's title and forwards its frame.
        assert!(state.receive_command_label("2\nShell\n⠦ codex"));
        assert_eq!(state.get_tab_display_name(&tab), "⠦ codex");
        assert!(state.receive_command_label("2\nShell\n⠧ codex"));
        assert_eq!(state.get_tab_display_name(&tab), "⠧ codex");
        assert!(state.receive_command_label("2\nShell\ncodex"));
        assert_eq!(state.get_tab_display_name(&tab), "codex");
        // A pane manifest no longer invents a frame of its own.
        let pane = PaneInfo {
            id: 7,
            is_focused: true,
            is_selectable: true,
            title: "⠦ private native conversation | project".into(),
            ..Default::default()
        };
        let mut manifest = PaneManifest::default();
        manifest.panes.insert(2, vec![pane]);
        state.tabs = vec![tab.clone()];
        state.handle_pane_update(manifest);
        assert_eq!(state.get_tab_display_name(&tab), "codex");
        let manual = TabInfo {
            name: "My shell".into(),
            ..tab.clone()
        };
        assert_eq!(state.get_tab_display_name(&manual), "My shell");
        assert_eq!(tab.name, "Shell");
        assert!(!state.receive_command_label("2\nMy shell\nsecret"));
        assert!(!state.receive_command_label("2\nShell\nsecret\nextra"));
        assert!(!state.receive_command_label("2\nShell\nhttps://secret"));
        assert!(!state.receive_command_label("2\nShell\n⠦ private: title"));
        assert!(!state.receive_command_label("2\nShell\n⠦⠧ codex"));
        assert!(!state.receive_command_label("2\nShell\n⠦ "));
        assert!(state.receive_command_label("2\n\n"));
        assert_eq!(state.get_tab_display_name(&tab), "Shell");
    }

    #[test]
    fn session_canvas_discovers_actual_producer_runtime_and_waits_for_late_start() {
        let mut state = State::default();
        let mut session = SessionInfo {
            is_current_session: true,
            ..Default::default()
        };
        assert!(!state.handle_session_update(vec![session.clone()]));
        session.plugins.insert(
            42,
            PluginInfo {
                location: "vc-frame:vc-tab-title".into(),
                ..Default::default()
            },
        );
        assert!(state.handle_session_update(vec![session.clone()]));
        assert_eq!(state.title_plugin_id, Some(42));
        assert!(!state.handle_session_update(vec![session.clone()]));
        state
            .command_labels
            .insert(2, ("Shell".into(), "codex".into()));
        session.plugins.clear();
        session.plugins.insert(
            43,
            PluginInfo {
                location: "file:/fixture/vc_tab_title.wasm".into(),
                ..Default::default()
            },
        );
        assert!(state.handle_session_update(vec![session]));
        assert_eq!(state.title_plugin_id, Some(43));
        assert!(state.command_labels.is_empty());
    }

    #[test]
    fn producer_identity_is_the_utility_leaf_not_a_substring() {
        for location in [
            "vc-frame:vc-tab-title",
            "zellij:vc-tab-title",
            "file:/opt/plugins/vc-tab-title.wasm",
            "file:/fixture/vc_tab_title.wasm",
        ] {
            assert!(is_tab_title_producer(location), "{location}");
        }
        for location in [
            "vc-frame:compact-bar",
            "file:/opt/vc-tab-title/other.wasm",
            "vc-frame:vc-tab-title-extra",
        ] {
            assert!(!is_tab_title_producer(location), "{location}");
        }
    }

    #[test]
    fn voc_entry_summons_and_dismisses_the_one_console() {
        let mut state = State {
            active_tab_idx: 1,
            ..Default::default()
        };
        let voc = |focused: bool, suppressed: bool| PaneManifest {
            panes: std::collections::HashMap::from([(
                0,
                vec![PaneInfo {
                    id: 17,
                    title: VOC_PANE_NAME.to_owned(),
                    is_floating: true,
                    is_focused: focused,
                    is_suppressed: suppressed,
                    ..PaneInfo::default()
                }],
            )]),
        };
        let mut host = FakeVocPaneHost::default();
        state.handle_pane_update(voc(true, false));
        assert!(!state.open_or_focus_voc(&mut host, false).opened_pane);
        assert_eq!(
            host.hidden,
            vec![17],
            "console in front: the press hides it"
        );
        state.handle_pane_update(voc(true, true));
        state.open_or_focus_voc(&mut host, false);
        assert_eq!(
            host.focused,
            vec![17],
            "hidden console: the press brings it back"
        );
        state.handle_pane_update(voc(false, false));
        state.open_or_focus_voc(&mut host, true);
        assert_eq!(
            host.focused,
            vec![17, 17],
            "behind other work: focus, not hide"
        );
        assert_eq!(host.hidden, vec![17]);
        assert_eq!(host.open_count, 0, "never a second console");
    }

    #[test]
    fn voc_is_global_pinned_console() {
        assert_eq!(voc_coordinates().unwrap().pinned, Some(true));
        assert!(VOC_COMMAND.contains("exec vibecrafted tui"));
        assert!(!VOC_COMMAND.contains("--project"));
    }
}
