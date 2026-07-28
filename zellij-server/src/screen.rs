//! Things related to [`Screen`]s.
//!
//! # Tab Identification
//!
//! Tabs have two distinct identifiers:
//!
//! - **ID** (`tab.id`): Stable, unique identifier that never changes after creation.
//!   Used as BTreeMap key and for internal tracking. Monotonically increasing.
//!
//! - **Position** (`tab.position`): Current display order (0-based index in tab bar).
//!   Changes when tabs are moved or closed. Used for user-facing operations.
//!
//! # Terminology Convention
//!
//! - **"id"**: Always means stable identifier
//! - **"position"**: Always means 0-based display order
//! - **"index"**: Synonym for "position" (used in public/plugin APIs)
//!
//! Examples:
//! - `close_tab_by_id(5)` - Closes tab with stable ID 5
//! - `CloseTabWithIndex(2)` - Closes tab at position 2 (3rd tab visually)
//! - `get_tab_by_position(0)` - Gets first tab in display order
//! - `PluginInstruction::NewTab(tab_id, ...)` - Uses ID for async communication
//!
//! # Key Data Structures
//!
//! - `tabs: BTreeMap<usize, Tab>`: Keyed by tab.id (stable identifier)
//! - `active_tab_ids: BTreeMap<ClientId, usize>`: Maps clients to active tab ID
//! - `tab_history: BTreeMap<ClientId, Vec<usize>>`: History of tab IDs per client

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::rc::Rc;
use std::str;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::route::NotificationEnd;

use log::{debug, warn};
use uuid::Uuid;
use zellij_utils::data::{
    CommandOrPlugin, Direction, EventType, FloatingPaneCoordinates, GetFocusedPaneInfoResponse,
    HostTerminalThemeMode, KeyWithModifier, LayoutInfo, LayoutWithError, ListPanesResponse,
    ListTabsResponse, NewPanePlacement, PaneContents, PaneInfo, PaneListEntry, PaneManifest,
    PaneRenderReport, PaneScrollbackResponse, PluginPermission, RegexHighlight, Resize,
    ResizeStrategy, SessionInfo, Styling, TabInfo, TabPlacement, WebSharing,
};
use zellij_utils::errors::prelude::*;
use zellij_utils::input::command::RunCommand;
use zellij_utils::input::config::Config;
use zellij_utils::input::keybinds::Keybinds;
use zellij_utils::input::mouse::{MouseEvent, MouseEventType};
use zellij_utils::input::options::Clipboard;
use zellij_utils::ipc::{ExitReason, ServerToClientMsg};
use zellij_utils::pane_size::{PaneGeom, Size, SizeInPixels};
use zellij_utils::run_triage::{ViewerCreationFence, ViewerCreationFenceRejection};
use zellij_utils::shared::clean_string_from_control_and_linebreak;
use zellij_utils::{
    channels,
    consts::{ZELLIJ_SOCK_DIR, session_info_folder_for_session},
    envs::set_session_name,
    input::command::TerminalAction,
    input::layout::{
        FloatingPaneLayout, Layout, PercentOrFixed, Run, RunPluginOrAlias, SwapFloatingLayout,
        SwapTiledLayout, TabLayoutInfo, TiledPaneLayout,
    },
    position::Position,
};

use crate::background_jobs::{BackgroundJob, reserve_session_state_generation};
use crate::os_input_output::ResizeCache;
use crate::pane_groups::PaneGroups;
use crate::panes::alacritty_functions::xparse_color;
use crate::panes::terminal_character::AnsiCode;
use crate::panes::terminal_pane::{BRACKETED_PASTE_BEGIN, BRACKETED_PASTE_END};
use crate::session_layout_metadata::{PaneLayoutMetadata, SessionLayoutMetadata};

use crate::{
    ClientId, ServerInstruction,
    output::Output,
    panes::PaneId,
    panes::sixel::SixelImageStore,
    plugins::{
        DumpSessionLayoutResponse, LayoutPluginReceipt, LayoutPluginResolution, PluginId,
        PluginInstruction, PluginRenderAsset,
    },
    pty::{
        ClientTabIndexOrPaneId, LayoutCommitAck, LayoutCommitOutcome, LayoutTransactionId,
        PtyInstruction, VteBytes, get_default_shell,
    },
    pty_writer::PtyWriteInstruction,
    tab::{
        Pane, PendingTabLayoutCleanup, SuppressedPanes, Tab, TabLayoutCommitEffects,
        TabLayoutTransaction, TabTopologyTransaction,
    },
    thread_bus::{Bus, ThreadSenders},
    ui::loading_indication::LoadingIndication,
};
use zellij_utils::{
    data::{Event, InputMode, ModeInfo, Palette, PaletteColor, PluginCapabilities, Style},
    errors::{ContextType, ScreenContext},
    input::get_mode_info,
    ipc::{ClientAttributes, PixelDimensions},
};

/// Parses a namespaced OSC 99 response and extracts the original pane ID
/// and un-namespaced response bytes.
///
/// Input bytes (the termwiz OperatingSystemCommand payload after stripping "99;"):
/// e.g. b"i=p42.mynotif" or b"i=p42.mynotif:p=close;some_data"
///
/// Returns Some((terminal_id, full_osc_bytes)) where full_osc_bytes is
/// the complete reconstructed OSC 99 sequence with original identifier,
/// ready to write to the pane's PTY.
/// Denormalizes a namespaced OSC 99 response.
///
/// Parses the namespaced `i=p<N>[r][q].<original_id>` format and returns:
/// - `pane_id`: the terminal pane that originated the notification
/// - `app_wants_report`: `r` flag — app originally requested `a=report`
/// - `is_query`: `q` flag — this was a capability query (`p=?`)
/// - `restored_response_bytes`: the response with the original `i=` value restored
pub(crate) fn denormalize_notification_response(
    payload: &[u8],
) -> Option<(u32, bool, bool, Vec<u8>)> {
    let payload_str = str::from_utf8(payload).ok()?;

    // Split into metadata and response payload on first ';'
    let (metadata, response_payload) = match payload_str.find(';') {
        Some(idx) => (
            payload_str.get(..idx).unwrap_or_default(),
            payload_str.get(idx..).unwrap_or_default(),
        ),
        None => (payload_str, ""),
    };

    // Find the i= key in colon-separated metadata
    let mut terminal_id = None;
    let mut app_wants_report = false;
    let mut is_query = false;
    let mut restored_parts = Vec::new();

    for kv in metadata.split(':') {
        if let Some(namespaced_value) = kv.strip_prefix("i=p") {
            // Parse "p<N>[r][q].<original_id>"
            if let Some(dot_pos) = namespaced_value.find('.') {
                let flags_part = namespaced_value.get(..dot_pos).unwrap_or_default();
                let original_id = namespaced_value.get(dot_pos + 1..).unwrap_or_default();
                let pane_id_str = flags_part.trim_end_matches(['r', 'q']);
                let flag_chars = flags_part.get(pane_id_str.len()..).unwrap_or_default();
                if let Ok(pid) = pane_id_str.parse::<u32>() {
                    terminal_id = Some(pid);
                    app_wants_report = flag_chars.contains('r');
                    is_query = flag_chars.contains('q');
                    // Empty original_id means the app never sent an i= key;
                    // don't inject one into the response
                    if !original_id.is_empty() {
                        restored_parts.push(format!("i={}", original_id));
                    }
                    continue;
                }
            }
        }
        restored_parts.push(kv.to_string());
    }

    let terminal_id = terminal_id?;
    let restored_metadata = restored_parts.join(":");
    let full_response = format!("\x1b]99;{}{}\x1b\\", restored_metadata, response_payload);
    Some((
        terminal_id,
        app_wants_report,
        is_query,
        full_response.into_bytes(),
    ))
}

/// Get the active tab and call a closure on it
///
/// If no active tab can be found, an error is logged instead.
///
/// # Parameters
///
/// - screen: An instance of `Screen` to operate on
/// - client_id: The client_id, usually taken from the `ScreenInstruction` that's being processed
/// - closure: A closure satisfying `|tab: &mut Tab| -> ()` OR `|tab: &mut Tab| -> Result<T>` (see
///   '?' below)
/// - ?: A literal "?", to append a `?` to the closure when it returns a `Result` type. This
///   argument is optional and not needed when the closure returns `()`
macro_rules! active_tab {
    ($screen:ident, $client_id:ident, $closure:expr) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
                // This could be made more ergonomic by declaring the type of 'active_tab' in the
                // closure, known as "Type Ascription". Then we could hint the type here and forego the
                // "&mut Tab" in all the closures below...
                // See: https://github.com/rust-lang/rust/issues/23416
                $closure(active_tab);
            },
            Err(err) => Err::<(), _>(err).non_fatal(),
        };
    };
    // Same as above, but with an added `?` for when the close returns a `Result` type.
    ($screen:ident, $client_id:ident, $closure:expr, ?) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
            $closure(active_tab)?;
            },
            Err(err) => Err::<(), _>(err).non_fatal(),
        };
    };
}

macro_rules! active_tab_and_connected_client_id {
    ($screen:ident, $client_id:ident, $closure:expr) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
                $closure(active_tab, $client_id);
            },
            Err(_) => {
                if let Some(client_id) = $screen.get_first_client_id() {
                    match $screen.get_active_tab_mut(client_id) {
                        Ok(active_tab) => {
                            $closure(active_tab, client_id);
                        },
                        Err(err) => Err::<(), _>(err).non_fatal(),
                    }
                } else {
                    log::error!("No client ids in screen found");
                };
            },
        }
    };
    // Same as above, but with an added `?` for when the closure returns a `Result` type.
    ($screen:ident, $client_id:ident, $closure:expr, ?) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
                $closure(active_tab, $client_id).non_fatal();
            },
            Err(_) => {
                if let Some(client_id) = $screen.get_first_client_id() {
                    match $screen.get_active_tab_mut(client_id) {
                        Ok(active_tab) => {
                            $closure(active_tab, client_id)?;
                        },
                        Err(err) => Err::<(), _>(err).non_fatal(),
                    }
                } else {
                    log::error!("No client ids in screen found");
                };
            },
        }
    };
}

macro_rules! active_tab_and_connected_client_id_with_first_tab_fallback {
    ($screen:ident, $client_id:ident, $closure:expr) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
                $closure(active_tab, Some($client_id));
            },
            Err(_) => {
                if let Some(client_id) = $screen.get_first_client_id() {
                    match $screen.get_active_tab_mut(client_id) {
                        Ok(active_tab) => {
                            $closure(active_tab, Some(client_id));
                        },
                        Err(err) => Err::<(), _>(err).non_fatal(),
                    }
                } else {
                    match $screen.get_indexed_tab_mut(0) {
                        Some(first_tab) => {
                            $closure(first_tab, None);
                        },
                        None => {
                            log::error!("Not tabs found!");
                        },
                    }
                };
            },
        }
    };
    // Same as above, but with an added `?` for when the closure returns a `Result` type.
    ($screen:ident, $client_id:ident, $closure:expr, ?) => {
        match $screen.get_active_tab_mut($client_id) {
            Ok(active_tab) => {
                $closure(active_tab, Some($client_id)).non_fatal();
            },
            Err(_) => {
                if let Some(client_id) = $screen.get_first_client_id() {
                    match $screen.get_active_tab_mut(client_id) {
                        Ok(active_tab) => {
                            $closure(active_tab, Some(client_id))?;
                        },
                        Err(err) => Err::<(), _>(err).non_fatal(),
                    }
                } else {
                    match $screen.get_indexed_tab_mut(0) {
                        Some(first_tab) => {
                            $closure(first_tab, None)?;
                        },
                        None => {
                            log::error!("Not tabs found!");
                        },
                    }
                };
            },
        }
    };
}

type InitialTitle = String;
type HoldForCommand = Option<RunCommand>;

/// Result of overriding a single tab's layout
#[derive(Debug, Clone)]
pub struct TabOverrideResult {
    pub tab_index: usize,
    pub tab_name: Option<String>,
    pub tiled_layout: TiledPaneLayout,
    pub floating_layouts: Vec<FloatingPaneLayout>,
    pub swap_tiled_layouts: Option<Vec<SwapTiledLayout>>,
    pub swap_floating_layouts: Option<Vec<SwapFloatingLayout>>,
    pub new_terminal_pids: Vec<(u32, HoldForCommand)>,
    pub new_floating_pane_pids: Vec<(u32, HoldForCommand)>,
    pub plugin_ids: HashMap<RunPluginOrAlias, Vec<u32>>,
}

/// Ephemeral fence for one durable tab layout writer.
///
/// It is minted by the screen thread after validating the stable ID, exact
/// name and durable token, then carried through plugin/PTY workers back to the
/// screen. A newer retry replaces the current generation and makes every older
/// completion stale.
#[derive(Clone, PartialEq, Eq)]
pub struct DurableTabLayoutGeneration {
    pub tab_id: usize,
    pub tab_name: String,
    pub tab_instance_id: String,
    pub generation: u64,
    pub viewer_creation_fence: Option<ViewerCreationFence>,
}

impl std::fmt::Debug for DurableTabLayoutGeneration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DurableTabLayoutGeneration")
            .field("tab_id", &self.tab_id)
            .field("tab_name", &self.tab_name)
            .field("tab_instance_id", &self.tab_instance_id)
            .field("generation", &self.generation)
            .field(
                "has_viewer_creation_fence",
                &self.viewer_creation_fence.is_some(),
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ReconfigureParams {
    pub client_id: ClientId,
    pub keybinds: Keybinds,
    pub default_mode: InputMode,
    pub theme: Styling,
    pub host_theme_dark: Option<Styling>,
    pub host_theme_light: Option<Styling>,
    pub simplified_ui: bool,
    pub default_shell: Option<PathBuf>,
    pub pane_frames: bool,
    pub copy_command: Option<String>,
    pub copy_to_clipboard: Option<Clipboard>,
    pub copy_on_select: bool,
    pub auto_layout: bool,
    pub rounded_corners: bool,
    pub hide_session_name: bool,
    pub stacked_resize: bool,
    pub default_editor: Option<PathBuf>,
    pub advanced_mouse_actions: bool,
    pub mouse_hover_effects: bool,
    pub visual_bell: bool,
    pub focus_follows_mouse: bool,
    pub mouse_click_through: bool,
}

#[derive(Debug, Clone)]
pub struct DumpScreenTargetIdentity {
    pub tab_id: usize,
    pub tab_name: String,
    pub session_incarnation: String,
    pub tab_instance_id: String,
}

fn dump_screen_error_message(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutPreparationCleanup {
    /// The producer either allocated nothing or already released everything.
    Resolved,
    /// PTY has finished its own rollback and Screen must still obtain the
    /// exact Plugin release receipt before mutating the pending topology.
    ReleasePluginReservation {
        plugin_ids: Vec<PluginId>,
        pty_cleanup_succeeded: bool,
    },
}

/// Instructions that can be sent to the [`Screen`].
#[derive(Debug, Clone)]
pub enum ScreenInstruction {
    PtyBytes(u32, VteBytes),
    PluginBytes(Vec<PluginRenderAsset>),
    Render,
    /// Wakes the Screen loop so it can consume completed layout maintenance
    /// results without scheduling user-visible render work.
    LayoutMaintenanceWake,
    RenderToClients,
    NewPane(
        PaneId,
        Option<InitialTitle>,
        HoldForCommand,
        Option<Run>, // invoked with
        NewPanePlacement,
        bool, // start suppressed
        ClientTabIndexOrPaneId,
        Option<NotificationEnd>, // completion signal
        bool,                    // set_blocking
    ),
    OpenInPlaceEditor(PaneId, ClientTabIndexOrPaneId),
    TogglePaneEmbedOrFloating(ClientId, Option<NotificationEnd>),
    ToggleFloatingPanes(ClientId, Option<TerminalAction>, Option<NotificationEnd>),
    ShowFloatingPanes {
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    },
    HideFloatingPanes {
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    },
    AreFloatingPanesVisible {
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    },
    WriteCharacter(
        Option<KeyWithModifier>,
        Vec<u8>,
        bool,
        ClientId,
        Option<NotificationEnd>,
    ), // bool ->
    // is_kitty_keyboard_protocol
    Resize(ClientId, ResizeStrategy, Option<NotificationEnd>),
    SwitchFocus(ClientId, Option<NotificationEnd>),
    FocusNextPane(ClientId, Option<NotificationEnd>),
    FocusPreviousPane(ClientId, Option<NotificationEnd>),
    MoveFocusLeft(ClientId, Option<NotificationEnd>),
    MoveFocusLeftOrPreviousTab(ClientId, Option<NotificationEnd>),
    MoveFocusDown(ClientId, Option<NotificationEnd>),
    MoveFocusUp(ClientId, Option<NotificationEnd>),
    MoveFocusRight(ClientId, Option<NotificationEnd>),
    MoveFocusRightOrNextTab(ClientId, Option<NotificationEnd>),
    MovePane(ClientId, Option<NotificationEnd>),
    MovePaneBackwards(ClientId, Option<NotificationEnd>),
    MovePaneUp(ClientId, Option<NotificationEnd>),
    MovePaneDown(ClientId, Option<NotificationEnd>),
    MovePaneRight(ClientId, Option<NotificationEnd>),
    MovePaneLeft(ClientId, Option<NotificationEnd>),
    Exit,
    ClearScreen(ClientId, Option<NotificationEnd>),
    DumpScreen(
        Option<String>,
        ClientId,
        bool,
        Option<PaneId>,
        Option<NotificationEnd>,
        Option<ClientId>, // cli_client_id - used to send output to the CLI client's STDOUT
        bool,             // ansi - preserve ANSI styling in the dump output
        Option<DumpScreenTargetIdentity>,
    ),
    CopyPaneScrollback(ClientId, Option<NotificationEnd>),
    DumpLayout(Option<PathBuf>, ClientId, Option<NotificationEnd>), // PathBuf is the default configured
    // shell
    SaveSession(ClientId, Option<NotificationEnd>),
    DumpLayoutToPlugin {
        plugin_id: PluginId,
        tab_index: Option<usize>,
        response_channel: crossbeam::channel::Sender<DumpSessionLayoutResponse>,
    },
    GetFocusedPaneInfo {
        client_id: ClientId,
        response_channel: crossbeam::channel::Sender<GetFocusedPaneInfoResponse>,
    },
    GetPaneInfo {
        pane_id: PaneId,
        response_channel: crossbeam::channel::Sender<Option<PaneInfo>>,
    },
    GetTabInfo {
        tab_id: usize,
        response_channel: crossbeam::channel::Sender<Option<TabInfo>>,
    },
    EditScrollback(ClientId, bool, Option<NotificationEnd>),
    GetPaneScrollback {
        pane_id: PaneId,
        client_id: ClientId,
        get_full_scrollback: bool,
        response_channel: crossbeam::channel::Sender<PaneScrollbackResponse>,
    },
    ScrollUp(ClientId, Option<NotificationEnd>),
    ScrollUpAt(Position, ClientId, Option<NotificationEnd>),
    ScrollDown(ClientId, Option<NotificationEnd>),
    ScrollDownAt(Position, ClientId, Option<NotificationEnd>),
    ScrollToBottom(ClientId, Option<NotificationEnd>),
    ScrollToTop(ClientId, Option<NotificationEnd>),
    PageScrollUp(ClientId, Option<NotificationEnd>),
    PageScrollDown(ClientId, Option<NotificationEnd>),
    HalfPageScrollUp(ClientId, Option<NotificationEnd>),
    HalfPageScrollDown(ClientId, Option<NotificationEnd>),
    ClearScroll(ClientId),
    CloseFocusedPane(ClientId, Option<NotificationEnd>),
    ToggleActiveTerminalFullscreen(ClientId, Option<NotificationEnd>),
    TogglePaneFrames(Option<NotificationEnd>),
    SetSelectable(PaneId, bool),
    ShowPluginCursor(u32, ClientId, Option<(usize, usize)>),
    ClosePane(
        PaneId,
        Option<ClientId>,
        Option<NotificationEnd>,
        Option<i32>,
    ), // i32 -> optional exit
    // status
    HoldPane(PaneId, Option<i32>, RunCommand),
    UpdatePaneName(Vec<u8>, ClientId, Option<NotificationEnd>),
    UndoRenamePane(ClientId, Option<NotificationEnd>),
    NewTab(
        Option<PathBuf>,
        Option<TerminalAction>,
        Option<TiledPaneLayout>,
        Vec<FloatingPaneLayout>,
        Option<String>,
        // `None` falls back to `Screen::default_layout`'s swap layouts.
        (
            Option<Vec<SwapTiledLayout>>,
            Option<Vec<SwapFloatingLayout>>,
        ),
        Option<Vec<CommandOrPlugin>>, // initial_panes
        bool,                         // block_on_first_terminal
        bool,                         // should_change_focus_to_new_tab
        TabPlacement,                 // where the tab lands in the tab bar
        (ClientId, bool),             // bool -> is_web_client
        Option<NotificationEnd>,      // completion signal
    ),
    /// Apply layout to tab with given stable ID.
    ///
    /// The sixth parameter (usize) is a stable identifier (not position) from the
    /// NewTab → ApplyLayout async flow that passes IDs between threads.
    ApplyLayout(
        TiledPaneLayout,
        Vec<FloatingPaneLayout>,
        Vec<(u32, HoldForCommand)>, // new pane pids
        Vec<(u32, HoldForCommand)>, // new floating pane pids
        HashMap<RunPluginOrAlias, Vec<u32>>,
        usize,                          // tab_id - stable identifier from NewTab instruction
        bool,                           // should change focus to new tab
        (ClientId, bool),               // bool -> is_web_client
        Option<NotificationEnd>,        // regular completion signal
        Option<(u32, NotificationEnd)>, // blocking_terminal (terminal_id, completion_tx)
        Option<Box<DurableTabLayoutGeneration>>,
        LayoutTransactionId,
    ),
    LayoutPreparationFailed {
        transaction_id: LayoutTransactionId,
        tab_id: Option<usize>,
        completion_tx: Option<NotificationEnd>,
        layout_generation: Option<Box<DurableTabLayoutGeneration>>,
        message: String,
        cleanup: LayoutPreparationCleanup,
    },
    #[cfg(test)]
    RetireLayoutTransactionsForTabForTest(usize),
    #[cfg(test)]
    QueryLayoutTransactionStateForTest {
        transaction_id: LayoutTransactionId,
        // active owner, background reconciliation owner, pending render/event gate
        response_channel: channels::Sender<(bool, bool, bool)>,
    },
    SwitchTabNext(ClientId, Option<NotificationEnd>),
    SwitchTabPrev(ClientId, Option<NotificationEnd>),
    ToggleActiveSyncTab(ClientId, Option<NotificationEnd>),
    CloseTab(ClientId, Option<NotificationEnd>),
    GoToTab(u32, Option<ClientId>, Option<NotificationEnd>), // this Option is a hacky workaround, please do not copy this behaviour
    GoToTabName(
        String,
        Option<TerminalAction>, // default_shell
        bool,
        Option<ClientId>,
        Option<NotificationEnd>,
    ),
    ToggleTab(ClientId, Option<NotificationEnd>),
    UpdateTabName(Vec<u8>, ClientId, Option<NotificationEnd>),
    UndoRenameTab(ClientId, Option<NotificationEnd>),
    MoveTabLeft(ClientId, Option<NotificationEnd>),
    MoveTabRight(ClientId, Option<NotificationEnd>),
    GoToTabWithId(usize, Option<ClientId>, Option<NotificationEnd>),
    CloseTabWithId(usize, Option<NotificationEnd>),
    CloseTabWithIdIfName(usize, String, String, String, Option<NotificationEnd>),
    CloseTabWithIdIfNameIfQuiescent(usize, String, String, String, Option<NotificationEnd>),
    RenameTabWithId(usize, Vec<u8>, Option<NotificationEnd>),
    BreakPanesToTabWithId {
        pane_ids: Vec<PaneId>,
        tab_id: usize,
        should_change_focus_to_target_tab: bool,
        client_id: ClientId,
        completion_tx: Option<NotificationEnd>,
    },
    TerminalResize(Size),
    /// Update a regular client's known viewport size and recompute the size of
    /// that client's active tab. `(client_id, new_size)`.
    RecomputeTabSize(ClientId, Size),
    TerminalPixelDimensions(PixelDimensions),
    TerminalBackgroundColor(String),
    TerminalForegroundColor(String),
    TerminalColorRegisters(Vec<(usize, String)>),
    /// A pane's Grid intercepted an app-in-pane whitelisted query; Screen
    /// assigns a token, queues the forward, and dispatches to the client.
    /// `query` carries the classified form so Screen can match on it
    /// directly (for cache-fallback synthesis) without re-parsing bytes.
    ForwardHostQuery {
        pane_id: PaneId,
        query: crate::host_query::HostQuery,
    },
    /// The client observed the host's reply to a previously forwarded
    /// query (closed by the Primary-DA barrier or the 500 ms timeout).
    /// The reply bytes are written verbatim to the originating pane.
    ForwardedReplyFromHost {
        token: u32,
        reply_bytes: Vec<u8>,
    },
    /// Internal: a forwarded reply (or its cache-fallback synthesis,
    /// or a locally-answered query like `ColorPaletteMode`) is ready
    /// to be delivered to the originating pane. Routed through the
    /// owning Tab so that the bytes are written to PTY *and* any
    /// PTY input that arrived while the pane was forward-paused is
    /// drained and re-fed through vte in stream order. This preserves
    /// query/reply ordering even when sync replies (DA1, DSR, DECQRM)
    /// would otherwise race ahead of an async host round-trip.
    ResumePaneAfterForward {
        pane_id: PaneId,
        reply_bytes: Vec<u8>,
    },
    /// The host terminal reported a color-palette theme mode (DSR 997 reply
    /// to `CSI ? 996 n`, or unsolicited notification while `CSI ? 2031 h`
    /// is enabled). Triggers auto-theme switch (when configured), the
    /// `HostTerminalThemeChanged` plugin event, and per-pane DSR forwarding
    /// for panes that opted in via `CSI ? 2031 h`.
    HostTerminalThemeChanged(HostTerminalThemeMode),
    /// Manual theme actions issued via the CLI (e.g. `zellij action set-dark-theme`)
    /// or a keybinding. They share the same convergence point as
    /// `HostTerminalThemeChanged`, but additionally surface a CLI-friendly error
    /// via `NotificationEnd` if `theme_dark` and `theme_light` are not both set
    /// (the auto-switch gate). "Last one wins": these compete naively with
    /// terminal-driven notifications via the dedupe in
    /// `update_host_terminal_theme_mode`.
    SetDarkTheme(Option<NotificationEnd>),
    SetLightTheme(Option<NotificationEnd>),
    ToggleTheme(Option<NotificationEnd>),
    ChangeMode(
        InputMode,
        Option<InputMode>,
        ClientId,
        Option<NotificationEnd>,
    ),
    ChangeModeForAllClients(InputMode, Option<InputMode>, Option<NotificationEnd>),
    MouseEvent(MouseEvent, ClientId, Option<NotificationEnd>),
    Copy(ClientId, Option<NotificationEnd>),
    AddClient(
        ClientId,
        bool,                // is_web_client
        Size,                // client viewport size — used for per-tab sizing
        Option<usize>,       // tab position to focus
        Option<(u32, bool)>, // (pane_id, is_plugin) => pane_id to focus
    ),
    RemoveClient(ClientId),
    UpdateSearch(Vec<u8>, ClientId, Option<NotificationEnd>),
    SearchDown(ClientId, Option<NotificationEnd>),
    SearchUp(ClientId, Option<NotificationEnd>),
    SearchToggleCaseSensitivity(ClientId, Option<NotificationEnd>),
    SearchToggleWholeWord(ClientId, Option<NotificationEnd>),
    SearchToggleWrap(ClientId, Option<NotificationEnd>),
    AddRedPaneFrameColorOverride(Vec<PaneId>, Option<String>), // Option<String> => optional error text
    ClearPaneFrameColorOverride(Vec<PaneId>),
    SetTabBellFlash(usize, bool), // tab_id, is_flashing
    PreviousSwapLayout(ClientId, Option<NotificationEnd>),
    NextSwapLayout(ClientId, Option<NotificationEnd>),
    OverrideLayout(
        Option<PathBuf>,        // cwd (applies to all tabs)
        Option<TerminalAction>, // default_shell (applies to all tabs)
        Vec<TabLayoutInfo>,     // layouts for each tab to override
        bool,                   // retain_existing_terminal_panes
        bool,                   // retain_existing_plugin_panes
        bool,                   // apply_only_to_focused_tab
        ClientId,
        Option<NotificationEnd>,
    ),
    OverrideLayoutComplete(
        Vec<TabOverrideResult>, // results for each tab
        bool,                   // retain_existing_terminal_panes
        bool,                   // retain_existing_plugin_panes
        ClientId,
        Option<NotificationEnd>,
        Option<Box<DurableTabLayoutGeneration>>,
        LayoutTransactionId,
    ),
    QueryTabNames(ClientId, Option<NotificationEnd>),
    NewTiledPluginPane(
        RunPluginOrAlias,
        Option<String>,
        bool,
        Option<PathBuf>,
        ClientId,
        Option<NotificationEnd>,
        Option<usize>, // tab_id
    ), // Option<String> is
    // optional pane title, bool is skip cache, Option<PathBuf> is an optional cwd
    NewFloatingPluginPane(
        RunPluginOrAlias,
        Option<String>,
        bool,
        Option<PathBuf>,
        Option<FloatingPaneCoordinates>,
        ClientId,
        Option<NotificationEnd>,
        Option<usize>, // tab_id
    ), // Option<String> is an
    // optional pane title, bool
    // is skip cache, Option<PathBuf> is an optional cwd
    NewInPlacePluginPane(
        RunPluginOrAlias,
        Option<String>,
        PaneId,
        bool,
        bool,
        ClientId,
        Option<NotificationEnd>,
        Option<usize>, // tab_id
    ), // Option<String> is an
    // optional pane title, first bool is skip cache, second bool is close_replaced_pane
    StartOrReloadPluginPane(RunPluginOrAlias, Option<String>, Option<NotificationEnd>),
    AddPlugin(
        Option<bool>, // should_float
        bool,         // should be opened in place
        bool,         // close_replaced_pane
        RunPluginOrAlias,
        Option<String>, // pane title
        Option<usize>,  // tab index
        u32,            // plugin id
        Option<PaneId>,
        Option<PathBuf>, // cwd
        bool,            // start suppressed
        Option<FloatingPaneCoordinates>,
        Option<bool>, // should focus plugin
        Option<ClientId>,
        Option<NotificationEnd>, // completion signal
    ),
    UpdatePluginLoadingStage(u32, LoadingIndication), // u32 - plugin_id
    StartPluginLoadingIndication(u32, LoadingIndication), // u32 - plugin_id
    ProgressPluginLoadingOffset(u32),                 // u32 - plugin id
    RequestStateUpdateForPlugins,
    LaunchOrFocusPlugin(
        RunPluginOrAlias,
        bool,
        bool,
        bool,
        bool,
        Option<PaneId>,
        bool,
        ClientId,
        Option<NotificationEnd>,
        Option<usize>, // tab_id
    ), // bools are: should_float, move_to_focused_tab, should_open_in_place, close_replaced_pane, Option<PaneId> is the pane id to replace, bool following it is skip_cache
    LaunchPlugin(
        RunPluginOrAlias,
        bool,
        bool,
        bool,
        Option<PaneId>,
        bool,
        Option<PathBuf>,
        ClientId,
        Option<NotificationEnd>,
        Option<usize>, // tab_id
    ), // bools are: should_float, should_open_in_place, close_replaced_pane, Option<PaneId> is the pane id to replace, Option<PathBuf> is an optional cwd, bool after is skip_cache
    SuppressPane(PaneId, ClientId),
    UnsuppressPane(PaneId, bool), // bool -> should float if hidden
    UnsuppressOrExpandPane(PaneId, bool), // bool -> should float if hidden
    FocusPaneWithId(PaneId, bool, bool, ClientId, Option<NotificationEnd>), // bools:
    // should_float_if_hidden,
    // should_be_in_place_if_hidden
    RenamePane(PaneId, Vec<u8>, Option<NotificationEnd>),
    RenameActivePane(Vec<u8>, ClientId, Option<NotificationEnd>),
    RenameTab(usize, Vec<u8>, Option<NotificationEnd>),
    RequestPluginPermissions(
        u32, // u32 - plugin_id
        PluginPermission,
    ),
    BreakPane(Option<TerminalAction>, ClientId, Option<NotificationEnd>),
    BreakPaneRight(ClientId, Option<NotificationEnd>),
    BreakPaneLeft(ClientId, Option<NotificationEnd>),
    UpdateSessionInfos(
        BTreeMap<String, SessionInfo>, // String is the session name
        BTreeMap<String, Duration>,    // resurrectable sessions - <name, created>
    ),
    ReplacePane(
        PaneId,
        HoldForCommand,
        Option<InitialTitle>,
        Option<Run>,
        bool, // close replaced pane
        ClientTabIndexOrPaneId,
        Option<NotificationEnd>, // completion signal
    ),
    SerializeLayoutForResurrection,
    RenameSession(String, ClientId, Option<NotificationEnd>), // String -> new name
    ListClientsMetadata(Option<PathBuf>, ClientId, Option<NotificationEnd>), // Option<PathBuf> - default shell
    ListPanes {
        show_all: bool,
        response_channel: crossbeam::channel::Sender<ListPanesResponse>,
    },
    ListTabs {
        client_id: ClientId,
        response_channel: crossbeam::channel::Sender<ListTabsResponse>,
    },
    GetCurrentTabInfo {
        client_id: ClientId,
        response_channel: crossbeam::channel::Sender<Option<TabInfo>>,
    },
    Reconfigure(Box<ReconfigureParams>),
    RerunCommandPane(u32, Option<NotificationEnd>), // u32 - terminal pane id
    ResizePaneWithId(ResizeStrategy, PaneId),
    EditScrollbackForPaneWithId(PaneId, Option<NotificationEnd>),
    WriteToPaneId(Vec<u8>, PaneId, Option<NotificationEnd>),
    Paste(Vec<u8>, Option<PaneId>, ClientId, Option<NotificationEnd>),
    SetPaneColor(
        PaneId,
        Option<String>,
        Option<String>,
        Option<NotificationEnd>,
    ),
    WriteKeyToPaneId(
        Option<KeyWithModifier>,
        Vec<u8>,
        bool, // is_kitty_keyboard_protocol
        PaneId,
        Option<NotificationEnd>,
    ),
    CopyTextToClipboard(String, u32), // String - text to copy, u32 - plugin_id
    MovePaneWithPaneId(PaneId),
    MovePaneWithPaneIdInDirection(PaneId, Direction),
    ClearScreenForPaneId(PaneId),
    ScrollUpInPaneId(PaneId),
    ScrollDownInPaneId(PaneId),
    ScrollToTopInPaneId(PaneId),
    ScrollToBottomInPaneId(PaneId),
    PageScrollUpInPaneId(PaneId),
    PageScrollDownInPaneId(PaneId),
    TogglePaneIdFullscreen(PaneId),
    TogglePaneEmbedOrEjectForPaneId(PaneId),
    CloseTabWithIndex(usize),
    BreakPanesToNewTab {
        pane_ids: Vec<PaneId>,
        default_shell: Option<TerminalAction>,
        should_change_focus_to_new_tab: bool,
        new_tab_name: Option<String>,
        client_id: ClientId,
        completion_tx: Option<NotificationEnd>,
    },
    BreakPanesToTabWithIndex {
        pane_ids: Vec<PaneId>,
        tab_index: usize,
        should_change_focus_to_new_tab: bool,
        client_id: ClientId,
        completion_tx: Option<NotificationEnd>,
    },
    ListClientsToPlugin(PluginId, ClientId),
    TogglePanePinned(ClientId, Option<NotificationEnd>),
    SetFloatingPanePinned(PaneId, bool),
    StackPanes(Vec<PaneId>, ClientId, Option<NotificationEnd>),
    ChangeFloatingPanesCoordinates(
        Vec<(PaneId, FloatingPaneCoordinates)>,
        Option<NotificationEnd>,
    ),
    TogglePaneBorderless(PaneId, Option<NotificationEnd>),
    SetPaneBorderless(PaneId, bool, Option<NotificationEnd>),
    AddHighlightPaneFrameColorOverride(Vec<PaneId>, Option<String>), // Option<String> => optional
    // message
    GroupAndUngroupPanes(Vec<PaneId>, Vec<PaneId>, bool, ClientId), // panes_to_group, panes_to_ungroup, bool -> for all clients
    HighlightAndUnhighlightPanes(Vec<PaneId>, Vec<PaneId>, ClientId), // panes_to_highlight, panes_to_unhighlight
    FloatMultiplePanes(Vec<PaneId>, ClientId),
    EmbedMultiplePanes(Vec<PaneId>, ClientId),
    TogglePaneInGroup(ClientId, Option<NotificationEnd>),
    ToggleGroupMarking(ClientId, Option<NotificationEnd>),
    SessionSharingStatusChange(bool),
    SetMouseSelectionSupport(PaneId, bool),
    InterceptKeyPresses(PluginId, ClientId),
    ClearKeyPressesIntercepts(ClientId),
    ReplacePaneWithExistingPane(PaneId, PaneId, bool, Option<NotificationEnd>), // bool -> suppress_replaced_pane
    AddWatcherClient(ClientId, Size),
    RemoveWatcherClient(ClientId),
    SetFollowedClient(ClientId),
    WatcherTerminalResize(ClientId, Size),
    ClearMouseHelpText(ClientId),
    UpdateAvailableLayouts(Vec<LayoutInfo>, Vec<LayoutWithError>),
    SetPluginRegexHighlights {
        pane_id: PaneId,
        plugin_id: u32,
        highlights: Vec<RegexHighlight>,
    },
    ClearPluginHighlights {
        pane_id: PaneId,
        plugin_id: u32,
    },
    ClearAllPluginHighlights(u32), // plugin_id — clears across all panes
    SubscribeToPaneRenders {
        client_id: ClientId,
        pane_ids: Vec<zellij_utils::data::PaneId>,
        scrollback: Option<usize>,
        ansi: bool,
    },
    NotifyPaneClosedToSubscribers {
        pane_id: zellij_utils::data::PaneId,
    },
    DesktopNotificationResponse(Vec<u8>, ClientId),
    PluginSubscribedToAnsiPaneContents(bool), // true = at least one plugin needs ANSI content
    UpdateBackgroundPluginSubscriptions(PluginId, ClientId, HashSet<EventType>),
    BroadcastModeUpdate(ModeInfo, Option<ClientId>), // ModeInfo, optional specific client_id (None = all clients)
    // Pane-targeting CLI variants
    ScrollUpWithPaneId(PaneId, Option<NotificationEnd>),
    ScrollDownWithPaneId(PaneId, Option<NotificationEnd>),
    ScrollToTopWithPaneId(PaneId, Option<NotificationEnd>),
    ScrollToBottomWithPaneId(PaneId, Option<NotificationEnd>),
    PageScrollUpWithPaneId(PaneId, Option<NotificationEnd>),
    PageScrollDownWithPaneId(PaneId, Option<NotificationEnd>),
    HalfPageScrollUpWithPaneId(PaneId, Option<NotificationEnd>),
    HalfPageScrollDownWithPaneId(PaneId, Option<NotificationEnd>),
    ResizeWithPaneId(PaneId, ResizeStrategy, Option<NotificationEnd>),
    MovePaneWithPaneIdCli(PaneId, Option<Direction>, Option<NotificationEnd>),
    MovePaneBackwardsWithPaneId(PaneId, Option<NotificationEnd>),
    ClearScreenWithPaneId(PaneId, Option<NotificationEnd>),
    EditScrollbackWithPaneId(PaneId, bool, Option<NotificationEnd>),
    ToggleFullscreenWithPaneId(PaneId, Option<NotificationEnd>),
    TogglePaneEmbedOrFloatingWithPaneId(PaneId, Option<NotificationEnd>),
    CloseFocusWithPaneId(PaneId, Option<NotificationEnd>),
    RenamePaneWithPaneId(PaneId, Vec<u8>, Option<NotificationEnd>),
    UndoRenamePaneWithPaneId(PaneId, Option<NotificationEnd>),
    TogglePanePinnedWithPaneId(PaneId, Option<NotificationEnd>),
    // Tab-targeting CLI variants
    UndoRenameTabWithTabId(usize, Option<NotificationEnd>),
    ToggleActiveSyncTabWithTabId(usize, Option<NotificationEnd>),
    ToggleFloatingPanesWithTabId(usize, Option<TerminalAction>, Option<NotificationEnd>),
    PreviousSwapLayoutWithTabId(usize, Option<NotificationEnd>),
    NextSwapLayoutWithTabId(usize, Option<NotificationEnd>),
    MoveTabWithTabId(usize, Direction, Option<NotificationEnd>),
}

impl From<&ScreenInstruction> for ScreenContext {
    fn from(screen_instruction: &ScreenInstruction) -> Self {
        match *screen_instruction {
            ScreenInstruction::PtyBytes(..) => ScreenContext::HandlePtyBytes,
            ScreenInstruction::PluginBytes(..) => ScreenContext::PluginBytes,
            ScreenInstruction::Render => ScreenContext::Render,
            ScreenInstruction::LayoutMaintenanceWake => ScreenContext::Render,
            ScreenInstruction::RenderToClients => ScreenContext::RenderToClients,
            ScreenInstruction::NewPane(..) => ScreenContext::NewPane,
            ScreenInstruction::OpenInPlaceEditor(..) => ScreenContext::OpenInPlaceEditor,
            ScreenInstruction::TogglePaneEmbedOrFloating(..) => {
                ScreenContext::TogglePaneEmbedOrFloating
            },
            ScreenInstruction::ToggleFloatingPanes(..) => ScreenContext::ToggleFloatingPanes,
            ScreenInstruction::ShowFloatingPanes { .. } => ScreenContext::ShowFloatingPanes,
            ScreenInstruction::HideFloatingPanes { .. } => ScreenContext::HideFloatingPanes,
            ScreenInstruction::AreFloatingPanesVisible { .. } => {
                ScreenContext::AreFloatingPanesVisible
            },
            ScreenInstruction::WriteCharacter(..) => ScreenContext::WriteCharacter,
            ScreenInstruction::Resize(.., strategy, _) => match strategy {
                ResizeStrategy {
                    resize: Resize::Increase,
                    direction,
                    ..
                } => match direction {
                    Some(Direction::Left) => ScreenContext::ResizeIncreaseLeft,
                    Some(Direction::Down) => ScreenContext::ResizeIncreaseDown,
                    Some(Direction::Up) => ScreenContext::ResizeIncreaseUp,
                    Some(Direction::Right) => ScreenContext::ResizeIncreaseRight,
                    None => ScreenContext::ResizeIncreaseAll,
                },
                ResizeStrategy {
                    resize: Resize::Decrease,
                    direction,
                    ..
                } => match direction {
                    Some(Direction::Left) => ScreenContext::ResizeDecreaseLeft,
                    Some(Direction::Down) => ScreenContext::ResizeDecreaseDown,
                    Some(Direction::Up) => ScreenContext::ResizeDecreaseUp,
                    Some(Direction::Right) => ScreenContext::ResizeDecreaseRight,
                    None => ScreenContext::ResizeDecreaseAll,
                },
            },
            ScreenInstruction::SwitchFocus(..) => ScreenContext::SwitchFocus,
            ScreenInstruction::FocusNextPane(..) => ScreenContext::FocusNextPane,
            ScreenInstruction::FocusPreviousPane(..) => ScreenContext::FocusPreviousPane,
            ScreenInstruction::MoveFocusLeft(..) => ScreenContext::MoveFocusLeft,
            ScreenInstruction::MoveFocusLeftOrPreviousTab(..) => {
                ScreenContext::MoveFocusLeftOrPreviousTab
            },
            ScreenInstruction::MoveFocusDown(..) => ScreenContext::MoveFocusDown,
            ScreenInstruction::MoveFocusUp(..) => ScreenContext::MoveFocusUp,
            ScreenInstruction::MoveFocusRight(..) => ScreenContext::MoveFocusRight,
            ScreenInstruction::MoveFocusRightOrNextTab(..) => {
                ScreenContext::MoveFocusRightOrNextTab
            },
            ScreenInstruction::MovePane(..) => ScreenContext::MovePane,
            ScreenInstruction::MovePaneBackwards(..) => ScreenContext::MovePaneBackwards,
            ScreenInstruction::MovePaneDown(..) => ScreenContext::MovePaneDown,
            ScreenInstruction::MovePaneUp(..) => ScreenContext::MovePaneUp,
            ScreenInstruction::MovePaneRight(..) => ScreenContext::MovePaneRight,
            ScreenInstruction::MovePaneLeft(..) => ScreenContext::MovePaneLeft,
            ScreenInstruction::Exit => ScreenContext::Exit,
            ScreenInstruction::ClearScreen(..) => ScreenContext::ClearScreen,
            ScreenInstruction::DumpScreen(..) => ScreenContext::DumpScreen,
            ScreenInstruction::CopyPaneScrollback(..) => ScreenContext::CopyPaneScrollback,
            ScreenInstruction::DumpLayout(..) => ScreenContext::DumpLayout,
            ScreenInstruction::SaveSession(..) => ScreenContext::SaveSession,
            ScreenInstruction::DumpLayoutToPlugin { .. } => ScreenContext::DumpLayoutToPlugin,
            ScreenInstruction::GetFocusedPaneInfo { .. } => ScreenContext::GetFocusedPaneInfo,
            ScreenInstruction::GetPaneInfo { .. } => ScreenContext::GetPaneInfo,
            ScreenInstruction::GetTabInfo { .. } => ScreenContext::GetTabInfo,
            ScreenInstruction::EditScrollback(..) => ScreenContext::EditScrollback,
            ScreenInstruction::GetPaneScrollback { .. } => ScreenContext::GetPaneScrollback,
            ScreenInstruction::ScrollUp(..) => ScreenContext::ScrollUp,
            ScreenInstruction::ScrollDown(..) => ScreenContext::ScrollDown,
            ScreenInstruction::ScrollToBottom(..) => ScreenContext::ScrollToBottom,
            ScreenInstruction::ScrollToTop(..) => ScreenContext::ScrollToTop,
            ScreenInstruction::PageScrollUp(..) => ScreenContext::PageScrollUp,
            ScreenInstruction::PageScrollDown(..) => ScreenContext::PageScrollDown,
            ScreenInstruction::HalfPageScrollUp(..) => ScreenContext::HalfPageScrollUp,
            ScreenInstruction::HalfPageScrollDown(..) => ScreenContext::HalfPageScrollDown,
            ScreenInstruction::ClearScroll(..) => ScreenContext::ClearScroll,
            ScreenInstruction::CloseFocusedPane(..) => ScreenContext::CloseFocusedPane,
            ScreenInstruction::ToggleActiveTerminalFullscreen(..) => {
                ScreenContext::ToggleActiveTerminalFullscreen
            },
            ScreenInstruction::TogglePaneFrames(..) => ScreenContext::TogglePaneFrames,
            ScreenInstruction::SetSelectable(..) => ScreenContext::SetSelectable,
            ScreenInstruction::ShowPluginCursor(..) => ScreenContext::ShowPluginCursor,
            ScreenInstruction::ClosePane(..) => ScreenContext::ClosePane,
            ScreenInstruction::HoldPane(..) => ScreenContext::HoldPane,
            ScreenInstruction::UpdatePaneName(..) => ScreenContext::UpdatePaneName,
            ScreenInstruction::UndoRenamePane(..) => ScreenContext::UndoRenamePane,
            ScreenInstruction::NewTab(..) => ScreenContext::NewTab,
            ScreenInstruction::ApplyLayout(..) => ScreenContext::ApplyLayout,
            ScreenInstruction::LayoutPreparationFailed { .. } => {
                ScreenContext::LayoutPreparationFailed
            },
            #[cfg(test)]
            ScreenInstruction::RetireLayoutTransactionsForTabForTest(..) => {
                ScreenContext::LayoutPreparationFailed
            },
            #[cfg(test)]
            ScreenInstruction::QueryLayoutTransactionStateForTest { .. } => {
                ScreenContext::LayoutPreparationFailed
            },
            ScreenInstruction::SwitchTabNext(..) => ScreenContext::SwitchTabNext,
            ScreenInstruction::SwitchTabPrev(..) => ScreenContext::SwitchTabPrev,
            ScreenInstruction::CloseTab(..) => ScreenContext::CloseTab,
            ScreenInstruction::GoToTab(..) => ScreenContext::GoToTab,
            ScreenInstruction::GoToTabName(..) => ScreenContext::GoToTabName,
            ScreenInstruction::UpdateTabName(..) => ScreenContext::UpdateTabName,
            ScreenInstruction::UndoRenameTab(..) => ScreenContext::UndoRenameTab,
            ScreenInstruction::MoveTabLeft(..) => ScreenContext::MoveTabLeft,
            ScreenInstruction::MoveTabRight(..) => ScreenContext::MoveTabRight,
            ScreenInstruction::GoToTabWithId(..) => ScreenContext::GoToTabWithId,
            ScreenInstruction::CloseTabWithId(..) => ScreenContext::CloseTabWithId,
            ScreenInstruction::CloseTabWithIdIfName(..) => ScreenContext::CloseTabWithIdIfName,
            ScreenInstruction::CloseTabWithIdIfNameIfQuiescent(..) => {
                ScreenContext::CloseTabWithIdIfName
            },
            ScreenInstruction::RenameTabWithId(..) => ScreenContext::RenameTabWithId,
            ScreenInstruction::BreakPanesToTabWithId { .. } => ScreenContext::BreakPanesToTabWithId,
            ScreenInstruction::TerminalResize(..) => ScreenContext::TerminalResize,
            ScreenInstruction::RecomputeTabSize(..) => ScreenContext::RecomputeTabSize,
            ScreenInstruction::TerminalPixelDimensions(..) => {
                ScreenContext::TerminalPixelDimensions
            },
            ScreenInstruction::TerminalBackgroundColor(..) => {
                ScreenContext::TerminalBackgroundColor
            },
            ScreenInstruction::TerminalForegroundColor(..) => {
                ScreenContext::TerminalForegroundColor
            },
            ScreenInstruction::TerminalColorRegisters(..) => ScreenContext::TerminalColorRegisters,
            ScreenInstruction::ForwardHostQuery { .. } => ScreenContext::ForwardHostQuery,
            ScreenInstruction::ForwardedReplyFromHost { .. } => {
                ScreenContext::ForwardedReplyFromHost
            },
            ScreenInstruction::ResumePaneAfterForward { .. } => {
                ScreenContext::ResumePaneAfterForward
            },
            ScreenInstruction::HostTerminalThemeChanged(..) => {
                ScreenContext::HostTerminalThemeChanged
            },
            ScreenInstruction::SetDarkTheme(..) => ScreenContext::SetDarkTheme,
            ScreenInstruction::SetLightTheme(..) => ScreenContext::SetLightTheme,
            ScreenInstruction::ToggleTheme(..) => ScreenContext::ToggleTheme,
            ScreenInstruction::ChangeMode(..) => ScreenContext::ChangeMode,
            ScreenInstruction::ChangeModeForAllClients(..) => {
                ScreenContext::ChangeModeForAllClients
            },
            ScreenInstruction::ToggleActiveSyncTab(..) => ScreenContext::ToggleActiveSyncTab,
            ScreenInstruction::ScrollUpAt(..) => ScreenContext::ScrollUpAt,
            ScreenInstruction::ScrollDownAt(..) => ScreenContext::ScrollDownAt,
            ScreenInstruction::MouseEvent(..) => ScreenContext::MouseEvent,
            ScreenInstruction::Copy(..) => ScreenContext::Copy,
            ScreenInstruction::ToggleTab(..) => ScreenContext::ToggleTab,
            ScreenInstruction::AddClient(..) => ScreenContext::AddClient,
            ScreenInstruction::RemoveClient(..) => ScreenContext::RemoveClient,
            ScreenInstruction::UpdateSearch(..) => ScreenContext::UpdateSearch,
            ScreenInstruction::SearchDown(..) => ScreenContext::SearchDown,
            ScreenInstruction::SearchUp(..) => ScreenContext::SearchUp,
            ScreenInstruction::SearchToggleCaseSensitivity(..) => {
                ScreenContext::SearchToggleCaseSensitivity
            },
            ScreenInstruction::SearchToggleWholeWord(..) => ScreenContext::SearchToggleWholeWord,
            ScreenInstruction::SearchToggleWrap(..) => ScreenContext::SearchToggleWrap,
            ScreenInstruction::AddRedPaneFrameColorOverride(..) => {
                ScreenContext::AddRedPaneFrameColorOverride
            },
            ScreenInstruction::ClearPaneFrameColorOverride(..) => {
                ScreenContext::ClearPaneFrameColorOverride
            },
            ScreenInstruction::SetTabBellFlash(..) => ScreenContext::SetTabBellFlash,
            ScreenInstruction::PreviousSwapLayout(..) => ScreenContext::PreviousSwapLayout,
            ScreenInstruction::NextSwapLayout(..) => ScreenContext::NextSwapLayout,
            ScreenInstruction::OverrideLayout(..) => ScreenContext::OverrideLayout,
            ScreenInstruction::OverrideLayoutComplete(..) => ScreenContext::OverrideLayoutComplete,
            ScreenInstruction::QueryTabNames(..) => ScreenContext::QueryTabNames,
            ScreenInstruction::NewTiledPluginPane(..) => ScreenContext::NewTiledPluginPane,
            ScreenInstruction::NewFloatingPluginPane(..) => ScreenContext::NewFloatingPluginPane,
            ScreenInstruction::StartOrReloadPluginPane(..) => {
                ScreenContext::StartOrReloadPluginPane
            },
            ScreenInstruction::AddPlugin(..) => ScreenContext::AddPlugin,
            ScreenInstruction::UpdatePluginLoadingStage(..) => {
                ScreenContext::UpdatePluginLoadingStage
            },
            ScreenInstruction::ProgressPluginLoadingOffset(..) => {
                ScreenContext::ProgressPluginLoadingOffset
            },
            ScreenInstruction::StartPluginLoadingIndication(..) => {
                ScreenContext::StartPluginLoadingIndication
            },
            ScreenInstruction::RequestStateUpdateForPlugins => {
                ScreenContext::RequestStateUpdateForPlugins
            },
            ScreenInstruction::LaunchOrFocusPlugin(..) => ScreenContext::LaunchOrFocusPlugin,
            ScreenInstruction::LaunchPlugin(..) => ScreenContext::LaunchPlugin,
            ScreenInstruction::SuppressPane(..) => ScreenContext::SuppressPane,
            ScreenInstruction::UnsuppressPane(..) => ScreenContext::UnsuppressPane,
            ScreenInstruction::UnsuppressOrExpandPane(..) => ScreenContext::UnsuppressOrExpandPane,
            ScreenInstruction::FocusPaneWithId(..) => ScreenContext::FocusPaneWithId,
            ScreenInstruction::RenamePane(..) => ScreenContext::RenamePane,
            ScreenInstruction::RenameActivePane(..) => ScreenContext::RenameActivePane,
            ScreenInstruction::RenameTab(..) => ScreenContext::RenameTab,
            ScreenInstruction::RequestPluginPermissions(..) => {
                ScreenContext::RequestPluginPermissions
            },
            ScreenInstruction::BreakPane(..) => ScreenContext::BreakPane,
            ScreenInstruction::BreakPaneRight(..) => ScreenContext::BreakPaneRight,
            ScreenInstruction::BreakPaneLeft(..) => ScreenContext::BreakPaneLeft,
            ScreenInstruction::UpdateSessionInfos(..) => ScreenContext::UpdateSessionInfos,
            ScreenInstruction::ReplacePane(..) => ScreenContext::ReplacePane,
            ScreenInstruction::NewInPlacePluginPane(..) => ScreenContext::NewInPlacePluginPane,
            ScreenInstruction::SerializeLayoutForResurrection => {
                ScreenContext::SerializeLayoutForResurrection
            },
            ScreenInstruction::RenameSession(..) => ScreenContext::RenameSession,
            ScreenInstruction::ListClientsMetadata(..) => ScreenContext::ListClientsMetadata,
            ScreenInstruction::ListPanes { .. } => ScreenContext::ListPanes,
            ScreenInstruction::ListTabs { .. } => ScreenContext::ListTabs,
            ScreenInstruction::GetCurrentTabInfo { .. } => ScreenContext::GetCurrentTabInfo,
            ScreenInstruction::Reconfigure(..) => ScreenContext::Reconfigure,
            ScreenInstruction::RerunCommandPane { .. } => ScreenContext::RerunCommandPane,
            ScreenInstruction::ResizePaneWithId(..) => ScreenContext::ResizePaneWithId,
            ScreenInstruction::EditScrollbackForPaneWithId(..) => {
                ScreenContext::EditScrollbackForPaneWithId
            },
            ScreenInstruction::WriteToPaneId(..) => ScreenContext::WriteToPaneId,
            ScreenInstruction::Paste(..) => ScreenContext::Paste,
            ScreenInstruction::SetPaneColor(..) => ScreenContext::SetPaneColor,
            ScreenInstruction::WriteKeyToPaneId(..) => ScreenContext::WriteKeyToPaneId,
            ScreenInstruction::CopyTextToClipboard(..) => ScreenContext::CopyTextToClipboard,
            ScreenInstruction::MovePaneWithPaneId(..) => ScreenContext::MovePaneWithPaneId,
            ScreenInstruction::MovePaneWithPaneIdInDirection(..) => {
                ScreenContext::MovePaneWithPaneIdInDirection
            },
            ScreenInstruction::ClearScreenForPaneId(..) => ScreenContext::ClearScreenForPaneId,
            ScreenInstruction::ScrollUpInPaneId(..) => ScreenContext::ScrollUpInPaneId,
            ScreenInstruction::ScrollDownInPaneId(..) => ScreenContext::ScrollDownInPaneId,
            ScreenInstruction::ScrollToTopInPaneId(..) => ScreenContext::ScrollToTopInPaneId,
            ScreenInstruction::ScrollToBottomInPaneId(..) => ScreenContext::ScrollToBottomInPaneId,
            ScreenInstruction::PageScrollUpInPaneId(..) => ScreenContext::PageScrollUpInPaneId,
            ScreenInstruction::PageScrollDownInPaneId(..) => ScreenContext::PageScrollDownInPaneId,
            ScreenInstruction::TogglePaneIdFullscreen(..) => ScreenContext::TogglePaneIdFullscreen,
            ScreenInstruction::TogglePaneEmbedOrEjectForPaneId(..) => {
                ScreenContext::TogglePaneEmbedOrEjectForPaneId
            },
            ScreenInstruction::CloseTabWithIndex(..) => ScreenContext::CloseTabWithIndex,
            ScreenInstruction::BreakPanesToNewTab { .. } => ScreenContext::BreakPanesToNewTab,
            ScreenInstruction::BreakPanesToTabWithIndex { .. } => {
                ScreenContext::BreakPanesToTabWithIndex
            },
            ScreenInstruction::ListClientsToPlugin(..) => ScreenContext::ListClientsToPlugin,
            ScreenInstruction::TogglePanePinned(..) => ScreenContext::TogglePanePinned,
            ScreenInstruction::SetFloatingPanePinned(..) => ScreenContext::SetFloatingPanePinned,
            ScreenInstruction::StackPanes(..) => ScreenContext::StackPanes,
            ScreenInstruction::ChangeFloatingPanesCoordinates(..) => {
                ScreenContext::ChangeFloatingPanesCoordinates
            },
            ScreenInstruction::TogglePaneBorderless(..) => ScreenContext::TogglePaneBorderless,
            ScreenInstruction::SetPaneBorderless(..) => ScreenContext::SetPaneBorderless,
            ScreenInstruction::AddHighlightPaneFrameColorOverride(..) => {
                ScreenContext::AddHighlightPaneFrameColorOverride
            },
            ScreenInstruction::GroupAndUngroupPanes(..) => ScreenContext::GroupAndUngroupPanes,
            ScreenInstruction::HighlightAndUnhighlightPanes(..) => {
                ScreenContext::HighlightAndUnhighlightPanes
            },
            ScreenInstruction::FloatMultiplePanes(..) => ScreenContext::FloatMultiplePanes,
            ScreenInstruction::EmbedMultiplePanes(..) => ScreenContext::EmbedMultiplePanes,
            ScreenInstruction::TogglePaneInGroup(..) => ScreenContext::TogglePaneInGroup,
            ScreenInstruction::ToggleGroupMarking(..) => ScreenContext::ToggleGroupMarking,
            ScreenInstruction::SessionSharingStatusChange(..) => {
                ScreenContext::SessionSharingStatusChange
            },
            ScreenInstruction::SetMouseSelectionSupport(..) => {
                ScreenContext::SetMouseSelectionSupport
            },
            ScreenInstruction::InterceptKeyPresses(..) => ScreenContext::InterceptKeyPresses,
            ScreenInstruction::ClearKeyPressesIntercepts(..) => {
                ScreenContext::ClearKeyPressesIntercepts
            },
            ScreenInstruction::ReplacePaneWithExistingPane(..) => {
                ScreenContext::ReplacePaneWithExistingPane
            },
            ScreenInstruction::AddWatcherClient(..) => ScreenContext::AddWatcherClient,
            ScreenInstruction::RemoveWatcherClient(..) => ScreenContext::RemoveWatcherClient,
            ScreenInstruction::SetFollowedClient(..) => ScreenContext::SetFollowedClient,
            ScreenInstruction::WatcherTerminalResize(..) => ScreenContext::WatcherTerminalResize,
            ScreenInstruction::ClearMouseHelpText(..) => ScreenContext::ClearMouseHelpText,
            ScreenInstruction::UpdateAvailableLayouts(..) => ScreenContext::UpdateAvailableLayouts,
            ScreenInstruction::SetPluginRegexHighlights { .. } => {
                ScreenContext::SetPluginRegexHighlights
            },
            ScreenInstruction::ClearPluginHighlights { .. } => ScreenContext::ClearPluginHighlights,
            ScreenInstruction::ClearAllPluginHighlights(..) => ScreenContext::ClearPluginHighlights,
            ScreenInstruction::DesktopNotificationResponse(..) => {
                ScreenContext::DesktopNotificationResponse
            },
            ScreenInstruction::SubscribeToPaneRenders { .. } => {
                ScreenContext::SubscribeToPaneRenders
            },
            ScreenInstruction::NotifyPaneClosedToSubscribers { .. } => {
                ScreenContext::NotifyPaneClosedToSubscribers
            },
            ScreenInstruction::PluginSubscribedToAnsiPaneContents(..) => {
                ScreenContext::PluginSubscribedToAnsiPaneContents
            },
            ScreenInstruction::UpdateBackgroundPluginSubscriptions(..) => {
                ScreenContext::UpdateBackgroundPluginSubscriptions
            },
            ScreenInstruction::BroadcastModeUpdate(..) => ScreenContext::BroadcastModeUpdate,
            // Pane-targeting CLI variants
            ScreenInstruction::ScrollUpWithPaneId(..) => ScreenContext::ScrollUpWithPaneId,
            ScreenInstruction::ScrollDownWithPaneId(..) => ScreenContext::ScrollDownWithPaneId,
            ScreenInstruction::ScrollToTopWithPaneId(..) => ScreenContext::ScrollToTopWithPaneId,
            ScreenInstruction::ScrollToBottomWithPaneId(..) => {
                ScreenContext::ScrollToBottomWithPaneId
            },
            ScreenInstruction::PageScrollUpWithPaneId(..) => ScreenContext::PageScrollUpWithPaneId,
            ScreenInstruction::PageScrollDownWithPaneId(..) => {
                ScreenContext::PageScrollDownWithPaneId
            },
            ScreenInstruction::HalfPageScrollUpWithPaneId(..) => {
                ScreenContext::HalfPageScrollUpWithPaneId
            },
            ScreenInstruction::HalfPageScrollDownWithPaneId(..) => {
                ScreenContext::HalfPageScrollDownWithPaneId
            },
            ScreenInstruction::ResizeWithPaneId(..) => ScreenContext::ResizeWithPaneId,
            ScreenInstruction::MovePaneWithPaneIdCli(..) => ScreenContext::MovePaneWithPaneIdCli,
            ScreenInstruction::MovePaneBackwardsWithPaneId(..) => {
                ScreenContext::MovePaneBackwardsWithPaneId
            },
            ScreenInstruction::ClearScreenWithPaneId(..) => ScreenContext::ClearScreenWithPaneId,
            ScreenInstruction::EditScrollbackWithPaneId(..) => {
                ScreenContext::EditScrollbackWithPaneId
            },
            ScreenInstruction::ToggleFullscreenWithPaneId(..) => {
                ScreenContext::ToggleFullscreenWithPaneId
            },
            ScreenInstruction::TogglePaneEmbedOrFloatingWithPaneId(..) => {
                ScreenContext::TogglePaneEmbedOrFloatingWithPaneId
            },
            ScreenInstruction::CloseFocusWithPaneId(..) => ScreenContext::CloseFocusWithPaneId,
            ScreenInstruction::RenamePaneWithPaneId(..) => ScreenContext::RenamePaneWithPaneId,
            ScreenInstruction::UndoRenamePaneWithPaneId(..) => {
                ScreenContext::UndoRenamePaneWithPaneId
            },
            ScreenInstruction::TogglePanePinnedWithPaneId(..) => {
                ScreenContext::TogglePanePinnedWithPaneId
            },
            // Tab-targeting CLI variants
            ScreenInstruction::UndoRenameTabWithTabId(..) => ScreenContext::UndoRenameTabWithTabId,
            ScreenInstruction::ToggleActiveSyncTabWithTabId(..) => {
                ScreenContext::ToggleActiveSyncTabWithTabId
            },
            ScreenInstruction::ToggleFloatingPanesWithTabId(..) => {
                ScreenContext::ToggleFloatingPanesWithTabId
            },
            ScreenInstruction::PreviousSwapLayoutWithTabId(..) => {
                ScreenContext::PreviousSwapLayoutWithTabId
            },
            ScreenInstruction::NextSwapLayoutWithTabId(..) => {
                ScreenContext::NextSwapLayoutWithTabId
            },
            ScreenInstruction::MoveTabWithTabId(..) => ScreenContext::MoveTabWithTabId,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CopyOptions {
    pub command: Option<String>,
    pub clipboard: Clipboard,
    pub copy_on_select: bool,
}

impl CopyOptions {
    pub(crate) fn new(
        copy_command: Option<String>,
        copy_clipboard: Clipboard,
        copy_on_select: bool,
    ) -> Self {
        Self {
            command: copy_command,
            clipboard: copy_clipboard,
            copy_on_select,
        }
    }

    #[cfg(test)]
    pub(crate) fn default() -> Self {
        Self {
            command: None,
            clipboard: Clipboard::default(),
            copy_on_select: true,
        }
    }
}

// We use this to delay rendering when a new tab opens so that we make sure all plugins
// (representing portions of the UI) have been fully loaded before the tab is first rendered (with
// a sensible timeout of 100ms)
#[derive(Debug, Clone)]
pub struct RenderBlocker {
    blocking_plugins: HashMap<u32, Instant>,
    #[cfg_attr(test, allow(dead_code))]
    timeout_ms: u64,
}

impl RenderBlocker {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            blocking_plugins: HashMap::new(),
            timeout_ms,
        }
    }

    pub fn register_blocking_plugin(&mut self, plugin_id: u32) {
        self.blocking_plugins.insert(plugin_id, Instant::now());
    }

    pub fn remove_blocking_plugin(&mut self, plugin_id: u32) {
        self.blocking_plugins.remove(&plugin_id);
    }

    #[cfg(test)]
    pub fn can_render(&mut self) -> bool {
        // we want the tests to be more deterministic and so we always render without any
        // optimizations
        true
    }

    #[cfg(not(test))]
    pub fn can_render(&mut self) -> bool {
        let ret = if self.blocking_plugins.is_empty() {
            true
        } else {
            let timeout = Duration::from_millis(self.timeout_ms);
            let now = Instant::now();

            self.blocking_plugins
                .values()
                .all(|&registered_at| now.duration_since(registered_at) >= timeout)
        };
        if ret {
            self.blocking_plugins.clear();
        }
        ret
    }
}

/// State information for a watcher client
#[derive(Debug, Clone)]
pub(crate) struct WatcherState {
    size: Size,
    should_force_render: bool,
}

impl WatcherState {
    pub fn new(size: Size) -> Self {
        WatcherState {
            size,
            should_force_render: true,
        }
    }

    pub fn size(&self) -> Size {
        self.size
    }

    pub fn set_size(&mut self, size: Size) {
        self.size = size;
    }

    pub fn should_force_render(&self) -> bool {
        self.should_force_render
    }

    pub fn clear_force_render(&mut self) {
        self.should_force_render = false;
    }

    pub fn set_force_render(&mut self) {
        self.should_force_render = true;
    }
}

struct PaneRenderSubscription {
    pane_ids: HashSet<zellij_utils::data::PaneId>,
    previous_viewports: HashMap<zellij_utils::data::PaneId, Vec<String>>,
    ansi: bool,
}

/// A [`Screen`] holds multiple [`Tab`]s, each one holding multiple [`panes`](crate::client::panes).
/// It only directly controls which tab is active, delegating the rest to the individual `Tab`.
pub(crate) struct Screen {
    /// A Bus for sending and receiving messages with the other threads.
    pub bus: Bus<ScreenInstruction>,
    /// An optional maximal amount of panes allowed per [`Tab`] in this [`Screen`] instance.
    max_panes: Option<usize>,
    /// A map between this [`Screen`]'s tabs and their ID/key.
    tabs: BTreeMap<usize, Tab>,
    /// The next stable tab ID. IDs are reserved monotonically and never reused
    /// during a server lifetime, so a delayed close-by-ID cannot hit a new tab
    /// that inherited the identity of a recently closed one.
    next_tab_id: usize,
    /// Screen owns layout transaction identity from before the first Plugin
    /// handoff until PTY acknowledges the terminal commit decision.
    next_layout_transaction_id: LayoutTransactionId,
    active_layout_transactions: HashMap<LayoutTransactionId, ActiveLayoutTransaction>,
    /// Prepared Screen rollback owners whose external Plugin/PTY outcome is
    /// still unknown after bounded inline replay. Background reconciliation
    /// keeps retrying the exact worker decision while these owners remain
    /// quarantined.
    indeterminate_layout_transactions: HashMap<LayoutTransactionId, IndeterminatePreparedLayout>,
    layout_reconciliation_results: Arc<Mutex<Vec<BackgroundLayoutReconciliationResult>>>,
    layout_reconciliations_in_flight: HashSet<LayoutTransactionId>,
    layout_reconciliation_attempts: HashMap<LayoutTransactionId, u32>,
    /// Removed panes remain owned here until exact PTY/Plugin execution
    /// receipts arrive. Channel acceptance alone never transfers ownership.
    pending_layout_cleanup: HashMap<LayoutTransactionId, PendingTabLayoutCleanup>,
    layout_cleanup_retry_results: Arc<Mutex<Vec<BackgroundLayoutCleanupResult>>>,
    layout_cleanup_retries_in_flight: HashSet<LayoutTransactionId>,
    layout_cleanup_retry_attempts: HashMap<LayoutTransactionId, u32>,
    /// Bounded Screen-side decision receipts make exact completion replay a
    /// no-op and prevent a late duplicate from compensating resources that
    /// were already committed.
    resolved_layout_transactions: HashMap<LayoutTransactionId, ResolvedLayoutTransaction>,
    resolved_layout_transaction_order: VecDeque<LayoutTransactionId>,
    /// Unique to this server lifetime. Stable tab IDs are only meaningful
    /// together with this incarnation.
    session_incarnation: String,
    /// The full size of this [`Screen`].
    size: Size,
    pixel_dimensions: PixelDimensions,
    character_cell_size: Rc<RefCell<Option<SizeInPixels>>>,
    stacked_resize: Rc<RefCell<bool>>,
    sixel_image_store: Rc<RefCell<SixelImageStore>>,
    terminal_emulator_colors: Rc<RefCell<Palette>>,
    terminal_emulator_color_codes: Rc<RefCell<HashMap<usize, String>>>,
    connected_clients: Rc<RefCell<HashMap<ClientId, bool>>>, // bool -> is_web_client
    /// The indices of this [`Screen`]'s active [`Tab`]s.
    active_tab_ids: BTreeMap<ClientId, usize>,
    /// Per-regular-client viewport sizes, used to compute per-tab sizing.
    client_sizes: HashMap<ClientId, Size>,
    global_last_active_tab_id: usize,
    tab_history: BTreeMap<ClientId, Vec<usize>>,
    pane_history: BTreeMap<ClientId, Vec<PaneId>>,
    mode_info: BTreeMap<ClientId, ModeInfo>,
    default_mode_info: ModeInfo, // TODO: restructure ModeInfo to prevent this duplication
    style: Style,
    draw_pane_frames: bool,
    auto_layout: bool,
    session_serialization: bool,
    serialize_pane_viewport: bool,
    scrollback_lines_to_serialize: Option<usize>,
    session_is_mirrored: bool,
    copy_options: CopyOptions,
    debug: bool,
    session_name: String,
    peer_sessions_cache: BTreeMap<String, SessionInfo>, // String is the session name, can
    // also be this session
    resurrectable_sessions_cache: BTreeMap<String, Duration>, // String is the session name,
    // duration is its creation time
    default_layout: Box<Layout>,
    default_shell: PathBuf,
    styled_underlines: bool,
    osc8_hyperlinks: bool,
    arrow_fonts: bool,
    #[cfg_attr(test, allow(dead_code))]
    layout_dir: Option<PathBuf>,
    #[cfg_attr(test, allow(dead_code))]
    default_layout_name: Option<String>,
    explicitly_disable_kitty_keyboard_protocol: bool,
    default_editor: Option<PathBuf>,
    web_clients_allowed: bool,
    web_sharing: WebSharing,
    current_pane_group: Rc<RefCell<PaneGroups>>,
    advanced_mouse_actions: bool,
    mouse_hover_effects: bool,
    visual_bell: bool,
    focus_follows_mouse: bool,
    mouse_click_through: bool,
    currently_marking_pane_group: Rc<RefCell<HashMap<ClientId, bool>>>,
    // the below are the configured values - the ones that will be set if and when the web server
    // is brought online
    web_server_ip: IpAddr,
    web_server_port: u16,
    render_blocker: RenderBlocker,
    watcher_clients: HashMap<ClientId, WatcherState>,
    followed_client_id: Option<ClientId>,
    cached_layouts: Vec<LayoutInfo>,
    cached_layout_errors: Vec<LayoutWithError>,
    pane_render_subscribers: HashMap<ClientId, PaneRenderSubscription>,
    plugins_need_ansi_pane_contents: bool,
    background_plugin_subscriptions: HashMap<(PluginId, ClientId), HashSet<EventType>>,
    has_clients_flag: Arc<AtomicBool>,
    /// Monotonic counter used to tag each forwarded host-terminal query
    /// with a unique token. 0 is reserved as a sentinel (see
    /// `STARTUP_SENTINEL_TOKEN`); real forwards start at 1.
    next_forward_token: u32,
    /// Map of forwarded-query token → the originating pane plus the
    /// raw query bytes. When the client sends back the reply for
    /// `token`, the server looks up the pane here and writes the
    /// reply bytes to its pty. The retained `query_bytes` feed the
    /// cache-fallback synthesis: if the reply comes back empty
    /// (detached session, client crash, host timeout), we use the
    /// cached bg/fg/pixel/palette state to answer in the host's
    /// stead.
    pending_forwarded_queries: HashMap<u32, PendingForwardEntry>,
    /// Serialization queue for forwarded queries. Invariant: at most one
    /// forward is in flight to the client at a time (enforced by
    /// `forward_in_flight`). When the reply (or timeout) closes the
    /// active slot, the next queued forward is dispatched.
    forward_queue: VecDeque<PendingForward>,
    /// Token of the forward currently in flight to the (single)
    /// connected client, if any. Used to:
    /// * serialize forwarded queries globally (only one at a time);
    /// * gate slot release on `handle_forwarded_reply_from_host` to a
    ///   token-equality match, so a late server-side timeout for an
    ///   already-answered forward cannot clobber a queued forward that
    ///   the real reply just dispatched.
    forward_in_flight_token: Option<u32>,
    /// Last-known host terminal color-palette theme mode (CSI 2031 /
    /// DSR 997). `None` until the host first reports. Used both for
    /// auto-theme switching and to dedupe duplicate notifications.
    host_terminal_theme_mode: Option<HostTerminalThemeMode>,
    /// Resolved styling to apply when `host_terminal_theme_mode == Dark`.
    /// `None` disables auto-switch. Refreshed on each reconfigure.
    host_theme_dark_styling: Option<Styling>,
    /// Resolved styling to apply when `host_terminal_theme_mode == Light`.
    /// `None` disables auto-switch. Refreshed on each reconfigure.
    host_theme_light_styling: Option<Styling>,
}

struct PreparedApplyLayout {
    tab_id: usize,
    transaction: Box<TabLayoutTransaction>,
    should_change_client_focus: bool,
    client_id: ClientId,
    is_web_client: bool,
}

#[derive(Clone)]
enum LayoutReconciliationIntent {
    Activate,
    Reject(String),
    RejectByOwner(String),
    PreparationFailure {
        failure_message: String,
        pty_cleanup_succeeded: bool,
    },
}

#[derive(Clone)]
struct LayoutReconciliationPlan {
    intent: LayoutReconciliationIntent,
    expected_plugin_ids: Vec<PluginId>,
    resource_ids: Vec<PaneId>,
    preserve_pending_tab_on_rejection: bool,
    close_fenced_tab_on_rejection: bool,
    layout_generation: Option<DurableTabLayoutGeneration>,
}

/// Topology-safe quarantine for a prepared layout while Plugin and PTY are
/// being reconciled in the background after bounded foreground ACK attempts
/// were lost.
enum IndeterminatePreparedLayout {
    Apply {
        prepared: PreparedApplyLayout,
        plan: LayoutReconciliationPlan,
    },
    Override {
        prepared_layouts: Vec<(usize, TabLayoutTransaction)>,
        created_tab_ids: Vec<usize>,
        plan: LayoutReconciliationPlan,
    },
    ResolutionOnly {
        target_tab_ids: Vec<usize>,
        plan: LayoutReconciliationPlan,
    },
}

impl IndeterminatePreparedLayout {
    fn target_tab_ids(&self) -> Vec<usize> {
        match self {
            IndeterminatePreparedLayout::Apply { prepared, .. } => vec![prepared.tab_id],
            IndeterminatePreparedLayout::Override {
                prepared_layouts, ..
            } => prepared_layouts.iter().map(|(tab_id, _)| *tab_id).collect(),
            IndeterminatePreparedLayout::ResolutionOnly { target_tab_ids, .. } => {
                target_tab_ids.clone()
            },
        }
    }

    fn mark_blocking_completion_failed(&mut self, message: &str) {
        match self {
            IndeterminatePreparedLayout::Apply { prepared, .. } => prepared
                .transaction
                .mark_blocking_completion_failed(message),
            IndeterminatePreparedLayout::Override {
                prepared_layouts,
                created_tab_ids,
                ..
            } => {
                let _preserved_created_tab_count = created_tab_ids.len();
                for (_, transaction) in prepared_layouts {
                    transaction.mark_blocking_completion_failed(message);
                }
            },
            IndeterminatePreparedLayout::ResolutionOnly { .. } => {},
        }
    }

    fn replay_rejection(&self, transaction_id: LayoutTransactionId) -> String {
        format!(
            "layout transaction {transaction_id} remains indeterminate and still owns prepared topology for tabs {:?}",
            self.target_tab_ids()
        )
    }

    fn reconciliation_plan(&self) -> LayoutReconciliationPlan {
        match self {
            IndeterminatePreparedLayout::Apply { plan, .. }
            | IndeterminatePreparedLayout::Override { plan, .. }
            | IndeterminatePreparedLayout::ResolutionOnly { plan, .. } => plan.clone(),
        }
    }
}

struct CommittedApplyLayout {
    tab_id: usize,
    effects: TabLayoutCommitEffects,
    should_change_client_focus: bool,
    client_id: ClientId,
    is_web_client: bool,
}

enum CommittedOverrideLayout {
    Complete(Vec<(usize, TabLayoutCommitEffects)>),
    Indeterminate {
        missing_tab_id: usize,
        committed_effects: Vec<(usize, TabLayoutCommitEffects)>,
        remaining_prepared: Vec<(usize, TabLayoutTransaction)>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenLayoutTransactionKind {
    NewTab,
    BreakPane,
    DurableRecovery,
    Override,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ExpectedLayoutTab {
    Present { instance_id: String },
    Absent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LayoutTabOwner {
    tab_id: usize,
    expected: ExpectedLayoutTab,
}

impl LayoutTabOwner {
    fn capture(screen: &Screen, tab_id: usize) -> Self {
        let expected = screen
            .tabs
            .get(&tab_id)
            .map_or(ExpectedLayoutTab::Absent, |tab| {
                ExpectedLayoutTab::Present {
                    instance_id: tab.instance_id.clone(),
                }
            });
        Self { tab_id, expected }
    }

    fn is_current(&self, screen: &Screen) -> bool {
        match (&self.expected, screen.tabs.get(&self.tab_id)) {
            (ExpectedLayoutTab::Absent, None) => true,
            (ExpectedLayoutTab::Present { instance_id }, Some(tab)) => {
                tab.instance_id == *instance_id
            },
            _ => false,
        }
    }

    fn is_current_or_absent(&self, screen: &Screen) -> bool {
        self.is_current(screen) || !screen.tabs.contains_key(&self.tab_id)
    }
}

#[derive(Clone, Debug)]
struct ActiveLayoutTransaction {
    kind: ScreenLayoutTransactionKind,
    targets: Vec<LayoutTabOwner>,
    created_pending_tabs: Vec<LayoutTabOwner>,
    /// Existing tabs whose last committed frame must remain visible while a
    /// transaction temporarily moves panes out of them. These owners are not
    /// worker completion targets, so they stay separate from `targets`, but
    /// they participate in exact-incarnation validation and pending render
    /// gate retirement.
    render_fenced_tabs: Vec<LayoutTabOwner>,
    tabs_to_close_after_commit: Vec<LayoutTabOwner>,
    /// Existing panes deliberately moved into a newly-created pending tab
    /// before Plugin/PTY preparation. A rejected break transaction must keep
    /// these panes alive by activating that baseline tab in degraded mode,
    /// never by discarding it like an ordinary failed NewTab.
    moved_original_panes: Vec<PaneId>,
    generation: Option<DurableTabLayoutGeneration>,
}

#[derive(Debug)]
pub(crate) struct BreakPaneTransfer {
    destination_tab_id: usize,
    source_tab_ids: Vec<usize>,
}

struct ExtractedBreakPane {
    source_tab_id: usize,
    was_floating: bool,
    original_geom: PaneGeom,
    pane: Box<dyn Pane>,
}

impl BreakPaneTransfer {
    fn pending_gate_tab_ids(&self) -> impl Iterator<Item = usize> + '_ {
        std::iter::once(self.destination_tab_id).chain(self.source_tab_ids.iter().copied())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScreenLayoutDecision {
    Committed,
    CommittedWithCleanupDebt(String),
    CommittedWithPostCommitError(String),
    Rejected(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedLayoutTransaction {
    kind: ScreenLayoutTransactionKind,
    target_ids: Vec<usize>,
    generation: Option<DurableTabLayoutGeneration>,
    resource_ids: Vec<PaneId>,
    decision: ScreenLayoutDecision,
}

struct BackgroundLayoutCleanupResult {
    transaction_id: LayoutTransactionId,
    acknowledged_ids: Vec<PaneId>,
    failures: Vec<String>,
}

struct BackgroundLayoutReconciliationResult {
    transaction_id: LayoutTransactionId,
    coordination: LayoutCoordination,
}

impl ActiveLayoutTransaction {
    fn target_ids_match(&self, target_ids: &[usize]) -> bool {
        let expected_raw = self
            .targets
            .iter()
            .map(|target| target.tab_id)
            .collect::<Vec<_>>();
        let mut expected = expected_raw.clone();
        let mut actual = target_ids.to_vec();
        expected.sort_unstable();
        expected.dedup();
        actual.sort_unstable();
        actual.dedup();
        expected_raw.len() == expected.len()
            && target_ids.len() == actual.len()
            && expected == actual
    }

    fn exact_targets_are_current(&self, screen: &Screen) -> bool {
        self.targets.iter().all(|target| target.is_current(screen))
    }

    fn exact_render_fences_are_current(&self, screen: &Screen) -> bool {
        self.render_fenced_tabs
            .iter()
            .all(|target| target.is_current(screen))
    }

    fn pending_gate_owners(&self) -> impl Iterator<Item = &LayoutTabOwner> {
        self.targets
            .iter()
            .chain(self.created_pending_tabs.iter())
            .chain(self.render_fenced_tabs.iter())
    }

    fn generation_matches(&self, generation: Option<&DurableTabLayoutGeneration>) -> bool {
        self.generation.as_ref() == generation
    }
}

/// A pending forward waiting to be dispatched once the current in-flight
/// forward's barrier reply (or timeout) arrives.
#[derive(Debug, Clone)]
struct PendingForward {
    token: u32,
    pane_id: PaneId,
    query: crate::host_query::HostQuery,
}

/// A forward currently in flight (dispatched to the client, waiting
/// for a reply). Retains the `HostQuery` classification so we can
/// fall back to cache-synthesized replies when the live reply is
/// empty (no client attached, client-side timeout, etc.) without
/// re-parsing byte strings.
#[derive(Debug, Clone)]
struct PendingForwardEntry {
    pane_id: PaneId,
    query: crate::host_query::HostQuery,
}

/// Reserved sentinel token for Zellij's own startup batch of host
/// queries (pixel dims, bg/fg, sync-output, palette registers). The
/// existing fire-and-forget startup plumbing is unchanged today; the
/// sentinel exists so a future migration can route the startup batch
/// through the same forwarded-query mechanism as per-pane queries
/// while keeping its replies routable to `Screen`'s cached state
/// rather than to a pane's pty. Reserved value 0 is excluded from
/// `next_forward_token` allocation by the wrap-skip in
/// `forward_host_query`.
const STARTUP_SENTINEL_TOKEN: u32 = 0;

/// Server-side deadline for a forwarded host query. If no
/// `ForwardedReplyFromHost` arrives within this window, the server
/// synthesizes an empty reply for itself so the in-flight slot
/// releases and queued forwards continue to drain.
///
/// This recovers the worst-case where the connected client doesn't
/// understand `ServerToClientMsg::ForwardQueryToHost` (an old client
/// against a new server) — without it the slot would deadlock and
/// every app-issued host query would hang. The window is generous
/// relative to the 500 ms client-side deadline so a well-behaved new
/// client always replies first; only the old-client and
/// network-pathological cases ever see this fire.
const SERVER_FORWARD_TIMEOUT_MS: u64 = 1000;
#[cfg(not(test))]
const LAYOUT_COMMIT_ACK_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const LAYOUT_COMMIT_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const LAYOUT_COMMIT_ACK_ATTEMPTS: usize = 2;
#[cfg(not(test))]
const LAYOUT_CLEANUP_ACK_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(test)]
const LAYOUT_CLEANUP_ACK_TIMEOUT: Duration = Duration::from_millis(250);
const LAYOUT_CLEANUP_ACK_ATTEMPTS: usize = 2;
#[cfg(not(test))]
const LAYOUT_CLEANUP_RETRY_BASE: Duration = Duration::from_millis(250);
#[cfg(test)]
const LAYOUT_CLEANUP_RETRY_BASE: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const LAYOUT_CLEANUP_RETRY_MAX: Duration = Duration::from_secs(30);
#[cfg(test)]
const LAYOUT_CLEANUP_RETRY_MAX: Duration = Duration::from_millis(100);
const MAX_RESOLVED_LAYOUT_TRANSACTIONS: usize = 512;

#[derive(Debug)]
enum LayoutPluginAck {
    Resolved(LayoutPluginReceipt),
    Failed(String),
    Unknown(String),
}

enum LayoutCoordination {
    Commit,
    Rollback(String),
    Unknown(String),
}

fn layout_plugin_receipt_ids(receipt: &LayoutPluginReceipt) -> &[PluginId] {
    match receipt {
        LayoutPluginReceipt::Activated { plugin_ids }
        | LayoutPluginReceipt::Released { plugin_ids }
        | LayoutPluginReceipt::Compensated { plugin_ids }
        | LayoutPluginReceipt::ActivationRolledBack { plugin_ids, .. } => plugin_ids,
    }
}

fn validate_layout_plugin_receipt(
    transaction_id: LayoutTransactionId,
    resolution: &LayoutPluginResolution,
    expected_plugin_ids: &[PluginId],
    receipt: LayoutPluginReceipt,
) -> std::result::Result<LayoutPluginReceipt, String> {
    let receipt_matches_resolution = matches!(
        (resolution, &receipt),
        (
            LayoutPluginResolution::Activate,
            LayoutPluginReceipt::Activated { .. }
                | LayoutPluginReceipt::ActivationRolledBack { .. }
        ) | (
            LayoutPluginResolution::Release { .. },
            LayoutPluginReceipt::Released { .. }
        ) | (
            LayoutPluginResolution::Compensate { .. },
            LayoutPluginReceipt::Compensated { .. }
        )
    );
    if !receipt_matches_resolution {
        return Err(format!(
            "layout plugin transaction {transaction_id} returned receipt {receipt:?} for incompatible resolution {resolution:?}"
        ));
    }
    let mut expected_plugin_ids = expected_plugin_ids.to_vec();
    expected_plugin_ids.sort_unstable();
    let mut receipt_plugin_ids = layout_plugin_receipt_ids(&receipt).to_vec();
    receipt_plugin_ids.sort_unstable();
    if receipt_plugin_ids != expected_plugin_ids {
        return Err(format!(
            "layout plugin transaction {transaction_id} receipt ids {receipt_plugin_ids:?} did not match expected ids {expected_plugin_ids:?}"
        ));
    }
    Ok(receipt)
}

fn resolve_layout_plugins_with_ack(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    resolution: LayoutPluginResolution,
    expected_plugin_ids: &[PluginId],
) -> LayoutPluginAck {
    if transaction_id == 0 {
        #[cfg(test)]
        {
            let receipt = match resolution {
                LayoutPluginResolution::Activate => LayoutPluginReceipt::Activated {
                    plugin_ids: expected_plugin_ids.to_vec(),
                },
                LayoutPluginResolution::Release { .. } => LayoutPluginReceipt::Released {
                    plugin_ids: expected_plugin_ids.to_vec(),
                },
                LayoutPluginResolution::Compensate { .. } => LayoutPluginReceipt::Compensated {
                    plugin_ids: expected_plugin_ids.to_vec(),
                },
            };
            return LayoutPluginAck::Resolved(receipt);
        }
        #[cfg(not(test))]
        {
            return LayoutPluginAck::Failed(
                "layout transaction id 0 is reserved and cannot activate Plugin resources"
                    .to_owned(),
            );
        }
    }
    let mut failures = vec![];
    for attempt in 1..=LAYOUT_COMMIT_ACK_ATTEMPTS {
        let (ack, ack_rx) = channels::bounded(1);
        let instruction = PluginInstruction::ResolveLayoutPlugins {
            transaction_id,
            resolution: resolution.clone(),
            expected_plugin_ids: expected_plugin_ids.to_vec(),
            ack,
        };
        if let Err(send_failure) = senders.send_to_plugin_recover(instruction) {
            let (_instruction, send_error) = send_failure.into_parts();
            failures.push(format!("attempt {attempt} delivery: {send_error:#}"));
            continue;
        }
        match ack_rx.recv_timeout(LAYOUT_COMMIT_ACK_TIMEOUT) {
            Ok(Ok(receipt)) => {
                return match validate_layout_plugin_receipt(
                    transaction_id,
                    &resolution,
                    expected_plugin_ids,
                    receipt,
                ) {
                    Ok(receipt) => LayoutPluginAck::Resolved(receipt),
                    Err(message) => LayoutPluginAck::Failed(message),
                };
            },
            Ok(Err(message)) => {
                return LayoutPluginAck::Failed(format!(
                    "attempt {attempt} Plugin resolution: {message}"
                ));
            },
            Err(channels::RecvTimeoutError::Timeout) => {
                failures.push(format!("attempt {attempt} ACK timeout"));
            },
            Err(channels::RecvTimeoutError::Disconnected) => {
                failures.push(format!("attempt {attempt} ACK disconnect"));
            },
        }
    }
    LayoutPluginAck::Unknown(format!(
        "layout plugin transaction {transaction_id} resolution remained unknown after {} attempts: {}",
        LAYOUT_COMMIT_ACK_ATTEMPTS,
        failures.join("; ")
    ))
}

fn release_layout_plugins_by_transaction_with_ack(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    reason: String,
) -> LayoutPluginAck {
    if transaction_id == 0 {
        return LayoutPluginAck::Failed(
            "layout transaction id 0 is reserved and cannot release Plugin resources".to_owned(),
        );
    }
    let mut failures = vec![];
    for attempt in 1..=LAYOUT_COMMIT_ACK_ATTEMPTS {
        let (ack, ack_rx) = channels::bounded(1);
        let instruction = PluginInstruction::ReleaseLayoutPluginsByTransaction {
            transaction_id,
            reason: reason.clone(),
            ack,
        };
        if let Err(send_failure) = senders.send_to_plugin_recover(instruction) {
            let (_instruction, send_error) = send_failure.into_parts();
            failures.push(format!("attempt {attempt} delivery: {send_error:#}"));
            continue;
        }
        match ack_rx.recv_timeout(LAYOUT_COMMIT_ACK_TIMEOUT) {
            Ok(Ok(receipt @ LayoutPluginReceipt::Released { .. })) => {
                return LayoutPluginAck::Resolved(receipt);
            },
            Ok(Ok(receipt)) => {
                return LayoutPluginAck::Failed(format!(
                    "layout plugin transaction {transaction_id} returned incompatible by-owner release receipt {receipt:?}"
                ));
            },
            Ok(Err(message)) => {
                return LayoutPluginAck::Failed(format!(
                    "attempt {attempt} Plugin by-owner release: {message}"
                ));
            },
            Err(channels::RecvTimeoutError::Timeout) => {
                failures.push(format!("attempt {attempt} ACK timeout"));
            },
            Err(channels::RecvTimeoutError::Disconnected) => {
                failures.push(format!("attempt {attempt} ACK disconnect"));
            },
        }
    }
    LayoutPluginAck::Unknown(format!(
        "layout plugin transaction {transaction_id} by-owner release remained unknown after {} attempts: {}",
        LAYOUT_COMMIT_ACK_ATTEMPTS,
        failures.join("; ")
    ))
}

fn resolve_layout_commit_with_pty_ack(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    outcome: LayoutCommitOutcome,
) -> Result<LayoutCommitAck> {
    if transaction_id == 0 {
        #[cfg(test)]
        {
            return Ok(LayoutCommitAck::Resolved);
        }
        #[cfg(not(test))]
        {
            bail!("layout transaction id 0 is reserved and cannot commit PTY resources");
        }
    }
    let mut failures = vec![];
    for attempt in 1..=LAYOUT_COMMIT_ACK_ATTEMPTS {
        let (ack, ack_rx) = channels::bounded(1);
        let instruction = PtyInstruction::LayoutCommitResolved {
            transaction_id,
            outcome: outcome.clone(),
            ack,
        };
        if let Err(send_failure) = senders.send_to_pty_recover(instruction) {
            let (_instruction, send_error) = send_failure.into_parts();
            failures.push(format!("attempt {attempt} delivery: {send_error:#}"));
            continue;
        }
        match ack_rx.recv_timeout(LAYOUT_COMMIT_ACK_TIMEOUT) {
            Ok(Ok(ack)) => return Ok(ack),
            Ok(Err(message)) => {
                failures.push(format!("attempt {attempt} PTY resolution: {message}"));
            },
            Err(channels::RecvTimeoutError::Timeout) => {
                failures.push(format!("attempt {attempt} ACK timeout"));
            },
            Err(channels::RecvTimeoutError::Disconnected) => {
                failures.push(format!("attempt {attempt} ACK disconnect"));
            },
        }
    }
    bail!(
        "layout transaction {transaction_id} resolution remained unknown after {} attempts: {}",
        LAYOUT_COMMIT_ACK_ATTEMPTS,
        failures.join("; ")
    )
}

fn coordinate_layout_activation(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    expected_plugin_ids: &[PluginId],
) -> LayoutCoordination {
    match resolve_layout_plugins_with_ack(
        senders,
        transaction_id,
        LayoutPluginResolution::Activate,
        expected_plugin_ids,
    ) {
        LayoutPluginAck::Resolved(LayoutPluginReceipt::Activated { .. }) => {
            match resolve_layout_commit_with_pty_ack(
                senders,
                transaction_id,
                LayoutCommitOutcome::Committed,
            ) {
                Ok(LayoutCommitAck::Resolved) => LayoutCoordination::Commit,
                Ok(LayoutCommitAck::ActivationRolledBack(message)) => {
                    let compensation_reason = format!(
                        "PTY activation rolled back layout transaction {transaction_id}: {message}"
                    );
                    match resolve_layout_plugins_with_ack(
                        senders,
                        transaction_id,
                        LayoutPluginResolution::Compensate {
                            reason: compensation_reason.clone(),
                        },
                        expected_plugin_ids,
                    ) {
                        LayoutPluginAck::Resolved(LayoutPluginReceipt::Compensated { .. }) => {
                            LayoutCoordination::Rollback(compensation_reason)
                        },
                        LayoutPluginAck::Resolved(receipt) => LayoutCoordination::Unknown(format!(
                            "{compensation_reason}; Plugin returned unexpected compensation receipt {receipt:?}"
                        )),
                        LayoutPluginAck::Failed(error) | LayoutPluginAck::Unknown(error) => {
                            LayoutCoordination::Unknown(format!(
                                "{compensation_reason}; Plugin compensation was not certified: {error}"
                            ))
                        },
                    }
                },
                Err(error) => LayoutCoordination::Unknown(format!(
                    "layout transaction {transaction_id} Plugin activation succeeded but PTY commit remained unknown: {error:#}"
                )),
            }
        },
        LayoutPluginAck::Resolved(LayoutPluginReceipt::ActivationRolledBack {
            message, ..
        }) => {
            let rejection = format!(
                "Plugin activation rolled back layout transaction {transaction_id}: {message}"
            );
            match resolve_layout_commit_with_pty_ack(
                senders,
                transaction_id,
                LayoutCommitOutcome::Rejected(rejection.clone()),
            ) {
                Ok(LayoutCommitAck::Resolved) => LayoutCoordination::Rollback(rejection),
                Ok(LayoutCommitAck::ActivationRolledBack(message)) => {
                    LayoutCoordination::Unknown(format!(
                        "{rejection}; PTY returned an activation rollback for a rejection: {message}"
                    ))
                },
                Err(error) => LayoutCoordination::Unknown(format!(
                    "{rejection}; PTY rejection remained unknown: {error:#}"
                )),
            }
        },
        LayoutPluginAck::Resolved(receipt) => LayoutCoordination::Unknown(format!(
            "layout transaction {transaction_id} returned unexpected Plugin activation receipt {receipt:?}"
        )),
        LayoutPluginAck::Failed(error) => {
            let rejection = format!(
                "Plugin activation failed for layout transaction {transaction_id}: {error}"
            );
            match resolve_layout_plugins_with_ack(
                senders,
                transaction_id,
                LayoutPluginResolution::Release {
                    reason: rejection.clone(),
                },
                expected_plugin_ids,
            ) {
                LayoutPluginAck::Resolved(LayoutPluginReceipt::Released { .. }) => {
                    match resolve_layout_commit_with_pty_ack(
                        senders,
                        transaction_id,
                        LayoutCommitOutcome::Rejected(rejection.clone()),
                    ) {
                        Ok(LayoutCommitAck::Resolved) => LayoutCoordination::Rollback(rejection),
                        Ok(LayoutCommitAck::ActivationRolledBack(message)) => {
                            LayoutCoordination::Unknown(format!(
                                "{rejection}; PTY returned an activation rollback for a rejection: {message}"
                            ))
                        },
                        Err(error) => LayoutCoordination::Unknown(format!(
                            "{rejection}; PTY rejection remained unknown: {error:#}"
                        )),
                    }
                },
                LayoutPluginAck::Resolved(receipt) => LayoutCoordination::Unknown(format!(
                    "{rejection}; Plugin returned unexpected release receipt {receipt:?}"
                )),
                LayoutPluginAck::Failed(release_error)
                | LayoutPluginAck::Unknown(release_error) => LayoutCoordination::Unknown(format!(
                    "{rejection}; Plugin release was not certified: {release_error}"
                )),
            }
        },
        LayoutPluginAck::Unknown(error) => LayoutCoordination::Unknown(format!(
            "layout transaction {transaction_id} Plugin activation remained unknown: {error}"
        )),
    }
}

fn coordinate_layout_rejection(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    expected_plugin_ids: &[PluginId],
    rejection: String,
) -> LayoutCoordination {
    match resolve_layout_plugins_with_ack(
        senders,
        transaction_id,
        LayoutPluginResolution::Release {
            reason: rejection.clone(),
        },
        expected_plugin_ids,
    ) {
        LayoutPluginAck::Unknown(error) => LayoutCoordination::Unknown(format!(
            "{rejection}; Plugin release remained unknown: {error}"
        )),
        LayoutPluginAck::Resolved(LayoutPluginReceipt::Released { .. }) => {
            match resolve_layout_commit_with_pty_ack(
                senders,
                transaction_id,
                LayoutCommitOutcome::Rejected(rejection.clone()),
            ) {
                Ok(LayoutCommitAck::Resolved) => LayoutCoordination::Rollback(rejection),
                Ok(LayoutCommitAck::ActivationRolledBack(message)) => {
                    LayoutCoordination::Unknown(format!(
                        "{rejection}; PTY returned an activation rollback for a rejection: {message}"
                    ))
                },
                Err(error) => LayoutCoordination::Unknown(format!(
                    "{rejection}; PTY rejection remained unknown: {error:#}"
                )),
            }
        },
        LayoutPluginAck::Resolved(receipt) => LayoutCoordination::Unknown(format!(
            "{rejection}; Plugin returned unexpected release receipt {receipt:?}"
        )),
        LayoutPluginAck::Failed(error) => LayoutCoordination::Unknown(format!(
            "{rejection}; Plugin release failed explicitly and cleanup is unverified: {error}"
        )),
    }
}

fn coordinate_layout_rejection_by_owner(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    rejection: String,
) -> LayoutCoordination {
    match release_layout_plugins_by_transaction_with_ack(senders, transaction_id, rejection.clone())
    {
        LayoutPluginAck::Unknown(error) => LayoutCoordination::Unknown(format!(
            "{rejection}; exact Plugin by-owner release remained unknown: {error}"
        )),
        LayoutPluginAck::Resolved(LayoutPluginReceipt::Released { .. }) => {
            match resolve_layout_commit_with_pty_ack(
                senders,
                transaction_id,
                LayoutCommitOutcome::Rejected(rejection.clone()),
            ) {
                Ok(LayoutCommitAck::Resolved) => LayoutCoordination::Rollback(rejection),
                Ok(LayoutCommitAck::ActivationRolledBack(message)) => {
                    LayoutCoordination::Unknown(format!(
                        "{rejection}; PTY returned an activation rollback for a rejection: {message}"
                    ))
                },
                Err(error) => LayoutCoordination::Unknown(format!(
                    "{rejection}; PTY by-owner rejection remained unknown: {error:#}"
                )),
            }
        },
        LayoutPluginAck::Resolved(receipt) => LayoutCoordination::Unknown(format!(
            "{rejection}; Plugin returned unexpected by-owner release receipt {receipt:?}"
        )),
        LayoutPluginAck::Failed(error) => LayoutCoordination::Unknown(format!(
            "{rejection}; exact Plugin by-owner release failed and cleanup is unverified: {error}"
        )),
    }
}

fn certify_layout_preparation_cleanup(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    cleanup: LayoutPreparationCleanup,
    failure_message: &str,
) -> std::result::Result<(), String> {
    match cleanup {
        LayoutPreparationCleanup::Resolved => Ok(()),
        LayoutPreparationCleanup::ReleasePluginReservation {
            plugin_ids,
            pty_cleanup_succeeded,
        } => {
            let release_reason = format!(
                "PTY rejected layout transaction {transaction_id} during preparation: {failure_message}"
            );
            match resolve_layout_plugins_with_ack(
                senders,
                transaction_id,
                LayoutPluginResolution::Release {
                    reason: release_reason.clone(),
                },
                &plugin_ids,
            ) {
                LayoutPluginAck::Resolved(LayoutPluginReceipt::Released { .. })
                    if pty_cleanup_succeeded =>
                {
                    Ok(())
                },
                LayoutPluginAck::Resolved(LayoutPluginReceipt::Released { .. }) => Err(format!(
                    "{release_reason}; Plugin release was certified but PTY cleanup was not"
                )),
                LayoutPluginAck::Resolved(receipt) => Err(format!(
                    "{release_reason}; Plugin returned unexpected preparation-release receipt {receipt:?}"
                )),
                LayoutPluginAck::Failed(error) | LayoutPluginAck::Unknown(error) => Err(format!(
                    "{release_reason}; Plugin release was not certified: {error}"
                )),
            }
        },
    }
}

impl Screen {
    fn discard_pending_tab_after_layout_rejection(&mut self, tab_id: usize) -> Result<()> {
        let tab = self
            .tabs
            .get(&tab_id)
            .with_context(|| format!("rejected pending tab {tab_id} disappeared"))?;
        if !tab.is_pending() {
            bail!("refusing to discard committed tab {tab_id} after layout rejection");
        }
        let removed_position = tab.position;
        self.tabs.remove(&tab_id);
        self.active_tab_ids
            .retain(|_, active_tab_id| *active_tab_id != tab_id);
        for tab in self.tabs.values_mut() {
            if tab.position > removed_position {
                tab.position -= 1;
            }
        }
        Ok(())
    }

    /// Creates and returns a new [`Screen`].
    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    pub fn new(
        bus: Bus<ScreenInstruction>,
        client_attributes: &ClientAttributes,
        max_panes: Option<usize>,
        mode_info: ModeInfo,
        draw_pane_frames: bool,
        auto_layout: bool,
        session_is_mirrored: bool,
        copy_options: CopyOptions,
        debug: bool,
        default_layout: Box<Layout>,
        default_layout_name: Option<String>,
        default_shell: PathBuf,
        session_serialization: bool,
        serialize_pane_viewport: bool,
        scrollback_lines_to_serialize: Option<usize>,
        styled_underlines: bool,
        osc8_hyperlinks: bool,
        arrow_fonts: bool,
        layout_dir: Option<PathBuf>,
        explicitly_disable_kitty_keyboard_protocol: bool,
        stacked_resize: bool,
        default_editor: Option<PathBuf>,
        web_clients_allowed: bool,
        web_sharing: WebSharing,
        advanced_mouse_actions: bool,
        mouse_hover_effects: bool,
        visual_bell: bool,
        focus_follows_mouse: bool,
        mouse_click_through: bool,
        web_server_ip: IpAddr,
        web_server_port: u16,
        has_clients_flag: Arc<AtomicBool>,
    ) -> Self {
        let session_name = mode_info.session_name.clone().unwrap_or_default();
        let session_info = SessionInfo::new(session_name.clone());
        let mut peer_sessions_cache = BTreeMap::new();
        let resurrectable_sessions_cache = BTreeMap::new();
        peer_sessions_cache.insert(session_name.clone(), session_info);
        let current_pane_group = PaneGroups::new(bus.senders.clone());
        Screen {
            bus,
            max_panes,
            size: client_attributes.size,
            pixel_dimensions: Default::default(),
            character_cell_size: Rc::new(RefCell::new(None)),
            stacked_resize: Rc::new(RefCell::new(stacked_resize)),
            sixel_image_store: Rc::new(RefCell::new(SixelImageStore::default())),
            style: client_attributes.style,
            connected_clients: Rc::new(RefCell::new(HashMap::new())),
            active_tab_ids: BTreeMap::new(),
            client_sizes: HashMap::new(),
            global_last_active_tab_id: 0,
            tabs: BTreeMap::new(),
            next_tab_id: 0,
            next_layout_transaction_id: 1,
            active_layout_transactions: HashMap::new(),
            indeterminate_layout_transactions: HashMap::new(),
            layout_reconciliation_results: Arc::new(Mutex::new(vec![])),
            layout_reconciliations_in_flight: HashSet::new(),
            layout_reconciliation_attempts: HashMap::new(),
            pending_layout_cleanup: HashMap::new(),
            layout_cleanup_retry_results: Arc::new(Mutex::new(vec![])),
            layout_cleanup_retries_in_flight: HashSet::new(),
            layout_cleanup_retry_attempts: HashMap::new(),
            resolved_layout_transactions: HashMap::new(),
            resolved_layout_transaction_order: VecDeque::new(),
            session_incarnation: Uuid::new_v4().to_string(),
            terminal_emulator_colors: Rc::new(RefCell::new(Palette::default())),
            terminal_emulator_color_codes: Rc::new(RefCell::new(HashMap::new())),
            tab_history: BTreeMap::new(),
            pane_history: BTreeMap::new(),
            mode_info: BTreeMap::new(),
            default_mode_info: mode_info,
            draw_pane_frames,
            auto_layout,
            session_is_mirrored,
            copy_options,
            debug,
            session_name,
            peer_sessions_cache,
            default_layout,
            default_layout_name,
            default_shell,
            session_serialization,
            serialize_pane_viewport,
            scrollback_lines_to_serialize,
            styled_underlines,
            osc8_hyperlinks,
            arrow_fonts,
            resurrectable_sessions_cache,
            layout_dir,
            explicitly_disable_kitty_keyboard_protocol,
            default_editor,
            web_clients_allowed,
            web_sharing,
            current_pane_group: Rc::new(RefCell::new(current_pane_group)),
            currently_marking_pane_group: Rc::new(RefCell::new(HashMap::new())),
            advanced_mouse_actions,
            mouse_hover_effects,
            visual_bell,
            focus_follows_mouse,
            mouse_click_through,
            web_server_ip,
            web_server_port,
            render_blocker: RenderBlocker::new(100),
            watcher_clients: HashMap::new(),
            followed_client_id: None,
            cached_layouts: vec![],
            cached_layout_errors: vec![],
            pane_render_subscribers: HashMap::new(),
            plugins_need_ansi_pane_contents: false,
            background_plugin_subscriptions: HashMap::new(),
            has_clients_flag,
            next_forward_token: 1, // 0 is reserved as the startup sentinel
            pending_forwarded_queries: HashMap::new(),
            forward_queue: VecDeque::new(),
            forward_in_flight_token: None,
            host_terminal_theme_mode: None,
            host_theme_dark_styling: None,
            host_theme_light_styling: None,
        }
    }

    fn get_new_tab_id(&mut self) -> usize {
        let tab_id = self.next_tab_id;
        self.next_tab_id = self
            .next_tab_id
            .checked_add(1)
            .expect("stable tab ID space exhausted");
        tab_id
    }

    fn reserve_layout_transaction_id(&mut self) -> LayoutTransactionId {
        loop {
            let transaction_id = self.next_layout_transaction_id;
            self.next_layout_transaction_id = self.next_layout_transaction_id.wrapping_add(1);
            if self.next_layout_transaction_id == 0 {
                self.next_layout_transaction_id = 1;
            }
            if transaction_id != 0
                && !self
                    .active_layout_transactions
                    .contains_key(&transaction_id)
                && !self
                    .resolved_layout_transactions
                    .contains_key(&transaction_id)
                && !self
                    .indeterminate_layout_transactions
                    .contains_key(&transaction_id)
                && !self.pending_layout_cleanup.contains_key(&transaction_id)
            {
                return transaction_id;
            }
        }
    }

    fn retain_layout_cleanup(
        &mut self,
        transaction_id: LayoutTransactionId,
        cleanup: PendingTabLayoutCleanup,
    ) {
        if cleanup.is_empty() {
            return;
        }
        self.pending_layout_cleanup
            .entry(transaction_id)
            .or_default()
            .append(cleanup);
    }

    fn flush_layout_cleanup(&mut self, transaction_id: LayoutTransactionId) {
        let senders = self.bus.senders.clone();
        let Some(cleanup) = self.pending_layout_cleanup.get_mut(&transaction_id) else {
            return;
        };
        let failures = cleanup.flush(
            transaction_id,
            &senders,
            LAYOUT_CLEANUP_ACK_TIMEOUT,
            LAYOUT_CLEANUP_ACK_ATTEMPTS,
        );
        if cleanup.is_empty() {
            self.finish_layout_cleanup(transaction_id);
        } else if !failures.is_empty() {
            log::error!(
                "layout transaction {transaction_id} retains cleanup ownership without exact worker execution receipts: {}",
                failures.join("; ")
            );
        }
    }

    fn finish_layout_cleanup(&mut self, transaction_id: LayoutTransactionId) {
        self.pending_layout_cleanup.remove(&transaction_id);
        self.layout_cleanup_retries_in_flight
            .remove(&transaction_id);
        self.layout_cleanup_retry_attempts.remove(&transaction_id);
        if let Some(receipt) = self.resolved_layout_transactions.get_mut(&transaction_id)
            && matches!(
                &receipt.decision,
                ScreenLayoutDecision::CommittedWithCleanupDebt(_)
            )
        {
            receipt.decision = ScreenLayoutDecision::Committed;
        }
    }

    fn retry_pending_layout_cleanup_in_background(&mut self) {
        let completed = {
            let mut results = self
                .layout_cleanup_retry_results
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            std::mem::take(&mut *results)
        };
        for result in completed {
            self.layout_cleanup_retries_in_flight
                .remove(&result.transaction_id);
            if let Some(cleanup) = self.pending_layout_cleanup.get_mut(&result.transaction_id) {
                cleanup.acknowledge(result.acknowledged_ids);
                if !result.failures.is_empty() {
                    log::error!(
                        "layout transaction {} still retains cleanup debt after background execution probe: {}",
                        result.transaction_id,
                        result.failures.join("; ")
                    );
                }
                if cleanup.is_empty() {
                    self.finish_layout_cleanup(result.transaction_id);
                } else {
                    self.layout_cleanup_retry_attempts
                        .entry(result.transaction_id)
                        .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                        .or_insert(1);
                }
            }
        }

        let transaction_ids = self
            .pending_layout_cleanup
            .keys()
            .filter(|transaction_id| {
                !self
                    .layout_cleanup_retries_in_flight
                    .contains(transaction_id)
            })
            .copied()
            .collect::<Vec<_>>();
        for transaction_id in transaction_ids {
            let Some(pane_ids) = self
                .pending_layout_cleanup
                .get(&transaction_id)
                .map(PendingTabLayoutCleanup::pane_ids)
            else {
                continue;
            };
            let senders = self.bus.senders.clone();
            let wake_screen = senders.clone();
            let results = self.layout_cleanup_retry_results.clone();
            let retry_attempt = self
                .layout_cleanup_retry_attempts
                .get(&transaction_id)
                .copied()
                .unwrap_or(0)
                .min(8);
            let retry_delay = LAYOUT_CLEANUP_RETRY_BASE
                .checked_mul(1_u32 << retry_attempt)
                .unwrap_or(LAYOUT_CLEANUP_RETRY_MAX)
                .min(LAYOUT_CLEANUP_RETRY_MAX);
            self.layout_cleanup_retries_in_flight.insert(transaction_id);
            let spawn_result = std::thread::Builder::new()
                .name(format!("layout-cleanup-{transaction_id}"))
                .spawn(move || {
                    std::thread::sleep(retry_delay);
                    let probe = PendingTabLayoutCleanup::probe(
                        transaction_id,
                        pane_ids,
                        &senders,
                        LAYOUT_CLEANUP_ACK_TIMEOUT,
                        LAYOUT_CLEANUP_ACK_ATTEMPTS,
                    );
                    let (acknowledged_ids, failures) = probe.into_parts();
                    results
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(BackgroundLayoutCleanupResult {
                            transaction_id,
                            acknowledged_ids,
                            failures,
                        });
                    let _ = wake_screen.send_to_screen(ScreenInstruction::LayoutMaintenanceWake);
                });
            if let Err(error) = spawn_result {
                self.layout_cleanup_retries_in_flight
                    .remove(&transaction_id);
                self.layout_cleanup_retry_attempts
                    .entry(transaction_id)
                    .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                    .or_insert(1);
                log::error!(
                    "failed to start background cleanup retry for layout transaction {transaction_id}: {error}"
                );
            }
        }
    }

    fn take_resolved_layout_reconciliations(
        &mut self,
    ) -> Vec<(LayoutTransactionId, LayoutCoordination)> {
        let completed = {
            let mut results = self
                .layout_reconciliation_results
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            std::mem::take(&mut *results)
        };
        let mut resolved = vec![];
        for result in completed {
            self.layout_reconciliations_in_flight
                .remove(&result.transaction_id);
            match result.coordination {
                LayoutCoordination::Unknown(message) => {
                    self.layout_reconciliation_attempts
                        .entry(result.transaction_id)
                        .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                        .or_insert(1);
                    log::error!(
                        "layout transaction {} remains indeterminate after background reconciliation: {}",
                        result.transaction_id,
                        message
                    );
                },
                coordination => {
                    resolved.push((result.transaction_id, coordination));
                },
            }
        }
        resolved
    }

    fn retry_indeterminate_layout_transactions_in_background(&mut self) {
        let transaction_ids = self
            .indeterminate_layout_transactions
            .keys()
            .filter(|transaction_id| {
                !self
                    .layout_reconciliations_in_flight
                    .contains(transaction_id)
            })
            .copied()
            .collect::<Vec<_>>();
        for transaction_id in transaction_ids {
            let Some(plan) = self
                .indeterminate_layout_transactions
                .get(&transaction_id)
                .map(IndeterminatePreparedLayout::reconciliation_plan)
            else {
                continue;
            };
            let retry_attempt = self
                .layout_reconciliation_attempts
                .get(&transaction_id)
                .copied()
                .unwrap_or(0)
                .min(8);
            let retry_delay = LAYOUT_CLEANUP_RETRY_BASE
                .checked_mul(1_u32 << retry_attempt)
                .unwrap_or(LAYOUT_CLEANUP_RETRY_MAX)
                .min(LAYOUT_CLEANUP_RETRY_MAX);
            let senders = self.bus.senders.clone();
            let wake_screen = senders.clone();
            let results = self.layout_reconciliation_results.clone();
            self.layout_reconciliations_in_flight.insert(transaction_id);
            let spawn_result = std::thread::Builder::new()
                .name(format!("layout-reconcile-{transaction_id}"))
                .spawn(move || {
                    std::thread::sleep(retry_delay);
                    let coordination = match plan.intent {
                        LayoutReconciliationIntent::Activate => coordinate_layout_activation(
                            &senders,
                            transaction_id,
                            &plan.expected_plugin_ids,
                        ),
                        LayoutReconciliationIntent::Reject(rejection) => {
                            coordinate_layout_rejection(
                                &senders,
                                transaction_id,
                                &plan.expected_plugin_ids,
                                rejection,
                            )
                        },
                        LayoutReconciliationIntent::RejectByOwner(rejection) => {
                            coordinate_layout_rejection_by_owner(
                                &senders,
                                transaction_id,
                                rejection,
                            )
                        },
                        LayoutReconciliationIntent::PreparationFailure {
                            failure_message,
                            pty_cleanup_succeeded,
                        } => match certify_layout_preparation_cleanup(
                            &senders,
                            transaction_id,
                            LayoutPreparationCleanup::ReleasePluginReservation {
                                plugin_ids: plan.expected_plugin_ids,
                                pty_cleanup_succeeded,
                            },
                            &failure_message,
                        ) {
                            Ok(()) => LayoutCoordination::Rollback(failure_message),
                            Err(error) => LayoutCoordination::Unknown(error),
                        },
                    };
                    results
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .push(BackgroundLayoutReconciliationResult {
                            transaction_id,
                            coordination,
                        });
                    let _ = wake_screen.send_to_screen(ScreenInstruction::LayoutMaintenanceWake);
                });
            if let Err(error) = spawn_result {
                self.layout_reconciliations_in_flight
                    .remove(&transaction_id);
                self.layout_reconciliation_attempts
                    .entry(transaction_id)
                    .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                    .or_insert(1);
                log::error!(
                    "failed to start background reconciliation for layout transaction {transaction_id}: {error}"
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn reconcile_indeterminate_layout_transaction(
        &mut self,
        transaction_id: LayoutTransactionId,
        coordination: LayoutCoordination,
        pending_tab_ids: &mut HashSet<usize>,
        durable_tab_layout_generations: &HashMap<String, DurableTabLayoutGeneration>,
        pending_tab_switches: &mut HashSet<(usize, ClientId)>,
        pending_events_waiting_for_client: &mut Vec<ScreenInstruction>,
        pending_events_waiting_for_tab: &mut Vec<ScreenInstruction>,
        plugin_loading_message_cache: &mut HashMap<PluginId, LoadingIndication>,
    ) -> Result<()> {
        let Some(indeterminate) = self
            .indeterminate_layout_transactions
            .remove(&transaction_id)
        else {
            return Ok(());
        };
        let Some(owner) = self
            .active_layout_transactions
            .get(&transaction_id)
            .cloned()
        else {
            self.indeterminate_layout_transactions
                .insert(transaction_id, indeterminate);
            self.layout_reconciliation_attempts
                .entry(transaction_id)
                .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                .or_insert(1);
            bail!(
                "layout transaction {transaction_id} recovered a worker decision but lost its active Screen owner"
            );
        };
        let owner_targets_are_current = owner.exact_targets_are_current(self);
        let exact_by_owner_rejection =
            matches!(
                indeterminate.reconciliation_plan().intent,
                LayoutReconciliationIntent::RejectByOwner(_)
            ) && matches!(&coordination, LayoutCoordination::Rollback(_));
        if !owner_targets_are_current && !exact_by_owner_rejection {
            self.indeterminate_layout_transactions
                .insert(transaction_id, indeterminate);
            self.layout_reconciliation_attempts
                .entry(transaction_id)
                .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                .or_insert(1);
            bail!(
                "layout transaction {transaction_id} recovered a worker decision but no longer owns the exact target tab incarnation"
            );
        }

        match (coordination, indeterminate) {
            (LayoutCoordination::Commit, IndeterminatePreparedLayout::Apply { prepared, plan }) => {
                let tab_id = prepared.tab_id;
                let should_change_client_focus = prepared.should_change_client_focus;
                let client_id = prepared.client_id;
                let mut committed = match self.commit_apply_layout_state(prepared) {
                    Ok(committed) => committed,
                    Err(prepared) => {
                        self.indeterminate_layout_transactions.insert(
                            transaction_id,
                            IndeterminatePreparedLayout::Apply { prepared, plan },
                        );
                        self.layout_reconciliation_attempts
                            .entry(transaction_id)
                            .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                            .or_insert(1);
                        bail!(
                            "layout transaction {transaction_id} recovered a commit receipt but target tab {tab_id} disappeared before Screen reconciliation"
                        );
                    },
                };
                let cleanup = committed.effects.take_pending_cleanup();
                self.retain_layout_cleanup(transaction_id, cleanup);
                if let Some((_, mut completion)) = self.emit_committed_apply_layout(committed) {
                    completion.mark_failure(format!(
                        "layout transaction {transaction_id} committed during background reconciliation after its foreground completion had already reported an indeterminate outcome"
                    ));
                }

                let mut post_commit_error = self
                    .close_owned_tabs_after_layout_commit(transaction_id, &owner)
                    .err()
                    .map(|error| format!("{error:#}"));
                self.flush_layout_cleanup(transaction_id);
                let cleanup_decision = self.pending_layout_cleanup_message(transaction_id);
                let decision = post_commit_error.take().map_or_else(
                    || {
                        cleanup_decision.clone().map_or(
                            ScreenLayoutDecision::Committed,
                            ScreenLayoutDecision::CommittedWithCleanupDebt,
                        )
                    },
                    ScreenLayoutDecision::CommittedWithPostCommitError,
                );
                self.record_resolved_layout_transaction(
                    transaction_id,
                    &owner,
                    plan.resource_ids.clone(),
                    decision,
                );

                self.retire_layout_transaction_from_pending_gate(
                    transaction_id,
                    &owner,
                    pending_tab_ids,
                );
                if pending_tab_ids.is_empty() {
                    for (tab_index, pending_client_id) in pending_tab_switches.drain() {
                        self.go_to_tab(tab_index + 1, pending_client_id).non_fatal();
                    }
                    if should_change_client_focus
                        && let Some(tab_position) = self.get_tab_position_by_id(tab_id)
                    {
                        self.go_to_tab(tab_position + 1, client_id).non_fatal();
                    }
                } else if should_change_client_focus {
                    let client_id_to_switch = if self.active_tab_ids.contains_key(&client_id) {
                        Some(client_id)
                    } else {
                        self.active_tab_ids.keys().next().copied()
                    };
                    if let Some(client_id_to_switch) = client_id_to_switch
                        && let Some(tab_position) = self.get_tab_position_by_id(tab_id)
                    {
                        pending_tab_switches.insert((tab_position, client_id_to_switch));
                    }
                }

                for resource_id in &plan.resource_ids {
                    let PaneId::Plugin(plugin_id) = resource_id else {
                        continue;
                    };
                    if let Some(loading_indication) = plugin_loading_message_cache.remove(plugin_id)
                    {
                        self.update_plugin_loading_stage(*plugin_id, loading_indication);
                    }
                    self.render_blocker.register_blocking_plugin(*plugin_id);
                }
                for event in pending_events_waiting_for_client.drain(..) {
                    self.bus.senders.send_to_screen(event).non_fatal();
                }
                for event in pending_events_waiting_for_tab.drain(..) {
                    self.bus.senders.send_to_screen(event).non_fatal();
                }
                self.render(None).non_fatal();
                if let Some(os_input) = &mut self.bus.os_input {
                    for (connected_client_id, _) in self.connected_clients.borrow().iter() {
                        let _ = os_input.send_to_client(
                            *connected_client_id,
                            ServerToClientMsg::QueryTerminalSize,
                        );
                    }
                }
                self.active_layout_transactions.remove(&transaction_id);
                self.layout_reconciliation_attempts.remove(&transaction_id);
                self.log_and_report_session_state().non_fatal();
                log::info!(
                    "layout transaction {transaction_id} committed after exact background reconciliation"
                );
            },
            (
                LayoutCoordination::Commit,
                IndeterminatePreparedLayout::Override {
                    prepared_layouts,
                    created_tab_ids,
                    plan,
                },
            ) => match self.commit_override_layout_state(prepared_layouts) {
                CommittedOverrideLayout::Complete(mut committed_effects) => {
                    let mut cleanup = PendingTabLayoutCleanup::default();
                    for (_, effects) in &mut committed_effects {
                        cleanup.append(effects.take_pending_cleanup());
                    }
                    self.retain_layout_cleanup(transaction_id, cleanup);
                    let mut post_commit_error = None;
                    for (tab_id, effects) in committed_effects {
                        if let Some(tab) = self.tabs.get_mut(&tab_id) {
                            if let Some((_, mut completion)) = effects.emit(tab) {
                                let message = format!(
                                    "Override transaction {transaction_id} retained an unexpected blocking completion during background reconciliation"
                                );
                                completion.mark_failure(message.clone());
                                post_commit_error = Some(message);
                            }
                        } else {
                            post_commit_error = Some(format!(
                                "committed Override target tab {tab_id} disappeared before reconciled local effects"
                            ));
                        }
                    }
                    if let Err(error) =
                        self.close_owned_tabs_after_layout_commit(transaction_id, &owner)
                    {
                        post_commit_error = Some(format!("{error:#}"));
                    }
                    self.flush_layout_cleanup(transaction_id);
                    let cleanup_decision = self.pending_layout_cleanup_message(transaction_id);
                    let decision = post_commit_error.map_or_else(
                        || {
                            cleanup_decision.map_or(
                                ScreenLayoutDecision::Committed,
                                ScreenLayoutDecision::CommittedWithCleanupDebt,
                            )
                        },
                        ScreenLayoutDecision::CommittedWithPostCommitError,
                    );
                    self.record_resolved_layout_transaction(
                        transaction_id,
                        &owner,
                        plan.resource_ids.clone(),
                        decision,
                    );
                    self.retire_layout_transaction_from_pending_gate(
                        transaction_id,
                        &owner,
                        pending_tab_ids,
                    );
                    if pending_tab_ids.is_empty() {
                        for (tab_index, pending_client_id) in pending_tab_switches.drain() {
                            self.go_to_tab(tab_index + 1, pending_client_id).non_fatal();
                        }
                    }
                    for event in pending_events_waiting_for_client.drain(..) {
                        self.bus.senders.send_to_screen(event).non_fatal();
                    }
                    for event in pending_events_waiting_for_tab.drain(..) {
                        self.bus.senders.send_to_screen(event).non_fatal();
                    }
                    self.active_layout_transactions.remove(&transaction_id);
                    self.layout_reconciliation_attempts.remove(&transaction_id);
                    self.log_and_report_session_state().non_fatal();
                    self.render(None).non_fatal();
                    log::info!(
                        "Override transaction {transaction_id} committed after exact background reconciliation"
                    );
                },
                CommittedOverrideLayout::Indeterminate {
                    missing_tab_id,
                    mut committed_effects,
                    remaining_prepared,
                } => {
                    let mut cleanup = PendingTabLayoutCleanup::default();
                    for (_, effects) in &mut committed_effects {
                        cleanup.append(effects.take_pending_cleanup());
                    }
                    self.retain_layout_cleanup(transaction_id, cleanup);
                    for (tab_id, effects) in committed_effects {
                        if let Some(tab) = self.tabs.get_mut(&tab_id)
                            && let Some((_, mut completion)) = effects.emit(tab)
                        {
                            completion.mark_failure(format!(
                                "partially reconciled Override transaction {transaction_id} retained an unexpected blocking completion"
                            ));
                        }
                    }
                    self.flush_layout_cleanup(transaction_id);
                    self.indeterminate_layout_transactions.insert(
                        transaction_id,
                        IndeterminatePreparedLayout::Override {
                            prepared_layouts: remaining_prepared,
                            created_tab_ids,
                            plan,
                        },
                    );
                    self.layout_reconciliation_attempts
                        .entry(transaction_id)
                        .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                        .or_insert(1);
                    bail!(
                        "Override transaction {transaction_id} recovered a commit receipt but target tab {missing_tab_id} disappeared during Screen reconciliation"
                    );
                },
            },
            (
                LayoutCoordination::Rollback(message),
                IndeterminatePreparedLayout::Apply { prepared, plan },
            ) => {
                self.rollback_prepared_apply_layout(prepared, &message);
                for resource_id in &plan.resource_ids {
                    if let PaneId::Plugin(plugin_id) = resource_id {
                        plugin_loading_message_cache.remove(plugin_id);
                    }
                }
                remove_layout_resources_from_screen(self, &plan.resource_ids);
                self.record_resolved_layout_transaction(
                    transaction_id,
                    &owner,
                    plan.resource_ids.clone(),
                    ScreenLayoutDecision::Rejected(message.clone()),
                );
                if plan.close_fenced_tab_on_rejection {
                    if let Some(layout_generation) = plan.layout_generation.as_ref() {
                        close_globally_stale_fenced_tab(
                            self,
                            layout_generation,
                            &plan.resource_ids,
                        )
                        .non_fatal();
                        pending_tab_ids.remove(&layout_generation.tab_id);
                    }
                } else if !plan.preserve_pending_tab_on_rejection && owner_targets_are_current {
                    if owner.kind == ScreenLayoutTransactionKind::BreakPane {
                        if let Err(error) =
                            self.activate_degraded_break_tab(&owner, pending_tab_ids)
                        {
                            self.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                &owner,
                                pending_tab_ids,
                            );
                            log::error!(
                                "layout transaction {transaction_id} could not activate its degraded break-pane destination after reconciliation: {error:#}"
                            );
                        }
                    } else {
                        self.discard_owned_pending_tabs(&owner, pending_tab_ids);
                    }
                }
                self.retire_layout_transaction_from_pending_gate(
                    transaction_id,
                    &owner,
                    pending_tab_ids,
                );
                release_pending_layout_gate_if_ready(
                    self,
                    pending_tab_ids,
                    pending_tab_switches,
                    pending_events_waiting_for_client,
                    pending_events_waiting_for_tab,
                );
                self.active_layout_transactions.remove(&transaction_id);
                self.layout_reconciliation_attempts.remove(&transaction_id);
                self.log_and_report_session_state().non_fatal();
                self.render(None).non_fatal();
                log::warn!(
                    "layout transaction {transaction_id} rejected after exact background reconciliation: {message}"
                );
            },
            (
                LayoutCoordination::Rollback(message),
                IndeterminatePreparedLayout::Override {
                    prepared_layouts,
                    created_tab_ids,
                    plan,
                },
            ) => {
                for (tab_id, transaction) in prepared_layouts.into_iter().rev() {
                    if let Some(tab) = self.tabs.get_mut(&tab_id) {
                        transaction.rollback(tab, &message);
                    }
                }
                let excluded_pty_resource_ids = plan.resource_ids.iter().copied().collect();
                for created_tab_id in created_tab_ids.iter().rev() {
                    if self.tabs.contains_key(created_tab_id) {
                        self.close_tab_by_id_excluding_pty_resources(
                            *created_tab_id,
                            &excluded_pty_resource_ids,
                        )
                        .non_fatal();
                    }
                }
                for resource_id in &plan.resource_ids {
                    if let PaneId::Plugin(plugin_id) = resource_id {
                        plugin_loading_message_cache.remove(plugin_id);
                    }
                }
                remove_layout_resources_from_screen(self, &plan.resource_ids);
                self.record_resolved_layout_transaction(
                    transaction_id,
                    &owner,
                    plan.resource_ids.clone(),
                    ScreenLayoutDecision::Rejected(message.clone()),
                );
                if plan.close_fenced_tab_on_rejection {
                    if let Some(layout_generation) = plan.layout_generation.as_ref() {
                        close_globally_stale_fenced_tab(
                            self,
                            layout_generation,
                            &plan.resource_ids,
                        )
                        .non_fatal();
                        pending_tab_ids.remove(&layout_generation.tab_id);
                    }
                } else if !plan.preserve_pending_tab_on_rejection
                    && let Some(layout_generation) = plan.layout_generation.as_ref()
                    && durable_tab_layout_generation_is_current(
                        self,
                        durable_tab_layout_generations,
                        layout_generation,
                    )
                {
                    pending_tab_ids.remove(&layout_generation.tab_id);
                }
                self.retire_layout_transaction_from_pending_gate(
                    transaction_id,
                    &owner,
                    pending_tab_ids,
                );
                release_pending_layout_gate_if_ready(
                    self,
                    pending_tab_ids,
                    pending_tab_switches,
                    pending_events_waiting_for_client,
                    pending_events_waiting_for_tab,
                );
                self.active_layout_transactions.remove(&transaction_id);
                self.layout_reconciliation_attempts.remove(&transaction_id);
                self.log_and_report_session_state().non_fatal();
                self.render(None).non_fatal();
                log::warn!(
                    "Override transaction {transaction_id} rejected after exact background reconciliation: {message}"
                );
            },
            (
                LayoutCoordination::Rollback(message),
                IndeterminatePreparedLayout::ResolutionOnly {
                    target_tab_ids: _,
                    plan,
                },
            ) => {
                for resource_id in &plan.resource_ids {
                    if let PaneId::Plugin(plugin_id) = resource_id {
                        plugin_loading_message_cache.remove(plugin_id);
                    }
                }
                remove_layout_resources_from_screen(self, &plan.resource_ids);
                self.record_resolved_layout_transaction(
                    transaction_id,
                    &owner,
                    plan.resource_ids.clone(),
                    ScreenLayoutDecision::Rejected(message.clone()),
                );
                if plan.close_fenced_tab_on_rejection {
                    if let Some(layout_generation) = plan.layout_generation.as_ref() {
                        close_globally_stale_fenced_tab(
                            self,
                            layout_generation,
                            &plan.resource_ids,
                        )
                        .non_fatal();
                        pending_tab_ids.remove(&layout_generation.tab_id);
                    }
                } else if !plan.preserve_pending_tab_on_rejection && owner_targets_are_current {
                    if owner.kind == ScreenLayoutTransactionKind::BreakPane {
                        if let Err(error) =
                            self.activate_degraded_break_tab(&owner, pending_tab_ids)
                        {
                            self.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                &owner,
                                pending_tab_ids,
                            );
                            log::error!(
                                "layout transaction {transaction_id} could not activate its degraded break-pane destination after resolution-only reconciliation: {error:#}"
                            );
                        }
                    } else {
                        self.discard_owned_pending_tabs(&owner, pending_tab_ids);
                    }
                }
                self.retire_layout_transaction_from_pending_gate(
                    transaction_id,
                    &owner,
                    pending_tab_ids,
                );
                release_pending_layout_gate_if_ready(
                    self,
                    pending_tab_ids,
                    pending_tab_switches,
                    pending_events_waiting_for_client,
                    pending_events_waiting_for_tab,
                );
                self.active_layout_transactions.remove(&transaction_id);
                self.layout_reconciliation_attempts.remove(&transaction_id);
                self.log_and_report_session_state().non_fatal();
                self.render(None).non_fatal();
                log::warn!(
                    "layout transaction {transaction_id} rejected after resolution-only background reconciliation: {message}"
                );
            },
            (
                LayoutCoordination::Commit,
                indeterminate @ IndeterminatePreparedLayout::ResolutionOnly { .. },
            ) => {
                self.indeterminate_layout_transactions
                    .insert(transaction_id, indeterminate);
                self.layout_reconciliation_attempts
                    .entry(transaction_id)
                    .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                    .or_insert(1);
                bail!(
                    "layout transaction {transaction_id} returned an impossible commit decision for a rejection-only reconciliation"
                );
            },
            (LayoutCoordination::Unknown(message), indeterminate) => {
                self.indeterminate_layout_transactions
                    .insert(transaction_id, indeterminate);
                self.layout_reconciliation_attempts
                    .entry(transaction_id)
                    .and_modify(|attempts| *attempts = attempts.saturating_add(1))
                    .or_insert(1);
                bail!(
                    "layout transaction {transaction_id} remained indeterminate during Screen reconciliation: {message}"
                );
            },
        }
        Ok(())
    }

    fn pending_layout_cleanup_message(
        &self,
        transaction_id: LayoutTransactionId,
    ) -> Option<String> {
        let cleanup = self.pending_layout_cleanup.get(&transaction_id)?;
        Some(format!(
            "layout transaction {transaction_id} committed its Screen topology but exact worker cleanup ACK remains unresolved for {:?}; Screen retained every cleanup owner",
            cleanup.pane_ids()
        ))
    }

    fn record_resolved_layout_transaction(
        &mut self,
        transaction_id: LayoutTransactionId,
        owner: &ActiveLayoutTransaction,
        mut resource_ids: Vec<PaneId>,
        decision: ScreenLayoutDecision,
    ) {
        let mut target_ids = owner
            .targets
            .iter()
            .map(|target| target.tab_id)
            .collect::<Vec<_>>();
        target_ids.sort_unstable();
        target_ids.dedup();
        resource_ids.sort_unstable();
        resource_ids.dedup();
        let receipt = ResolvedLayoutTransaction {
            kind: owner.kind,
            target_ids,
            generation: owner.generation.clone(),
            resource_ids,
            decision,
        };
        if let Some(existing) = self.resolved_layout_transactions.get(&transaction_id) {
            if existing != &receipt {
                log::error!(
                    "refusing to overwrite conflicting Screen receipt for layout transaction {transaction_id}: existing={existing:?}, new={receipt:?}"
                );
            }
            return;
        }
        self.resolved_layout_transactions
            .insert(transaction_id, receipt);
        self.resolved_layout_transaction_order
            .push_back(transaction_id);
        while self.resolved_layout_transaction_order.len() > MAX_RESOLVED_LAYOUT_TRANSACTIONS {
            if let Some(expired_transaction_id) = self.resolved_layout_transaction_order.pop_front()
            {
                self.resolved_layout_transactions
                    .remove(&expired_transaction_id);
            }
        }
    }

    fn replay_resolved_layout_transaction(
        &self,
        transaction_id: LayoutTransactionId,
        allowed_kinds: &[ScreenLayoutTransactionKind],
        target_ids: &[usize],
        generation: Option<&DurableTabLayoutGeneration>,
        resource_ids: &[PaneId],
    ) -> Option<std::result::Result<ScreenLayoutDecision, String>> {
        let receipt = self.resolved_layout_transactions.get(&transaction_id)?;
        let mut actual_target_ids = target_ids.to_vec();
        actual_target_ids.sort_unstable();
        actual_target_ids.dedup();
        let mut actual_resource_ids = resource_ids.to_vec();
        actual_resource_ids.sort_unstable();
        actual_resource_ids.dedup();
        if target_ids.len() != actual_target_ids.len()
            || resource_ids.len() != actual_resource_ids.len()
            || !allowed_kinds.contains(&receipt.kind)
            || receipt.target_ids != actual_target_ids
            || receipt.generation.as_ref() != generation
            || receipt.resource_ids != actual_resource_ids
        {
            return Some(Err(format!(
                "conflicting replay for resolved layout transaction {transaction_id}: receipt={receipt:?}, targets={actual_target_ids:?}, resources={actual_resource_ids:?}"
            )));
        }
        Some(Ok(receipt.decision.clone()))
    }

    fn register_layout_transaction(
        &mut self,
        transaction_id: LayoutTransactionId,
        transaction: ActiveLayoutTransaction,
    ) -> Result<()> {
        if transaction_id == 0 {
            bail!("layout transaction id 0 is reserved");
        }
        let target_id_list = transaction
            .targets
            .iter()
            .map(|target| target.tab_id)
            .collect::<Vec<_>>();
        let target_ids = target_id_list.iter().copied().collect::<HashSet<_>>();
        if target_ids.is_empty() {
            bail!("layout transaction {transaction_id} has no target tabs");
        }
        if target_ids.len() != target_id_list.len() {
            bail!(
                "layout transaction {transaction_id} contains duplicate target tabs: {target_id_list:?}"
            );
        }
        let mut render_fenced_tab_ids = transaction
            .render_fenced_tabs
            .iter()
            .map(|owner| owner.tab_id)
            .collect::<Vec<_>>();
        let render_fenced_tab_count = render_fenced_tab_ids.len();
        render_fenced_tab_ids.sort_unstable();
        render_fenced_tab_ids.dedup();
        if render_fenced_tab_ids.len() != render_fenced_tab_count {
            bail!(
                "layout transaction {transaction_id} contains duplicate render-fenced tabs: {render_fenced_tab_ids:?}"
            );
        }
        if !transaction.exact_render_fences_are_current(self) {
            bail!(
                "layout transaction {transaction_id} cannot install a stale render fence for tabs {render_fenced_tab_ids:?}"
            );
        }
        if let Some((blocking_transaction_id, blocked_tab_id)) = self
            .active_layout_transactions
            .iter()
            .find_map(|(active_transaction_id, active_transaction)| {
                let new_gate_ids = target_ids
                    .iter()
                    .copied()
                    .chain(render_fenced_tab_ids.iter().copied())
                    .collect::<HashSet<_>>();
                active_transaction
                    .render_fenced_tabs
                    .iter()
                    .find(|owner| new_gate_ids.contains(&owner.tab_id) && owner.is_current(self))
                    .map(|owner| (*active_transaction_id, owner.tab_id))
                    .or_else(|| {
                        active_transaction
                            .targets
                            .iter()
                            .find(|owner| {
                                render_fenced_tab_ids.contains(&owner.tab_id)
                                    && owner.is_current(self)
                            })
                            .map(|owner| (*active_transaction_id, owner.tab_id))
                    })
            })
        {
            bail!(
                "layout transaction {transaction_id} cannot fence tab {blocked_tab_id} while active transaction {blocking_transaction_id} owns its topology"
            );
        }
        if let Some((indeterminate_id, blocked_tab_id)) = self
            .indeterminate_layout_transactions
            .iter()
            .find_map(|(indeterminate_id, prepared)| {
                prepared
                    .target_tab_ids()
                    .into_iter()
                    .find(|tab_id| target_ids.contains(tab_id))
                    .map(|tab_id| (*indeterminate_id, tab_id))
            })
        {
            bail!(
                "layout transaction {transaction_id} cannot target tab {blocked_tab_id} while indeterminate transaction {indeterminate_id} still owns its prepared topology"
            );
        }
        if self
            .active_layout_transactions
            .contains_key(&transaction_id)
            || self
                .resolved_layout_transactions
                .contains_key(&transaction_id)
            || self
                .indeterminate_layout_transactions
                .contains_key(&transaction_id)
            || self.pending_layout_cleanup.contains_key(&transaction_id)
        {
            bail!("duplicate Screen layout transaction id {transaction_id}");
        }
        self.active_layout_transactions
            .insert(transaction_id, transaction);
        Ok(())
    }

    fn ensure_render_fence_tabs_are_available(&self, tab_ids: &[usize]) -> Result<()> {
        if let Some((blocking_transaction_id, blocked_tab_id)) = self
            .active_layout_transactions
            .iter()
            .find_map(|(transaction_id, transaction)| {
                transaction
                    .pending_gate_owners()
                    .find(|owner| tab_ids.contains(&owner.tab_id) && owner.is_current(self))
                    .map(|owner| (*transaction_id, owner.tab_id))
            })
        {
            bail!(
                "cannot move panes out of tab {blocked_tab_id} while layout transaction {blocking_transaction_id} owns its topology"
            );
        }
        Ok(())
    }

    fn rollback_break_source_transactions(
        &mut self,
        source_transactions: BTreeMap<usize, TabTopologyTransaction>,
        recovered_panes: Vec<ExtractedBreakPane>,
        destination_tab: &mut Tab,
    ) -> Vec<String> {
        let mut panes_by_source: BTreeMap<usize, BTreeMap<PaneId, Box<dyn Pane>>> = BTreeMap::new();
        for recovered in recovered_panes {
            panes_by_source
                .entry(recovered.source_tab_id)
                .or_default()
                .insert(recovered.pane.pid(), recovered.pane);
        }

        let mut failures = vec![];
        for (source_tab_id, transaction) in source_transactions {
            let recovered = panes_by_source.remove(&source_tab_id).unwrap_or_default();
            if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
                transaction.rollback(source_tab, recovered);
            } else {
                failures.push(format!(
                    "source tab {source_tab_id} disappeared during topology rollback"
                ));
                for (pane_id, pane) in recovered {
                    destination_tab.restore_extracted_pane(
                        pane,
                        pane_id,
                        false,
                        PaneGeom::from(&self.size),
                    );
                    if let Some(pane) = destination_tab.get_pane_with_id_mut(pane_id) {
                        pane.commit_layout_transaction();
                    }
                }
            }
        }
        for (source_tab_id, recovered) in panes_by_source {
            failures.push(format!(
                "source tab {source_tab_id} had recovered panes without a topology transaction"
            ));
            for (pane_id, pane) in recovered {
                destination_tab.restore_extracted_pane(
                    pane,
                    pane_id,
                    false,
                    PaneGeom::from(&self.size),
                );
                if let Some(pane) = destination_tab.get_pane_with_id_mut(pane_id) {
                    pane.commit_layout_transaction();
                }
            }
        }
        failures
    }

    fn validate_layout_transaction(
        &self,
        transaction_id: LayoutTransactionId,
        allowed_kinds: &[ScreenLayoutTransactionKind],
        target_ids: &[usize],
        generation: Option<&DurableTabLayoutGeneration>,
    ) -> Result<ActiveLayoutTransaction> {
        let transaction = self
            .active_layout_transactions
            .get(&transaction_id)
            .with_context(|| {
                format!("unknown or already resolved layout transaction {transaction_id}")
            })?;
        if !allowed_kinds.contains(&transaction.kind) {
            bail!(
                "layout transaction {transaction_id} has owner kind {:?}, expected one of {:?}",
                transaction.kind,
                allowed_kinds
            );
        }
        if !transaction.target_ids_match(target_ids) {
            bail!(
                "layout transaction {transaction_id} returned target IDs {:?}, owner expects {:?}",
                target_ids,
                transaction
                    .targets
                    .iter()
                    .map(|target| target.tab_id)
                    .collect::<Vec<_>>()
            );
        }
        if !transaction.generation_matches(generation) {
            bail!("layout transaction {transaction_id} returned a mismatched durable generation");
        }
        if !transaction.exact_targets_are_current(self) {
            bail!(
                "layout transaction {transaction_id} no longer owns the exact target tab incarnation"
            );
        }
        if let Some(changed_owner) = transaction
            .tabs_to_close_after_commit
            .iter()
            .find(|owner| !owner.is_current_or_absent(self))
        {
            bail!(
                "layout transaction {transaction_id} omitted tab {} changed incarnation before activation",
                changed_owner.tab_id
            );
        }
        Ok(transaction.clone())
    }

    #[cfg(test)]
    fn resolve_legacy_test_layout_transaction_id(
        &self,
        transaction_id: LayoutTransactionId,
        allowed_kinds: &[ScreenLayoutTransactionKind],
        target_ids: &[usize],
    ) -> LayoutTransactionId {
        if transaction_id != 0 {
            return transaction_id;
        }
        let mut matches =
            self.active_layout_transactions
                .iter()
                .filter_map(|(candidate_id, transaction)| {
                    let kinds_match = allowed_kinds.contains(&transaction.kind);
                    let targets_match = target_ids.iter().all(|target_id| {
                        transaction
                            .targets
                            .iter()
                            .any(|target| target.tab_id == *target_id)
                    });
                    (kinds_match && targets_match).then_some(*candidate_id)
                });
        let Some(candidate_id) = matches.next() else {
            return transaction_id;
        };
        if matches.next().is_some() {
            return transaction_id;
        }
        candidate_id
    }

    fn discard_owned_pending_tabs(
        &mut self,
        transaction: &ActiveLayoutTransaction,
        pending_tab_ids: &mut HashSet<usize>,
    ) {
        if !transaction.moved_original_panes.is_empty() {
            log::error!(
                "refusing to discard layout transaction {:?}: pending tab owns moved original panes {:?}",
                transaction.kind,
                transaction.moved_original_panes
            );
            return;
        }
        for owner in &transaction.created_pending_tabs {
            if !owner.is_current(self) {
                continue;
            }
            if self
                .tabs
                .get(&owner.tab_id)
                .is_some_and(|tab| tab.is_pending())
            {
                self.discard_pending_tab_after_layout_rejection(owner.tab_id)
                    .non_fatal();
                pending_tab_ids.remove(&owner.tab_id);
            }
        }
    }

    fn activate_degraded_break_tab(
        &mut self,
        transaction: &ActiveLayoutTransaction,
        pending_tab_ids: &mut HashSet<usize>,
    ) -> Result<()> {
        if transaction.kind != ScreenLayoutTransactionKind::BreakPane {
            bail!(
                "layout transaction {:?} is not a break-pane transaction",
                transaction.kind
            );
        }
        let [owner] = transaction.created_pending_tabs.as_slice() else {
            bail!(
                "break-pane transaction must own exactly one pending destination tab, found {}",
                transaction.created_pending_tabs.len()
            );
        };
        if !owner.is_current(self) {
            bail!(
                "break-pane destination tab {} changed incarnation before degraded activation",
                owner.tab_id
            );
        }
        let tab = self.tabs.get_mut(&owner.tab_id).with_context(|| {
            format!(
                "break-pane destination tab {} disappeared before degraded activation",
                owner.tab_id
            )
        })?;
        if !tab.is_pending() {
            bail!(
                "break-pane destination tab {} was already committed before degraded activation",
                owner.tab_id
            );
        }
        for pane_id in &transaction.moved_original_panes {
            if !tab.has_pane_with_pid(pane_id) {
                bail!(
                    "break-pane destination tab {} lost moved original pane {:?}",
                    owner.tab_id,
                    pane_id
                );
            }
        }
        tab.activate_degraded_pending_layout()?;
        pending_tab_ids.remove(&owner.tab_id);
        Ok(())
    }

    fn retire_layout_transaction_from_pending_gate(
        &self,
        transaction_id: LayoutTransactionId,
        transaction: &ActiveLayoutTransaction,
        pending_tab_ids: &mut HashSet<usize>,
    ) {
        let mut owned_tab_ids = transaction
            .pending_gate_owners()
            .map(|owner| owner.tab_id)
            .collect::<Vec<_>>();
        owned_tab_ids.sort_unstable();
        owned_tab_ids.dedup();
        for tab_id in owned_tab_ids {
            if !self.tab_has_other_active_layout_owner(transaction_id, tab_id) {
                pending_tab_ids.remove(&tab_id);
            }
        }
    }

    fn close_owned_tabs_after_layout_commit(
        &mut self,
        transaction_id: LayoutTransactionId,
        transaction: &ActiveLayoutTransaction,
    ) -> Result<()> {
        let mut failures = vec![];
        for owner in &transaction.tabs_to_close_after_commit {
            if owner.is_current(self) {
                if let Err(error) = self.close_tab_by_id_excluding_pty_resources_for_transaction(
                    owner.tab_id,
                    &HashSet::new(),
                    transaction_id,
                ) {
                    failures.push(format!("omitted tab {}: {error:#}", owner.tab_id));
                }
            } else if !self.tabs.contains_key(&owner.tab_id) {
                // Another already-committed action reached the same desired
                // absence. This is idempotent, not an ownership mismatch.
            } else {
                failures.push(format!(
                    "omitted tab {} changed incarnation after activation",
                    owner.tab_id
                ));
            }
        }
        if !failures.is_empty() {
            bail!(
                "layout transaction {transaction_id} could not close every omitted tab after retaining all reachable cleanup owners: {}",
                failures.join("; ")
            );
        }
        Ok(())
    }

    fn tab_has_other_active_layout_owner(
        &self,
        transaction_id: LayoutTransactionId,
        tab_id: usize,
    ) -> bool {
        self.active_layout_transactions
            .iter()
            .any(|(other_id, transaction)| {
                *other_id != transaction_id
                    && transaction
                        .pending_gate_owners()
                        .any(|target| target.tab_id == tab_id && target.is_current(self))
            })
    }

    fn assign_stable_tab_ids_to_layout(&mut self, tab_layouts: &mut [TabLayoutInfo]) -> Result<()> {
        let existing_id_by_position: HashMap<usize, usize> = self
            .tabs
            .values()
            .map(|tab| (tab.position, tab.id))
            .collect();
        let mut requested_positions = HashSet::new();

        for tab_layout in tab_layouts {
            let requested_position = tab_layout.tab_index;
            if !requested_positions.insert(requested_position) {
                return Err(anyhow!(
                    "override layout contains duplicate tab position {}",
                    requested_position
                ));
            }
            tab_layout.tab_index = existing_id_by_position
                .get(&requested_position)
                .copied()
                .unwrap_or_else(|| self.get_new_tab_id());
        }
        Ok(())
    }

    /// Gets a tab by its stable ID (BTreeMap key).
    ///
    /// Use this when you have a tab ID from active_tab_ids, tab_history, or tab.id.
    fn get_tab_by_id(&self, id: usize) -> Option<&Tab> {
        self.tabs.get(&id)
    }

    /// Gets a mutable tab by its stable ID (BTreeMap key).
    fn get_tab_by_id_mut(&mut self, id: usize) -> Option<&mut Tab> {
        self.tabs.get_mut(&id)
    }

    fn reusable_tab_id_for_instance(
        &self,
        instance_id: &str,
        requested_name: &str,
    ) -> Result<Option<usize>, String> {
        let tab_ids = self
            .tabs
            .values()
            .filter(|tab| tab.instance_id.eq_ignore_ascii_case(instance_id))
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        match tab_ids.as_slice() {
            [] => Ok(None),
            [tab_id] => {
                let existing_name = self
                    .tabs
                    .get(tab_id)
                    .map(|tab| tab.name.as_str())
                    .unwrap_or_default();
                if existing_name == requested_name {
                    Ok(Some(*tab_id))
                } else {
                    Err(format!(
                        "durable tab instance {} already belongs to tab '{}' instead of '{}'",
                        instance_id, existing_name, requested_name
                    ))
                }
            },
            duplicate_tab_ids => Err(format!(
                "durable tab instance {} is ambiguous across tabs {:?}",
                instance_id, duplicate_tab_ids
            )),
        }
    }

    /// Gets a mutable tab by its display position (0-based).
    fn get_tab_by_position_mut(&mut self, position: usize) -> Option<&mut Tab> {
        self.tabs.values_mut().find(|t| t.position == position)
    }

    /// Gets the stable ID of the tab at the given display position.
    ///
    /// Use this to convert position → ID for BTreeMap lookups.
    fn get_tab_id_at_position(&self, position: usize) -> Option<usize> {
        self.tabs
            .values()
            .find(|t| t.position == position)
            .map(|t| t.id)
    }

    /// Gets the display position of the tab with the given ID.
    ///
    /// Use this to convert ID → position for user-facing operations.
    fn get_tab_position_by_id(&self, id: usize) -> Option<usize> {
        self.tabs.get(&id).map(|t| t.position)
    }

    fn move_clients_from_closed_tab(
        &mut self,
        client_ids_and_mode_infos: Vec<(ClientId, ModeInfo)>,
    ) -> Result<()> {
        let err_context = || "failed to move clients from closed tab".to_string();

        if self.tabs.is_empty() {
            Err::<(), _>(anyhow!(
                "No tabs left, cannot move clients: {:?} from closed tab",
                client_ids_and_mode_infos
            ))
            .with_context(err_context)
            .non_fatal();

            return Ok(());
        }
        let first_tab_index = *self
            .tabs
            .keys()
            .next()
            .context("screen contained no tabs")
            .with_context(err_context)?;
        for (client_id, client_mode_info) in client_ids_and_mode_infos {
            let client_tab_history = self.tab_history.entry(client_id).or_default();
            if let Some(client_previous_tab) = client_tab_history.pop()
                && let Some(client_active_tab) = self.tabs.get_mut(&client_previous_tab)
            {
                self.active_tab_ids.insert(client_id, client_previous_tab);
                client_active_tab
                    .add_client(client_id, Some(client_mode_info))
                    .with_context(err_context)?;
                continue;
            }
            self.active_tab_ids.insert(client_id, first_tab_index);
            self.tabs
                .get_mut(&first_tab_index)
                .with_context(err_context)?
                .add_client(client_id, Some(client_mode_info))
                .with_context(err_context)?;
        }
        Ok(())
    }

    fn move_suppressed_panes_from_closed_tab(
        &mut self,
        suppressed_panes: SuppressedPanes,
    ) -> std::result::Result<(), SuppressedPanes> {
        // TODO: this is not entirely accurate, these also sometimes contain a pane who's
        // scrollback is being edited - in this case we need to close it or to move it to the
        // appropriate tab
        let Some(first_tab_index) = self.tabs.keys().next().copied() else {
            return Err(suppressed_panes);
        };
        let Some(destination) = self.tabs.get_mut(&first_tab_index) else {
            return Err(suppressed_panes);
        };
        destination.add_suppressed_panes(suppressed_panes);
        Ok(())
    }

    fn move_clients_between_tabs(
        &mut self,
        source_tab_index: usize,
        destination_tab_index: usize,
        update_mode_infos: bool,
        clients_to_move: Option<Vec<ClientId>>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to move clients from tab {source_tab_index} to tab {destination_tab_index}"
            )
        };

        // None ==> move all clients
        let drained_clients = self
            .get_indexed_tab_mut(source_tab_index)
            .map(|t| t.drain_connected_clients(clients_to_move));

        if let Some(client_mode_info_in_source_tab) = drained_clients {
            let destination_tab = self
                .get_indexed_tab_mut(destination_tab_index)
                .context("failed to get destination tab by index")
                .with_context(err_context)?;
            destination_tab
                .add_multiple_clients(client_mode_info_in_source_tab)
                .with_context(err_context)?;
            if update_mode_infos {
                destination_tab
                    .update_input_modes()
                    .with_context(err_context)?;
            }
            destination_tab.set_force_render();
            destination_tab.visible(true).with_context(err_context)?;
        }
        Ok(())
    }

    fn update_client_tab_focus(&mut self, client_id: ClientId, new_tab_index: usize) {
        match self.active_tab_ids.remove(&client_id) {
            Some(old_active_index) => {
                self.active_tab_ids.insert(client_id, new_tab_index);
                let client_tab_history = self.tab_history.entry(client_id).or_default();
                client_tab_history.retain(|&e| e != new_tab_index);
                client_tab_history.push(old_active_index);
            },
            None => {
                self.active_tab_ids.insert(client_id, new_tab_index);
            },
        }
        self.clear_bell_for_focused_pane(client_id);
    }

    /// A helper function to switch to a new tab at specified position.
    fn switch_active_tab(
        &mut self,
        new_tab_pos: usize,
        should_change_pane_focus: Option<Direction>,
        update_mode_infos: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "Failed to switch to active tab at position {new_tab_pos} for client id: {client_id:?}"
            )
        };

        if let Some(new_tab) = self.tabs.values().find(|t| t.position == new_tab_pos) {
            match self.get_active_tab(client_id) {
                Ok(current_tab) => {
                    // If new active tab is same as the current one, do nothing.
                    if current_tab.position == new_tab_pos {
                        return Ok(());
                    }

                    let current_tab_index = current_tab.id;
                    let new_tab_index = new_tab.id;
                    if self.session_is_mirrored {
                        self.move_clients_between_tabs(
                            current_tab_index,
                            new_tab_index,
                            update_mode_infos,
                            None,
                        )
                        .with_context(err_context)?;
                        let all_connected_clients: Vec<ClientId> =
                            self.connected_clients.borrow().keys().copied().collect();
                        for client_id in all_connected_clients {
                            self.update_client_tab_focus(client_id, new_tab_index);
                            if let (Some(direction), Some(new_tab)) = (
                                should_change_pane_focus,
                                self.get_indexed_tab_mut(new_tab_index),
                            ) {
                                new_tab.focus_pane_on_edge(direction, client_id);
                            }
                        }
                    } else {
                        self.move_clients_between_tabs(
                            current_tab_index,
                            new_tab_index,
                            update_mode_infos,
                            Some(vec![client_id]),
                        )
                        .with_context(err_context)?;
                        if let (Some(direction), Some(new_tab)) = (
                            should_change_pane_focus,
                            self.get_indexed_tab_mut(new_tab_index),
                        ) {
                            new_tab.focus_pane_on_edge(direction, client_id);
                        }
                        self.update_client_tab_focus(client_id, new_tab_index);
                    }

                    if let Some(current_tab) = self.get_indexed_tab_mut(current_tab_index) {
                        if current_tab.has_no_connected_clients() {
                            current_tab.visible(false).with_context(err_context)?;
                        }
                    } else {
                        Err::<(), _>(anyhow!("Tab index {:?} not found", current_tab_index))
                            .with_context(err_context)
                            .non_fatal();
                    }

                    // Clear tab bell notification for the newly active tab
                    if let Some(new_tab) = self.tabs.get_mut(&new_tab_index) {
                        let tab_id = new_tab.id;
                        new_tab.clear_tab_bell_notification();
                        let _ = self
                            .bus
                            .senders
                            .send_to_background_jobs(BackgroundJob::StopFlashTabBell(tab_id));
                    }

                    // Both the source and destination tabs may have changed
                    // their viewer sets; per-tab sizing is independent so each
                    // is recomputed against its current viewers.
                    self.recompute_tab_size(current_tab_index)
                        .with_context(err_context)?;
                    self.recompute_tab_size(new_tab_index)
                        .with_context(err_context)?;

                    self.log_and_report_session_state()
                        .with_context(err_context)?;
                    return self.render(None).with_context(err_context);
                },
                Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
            }
        }
        Ok(())
    }

    /// A helper function to switch to a new tab with specified name. Return true if tab [name] has
    /// been created, else false.
    fn switch_active_tab_name(&mut self, name: String, client_id: ClientId) -> Result<bool> {
        match self.tabs.values().find(|t| t.name == name) {
            Some(new_tab) => {
                self.switch_active_tab(new_tab.position, None, true, client_id)?;
                Ok(true)
            },
            None => Ok(false),
        }
    }

    /// Sets this [`Screen`]'s active [`Tab`] to the next tab.
    pub fn switch_tab_next(
        &mut self,
        should_change_pane_focus: Option<Direction>,
        update_mode_infos: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let err_context = || format!("failed to switch to next tab for client {client_id}");

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };

        if let Some(client_id) = client_id {
            match self.get_active_tab(client_id) {
                Ok(active_tab) => {
                    let active_tab_pos = active_tab.position;
                    let new_tab_pos = (active_tab_pos + 1) % self.tabs.len();
                    return self.switch_active_tab(
                        new_tab_pos,
                        should_change_pane_focus,
                        update_mode_infos,
                        client_id,
                    );
                },
                Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
            }
        }
        Ok(())
    }

    /// Sets this [`Screen`]'s active [`Tab`] to the previous tab.
    pub fn switch_tab_prev(
        &mut self,
        should_change_pane_focus: Option<Direction>,
        update_mode_infos: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let err_context = || format!("failed to switch to previous tab for client {client_id}");

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };

        if let Some(client_id) = client_id {
            match self.get_active_tab(client_id) {
                Ok(active_tab) => {
                    let active_tab_pos = active_tab.position;
                    let new_tab_pos = if active_tab_pos == 0 {
                        self.tabs.len() - 1
                    } else {
                        active_tab_pos - 1
                    };

                    return self.switch_active_tab(
                        new_tab_pos,
                        should_change_pane_focus,
                        update_mode_infos,
                        client_id,
                    );
                },
                Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
            }
        }
        Ok(())
    }

    pub fn go_to_tab(&mut self, tab_index: usize, client_id: ClientId) -> Result<()> {
        self.switch_active_tab(tab_index.saturating_sub(1), None, true, client_id)
    }

    pub fn go_to_tab_name(&mut self, name: String, client_id: ClientId) -> Result<bool> {
        self.switch_active_tab_name(name, client_id)
    }

    fn close_tab_by_id(&mut self, tab_id: usize) -> Result<()> {
        let cleanup_transaction_id = self.reserve_layout_transaction_id();
        let result = self.close_tab_by_id_excluding_pty_resources_for_transaction(
            tab_id,
            &HashSet::new(),
            cleanup_transaction_id,
        );
        self.flush_layout_cleanup(cleanup_transaction_id);
        result.map(|_| ()).and_then(|_| {
            if let Some(message) = self.pending_layout_cleanup_message(cleanup_transaction_id) {
                bail!("{message}");
            }
            Ok(())
        })
    }

    fn close_tab_by_id_excluding_pty_resources(
        &mut self,
        tab_id: usize,
        excluded_pty_resource_ids: &HashSet<PaneId>,
    ) -> Result<Vec<PaneId>> {
        let cleanup_transaction_id = self.reserve_layout_transaction_id();
        let result = self.close_tab_by_id_excluding_pty_resources_for_transaction(
            tab_id,
            excluded_pty_resource_ids,
            cleanup_transaction_id,
        );
        self.flush_layout_cleanup(cleanup_transaction_id);
        result.and_then(|pane_ids| {
            if let Some(message) = self.pending_layout_cleanup_message(cleanup_transaction_id) {
                bail!("{message}");
            }
            Ok(pane_ids)
        })
    }

    fn close_tab_by_id_excluding_pty_resources_for_transaction(
        &mut self,
        tab_id: usize,
        excluded_pty_resource_ids: &HashSet<PaneId>,
        cleanup_transaction_id: LayoutTransactionId,
    ) -> Result<Vec<PaneId>> {
        let err_context = || format!("failed to close tab at index {tab_id:?}");
        if let Some(blocking_transaction_id) =
            self.indeterminate_layout_transactions
                .keys()
                .find(|transaction_id| {
                    **transaction_id != cleanup_transaction_id
                        && self
                            .active_layout_transactions
                            .get(transaction_id)
                            .is_some_and(|transaction| {
                                transaction.targets.iter().any(|target| {
                                    target.tab_id == tab_id && target.is_current(self)
                                })
                            })
                })
        {
            bail!(
                "refusing to close tab {tab_id} while indeterminate layout transaction {blocking_transaction_id} owns its exact incarnation"
            );
        }

        let mut tab_to_close = self.tabs.remove(&tab_id).with_context(err_context)?;
        let mut pane_ids = tab_to_close.get_all_pane_ids();

        // here we extract the suppressed panes (these are background panes that don't care which
        // tab they are in, and in the future we should probably make them global to screen rather
        // than to each tab) and move them to another tab if there is one
        let suppressed_panes = tab_to_close.extract_suppressed_panes();
        let suppressed_runtime_ids = suppressed_panes
            .values()
            .map(|(_, pane)| pane.pid())
            .collect::<HashSet<_>>();
        let mut suppressed_transfer_error = None;
        let suppressed_panes_to_cleanup = if self.tabs.is_empty() {
            suppressed_panes
        } else {
            match self.move_suppressed_panes_from_closed_tab(suppressed_panes) {
                Ok(()) => {
                    pane_ids.retain(|pane_id| !suppressed_runtime_ids.contains(pane_id));
                    SuppressedPanes::new()
                },
                Err(suppressed_panes) => {
                    suppressed_transfer_error =
                        Some("failed to transfer suppressed panes to the remaining tab".to_owned());
                    suppressed_panes
                },
            }
        };
        let cleanup_pane_ids = pane_ids
            .iter()
            .copied()
            .filter(|pane_id| !excluded_pty_resource_ids.contains(pane_id))
            .collect::<Vec<_>>();
        let mut owned_panes = tab_to_close.take_panes_for_cleanup();
        for (_, (_, pane)) in suppressed_panes_to_cleanup {
            let pane_id = pane.pid();
            if !excluded_pty_resource_ids.contains(&pane_id) {
                owned_panes.entry(pane_id).or_insert(pane);
            }
        }
        for excluded_resource_id in excluded_pty_resource_ids {
            owned_panes.remove(excluded_resource_id);
        }
        self.retain_layout_cleanup(
            cleanup_transaction_id,
            PendingTabLayoutCleanup::from_owned_panes(cleanup_pane_ids, owned_panes),
        );

        let _ = self.bus.senders.send_to_plugin(PluginInstruction::Update(
            pane_ids
                .iter()
                .copied()
                .map(|p_id| (None, None, Event::PaneClosed(p_id.into())))
                .collect(),
        ));

        // Notify pane render subscribers of each closed pane
        for p_id in &pane_ids {
            self.notify_pane_closed_to_subscribers((*p_id).into());
        }

        if self.tabs.is_empty() {
            self.active_tab_ids.clear();
            self.bus
                .senders
                .send_to_server(ServerInstruction::Render(None))
                .with_context(err_context)?;
        } else {
            let client_mode_infos_in_closed_tab = tab_to_close.drain_connected_clients(None);
            self.move_clients_from_closed_tab(client_mode_infos_in_closed_tab)
                .with_context(err_context)?;
            let visible_tab_indices: HashSet<usize> =
                self.active_tab_ids.values().copied().collect();
            for t in self.tabs.values_mut() {
                if visible_tab_indices.contains(&t.id) {
                    t.set_force_render();
                    t.visible(true).with_context(err_context)?;
                }
                if t.position > tab_to_close.position {
                    t.position -= 1;
                }
            }
            self.log_and_report_session_state()
                .with_context(err_context)?;
            self.render(None).with_context(err_context)?;
        }
        if let Some(message) = suppressed_transfer_error {
            bail!("{message}");
        }
        Ok(pane_ids)
    }

    fn tab_by_expected_identity(
        &self,
        tab_id: usize,
        expected_name: &str,
        expected_session_incarnation: &str,
        expected_tab_instance_id: &str,
    ) -> Result<&Tab> {
        if self.session_incarnation != expected_session_incarnation {
            return Err(anyhow!(
                "refusing to close tab ID {}: expected session incarnation {:?}, current {:?}",
                tab_id,
                expected_session_incarnation,
                self.session_incarnation
            ));
        }
        let tab = self
            .get_tab_by_id(tab_id)
            .ok_or_else(|| anyhow!("failed to find tab with ID: {}", tab_id))?;
        if tab.instance_id != expected_tab_instance_id {
            return Err(anyhow!(
                "refusing to close tab ID {}: expected tab instance {:?}, found {:?}",
                tab_id,
                expected_tab_instance_id,
                tab.instance_id
            ));
        }
        if tab.name != expected_name {
            return Err(anyhow!(
                "refusing to close tab ID {}: expected name {:?}, found {:?}",
                tab_id,
                expected_name,
                tab.name
            ));
        }
        Ok(tab)
    }

    fn close_tab_by_id_if_name(
        &mut self,
        tab_id: usize,
        expected_name: &str,
        expected_session_incarnation: &str,
        expected_tab_instance_id: &str,
    ) -> Result<()> {
        self.tab_by_expected_identity(
            tab_id,
            expected_name,
            expected_session_incarnation,
            expected_tab_instance_id,
        )?;
        self.close_tab_by_id(tab_id)
    }

    fn close_tab_by_id_if_name_if_quiescent(
        &mut self,
        tab_id: usize,
        expected_name: &str,
        expected_session_incarnation: &str,
        expected_tab_instance_id: &str,
    ) -> Result<()> {
        {
            let tab = self.tab_by_expected_identity(
                tab_id,
                expected_name,
                expected_session_incarnation,
                expected_tab_instance_id,
            )?;
            if let Some(client_id) =
                self.active_tab_ids
                    .iter()
                    .find_map(|(client_id, active_tab_id)| {
                        (*active_tab_id == tab_id).then_some(*client_id)
                    })
            {
                return Err(anyhow!(
                    "GC-safe close refused for tab ID {}: tab is active for client {}",
                    tab_id,
                    client_id
                ));
            }
            tab.ensure_viewer_gc_quiescent()?;
        }
        self.close_tab_by_id(tab_id)
    }

    // Closes the client_id's focused tab
    pub fn close_tab(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || format!("failed to close tab for client {client_id:?}");

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };

        match client_id {
            Some(client_id) => {
                let active_tab_index = *self
                    .active_tab_ids
                    .get(&client_id)
                    .with_context(err_context)?;
                self.close_tab_by_id(active_tab_index)
                    .with_context(err_context)
            },
            None => Ok(()),
        }
    }

    pub fn resize_to_screen(&mut self, new_screen_size: Size) -> Result<()> {
        let err_context = || format!("failed to resize to screen size: {new_screen_size:#?}");

        if self.size != new_screen_size {
            self.size = new_screen_size;
            for tab in self.tabs.values_mut() {
                tab.resize_whole_tab(new_screen_size)
                    .with_context(err_context)?;
                tab.set_force_render();
            }
            self.log_and_report_session_state()
                .with_context(err_context)?;
            self.render(None).with_context(err_context)
        } else {
            Ok(())
        }
    }

    /// Record the viewport size most recently reported by `client_id`. Used as
    /// input to per-tab size computation; does not by itself trigger a resize.
    pub fn set_client_size(&mut self, client_id: ClientId, size: Size) {
        self.client_sizes.insert(client_id, size);
    }

    /// Recompute the size of `tab_id` from the viewports of every client whose
    /// `active_tab_ids` entry equals `tab_id`. `rows` and `cols` are sorted
    /// independently — each axis takes the minimum across viewers. If the tab
    /// has no viewers the size is left untouched (the tab retains its most
    /// recent viewer-derived dimensions). When the computed size differs from
    /// the tab's current size, the tab is resized (via `resize_whole_tab`)
    /// and a force-render is scheduled.
    pub fn recompute_tab_size(&mut self, tab_id: usize) -> Result<()> {
        let err_context = || format!("failed to recompute size for tab {tab_id}");

        let mut rows: Vec<usize> = Vec::new();
        let mut cols: Vec<usize> = Vec::new();
        for (client_id, active_tab) in self.active_tab_ids.iter() {
            if *active_tab == tab_id
                && let Some(size) = self.client_sizes.get(client_id)
            {
                rows.push(size.rows);
                cols.push(size.cols);
            }
        }
        if rows.is_empty() || cols.is_empty() {
            return Ok(());
        }
        rows.sort_unstable();
        cols.sort_unstable();
        let new_size = Size {
            rows: rows[0],
            cols: cols[0],
        };
        if let Some(tab) = self.tabs.get_mut(&tab_id)
            && tab.size != new_size
        {
            tab.resize_whole_tab(new_size).with_context(err_context)?;
            tab.set_force_render();
        }
        Ok(())
    }

    pub fn update_pixel_dimensions(&mut self, pixel_dimensions: PixelDimensions) {
        self.pixel_dimensions.merge(pixel_dimensions);
        if let Some(character_cell_size) = self.pixel_dimensions.character_cell_size {
            *self.character_cell_size.borrow_mut() = Some(character_cell_size);
        } else if let Some(text_area_size) = self.pixel_dimensions.text_area_size {
            let character_cell_size_height = text_area_size.height / self.size.rows;
            let character_cell_size_width = text_area_size.width / self.size.cols;
            let character_cell_size = SizeInPixels {
                height: character_cell_size_height,
                width: character_cell_size_width,
            };
            *self.character_cell_size.borrow_mut() = Some(character_cell_size);
        }
    }

    pub fn update_terminal_background_color(&mut self, background_color_instruction: String) {
        if let Some(AnsiCode::RgbCode((r, g, b))) =
            xparse_color(background_color_instruction.as_bytes())
        {
            let bg_palette_color = PaletteColor::Rgb((r, g, b));
            self.terminal_emulator_colors.borrow_mut().bg = bg_palette_color;
        }
    }

    pub fn update_terminal_foreground_color(&mut self, foreground_color_instruction: String) {
        if let Some(AnsiCode::RgbCode((r, g, b))) =
            xparse_color(foreground_color_instruction.as_bytes())
        {
            let fg_palette_color = PaletteColor::Rgb((r, g, b));
            self.terminal_emulator_colors.borrow_mut().fg = fg_palette_color;
        }
    }

    pub fn update_terminal_color_registers(&mut self, color_registers: Vec<(usize, String)>) {
        let mut terminal_emulator_color_codes = self.terminal_emulator_color_codes.borrow_mut();
        for (color_register, color_sequence) in color_registers {
            terminal_emulator_color_codes.insert(color_register, color_sequence);
        }
    }

    /// Enqueue a whitelisted host-terminal query from pane `pane_id`.
    /// Queries are serialized globally: at most one is in flight to the
    /// client at a time; the rest wait in `forward_queue`. Returns the
    /// allocated token so callers (tests) can observe it if useful.
    pub fn forward_host_query(
        &mut self,
        pane_id: PaneId,
        query: crate::host_query::HostQuery,
    ) -> u32 {
        // ColorPaletteMode is answered locally — Zellij already knows
        // the host's mode from its own startup query and from
        // unsolicited DSR 997 updates. Skip the entire forwarding
        // machinery (no token, no in-flight slot, no host round-trip)
        // and write the reply straight to the originating pane's pty.
        if let crate::host_query::HostQuery::ColorPaletteMode = query {
            self.answer_color_palette_mode_query_locally(pane_id);
            return STARTUP_SENTINEL_TOKEN; // sentinel: no real forward happened
        }
        let token = self.next_forward_token;
        // Skip over the reserved sentinel (0) on wrap; allocate a fresh
        // u32 for every forward.
        self.next_forward_token = self.next_forward_token.wrapping_add(1);
        if self.next_forward_token == STARTUP_SENTINEL_TOKEN {
            self.next_forward_token = 1;
        }
        if self.forward_in_flight_token.is_some() {
            self.forward_queue.push_back(PendingForward {
                token,
                pane_id,
                query,
            });
        } else {
            self.dispatch_forward(token, pane_id, query);
        }
        token
    }

    /// Synthesise the DSR 997 reply to a `CSI ? 996 n` query from
    /// `host_terminal_theme_mode` and write it directly to the
    /// originating pane's pty. Plugin panes are skipped (they receive
    /// `Event::HostTerminalThemeChanged` and have no notion of
    /// VT-protocol queries). When Zellij has not yet learned the host
    /// mode, the pane receives no reply — matching what a host that
    /// does not implement CSI 2031 would do. The Contour spec only
    /// defines `;1` (dark) and `;2` (light); fabricating any other
    /// code (e.g. `;0`) would be non-conformant.
    fn answer_color_palette_mode_query_locally(&mut self, pane_id: PaneId) {
        if matches!(pane_id, PaneId::Plugin(_)) {
            return;
        }
        let code: u8 = match self.host_terminal_theme_mode {
            Some(HostTerminalThemeMode::Dark) => 1,
            Some(HostTerminalThemeMode::Light) => 2,
            None => {
                log::debug!(
                    "CSI ?996n received but host_terminal_theme_mode is unknown; \
                     dropping (spec defines only 1=dark / 2=light)"
                );
                // The Contour spec defines only ;1 / ;2 — silence is
                // the conformant behaviour when the host's mode is
                // unknown. But the pane may have been forward-paused
                // on the dispatch; if so we still owe it an unblock
                // cycle so any buffered bytes get replayed. Skip the
                // empty resume when the pane is not paused, matching
                // the "stay silent" guarantee.
                if self.is_any_tab_pane_forward_paused(pane_id) {
                    let _ = self.resume_pane_after_forward(pane_id, Vec::new());
                }
                return;
            },
        };
        let reply = format!("\u{1b}[?997;{}n", code).into_bytes();
        // Route via Tab so the reply lands on the pane in the correct
        // stream position and any PTY input the app emitted while
        // waiting is replayed.
        let _ = self.resume_pane_after_forward(pane_id, reply);
    }

    /// Dispatch a forward to the client and mark the slot as in-flight.
    /// Spawns a one-shot timeout task on the global tokio runtime that
    /// synthesizes an empty reply for `token` after
    /// `SERVER_FORWARD_TIMEOUT_MS`. The handler's token-equality guard
    /// makes the timeout a no-op if a real reply arrived first, so no
    /// explicit cancellation is needed.
    fn dispatch_forward(
        &mut self,
        token: u32,
        pane_id: PaneId,
        query: crate::host_query::HostQuery,
    ) {
        let query_bytes = query.to_query_bytes();
        self.pending_forwarded_queries
            .insert(token, PendingForwardEntry { pane_id, query });
        self.forward_in_flight_token = Some(token);
        let _ = self
            .bus
            .senders
            .send_to_server(ServerInstruction::ForwardQueryToHost(token, query_bytes));
        let senders = self.bus.senders.clone();
        crate::global_async_runtime::get_tokio_runtime().spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(SERVER_FORWARD_TIMEOUT_MS)).await;
            let _ = senders.send_to_screen(ScreenInstruction::ForwardedReplyFromHost {
                token,
                reply_bytes: Vec::new(),
            });
        });
    }

    /// Handle a host-reply observed by the client for token `token`.
    /// Writes the bytes to the originating pane's pty (if still present)
    /// and releases the in-flight slot so the next queued forward can
    /// dispatch.
    ///
    /// An empty `reply_bytes` is treated as "nobody answered" — either
    /// because no client was attached to forward the query, or the
    /// chosen client's host timed out. In that case we try to answer
    /// from Zellij's cached view of the host (pixel dims, bg/fg,
    /// palette) so the pane still gets a well-formed reply instead of
    /// zero bytes. If the cache has nothing relevant either, the pane
    /// receives an empty write and the app decides what to do.
    pub fn handle_forwarded_reply_from_host(
        &mut self,
        token: u32,
        reply_bytes: Vec<u8>,
    ) -> Result<()> {
        // Stale-reply guard. Both the real `ForwardedReplyFromHost`
        // path and the server-side timeout path land here. If a real
        // reply landed first (releasing the slot AND dispatching the
        // next queued forward), the late timeout's empty reply for the
        // already-cleared token must be a no-op — releasing the slot
        // again would clobber the new in-flight forward. The same
        // logic also rejects duplicate replies and replies for tokens
        // belonging to a previously-closed pane.
        if self.forward_in_flight_token != Some(token) {
            log::debug!(
                "Dropping stale forwarded reply (token={}, in-flight={:?}, len={} bytes)",
                token,
                self.forward_in_flight_token,
                reply_bytes.len(),
            );
            return Ok(());
        }
        if let Some(entry) = self.pending_forwarded_queries.remove(&token) {
            let PendingForwardEntry { pane_id, query } = entry;
            match pane_id {
                PaneId::Terminal(_) => {
                    let payload = if reply_bytes.is_empty() {
                        self.synthesize_cached_reply(&query)
                    } else {
                        reply_bytes
                    };
                    // Route via Tab so the reply lands on the pane in
                    // the correct stream position and any PTY input
                    // the app emitted while waiting is replayed.
                    self.resume_pane_after_forward(pane_id, payload)?;
                },
                PaneId::Plugin(_) => {
                    // Plugin panes do not issue whitelisted host queries;
                    // if we reach here the mapping was populated
                    // erroneously. Drop the reply.
                    log::warn!(
                        "Discarding host reply for plugin pane (token={}); plugins do not forward CSI/OSC queries",
                        token
                    );
                },
            }
        }
        // Release the slot and dispatch the next queued forward, if any.
        self.forward_in_flight_token = None;
        if let Some(next) = self.forward_queue.pop_front() {
            self.dispatch_forward(next.token, next.pane_id, next.query);
        }
        Ok(())
    }

    /// Whether any tab owns `pane_id` AND the pane is currently
    /// forward-paused. Used by the `ColorPaletteMode` short-circuit
    /// to skip the empty-payload resume when no pane needs unblocking.
    fn is_any_tab_pane_forward_paused(&self, pane_id: PaneId) -> bool {
        self.tabs
            .values()
            .any(|tab| tab.is_pane_forward_paused(pane_id))
    }

    /// Deliver a forwarded reply (or cache-fallback synthesis, or a
    /// locally-answered query payload) to the originating pane via
    /// the owning Tab. The Tab handler writes the bytes to PTY and
    /// then re-feeds any PTY input that was buffered while the pane
    /// was forward-paused, preserving query/reply ordering.
    pub fn resume_pane_after_forward(
        &mut self,
        pane_id: PaneId,
        reply_bytes: Vec<u8>,
    ) -> Result<()> {
        let terminal_id = match pane_id {
            PaneId::Terminal(id) => id,
            PaneId::Plugin(_) => {
                // Plugin panes do not forward queries — dropping is
                // the correct behaviour matching the existing
                // forwarding path's plugin-pane guard.
                return Ok(());
            },
        };
        let mut found = false;
        for tab in self.tabs.values_mut() {
            if tab.has_pane_with_pid(&pane_id) {
                tab.resume_pane_after_forward(terminal_id, reply_bytes.clone())
                    .non_fatal();
                found = true;
                break;
            }
        }
        if !found {
            // No tab owns the pane — the pane was closed between the
            // forward dispatch and the reply arrival. The terminal id
            // is probably defunct; the write goes to the PTY writer
            // anyway because an empty payload is the canonical "host
            // declined" reply some apps rely on, and a non-empty
            // payload may still be deliverable until the pty drains.
            let _ = self
                .bus
                .senders
                .send_to_pty_writer(PtyWriteInstruction::Write(reply_bytes, terminal_id, None));
            log::debug!(
                "ResumePaneAfterForward: pane {:?} not in any tab, fell back to direct PTY write",
                pane_id
            );
        }
        Ok(())
    }

    /// Build a reply for `query` from whatever host state Zellij has
    /// cached, or an empty `Vec` if nothing relevant is cached. The
    /// classification happened at intercept time (Grid produces a
    /// [`HostQuery`]), so this method is pure structural dispatch —
    /// no re-parsing of bytes.
    fn synthesize_cached_reply(&self, query: &crate::host_query::HostQuery) -> Vec<u8> {
        use crate::host_query::HostQuery;
        match query {
            HostQuery::TextAreaPixelSize => self
                .pixel_dimensions
                .text_area_size
                .map(|dims| format!("\x1b[4;{};{}t", dims.height, dims.width).into_bytes())
                .unwrap_or_default(),
            HostQuery::CharacterCellPixelSize => self
                .pixel_dimensions
                .character_cell_size
                .map(|dims| format!("\x1b[6;{};{}t", dims.height, dims.width).into_bytes())
                .unwrap_or_default(),
            HostQuery::DefaultForeground { terminator }
            | HostQuery::DefaultBackground { terminator } => {
                let palette = self.terminal_emulator_colors.borrow();
                let (channel, color) = match query {
                    HostQuery::DefaultForeground { .. } => (10u32, palette.fg),
                    HostQuery::DefaultBackground { .. } => (11u32, palette.bg),
                    _ => unreachable!(),
                };
                if let PaletteColor::Rgb((r, g, b)) = color {
                    let mut out = format!(
                        "\x1b]{};rgb:{:04x}/{:04x}/{:04x}",
                        channel,
                        (r as u16) * 0x0101,
                        (g as u16) * 0x0101,
                        (b as u16) * 0x0101,
                    )
                    .into_bytes();
                    out.extend_from_slice(terminator.as_bytes());
                    out
                } else {
                    Vec::new()
                }
            },
            HostQuery::PaletteRegister { index, terminator } => {
                let codes = self.terminal_emulator_color_codes.borrow();
                if let Some(color) = codes.get(&(*index as usize)) {
                    let mut out = format!("\x1b]4;{};{}", index, color).into_bytes();
                    out.extend_from_slice(terminator.as_bytes());
                    out
                } else {
                    Vec::new()
                }
            },
            // Should not reach here: ColorPaletteMode short-circuits in
            // `forward_host_query` before any cache-fallback path runs.
            // The Contour spec only defines `;1` and `;2`; if the host
            // mode is unknown there is no compliant reply, so return
            // empty bytes (the existing convention for "no synthesis
            // possible").
            HostQuery::ColorPaletteMode => match self.host_terminal_theme_mode {
                Some(HostTerminalThemeMode::Dark) => b"\x1b[?997;1n".to_vec(),
                Some(HostTerminalThemeMode::Light) => b"\x1b[?997;2n".to_vec(),
                None => Vec::new(),
            },
        }
    }

    /// Returns true if there are any clients, watchers, or subscribers that need render output.
    fn has_render_recipients(&self) -> bool {
        !self.connected_clients.borrow().is_empty()
            || !self.watcher_clients.is_empty()
            || !self.pane_render_subscribers.is_empty()
    }

    pub fn render(&mut self, plugin_render_assets: Option<Vec<PluginRenderAsset>>) -> Result<()> {
        // here we schedule the RenderToClients background job which debounces renders every 10ms
        // rather than actually rendering
        //
        // when this job decides to render, it sends back the ScreenInstruction::RenderToClients
        // message, triggering our render_to_clients method which does the actual rendering

        if self.has_render_recipients() {
            let _ = self
                .bus
                .senders
                .send_to_background_jobs(BackgroundJob::RenderToClients);
        }
        if let Some(plugin_render_assets) = plugin_render_assets {
            let _ = self
                .bus
                .senders
                .send_to_plugin(PluginInstruction::UnblockCliPipes(plugin_render_assets))
                .context("failed to unblock input pipe");
        }
        Ok(())
    }

    pub fn render_to_clients(&mut self, pending_tab_ids: &HashSet<usize>) -> Result<()> {
        // this method does the actual rendering and is triggered by a debounced BackgroundJob (see
        // the render method for more details)
        let err_context = "failed to render screen";

        // Fast path: skip all work when nobody is listening
        if self.connected_clients.borrow().is_empty()
            && self.watcher_clients.is_empty()
            && self.pane_render_subscribers.is_empty()
        {
            return Ok(());
        }

        // Separate rendering for regular clients and watchers
        let has_regular_clients = self
            .connected_clients
            .borrow()
            .keys()
            .any(|id| !self.watcher_clients.contains_key(id));
        let has_watchers = !self.watcher_clients.is_empty(); // No change needed

        // Track whether non-watcher output was dirty for conditional watcher rendering
        let non_watcher_output_was_dirty;

        let mut tabs_to_close = vec![];

        // === PHASE 1: Render for regular clients ===
        if has_regular_clients {
            let mut output = Output::new(
                self.sixel_image_store.clone(),
                self.character_cell_size.clone(),
                self.styled_underlines,
                self.osc8_hyperlinks,
            );

            let has_ansi_subscribers = self.pane_render_subscribers.values().any(|s| s.ansi);
            output.collect_ansi_pane_contents =
                has_ansi_subscribers || self.plugins_need_ansi_pane_contents;

            for (tab_index, tab) in &mut self.tabs {
                if tab.is_pending() || pending_tab_ids.contains(tab_index) {
                    continue;
                }
                if tab.has_selectable_tiled_panes() {
                    // Pass None for normal client rendering
                    tab.render(&mut output, None).context(err_context)?;
                } else {
                    tabs_to_close.push(*tab_index);
                }
            }

            let pane_render_report = output.drain_pane_render_report();

            // Subscriber delivery — gated behind is_empty() for zero overhead
            if !self.pane_render_subscribers.is_empty() {
                self.deliver_to_pane_subscribers_from_report(&pane_render_report);
            }

            let _ = self
                .bus
                .senders
                .send_to_plugin(PluginInstruction::PaneRenderReport(pane_render_report));

            non_watcher_output_was_dirty = output.is_dirty();

            let mut bell_state_changed = false;
            let mut has_bell = false;

            if self.visual_bell {
                let mut panes_to_flash: Vec<PaneId> = vec![];
                let mut tabs_to_flash: Vec<usize> = vec![];

                let active_tab_ids_snapshot: Vec<usize> =
                    self.active_tab_ids.values().copied().collect();

                for tab in self.tabs.values_mut() {
                    let is_active = active_tab_ids_snapshot.contains(&tab.id);
                    let (new_panes, tab_newly_set) =
                        tab.check_and_handle_bell_notifications(is_active);
                    if !new_panes.is_empty() {
                        panes_to_flash.extend(new_panes);
                        bell_state_changed = true;
                    }
                    if tab_newly_set {
                        tabs_to_flash.push(tab.id);
                        bell_state_changed = true;
                    }
                }

                has_bell = !panes_to_flash.is_empty() || !tabs_to_flash.is_empty();
                if !panes_to_flash.is_empty() {
                    let _ = self
                        .bus
                        .senders
                        .send_to_background_jobs(BackgroundJob::FlashPaneBell(panes_to_flash));
                }
                for tab_id in tabs_to_flash {
                    let _ = self
                        .bus
                        .senders
                        .send_to_background_jobs(BackgroundJob::FlashTabBell(tab_id));
                }
            } else {
                // visual_bell disabled: still detect bell for ANSI BEL forwarding only
                for tab in self.tabs.values_mut() {
                    if tab.check_and_consume_bells_without_visual_notification() {
                        has_bell = true;
                    }
                }
            }

            if has_bell {
                output.add_post_vte_instruction_to_multiple_clients(
                    self.active_tab_ids.keys().copied(),
                    "\u{7}", // ANSI BEL
                );
            }

            if non_watcher_output_was_dirty || has_bell {
                let serialized_output = output.serialize().context(err_context)?;
                let _ = self
                    .bus
                    .senders
                    .send_to_server(ServerInstruction::Render(Some(serialized_output)))
                    .context(err_context);
            }

            if bell_state_changed {
                self.log_and_report_session_state()?;
            }
        } else {
            // No regular clients, output is not dirty
            non_watcher_output_was_dirty = false;

            // No regular clients but subscribers exist — query panes directly
            if !self.pane_render_subscribers.is_empty() {
                self.deliver_to_pane_subscribers_directly();
            }
        }

        // === PHASE 2: Render for watchers ===
        if has_watchers && let Some(followed_client_id) = self.followed_client_id {
            // Create fresh output for watchers
            let mut watcher_output = Output::new(
                self.sixel_image_store.clone(),
                self.character_cell_size.clone(),
                self.styled_underlines,
                self.osc8_hyperlinks,
            );

            let focused_tab_index_of_followed_client_id =
                *self.active_tab_ids.get(&followed_client_id).unwrap_or(&0);

            if let Some(tab) = self
                .tabs
                .get_mut(&focused_tab_index_of_followed_client_id)
                .as_mut()
                && !tab.is_pending()
                && !pending_tab_ids.contains(&focused_tab_index_of_followed_client_id)
            {
                // Only force render if:
                // 1. Non-watcher output was dirty, OR
                // 2. Any watcher needs a forced render (first render or after resize), OR
                // 3. No non-watcher clients are connected
                let any_watcher_needs_force_render = self
                    .watcher_clients
                    .values()
                    .any(|state| state.should_force_render());
                let should_force_render = non_watcher_output_was_dirty
                    || any_watcher_needs_force_render
                    || !has_regular_clients;

                if should_force_render {
                    tab.set_force_render();
                }
                tab.render(&mut watcher_output, Some(followed_client_id))
                    .context(err_context)?;
            }

            // Send the rendered output to all watcher clients
            if watcher_output.is_dirty() {
                let mut watcher_render_output: HashMap<ClientId, String> = HashMap::new();

                // For each watcher, clone the output and serialize with size constraints
                for (watcher_id, watcher_state) in &self.watcher_clients {
                    let mut watcher_specific_output = watcher_output.clone();

                    // Serialize this watcher's output with size constraints (cropping and padding handled inside)
                    let mut serialized_output = watcher_specific_output
                        .serialize_with_size(Some(watcher_state.size()), Some(self.size))
                        .context(err_context)?;

                    // Get the output for the followed client and map it to this watcher
                    if let Some(followed_output) = serialized_output.remove(&followed_client_id) {
                        watcher_render_output.insert(*watcher_id, followed_output);
                    }
                }

                // Send to server for delivery to watcher clients
                if !watcher_render_output.is_empty() {
                    let _ = self
                        .bus
                        .senders
                        .send_to_server(ServerInstruction::Render(Some(watcher_render_output)))
                        .context(err_context);
                }

                // Clear force render flag for all watchers after successful render
                for watcher_state in self.watcher_clients.values_mut() {
                    watcher_state.clear_force_render();
                }
            }
        }
        for tab_index in tabs_to_close {
            self.close_tab_by_id(tab_index)
                .context(err_context)
                .non_fatal();
        }

        Ok(())
    }

    /// Returns a mutable reference to this [`Screen`]'s tabs.
    pub fn get_tabs_mut(&mut self) -> &mut BTreeMap<usize, Tab> {
        &mut self.tabs
    }

    pub fn get_tabs(&self) -> &BTreeMap<usize, Tab> {
        &self.tabs
    }

    /// Returns an immutable reference to this [`Screen`]'s active [`Tab`].
    pub fn get_active_tab(&self, client_id: ClientId) -> Result<&Tab> {
        match self.active_tab_ids.get(&client_id) {
            Some(tab) => self
                .tabs
                .get(tab)
                .ok_or_else(|| anyhow!("active tab {} does not exist", tab)),
            None => Err(anyhow!("active tab not found for client {:?}", client_id)),
        }
    }

    pub fn get_client_input_mode(&self, client_id: ClientId) -> Option<InputMode> {
        self.get_active_tab(client_id)
            .ok()
            .and_then(|tab| tab.get_client_input_mode(client_id))
    }

    pub fn get_first_client_id(&self) -> Option<ClientId> {
        self.active_tab_ids.keys().next().copied()
    }

    /// Returns an immutable reference to this [`Screen`]'s previous active [`Tab`].
    /// Consumes the last entry in tab history.
    pub fn get_previous_tab(&mut self, client_id: ClientId) -> Result<Option<&Tab>> {
        Ok(
            match self
                .tab_history
                .get_mut(&client_id)
                .with_context(|| {
                    format!("failed to retrieve tab history for client {client_id:?}")
                })?
                .pop()
            {
                Some(tab) => self.tabs.get(&tab),
                None => None,
            },
        )
    }

    /// Returns a mutable reference to this [`Screen`]'s active [`Tab`].
    pub fn get_active_tab_mut(&mut self, client_id: ClientId) -> Result<&mut Tab> {
        match self.active_tab_ids.get(&client_id) {
            Some(tab) => self
                .tabs
                .get_mut(tab)
                .ok_or_else(|| anyhow!("active tab {} does not exist", tab)),
            None => Err(anyhow!("active tab not found for client {:?}", client_id)),
        }
    }

    /// Returns a mutable reference to this [`Screen`]'s indexed [`Tab`].
    pub fn get_indexed_tab_mut(&mut self, tab_index: usize) -> Option<&mut Tab> {
        self.get_tabs_mut().get_mut(&tab_index)
    }

    /// Clear bell notification for the currently focused pane of the given client.
    /// Also cancels any running flash jobs if applicable.
    pub fn clear_bell_for_focused_pane(&mut self, client_id: ClientId) {
        let tab_id_and_pane_id: Option<(usize, PaneId)> =
            self.get_active_tab_mut(client_id).ok().and_then(|tab| {
                tab.get_active_pane_id(client_id)
                    .map(|pane_id| (tab.id, pane_id))
            });
        if let Some((tab_id, focused_pane_id)) = tab_id_and_pane_id
            && let Some(tab) = self.tabs.get_mut(&tab_id)
            && tab.panes_with_pending_bell.contains(&focused_pane_id)
        {
            let tab_had_bell = tab.tab_has_pending_bell;
            tab.clear_bell_notification_for_pane(focused_pane_id);
            let tab_bell_now_cleared = tab_had_bell && !tab.tab_has_pending_bell;
            let _ = self
                .bus
                .senders
                .send_to_background_jobs(BackgroundJob::StopFlashPaneBell(vec![focused_pane_id]));
            if tab_bell_now_cleared {
                let _ = self
                    .bus
                    .senders
                    .send_to_background_jobs(BackgroundJob::StopFlashTabBell(tab_id));
            }
        }
    }

    /// Clear bell notification for a specific pane ID in the given client's active tab.
    pub fn clear_bell_for_pane_id(&mut self, pane_id: PaneId, client_id: ClientId) {
        let tab_id: Option<usize> = self.get_active_tab_mut(client_id).ok().map(|tab| tab.id);
        if let Some(tab_id) = tab_id
            && let Some(tab) = self.tabs.get_mut(&tab_id)
            && tab.panes_with_pending_bell.contains(&pane_id)
        {
            let tab_had_bell = tab.tab_has_pending_bell;
            tab.clear_bell_notification_for_pane(pane_id);
            let tab_bell_now_cleared = tab_had_bell && !tab.tab_has_pending_bell;
            let _ = self
                .bus
                .senders
                .send_to_background_jobs(BackgroundJob::StopFlashPaneBell(vec![pane_id]));
            if tab_bell_now_cleared {
                let _ = self
                    .bus
                    .senders
                    .send_to_background_jobs(BackgroundJob::StopFlashTabBell(tab_id));
            }
        }
    }

    pub fn show_floating_panes_in_tab(
        &mut self,
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    ) -> Result<()> {
        let tab = match tab_id {
            Some(id) => self.tabs.get_mut(&id),
            None => self.get_active_tab_mut(client_id).ok(),
        };
        match tab {
            None => {
                let mut completion = completion;
                if let Some(c) = completion.as_mut() {
                    c.set_exit_status(1);
                    c.set_error_message("Tab not found".to_string());
                }
                drop(completion);
            },
            Some(tab) => tab.show_floating_panes_atomic(completion),
        }
        Ok(())
    }

    pub fn hide_floating_panes_in_tab(
        &mut self,
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    ) -> Result<()> {
        let tab = match tab_id {
            Some(id) => self.tabs.get_mut(&id),
            None => self.get_active_tab_mut(client_id).ok(),
        };
        match tab {
            None => {
                let mut completion = completion;
                if let Some(c) = completion.as_mut() {
                    c.set_exit_status(1);
                    c.set_error_message("Tab not found".to_string());
                }
                drop(completion);
            },
            Some(tab) => tab.hide_floating_panes_atomic(completion),
        }
        Ok(())
    }

    pub fn are_floating_panes_visible_in_tab(
        &self,
        client_id: ClientId,
        tab_id: Option<usize>,
        completion: Option<NotificationEnd>,
    ) -> Result<()> {
        let tab = match tab_id {
            Some(id) => self.tabs.get(&id),
            None => self.get_active_tab(client_id).ok(),
        };
        let mut completion = completion;
        match tab {
            None => {
                if let Some(c) = completion.as_mut() {
                    c.set_error_message("Tab not found".to_string());
                }
            },
            Some(tab) => {
                if tab.are_floating_panes_visible() {
                    if let Some(c) = completion.as_mut() {
                        c.set_stdout_message("true".to_string());
                    }
                } else if let Some(c) = completion.as_mut() {
                    c.set_error_message("false".to_string());
                }
            },
        }
        drop(completion);
        Ok(())
    }

    /// Resolves the display position a tab created with `placement` should take,
    /// shifting the tabs it displaces to the right.
    ///
    /// This runs at creation time rather than as a follow-up move so the tab bar
    /// never renders the intermediate order — a create-then-move pair would flash
    /// the tab at the end before snapping it into place.
    fn claim_tab_position(&mut self, placement: TabPlacement) -> usize {
        let append_position = self.tabs.len();
        match placement {
            TabPlacement::Append => append_position,
            // With 0 or 1 existing tabs there is nothing to the right of the base
            // tab, so "after base" and "append" are the same position.
            TabPlacement::AfterBase if append_position < 2 => append_position,
            TabPlacement::AfterBase => {
                for tab in self.tabs.values_mut() {
                    if tab.position >= 1 {
                        tab.position += 1;
                    }
                }
                1
            },
        }
    }

    /// Creates a new [`Tab`] in this [`Screen`]
    pub fn new_tab(
        &mut self,
        tab_id: usize,
        swap_layouts: (Vec<SwapTiledLayout>, Vec<SwapFloatingLayout>),
        tab_name: Option<String>,
        client_id: Option<ClientId>,
        placement: TabPlacement,
    ) -> Result<()> {
        let err_context = || format!("failed to create new tab for client {client_id:?}",);
        let next_tab_id = tab_id
            .checked_add(1)
            .ok_or_else(|| anyhow!("stable tab ID space exhausted"))?;
        // Resolve every fallible creation prerequisite before mutating tab
        // positions. Callers that first extract a live pane can therefore
        // preflight this exact pair and know that `new_tab` cannot strand it
        // after a partial Screen mutation.
        let os_input = self
            .bus
            .os_input
            .as_ref()
            .with_context(err_context)?
            .clone();
        self.next_tab_id = self.next_tab_id.max(next_tab_id);

        let client_id = client_id.map(|client_id| {
            if self.get_active_tab(client_id).is_ok() {
                client_id
            } else if let Some(first_client_id) = self.get_first_client_id() {
                first_client_id
            } else {
                client_id
            }
        });

        let tab_name = tab_name.unwrap_or_default();

        let position = self.claim_tab_position(placement);
        let mut tab = Tab::new(
            tab_id,
            position,
            tab_name,
            self.size,
            self.character_cell_size.clone(),
            self.stacked_resize.clone(),
            self.sixel_image_store.clone(),
            os_input,
            self.bus.senders.clone(),
            self.max_panes,
            self.style,
            self.default_mode_info.clone(),
            self.draw_pane_frames,
            self.auto_layout,
            self.connected_clients.clone(),
            self.session_is_mirrored,
            client_id,
            self.copy_options.clone(),
            self.terminal_emulator_colors.clone(),
            self.terminal_emulator_color_codes.clone(),
            swap_layouts,
            self.default_shell.clone(),
            self.debug,
            self.arrow_fonts,
            self.styled_underlines,
            self.osc8_hyperlinks,
            self.explicitly_disable_kitty_keyboard_protocol,
            self.default_editor.clone(),
            self.web_clients_allowed,
            self.web_sharing,
            self.current_pane_group.clone(),
            self.currently_marking_pane_group.clone(),
            self.advanced_mouse_actions,
            self.mouse_hover_effects,
            self.focus_follows_mouse,
            self.mouse_click_through,
            self.web_server_ip,
            self.web_server_port,
        );
        for (client_id, mode_info) in &self.mode_info {
            tab.change_mode_info(mode_info.clone(), *client_id);
        }
        self.tabs.insert(tab_id, tab);
        Ok(())
    }
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    pub fn apply_layout(
        &mut self,
        layout: TiledPaneLayout,
        floating_panes_layout: Vec<FloatingPaneLayout>,
        new_terminal_ids: Vec<(u32, HoldForCommand)>,
        new_floating_terminal_ids: Vec<(u32, HoldForCommand)>,
        new_plugin_ids: HashMap<RunPluginOrAlias, Vec<u32>>,
        tab_id: usize,
        should_change_client_focus: bool,
        client_id_and_is_web_client: (ClientId, bool),
        blocking_terminal: Option<(u32, NotificationEnd)>,
    ) -> Result<()> {
        let prepared = self.prepare_apply_layout(
            layout,
            floating_panes_layout,
            new_terminal_ids,
            new_floating_terminal_ids,
            new_plugin_ids,
            tab_id,
            should_change_client_focus,
            client_id_and_is_web_client,
            blocking_terminal,
        )?;
        prepared
            .transaction
            .preflight_commit(self.tabs.get(&prepared.tab_id).with_context(|| {
                format!(
                    "prepared Apply target tab {} disappeared before commit preflight",
                    prepared.tab_id
                )
            })?)?;
        let cleanup_transaction_id = self.reserve_layout_transaction_id();
        let mut committed = match self.commit_apply_layout_state(prepared) {
            Ok(committed) => committed,
            Err(mut prepared) => {
                let message = format!(
                    "prepared Apply target tab {} disappeared before direct commit",
                    prepared.tab_id
                );
                prepared
                    .transaction
                    .mark_blocking_completion_failed(&message);
                self.indeterminate_layout_transactions.insert(
                    cleanup_transaction_id,
                    IndeterminatePreparedLayout::Apply {
                        prepared,
                        plan: LayoutReconciliationPlan {
                            intent: LayoutReconciliationIntent::Reject(message.clone()),
                            expected_plugin_ids: vec![],
                            resource_ids: vec![],
                            preserve_pending_tab_on_rejection: false,
                            close_fenced_tab_on_rejection: false,
                            layout_generation: None,
                        },
                    },
                );
                bail!("{message}");
            },
        };
        self.retain_layout_cleanup(
            cleanup_transaction_id,
            committed.effects.take_pending_cleanup(),
        );
        let committed_tab_id = committed.tab_id;
        let blocking_terminal = self.emit_committed_apply_layout(committed);
        self.render(None).non_fatal();
        self.flush_layout_cleanup(cleanup_transaction_id);
        if let Some(message) = self.pending_layout_cleanup_message(cleanup_transaction_id) {
            if let Some((_, mut completion)) = blocking_terminal {
                completion.mark_failure(message.clone());
            }
            bail!("{message}");
        }
        if let Some((terminal_id, completion)) = blocking_terminal {
            let Some(tab) = self.tabs.get_mut(&committed_tab_id) else {
                let mut completion = completion;
                let message = format!(
                    "committed Apply target tab {committed_tab_id} disappeared before blocking completion attachment"
                );
                completion.mark_failure(message.clone());
                bail!("{message}");
            };
            if let Err(mut completion) =
                tab.attach_blocking_layout_completion(terminal_id, completion)
            {
                let message = format!(
                    "terminal {terminal_id} rejected direct blocking completion attachment"
                );
                completion.mark_failure(message.clone());
                bail!("{message}");
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    fn prepare_apply_layout(
        &mut self,
        layout: TiledPaneLayout,
        floating_panes_layout: Vec<FloatingPaneLayout>,
        new_terminal_ids: Vec<(u32, HoldForCommand)>,
        new_floating_terminal_ids: Vec<(u32, HoldForCommand)>,
        new_plugin_ids: HashMap<RunPluginOrAlias, Vec<u32>>,
        tab_id: usize,
        should_change_client_focus: bool,
        client_id_and_is_web_client: (ClientId, bool),
        blocking_terminal: Option<(u32, NotificationEnd)>,
    ) -> Result<PreparedApplyLayout> {
        if !self.tabs.contains_key(&tab_id) {
            // TODO: we should prevent this situation with a UI - eg. cannot close tabs with a
            // pending state
            bail!("Tab with index {tab_id} not found. Cannot apply layout!");
        }
        let (client_id, mut is_web_client) = client_id_and_is_web_client;
        let client_id = if self.get_active_tab(client_id).is_ok() {
            if let Some(connected_client_is_web_client) =
                self.connected_clients.borrow().get(&client_id)
            {
                is_web_client = *connected_client_is_web_client;
            }
            client_id
        } else if let Some(first_client_id) = self.get_first_client_id() {
            if let Some(first_client_is_web_client) =
                self.connected_clients.borrow().get(&first_client_id)
            {
                is_web_client = *first_client_is_web_client;
            }
            first_client_id
        } else {
            client_id
        };
        let err_context = || format!("failed to apply layout for tab {tab_id:?}",);
        let transaction = self
            .tabs
            .get_mut(&tab_id)
            .context("couldn't find tab with index {tab_id}")?
            .begin_apply_layout(
                layout,
                floating_panes_layout,
                new_terminal_ids,
                new_floating_terminal_ids,
                new_plugin_ids,
                client_id,
                blocking_terminal,
            )
            .with_context(err_context)?;
        Ok(PreparedApplyLayout {
            tab_id,
            transaction: Box::new(transaction),
            should_change_client_focus,
            client_id,
            is_web_client,
        })
    }

    fn commit_apply_layout_state(
        &mut self,
        prepared: PreparedApplyLayout,
    ) -> std::result::Result<CommittedApplyLayout, PreparedApplyLayout> {
        let Some(tab) = self.tabs.get_mut(&prepared.tab_id) else {
            return Err(prepared);
        };
        let PreparedApplyLayout {
            tab_id,
            transaction,
            should_change_client_focus,
            client_id,
            is_web_client,
        } = prepared;
        let effects = (*transaction).commit_state(tab);
        Ok(CommittedApplyLayout {
            tab_id,
            effects,
            should_change_client_focus,
            client_id,
            is_web_client,
        })
    }

    fn commit_override_layout_state(
        &mut self,
        prepared_layouts: Vec<(usize, TabLayoutTransaction)>,
    ) -> CommittedOverrideLayout {
        let mut committed_effects = vec![];
        let mut remaining = prepared_layouts.into_iter();
        while let Some((tab_id, transaction)) = remaining.next() {
            let Some(tab) = self.tabs.get_mut(&tab_id) else {
                let mut remaining_prepared = vec![(tab_id, transaction)];
                remaining_prepared.extend(remaining);
                return CommittedOverrideLayout::Indeterminate {
                    missing_tab_id: tab_id,
                    committed_effects,
                    remaining_prepared,
                };
            };
            committed_effects.push((tab_id, transaction.commit_state(tab)));
        }
        CommittedOverrideLayout::Complete(committed_effects)
    }

    fn rollback_prepared_apply_layout(
        &mut self,
        prepared: PreparedApplyLayout,
        rejection_message: &str,
    ) {
        if let Some(tab) = self.tabs.get_mut(&prepared.tab_id) {
            (*prepared.transaction).rollback(tab, rejection_message);
        }
    }

    fn emit_committed_apply_layout(
        &mut self,
        committed: CommittedApplyLayout,
    ) -> Option<(u32, NotificationEnd)> {
        let CommittedApplyLayout {
            tab_id,
            mut effects,
            should_change_client_focus,
            client_id,
            is_web_client,
        } = committed;
        let mut blocking_terminal = effects.take_blocking_terminal();
        let Some(tab) = self.tabs.get_mut(&tab_id) else {
            let message = format!(
                "committed Apply target tab {tab_id} disappeared before infallible local effects"
            );
            if let Some((_, completion)) = blocking_terminal.as_mut() {
                completion.mark_failure(message.clone());
            }
            log::error!("{message}");
            return None;
        };
        if let Some((_, mut unexpected_completion)) = effects.emit(tab) {
            let message =
                "Apply effects retained a blocking completion after Screen extracted ownership";
            unexpected_completion.mark_failure(message);
            log::error!("{message}");
        }

        // move the relevant clients out of the current tab and place them in the new one
        let drained_clients = if should_change_client_focus {
            if self.session_is_mirrored {
                let client_mode_infos_in_source_tab = if let Ok(active_tab) =
                    self.get_active_tab_mut(client_id)
                {
                    let client_mode_infos_in_source_tab = active_tab.drain_connected_clients(None);
                    if active_tab.has_no_connected_clients() {
                        active_tab.visible(false).non_fatal();
                    }
                    Some(client_mode_infos_in_source_tab)
                } else {
                    None
                };
                let all_connected_clients: Vec<ClientId> =
                    self.connected_clients.borrow().keys().copied().collect();
                for client_id in all_connected_clients {
                    self.update_client_tab_focus(client_id, tab_id);
                }
                client_mode_infos_in_source_tab
            } else if let Ok(active_tab) = self.get_active_tab_mut(client_id) {
                let client_mode_info_in_source_tab =
                    active_tab.drain_connected_clients(Some(vec![client_id]));
                if active_tab.has_no_connected_clients() {
                    active_tab.visible(false).non_fatal();
                }
                self.update_client_tab_focus(client_id, tab_id);
                Some(client_mode_info_in_source_tab)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(tab) = self.tabs.get_mut(&tab_id) {
            tab.update_input_modes().non_fatal();
            if let Some(drained_clients) = drained_clients {
                tab.visible(true).non_fatal();
                tab.add_multiple_clients(drained_clients).non_fatal();
            }
            tab.resize_whole_tab(self.size).non_fatal();
            tab.set_force_render();
        }

        if !self.active_tab_ids.contains_key(&client_id) {
            // this means this is a new client and we need to add it to our state properly
            self.add_client(client_id, is_web_client).non_fatal();
        }
        // The new tab was just resized to `self.size` above as a default; if
        // any clients have been moved onto it, recompute to fit their actual
        // viewports.
        self.recompute_tab_size(tab_id).non_fatal();
        self.log_and_report_session_state().non_fatal();
        blocking_terminal
    }

    pub fn add_client(&mut self, client_id: ClientId, is_web_client: bool) -> Result<()> {
        let err_context = |tab_index| {
            format!("failed to attach client {client_id} to tab with index {tab_index}")
        };

        // Set followed_client_id to the first regular client if not already set
        if self.followed_client_id.is_none() && !self.watcher_clients.contains_key(&client_id) {
            self.followed_client_id = Some(client_id);
        }

        let mut tab_history = vec![];
        if let Some((_first_client, first_tab_history)) = self.tab_history.iter().next() {
            tab_history = first_tab_history.clone();
        }

        let tab_index = if let Some((_first_client, first_active_tab_index)) =
            self.active_tab_ids.iter().next()
        {
            *first_active_tab_index
        } else if self.tabs.contains_key(&self.global_last_active_tab_id) {
            self.global_last_active_tab_id
        } else if self.tabs.contains_key(&0) {
            0
        } else if let Some(tab_index) = self.tabs.keys().next() {
            tab_index.to_owned()
        } else {
            bail!("Can't find a valid tab to attach client to!");
        };

        self.active_tab_ids.insert(client_id, tab_index);
        self.connected_clients
            .borrow_mut()
            .insert(client_id, is_web_client);
        self.has_clients_flag.store(true, Ordering::Relaxed);
        self.tab_history.insert(client_id, tab_history);
        self.tabs
            .get_mut(&tab_index)
            .with_context(|| err_context(tab_index))?
            .add_client(client_id, None)
            .with_context(|| err_context(tab_index))?;
        // Resize the newly-active tab to the min of all viewers (this client
        // may shrink it, or may be the first viewer of an empty tab).
        self.recompute_tab_size(tab_index)
            .with_context(|| err_context(tab_index))?;
        Ok(())
    }

    pub fn remove_client(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || format!("failed to remove client {client_id}");

        // If the followed client disconnected, find the next regular client
        if Some(client_id) == self.followed_client_id {
            // Try to find another regular (non-watcher) client
            self.followed_client_id = self
                .connected_clients
                .borrow()
                .keys()
                .copied()
                .find(|id| !self.watcher_clients.contains_key(id) && id != &client_id);

            // If no regular client remains but we have watchers, keep the old followed_client_id
            // for terminal rendering (plugins will use their last state)
            if self.followed_client_id.is_none() && !self.watcher_clients.is_empty() {
                self.followed_client_id = Some(client_id); // Keep the disconnected client's ID
            }
        }

        for (_, tab) in self.tabs.iter_mut() {
            tab.remove_client(client_id);
            if tab.has_no_connected_clients() {
                tab.visible(false).with_context(err_context)?;
            }
        }
        let previously_active_tab_id = self.active_tab_ids.get(&client_id).copied();
        if let Some(prev_tab_id) = previously_active_tab_id {
            self.global_last_active_tab_id = prev_tab_id;
            self.active_tab_ids.remove(&client_id);
        }
        if self.tab_history.contains_key(&client_id) {
            self.tab_history.remove(&client_id);
        }
        self.connected_clients.borrow_mut().remove(&client_id);
        self.client_sizes.remove(&client_id);
        self.has_clients_flag.store(
            !self.connected_clients.borrow().is_empty(),
            Ordering::Relaxed,
        );
        self.pane_render_subscribers.remove(&client_id);
        // The vacated tab may have lost its smallest viewer; recompute so it
        // can grow back to fit the remaining clients (no-op if none remain).
        if let Some(prev_tab_id) = previously_active_tab_id {
            self.recompute_tab_size(prev_tab_id)
                .with_context(err_context)?;
        }
        self.log_and_report_session_state()
            .with_context(err_context)
    }

    pub fn add_watcher_client(&mut self, client_id: ClientId) -> Result<()> {
        // Initialize with a default size - will be updated when we receive the actual size
        let default_size = Size { rows: 24, cols: 80 }; // Reasonable default
        self.watcher_clients
            .insert(client_id, WatcherState::new(default_size));

        // Force a full render for the new watcher
        // This ensures they get complete state, not just delta
        self.render(None)?;

        Ok(())
    }

    pub fn remove_watcher_client(&mut self, client_id: ClientId) {
        self.watcher_clients.remove(&client_id);
    }

    pub fn set_followed_client(&mut self, client_id: ClientId) -> Result<()> {
        self.followed_client_id = Some(client_id);
        // Trigger re-render with new followed client
        self.render(None)?;
        Ok(())
    }

    pub fn set_watcher_size(&mut self, client_id: ClientId, size: Size) {
        // Update size if this client is a watcher
        if let Some(watcher_state) = self.watcher_clients.get_mut(&client_id) {
            watcher_state.set_size(size);
            watcher_state.set_force_render();
        }
    }

    pub fn generate_and_report_tab_state(&mut self) -> Result<Vec<TabInfo>> {
        let mut plugin_updates = vec![];
        let mut tab_infos_for_screen_state = BTreeMap::new();
        for tab in self.tabs.values() {
            let all_focused_clients: Vec<ClientId> = self
                .active_tab_ids
                .iter()
                .filter(|(_c_id, tab_position)| **tab_position == tab.id)
                .map(|(c_id, _)| c_id)
                .copied()
                .collect();
            let (active_swap_layout_name, is_swap_layout_dirty) = tab.swap_layout_info();
            let tab_viewport = tab.get_viewport();
            let tab_display_area = tab.get_display_area();
            let selectable_tiled_panes_count = tab.get_selectable_tiled_panes_count();
            let selectable_floating_panes_count = tab.get_selectable_floating_panes_count();
            let tab_info_for_screen = TabInfo {
                position: tab.position,
                name: tab.name.clone(),
                active: self.active_tab_ids.values().any(|i| i == &tab.id),
                panes_to_hide: tab.panes_to_hide_count(),
                is_fullscreen_active: tab.is_fullscreen_active(),
                is_sync_panes_active: tab.is_sync_panes_active(),
                are_floating_panes_visible: tab.are_floating_panes_visible(),
                other_focused_clients: all_focused_clients,
                active_swap_layout_name,
                is_swap_layout_dirty,
                viewport_rows: tab_viewport.rows,
                viewport_columns: tab_viewport.cols,
                display_area_rows: tab_display_area.rows,
                display_area_columns: tab_display_area.cols,
                selectable_tiled_panes_count,
                selectable_floating_panes_count,
                tab_id: tab.id,
                has_bell_notification: tab.tab_has_pending_bell
                    && !self.active_tab_ids.values().any(|i| i == &tab.id),
                is_flashing_bell: tab.tab_bell_flash
                    && !self.active_tab_ids.values().any(|i| i == &tab.id),
            };
            tab_infos_for_screen_state.insert(tab.position, tab_info_for_screen);
        }
        for (client_id, active_tab_index) in self.active_tab_ids.iter() {
            let mut plugin_tab_updates = vec![];
            for tab in self.tabs.values() {
                let other_focused_clients: Vec<ClientId> = if self.session_is_mirrored {
                    vec![]
                } else {
                    self.active_tab_ids
                        .iter()
                        .filter(|(c_id, tab_position)| {
                            **tab_position == tab.id && *c_id != client_id
                        })
                        .map(|(c_id, _)| c_id)
                        .copied()
                        .collect()
                };
                let (active_swap_layout_name, is_swap_layout_dirty) = tab.swap_layout_info();
                let tab_viewport = tab.get_viewport();
                let tab_display_area = tab.get_display_area();
                let selectable_tiled_panes_count = tab.get_selectable_tiled_panes_count();
                let selectable_floating_panes_count = tab.get_selectable_floating_panes_count();
                let tab_info_for_plugins = TabInfo {
                    position: tab.position,
                    name: tab.name.clone(),
                    active: *active_tab_index == tab.id,
                    panes_to_hide: tab.panes_to_hide_count(),
                    is_fullscreen_active: tab.is_fullscreen_active(),
                    is_sync_panes_active: tab.is_sync_panes_active(),
                    are_floating_panes_visible: tab.are_floating_panes_visible(),
                    other_focused_clients,
                    active_swap_layout_name,
                    is_swap_layout_dirty,
                    viewport_rows: tab_viewport.rows,
                    viewport_columns: tab_viewport.cols,
                    display_area_rows: tab_display_area.rows,
                    display_area_columns: tab_display_area.cols,
                    selectable_tiled_panes_count,
                    selectable_floating_panes_count,
                    tab_id: tab.id,
                    has_bell_notification: tab.tab_has_pending_bell && *active_tab_index != tab.id,
                    is_flashing_bell: tab.tab_bell_flash && *active_tab_index != tab.id,
                };
                plugin_tab_updates.push(tab_info_for_plugins);
            }
            plugin_tab_updates.sort_by_key(|a| a.position);
            let target_plugin_ids = self.targeted_plugin_ids(*client_id, EventType::TabUpdate);
            for plugin_id in target_plugin_ids {
                plugin_updates.push((
                    Some(plugin_id),
                    Some(*client_id),
                    Event::TabUpdate(plugin_tab_updates.clone()),
                ));
            }
        }
        self.bus
            .senders
            .send_to_plugin(PluginInstruction::Update(plugin_updates))
            .context("failed to update tabs")?;
        Ok(tab_infos_for_screen_state.values().cloned().collect())
    }
    fn generate_and_report_pane_state(&mut self) -> Result<PaneManifest> {
        let mut pane_manifest = PaneManifest::default();
        for tab in self.tabs.values() {
            pane_manifest.panes.insert(tab.position, tab.pane_infos());
        }
        let mut plugin_updates = vec![];
        let client_ids: Vec<ClientId> = self.active_tab_ids.keys().copied().collect();
        for client_id in client_ids {
            let target_plugin_ids = self.targeted_plugin_ids(client_id, EventType::PaneUpdate);
            for plugin_id in target_plugin_ids {
                plugin_updates.push((
                    Some(plugin_id),
                    Some(client_id),
                    Event::PaneUpdate(pane_manifest.clone()),
                ));
            }
        }
        if !plugin_updates.is_empty() {
            self.bus
                .senders
                .send_to_plugin(PluginInstruction::Update(plugin_updates))
                .context("failed to update pane state")?;
        }

        Ok(pane_manifest)
    }

    fn collect_pane_list(&self, show_all: bool) -> Result<ListPanesResponse> {
        fn should_include_pane(pane_info: &PaneInfo, show_all: bool) -> bool {
            pane_info.is_selectable || show_all
        }

        fn create_pane_list_entry(pane_info: PaneInfo, tab: &crate::tab::Tab) -> PaneListEntry {
            PaneListEntry {
                pane_info,
                tab_id: tab.id,
                tab_position: tab.position,
                tab_name: tab.name.clone(),
                pane_command: None,
                pane_cwd: None,
            }
        }

        fn sort_panes_by_tab_and_type(pane_entries: &mut [PaneListEntry]) {
            pane_entries.sort_by_key(|e| (e.tab_position, !e.pane_info.is_plugin, e.pane_info.id));
        }

        let mut pane_entries = Vec::new();

        for tab in self.tabs.values() {
            let pane_infos = tab.pane_infos();

            for pane_info in pane_infos {
                if should_include_pane(&pane_info, show_all) {
                    pane_entries.push(create_pane_list_entry(pane_info, tab));
                }
            }
        }

        sort_panes_by_tab_and_type(&mut pane_entries);
        Ok(pane_entries)
    }

    fn collect_tab_list(&self, _client_id: ClientId) -> Result<ListTabsResponse> {
        let mut tab_infos = Vec::new();
        let mut tab_instance_ids = BTreeMap::new();

        for tab in self.tabs.values() {
            if let Some(tab_info) = self.get_tab_info(tab.id) {
                tab_instance_ids.insert(tab.id, tab.instance_id.clone());
                tab_infos.push(tab_info);
            }
        }

        // Sort by position (display order)
        tab_infos.sort_by_key(|t| t.position);

        Ok(ListTabsResponse {
            session_incarnation: self.session_incarnation.clone(),
            tab_instance_ids,
            tabs: tab_infos,
        })
    }

    fn get_current_tab_info(&self, client_id: ClientId) -> Result<Option<TabInfo>> {
        match self.active_tab_ids.get(&client_id) {
            Some(active_tab_id) => Ok(self.get_tab_info(*active_tab_id)),
            None => Ok(None),
        }
    }

    fn log_and_report_session_state(&mut self) -> Result<()> {
        let err_context = || "Failed to log and report session state".to_string();

        self.update_active_pane_ids();
        // generate own session info
        let pane_manifest = self.generate_and_report_pane_state()?;
        let tab_infos = self.generate_and_report_tab_state()?;

        // Lazy-load layouts on first call if cache is empty
        // After that, cache is updated by watcher via UpdateAvailableLayouts instruction
        if self.cached_layouts.is_empty() {
            #[cfg(not(test))]
            {
                let (layouts, errors) = Layout::list_available_layouts(
                    self.layout_dir.clone(),
                    &self.default_layout_name,
                );
                self.cached_layouts = layouts;
                self.cached_layout_errors = errors;
            }
            #[cfg(test)]
            {
                self.cached_layouts = vec![];
                self.cached_layout_errors = vec![];
            }
        }
        let available_layouts = self.cached_layouts.clone();
        let creation_time = {
            let sock_path = ZELLIJ_SOCK_DIR.join(&self.session_name);
            std::fs::metadata(&sock_path)
                .ok()
                .and_then(|f| f.created().ok().or_else(|| f.modified().ok()))
                .and_then(|d| d.elapsed().ok())
                .map(|d| Duration::from_secs(d.as_secs()))
                .unwrap_or_default()
        };
        let session_info = SessionInfo {
            name: self.session_name.clone(),
            tabs: tab_infos,
            panes: pane_manifest,
            connected_clients: self.active_tab_ids.keys().len(),
            is_current_session: true,
            available_layouts,
            web_clients_allowed: self.web_sharing.web_clients_allowed(),
            web_client_count: self
                .connected_clients
                .borrow()
                .iter()
                .filter(|(_client_id, is_web_client)| **is_web_client)
                .count(),
            plugins: Default::default(), // these are filled in by the wasm thread
            tab_history: self.tab_history.clone(),
            pane_history: self
                .pane_history
                .iter()
                .map(|(k, v)| (*k, v.iter().map(|v| (*v).into()).collect()))
                .collect(),
            creation_time,
        };
        self.bus
            .senders
            .send_to_background_jobs(BackgroundJob::ReportSessionInfo(
                self.session_name.to_owned(),
                session_info.clone(),
            ))
            .with_context(err_context)?;

        self.peer_sessions_cache
            .insert(self.session_name.clone(), session_info);
        let mut live_sessions: Vec<SessionInfo> =
            self.peer_sessions_cache.values().cloned().collect();
        for info in live_sessions.iter_mut() {
            info.is_current_session = info.name == self.session_name;
        }
        let resurrectable_sessions: Vec<(String, Duration)> = self
            .resurrectable_sessions_cache
            .iter()
            .map(|(n, d)| (n.clone(), *d))
            .collect();
        self.bus
            .senders
            .send_to_plugin(PluginInstruction::Update(vec![(
                None,
                None,
                Event::SessionUpdate(live_sessions, resurrectable_sessions),
            )]))
            .with_context(err_context)?;

        self.bus
            .senders
            .send_to_background_jobs(BackgroundJob::QueryZellijWebServerStatus)
            .with_context(err_context)?;
        Ok(())
    }
    fn dump_layout_to_hd(&mut self) -> Result<()> {
        let err_context = || "Failed to log and report session state".to_string();
        let session_layout_metadata =
            self.get_layout_metadata(Some(self.default_shell.clone()), None);
        let generation =
            reserve_session_state_generation(&self.session_name).map_err(anyhow::Error::msg)?;
        self.bus
            .senders
            .send_to_plugin(PluginInstruction::LogLayoutToHd {
                session_name: self.session_name.clone(),
                generation,
                session_layout_metadata,
            })
            .with_context(err_context)?;

        Ok(())
    }
    pub fn update_session_infos(
        &mut self,
        new_session_infos: BTreeMap<String, SessionInfo>,
        resurrectable_sessions: BTreeMap<String, Duration>,
    ) -> Result<()> {
        self.peer_sessions_cache = new_session_infos;
        self.resurrectable_sessions_cache = resurrectable_sessions;
        self.bus
            .senders
            .send_to_plugin(PluginInstruction::Update(vec![(
                None,
                None,
                Event::SessionUpdate(
                    self.peer_sessions_cache.values().cloned().collect(),
                    self.resurrectable_sessions_cache
                        .iter()
                        .map(|(n, c)| (n.clone(), *c))
                        .collect(),
                ),
            )]))
            .context("failed to update session info")?;
        Ok(())
    }

    pub fn update_available_layouts(
        &mut self,
        layouts: Vec<LayoutInfo>,
        errors: Vec<LayoutWithError>,
    ) {
        self.cached_layouts = layouts;
        self.cached_layout_errors = errors;
    }

    pub fn update_active_tab_name(&mut self, buf: Vec<u8>, client_id: ClientId) -> Result<()> {
        let err_context =
            || format!("failed to update active tabs name for client id: {client_id:?}");

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };

        match client_id {
            Some(client_id) => {
                let s = str::from_utf8(&buf)
                    .with_context(|| format!("failed to construct tab name from buf: {buf:?}"))
                    .with_context(err_context)?;
                match self.get_active_tab_mut(client_id) {
                    Ok(active_tab) => {
                        match s {
                            "\0" => {
                                active_tab.name = String::new();
                            },
                            "\u{007F}" | "\u{0008}" => {
                                // delete and backspace keys
                                active_tab.name.pop();
                            },
                            c => {
                                active_tab
                                    .name
                                    .push_str(&clean_string_from_control_and_linebreak(c));
                            },
                        }
                        self.log_and_report_session_state()
                            .with_context(err_context)
                    },
                    Err(err) => {
                        Err::<(), _>(err).with_context(err_context).non_fatal();
                        Ok(())
                    },
                }
            },
            None => Ok(()),
        }
    }
    pub fn undo_active_rename_tab(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || format!("failed to undo active tab rename for client {}", client_id);

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };
        match client_id {
            Some(client_id) => {
                match self.get_active_tab_mut(client_id) {
                    Ok(active_tab) => {
                        if active_tab.name != active_tab.prev_name {
                            active_tab.name = active_tab.prev_name.clone();
                            self.log_and_report_session_state()
                                .context("failed to undo renaming of active tab")?;
                        }
                    },
                    Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
                };
                Ok(())
            },
            None => Ok(()),
        }
    }

    pub fn move_active_tab_to_left(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || "Failed to move active tab left";
        if self.tabs.len() < 2 {
            debug!("cannot move tab to left: only one tab exists");
            return Ok(());
        }
        let Some(client_id) = self.client_id(client_id) else {
            return Ok(());
        };

        match self.get_active_tab(client_id) {
            Ok(active_tab) => {
                let active_tab_pos = active_tab.position;
                let left_tab_pos = if active_tab_pos == 0 {
                    self.tabs.len() - 1
                } else {
                    active_tab_pos - 1
                };

                self.switch_tabs(active_tab_pos, left_tab_pos);
                self.log_and_report_session_state()
                    .context("failed to move tab to left")?;
            },
            Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
        }
        Ok(())
    }

    fn client_id(&mut self, client_id: ClientId) -> Option<u16> {
        if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        }
    }

    /// Switches tabs at two positions, swapping their display order.
    ///
    /// # Arguments
    /// * `active_tab_pos` - Current position of active tab (0-based)
    /// * `other_tab_pos` - Position to swap with (0-based)
    ///
    /// NOTE: this expects positions rather than IDs (see distinction at top of file)
    fn switch_tabs(&mut self, active_tab_pos: usize, other_tab_pos: usize) {
        let Some(active_tab_id) = self
            .tabs
            .values()
            .find(|t| t.position == active_tab_pos)
            .map(|t| t.id)
        else {
            log::error!("Failed to find active tab at position: {}", active_tab_pos);
            return;
        };
        let Some(other_tab_id) = self
            .tabs
            .values()
            .find(|t| t.position == other_tab_pos)
            .map(|t| t.id)
        else {
            log::error!(
                "Failed to find tab to switch to at position: {}",
                other_tab_pos
            );
            return;
        };

        if !self.tabs.contains_key(&active_tab_id) || !self.tabs.contains_key(&other_tab_id) {
            warn!(
                "failed to switch tabs: index {} or {} not found in {:?}",
                active_tab_id,
                other_tab_id,
                self.tabs.keys()
            );
            return;
        }

        // NOTE: Can `expect` here, because we checked that the keys exist above
        let mut active_tab = self
            .tabs
            .remove(&active_tab_id)
            .expect("active tab not found");
        let mut other_tab = self
            .tabs
            .remove(&other_tab_id)
            .expect("other tab not found");

        std::mem::swap(&mut active_tab.position, &mut other_tab.position);

        self.tabs.insert(active_tab_id, active_tab);
        self.tabs.insert(other_tab_id, other_tab);
    }

    pub fn move_active_tab_to_right(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || "Failed to move active tab right ";
        if self.tabs.len() < 2 {
            debug!("cannot move tab to right: only one tab exists");
            return Ok(());
        }
        let Some(client_id) = self.client_id(client_id) else {
            return Ok(());
        };

        match self.get_active_tab(client_id) {
            Ok(active_tab) => {
                let active_tab_pos = active_tab.position;
                let right_tab_pos = (active_tab_pos + 1) % self.tabs.len();

                self.switch_tabs(active_tab_pos, right_tab_pos);
                self.log_and_report_session_state()
                    .context("failed to move tab to the right")?;
            },
            Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
        }
        Ok(())
    }

    pub fn move_tab_by_id(&mut self, tab_id: usize, direction: Direction) -> Result<()> {
        if self.tabs.len() < 2 {
            return Ok(());
        }
        if let Some(tab) = self.tabs.get(&tab_id) {
            let tab_pos = tab.position;
            let swap_pos = match direction {
                Direction::Left => {
                    if tab_pos == 0 {
                        self.tabs.len() - 1
                    } else {
                        tab_pos - 1
                    }
                },
                Direction::Right => (tab_pos + 1) % self.tabs.len(),
                _ => return Ok(()),
            };
            self.switch_tabs(tab_pos, swap_pos);
            self.log_and_report_session_state()?;
        } else {
            log::error!("Tab with id {} not found", tab_id);
        }
        Ok(())
    }

    pub fn change_mode(
        &mut self,
        new_mode: InputMode,
        base_mode: Option<InputMode>,
        client_id: ClientId,
    ) -> Result<()> {
        let mut mode_info = self
            .mode_info
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| self.default_mode_info.clone());
        let previous_mode = mode_info.mode;
        mode_info.mode = new_mode;
        mode_info.base_mode = base_mode;
        if mode_info.session_name.as_ref() != Some(&self.session_name) {
            mode_info.session_name = Some(self.session_name.clone());
        }

        let err_context = || {
            format!(
                "failed to change from mode '{:?}' to mode '{:?}' for client {client_id}",
                previous_mode, new_mode
            )
        };

        // If we leave the Search-related modes, we need to clear all previous searches
        let search_related_modes = [InputMode::EnterSearch, InputMode::Search, InputMode::Scroll];
        if search_related_modes.contains(&previous_mode)
            && !search_related_modes.contains(&mode_info.mode)
        {
            active_tab!(self, client_id, |tab: &mut Tab| tab.clear_search(client_id));
        }

        if previous_mode == InputMode::Scroll
            && (mode_info.mode == InputMode::Normal || mode_info.mode == InputMode::Locked)
            && let Ok(active_tab) = self.get_active_tab_mut(client_id)
        {
            active_tab
                .clear_active_terminal_scroll(client_id)
                .with_context(err_context)?;
        }

        if mode_info.mode == InputMode::RenameTab
            && let Ok(active_tab) = self.get_active_tab_mut(client_id)
        {
            active_tab.prev_name = active_tab.name.clone();
        }

        if mode_info.mode == InputMode::RenamePane
            && let Ok(active_tab) = self.get_active_tab_mut(client_id)
            && let Some(active_pane) = active_tab.get_active_pane_or_floating_pane_mut(client_id)
        {
            active_pane.store_pane_name();
        }

        self.style = mode_info.style;
        self.mode_info.insert(client_id, mode_info.clone());
        for tab in self.tabs.values_mut() {
            tab.change_mode_info(mode_info.clone(), client_id);
            tab.mark_active_pane_for_rerender(client_id);
            tab.update_input_modes()?;
        }
        // Notify background plugins subscribed to ModeUpdate
        let mut bg_updates = vec![];
        for ((bg_pid, bg_cid), subs) in &self.background_plugin_subscriptions {
            if subs.contains(&EventType::ModeUpdate) && *bg_cid == client_id {
                bg_updates.push((
                    Some(*bg_pid),
                    Some(*bg_cid),
                    Event::ModeUpdate(mode_info.clone()),
                ));
            }
        }
        if !bg_updates.is_empty() {
            self.bus
                .senders
                .send_to_plugin(PluginInstruction::Update(bg_updates))
                .context("failed to update background plugins with mode info")?;
        }
        Ok(())
    }
    pub fn change_mode_for_all_clients(
        &mut self,
        new_mode: InputMode,
        base_mode: Option<InputMode>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to change input mode to {:?} for all clients",
                new_mode
            )
        };

        let connected_client_ids: Vec<ClientId> = self.active_tab_ids.keys().copied().collect();
        for client_id in connected_client_ids {
            self.change_mode(new_mode, base_mode, client_id)
                .with_context(err_context)?;
        }
        Ok(())
    }
    /// Collect plugin IDs that should receive a broadcast event for a given client.
    /// Returns plugin IDs from the client's active tab plus background plugins
    /// subscribed to the given event type.
    fn targeted_plugin_ids(&self, client_id: ClientId, event_type: EventType) -> Vec<PluginId> {
        let mut plugin_ids = Vec::new();
        // Active-tab plugins
        if let Some(active_tab_id) = self.active_tab_ids.get(&client_id)
            && let Some(tab) = self.tabs.get(active_tab_id)
        {
            plugin_ids.extend(tab.get_plugin_ids());
        }
        // Background plugins subscribed to this event type
        for ((bg_pid, bg_cid), subs) in &self.background_plugin_subscriptions {
            if subs.contains(&event_type) && *bg_cid == client_id && !plugin_ids.contains(bg_pid) {
                plugin_ids.push(*bg_pid);
            }
        }
        plugin_ids
    }
    /// Broadcast a ModeUpdate event to active-tab plugins and subscribed background plugins.
    pub fn broadcast_mode_update(
        &mut self,
        mode_info: ModeInfo,
        target_client_id: Option<ClientId>,
    ) -> Result<()> {
        let mut plugin_updates = vec![];
        let client_ids: Vec<ClientId> = if let Some(cid) = target_client_id {
            vec![cid]
        } else {
            self.active_tab_ids.keys().copied().collect()
        };
        for client_id in client_ids {
            let plugin_ids = self.targeted_plugin_ids(client_id, EventType::ModeUpdate);
            for plugin_id in plugin_ids {
                plugin_updates.push((
                    Some(plugin_id),
                    Some(client_id),
                    Event::ModeUpdate(mode_info.clone()),
                ));
            }
        }
        if !plugin_updates.is_empty() {
            self.bus
                .senders
                .send_to_plugin(PluginInstruction::Update(plugin_updates))
                .context("failed to broadcast mode update")?;
        }
        Ok(())
    }
    pub fn move_focus_left_or_previous_tab(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || {
            format!(
                "failed to move focus left or to previous tab for client {}",
                client_id
            )
        };

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };
        if let Some(client_id) = client_id {
            match self.get_active_tab_mut(client_id) {
                Ok(active_tab) => {
                    active_tab
                        .move_focus_left(client_id)
                        .and_then(|success| {
                            if !success {
                                self.switch_tab_prev(Some(Direction::Left), true, client_id)
                                    .context("failed to move focus to previous tab")
                            } else {
                                Ok(())
                            }
                        })
                        .with_context(err_context)?;
                },
                Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
            };
        }
        self.log_and_report_session_state()
            .with_context(err_context)?;
        Ok(())
    }
    pub fn move_focus_right_or_next_tab(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = || {
            format!(
                "failed to move focus right or to next tab for client {}",
                client_id
            )
        };

        let client_id = if self.get_active_tab(client_id).is_ok() {
            Some(client_id)
        } else {
            self.get_first_client_id()
        };

        if let Some(client_id) = client_id {
            match self.get_active_tab_mut(client_id) {
                Ok(active_tab) => {
                    active_tab
                        .move_focus_right(client_id)
                        .and_then(|success| {
                            if !success {
                                self.switch_tab_next(Some(Direction::Right), true, client_id)
                                    .context("failed to move focus to next tab")
                            } else {
                                Ok(())
                            }
                        })
                        .with_context(err_context)?;
                },
                Err(err) => Err::<(), _>(err).with_context(err_context).non_fatal(),
            };
        }
        self.log_and_report_session_state()
            .with_context(err_context)?;
        Ok(())
    }
    pub fn toggle_tab(&mut self, client_id: ClientId) -> Result<()> {
        let tab = self
            .get_previous_tab(client_id)
            .context("failed to toggle tabs")?;
        if let Some(t) = tab {
            let position = t.position;
            self.go_to_tab(position + 1, client_id)
                .context("failed to toggle tabs")?;
        };

        self.log_and_report_session_state()
            .context("failed to toggle tabs")?;
        self.render(None)
    }

    pub fn focus_plugin_pane(
        &mut self,
        run_plugin: &RunPluginOrAlias,
        should_float: bool,
        move_to_focused_tab: bool,
        should_be_in_place: bool,
        client_id: ClientId,
        completion_tx: &mut Option<NotificationEnd>,
    ) -> Result<bool> {
        // true => found and focused, false => not
        let err_context = || "failed to focus_plugin_pane".to_string();
        let mut tab_index_and_plugin_pane_id = None;
        let mut plugin_pane_to_move_to_active_tab = None;
        let focused_tab_index = *self.active_tab_ids.get(&client_id).unwrap_or(&0);
        let all_tabs = self.get_tabs_mut();
        for (tab_index, tab) in all_tabs.iter_mut() {
            if let Some(plugin_pane_id) = tab.find_plugin(run_plugin) {
                tab_index_and_plugin_pane_id = Some((*tab_index, plugin_pane_id));
                if move_to_focused_tab && focused_tab_index != *tab_index {
                    plugin_pane_to_move_to_active_tab = tab.extract_pane(plugin_pane_id, true);
                }

                break;
            }
        }
        if let Some(plugin_pane_to_move_to_active_tab) = plugin_pane_to_move_to_active_tab.take() {
            let pane_id = plugin_pane_to_move_to_active_tab.pid();
            let new_active_tab = self.get_active_tab_mut(client_id)?;

            if should_float {
                new_active_tab.show_floating_panes();
                new_active_tab.add_floating_pane(
                    plugin_pane_to_move_to_active_tab,
                    pane_id,
                    None,
                    true,
                )?;
            // TODO: also should_be_in_place
            } else {
                new_active_tab.hide_floating_panes();
                new_active_tab.add_tiled_pane(
                    plugin_pane_to_move_to_active_tab,
                    pane_id,
                    false,
                    Some(client_id),
                )?;
            }
            // Set affected pane ID for CLI client output
            if let Some(completion) = completion_tx {
                completion.set_affected_pane_id(pane_id);
            }
            return Ok(true);
        }
        match tab_index_and_plugin_pane_id {
            Some((tab_index, plugin_pane_id)) => {
                self.go_to_tab(tab_index + 1, client_id)?;
                self.tabs
                    .get_mut(&tab_index)
                    .with_context(err_context)?
                    .focus_pane_with_id(plugin_pane_id, should_float, should_be_in_place, client_id)
                    .context("failed to focus plugin pane")?;
                self.log_and_report_session_state()
                    .with_context(err_context)?;
                // Set affected pane ID for CLI client output
                if let Some(completion) = completion_tx {
                    completion.set_affected_pane_id(plugin_pane_id);
                }
                Ok(true)
            },
            None => Ok(false),
        }
    }

    pub fn focus_pane_with_id(
        &mut self,
        pane_id: PaneId,
        should_float_if_hidden: bool,
        should_open_in_place: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let err_context = || "failed to focus_plugin_pane".to_string();
        let tab_index = self
            .tabs
            .iter()
            .find(|(_tab_index, tab)| tab.has_pane_with_pid(&pane_id))
            .map(|(_tab_index, tab)| tab.position);
        match tab_index {
            Some(tab_index) => {
                self.go_to_tab(tab_index + 1, client_id)?;
                self.tabs
                    .iter_mut()
                    .find(|(_, t)| t.position == tab_index)
                    .map(|(_, t)| {
                        t.focus_pane_with_id(
                            pane_id,
                            should_float_if_hidden,
                            should_open_in_place,
                            client_id,
                        )
                    })
                    .with_context(err_context)
                    .non_fatal();
            },
            None => {
                log::error!("Could not find pane with id: {:?}", pane_id);
            },
        };
        Ok(())
    }
    pub fn rerun_command_pane_with_id(
        &mut self,
        terminal_pane_id: u32,
        completion_tx: Option<NotificationEnd>,
    ) {
        let mut found = false;
        for tab in self.tabs.values_mut() {
            if tab.has_pane_with_pid(&PaneId::Terminal(terminal_pane_id)) {
                tab.rerun_terminal_pane_with_id(terminal_pane_id, completion_tx);
                found = true;
                break;
            }
        }
        if !found {
            log::error!(
                "Failed to find terminal pane with id: {} to run",
                terminal_pane_id
            );
        }
    }
    pub fn resize_pane_with_id(&mut self, resize: ResizeStrategy, pane_id: PaneId) {
        let mut found = false;
        for tab in self.tabs.values_mut() {
            if tab.has_pane_with_pid(&pane_id) {
                tab.resize_pane_with_id(resize, pane_id).non_fatal();
                found = true;
                break;
            }
        }
        if !found {
            log::error!("Failed to find pane with id: {:?} to resize", pane_id);
        }
    }
    pub fn break_pane(
        &mut self,
        default_shell: Option<TerminalAction>,
        default_layout: Box<Layout>,
        client_id: ClientId,
        mut completion_tx: Option<NotificationEnd>,
    ) -> Result<Option<BreakPaneTransfer>> {
        let err_context = || "failed break pane out of tab".to_string();
        if let Some(completion) = completion_tx.as_mut() {
            completion.require_explicit_resolution();
        }
        let (source_tab_id, active_pane_id, active_pane_run_instruction) = {
            let active_tab = self.get_active_tab_mut(client_id)?;
            if active_tab.get_selectable_tiled_panes_count() <= 1
                && active_tab.get_visible_selectable_floating_panes_count() == 0
            {
                let active_pane_id =
                    active_tab.get_active_pane_id(client_id).with_context(|| {
                        format!("active pane disappeared before break for client {client_id}")
                    })?;
                let message = "Cannot break single pane out!";
                if let Some(completion) = completion_tx.as_mut() {
                    completion.mark_failure(message);
                }
                self.bus
                    .senders
                    .send_to_background_jobs(BackgroundJob::DisplayPaneError(
                        vec![active_pane_id],
                        message.into(),
                    ))
                    .with_context(err_context)?;
                return Ok(None);
            }
            let active_pane_id = active_tab.get_active_pane_id(client_id).with_context(|| {
                format!("active pane disappeared before break for client {client_id}")
            })?;
            let active_pane = active_tab
                .get_pane_with_id(active_pane_id)
                .with_context(|| {
                    format!("active pane {active_pane_id:?} disappeared before break metadata read")
                })?;
            let active_pane_run_instruction = active_pane.invoked_with().clone();
            (active_tab.id, active_pane_id, active_pane_run_instruction)
        };
        if let Err(error) = self.ensure_render_fence_tabs_are_available(&[source_tab_id]) {
            let message = format!("cannot start break-pane transaction: {error:#}");
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message.clone());
            }
            return Err(anyhow!(message));
        }

        let tab_index = self.get_new_tab_id();
        tab_index
            .checked_add(1)
            .ok_or_else(|| anyhow!("stable tab ID space exhausted"))?;
        self.bus
            .os_input
            .as_ref()
            .context("Screen OS input disappeared before break destination creation")?;

        // Preserve the established extraction -> destination ordering so the
        // render stream never exposes a blank destination tab. The two
        // fallible new-tab prerequisites above are the same ones `new_tab`
        // resolves before mutation.
        let mut source_transaction = Some(TabTopologyTransaction::begin(
            self.tabs
                .get_mut(&source_tab_id)
                .context("source tab disappeared before break transaction snapshot")?,
        ));
        let active_pane = self
            .tabs
            .get_mut(&source_tab_id)
            .and_then(|tab| tab.extract_pane(active_pane_id, false));
        let Some(active_pane) = active_pane else {
            if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
                source_transaction
                    .take()
                    .unwrap()
                    .rollback(source_tab, BTreeMap::new());
            }
            bail!("source pane {active_pane_id:?} disappeared before break extraction");
        };
        let swap_layouts = (
            default_layout.swap_tiled_layouts.clone(),
            default_layout.swap_floating_layouts.clone(),
        );
        if let Err(error) = self.new_tab(
            tab_index,
            swap_layouts,
            None,
            Some(client_id),
            TabPlacement::Append,
        ) {
            let source_tab = self
                .tabs
                .get_mut(&source_tab_id)
                .context("source tab disappeared while rolling back failed break-pane creation")?;
            source_transaction
                .take()
                .unwrap()
                .rollback(source_tab, BTreeMap::from([(active_pane_id, active_pane)]));
            return Err(error).with_context(err_context);
        }
        let (mut tiled_panes_layout, floating_panes_layout) = default_layout.new_tab();
        let without_relayout = true;
        let rejected_pane = self
            .tabs
            .get_mut(&tab_index)
            .with_context(|| {
                format!("break destination tab {tab_index} disappeared immediately after creation")
            })?
            .try_add_tiled_pane_retaining_ownership(
                active_pane,
                active_pane_id,
                without_relayout,
                Some(client_id),
            )?;
        if let Some(rejected_pane) = rejected_pane {
            let source_tab = self.tabs.get_mut(&source_tab_id).context(
                "source tab disappeared while rolling back rejected break-pane admission",
            )?;
            source_transaction.take().unwrap().rollback(
                source_tab,
                BTreeMap::from([(active_pane_id, rejected_pane)]),
            );
            self.discard_pending_tab_after_layout_rejection(tab_index)
                .non_fatal();
            let message =
                format!("break destination tab {tab_index} has no room for {active_pane_id:?}");
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message.clone());
            }
            bail!(message);
        }
        tiled_panes_layout.ignore_run_instruction(active_pane_run_instruction);
        let should_change_focus_to_new_tab = true;
        let is_web_client = self
            .connected_clients
            .borrow()
            .get(&client_id)
            .copied()
            .unwrap_or(false);
        let transaction_id = self.reserve_layout_transaction_id();
        let target = LayoutTabOwner::capture(self, tab_index);
        let source_render_fence = LayoutTabOwner::capture(self, source_tab_id);
        let transaction = ActiveLayoutTransaction {
            kind: ScreenLayoutTransactionKind::BreakPane,
            targets: vec![target.clone()],
            created_pending_tabs: vec![target],
            render_fenced_tabs: vec![source_render_fence],
            tabs_to_close_after_commit: vec![],
            moved_original_panes: vec![active_pane_id],
            generation: None,
        };
        if let Err(error) = self.register_layout_transaction(transaction_id, transaction.clone()) {
            let pane = self
                .tabs
                .get_mut(&tab_index)
                .and_then(|tab| tab.extract_pane(active_pane_id, true));
            match pane {
                Some(pane) => {
                    if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
                        source_transaction
                            .take()
                            .unwrap()
                            .rollback(source_tab, BTreeMap::from([(active_pane_id, pane)]));
                        self.discard_pending_tab_after_layout_rejection(tab_index)
                            .non_fatal();
                    } else {
                        source_transaction.take();
                        let recovery_geom = PaneGeom::from(&self.size);
                        if let Some(destination) = self.tabs.get_mut(&tab_index) {
                            destination.restore_extracted_pane(
                                pane,
                                active_pane_id,
                                false,
                                recovery_geom,
                            );
                        }
                        let mut no_pending_gate = HashSet::new();
                        self.activate_degraded_break_tab(&transaction, &mut no_pending_gate)
                            .non_fatal();
                    }
                },
                None => {
                    if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
                        source_transaction.take().unwrap().commit(source_tab);
                    } else {
                        source_transaction.take();
                    }
                    let mut no_pending_gate = HashSet::new();
                    self.activate_degraded_break_tab(&transaction, &mut no_pending_gate)
                        .non_fatal();
                },
            }
            let message = format!(
                "failed to register break-pane layout transaction {transaction_id}: {error:#}"
            );
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message.clone());
            }
            self.render(None).non_fatal();
            return Err(anyhow!(message));
        }
        if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
            source_transaction.take().unwrap().commit(source_tab);
        } else {
            source_transaction.take();
        }
        if let Some(pane) = self
            .tabs
            .get_mut(&tab_index)
            .and_then(|tab| tab.get_pane_with_id_mut(active_pane_id))
        {
            pane.commit_layout_transaction();
        }
        if let Some(completion) = completion_tx.as_mut() {
            completion.set_affected_tab_id(tab_index);
            completion.set_affected_pane_id(active_pane_id);
        }
        let instruction = PluginInstruction::NewTab(
            None,
            default_shell,
            Some(tiled_panes_layout),
            floating_panes_layout,
            tab_index,
            transaction_id,
            None,  // initial_panes
            false, // block_on_first_terminal
            should_change_focus_to_new_tab,
            (client_id, is_web_client),
            completion_tx,
            None,
        );
        if let Err(send_failure) = self.bus.senders.send_to_plugin_recover(instruction) {
            let (instruction, error) = send_failure.into_parts();
            let (mut recovered_completion, recovered_expected_kind) = match instruction {
                PluginInstruction::NewTab(
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    recovered_completion,
                    _,
                ) => (recovered_completion, true),
                _ => (None, false),
            };
            self.active_layout_transactions.remove(&transaction_id);
            let message = if recovered_expected_kind {
                format!(
                    "failed to hand break-pane layout transaction {transaction_id} to Plugin: {error:#}"
                )
            } else {
                format!(
                    "Plugin handoff returned an unexpected instruction while rejecting break-pane layout transaction {transaction_id}: {error:#}"
                )
            };
            if let Some(completion) = recovered_completion.as_mut() {
                completion.mark_failure(message.clone());
            }
            let mut no_pending_gate = HashSet::new();
            self.activate_degraded_break_tab(&transaction, &mut no_pending_gate)
                .with_context(|| {
                    format!("{message}; failed to preserve moved pane in degraded destination tab")
                })?;
            self.render(None).non_fatal();
            return Err(anyhow!(message));
        }
        Ok(Some(BreakPaneTransfer {
            destination_tab_id: tab_index,
            source_tab_ids: vec![source_tab_id],
        }))
    }
    pub fn break_multiple_panes_to_new_tab(
        &mut self,
        pane_ids: Vec<PaneId>,
        default_shell: Option<TerminalAction>,
        should_change_focus_to_new_tab: bool,
        new_tab_name: Option<String>,
        client_id: ClientId,
        mut completion_tx: Option<NotificationEnd>,
    ) -> Result<BreakPaneTransfer> {
        let err_context = || "failed break multiple panes to a new tab".to_string();
        if let Some(completion) = completion_tx.as_mut() {
            completion.require_explicit_resolution();
        }

        let mut seen_pane_ids = HashSet::new();
        let located_panes = pane_ids
            .iter()
            .copied()
            .filter(|pane_id| seen_pane_ids.insert(*pane_id))
            .filter_map(|pane_id| {
                self.tabs
                    .iter()
                    .find(|(_, tab)| tab.has_pane_with_pid(&pane_id))
                    .map(|(source_tab_id, _)| (pane_id, *source_tab_id))
            })
            .collect::<Vec<_>>();
        if located_panes.is_empty() {
            let message = "none of the requested panes existed";
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message);
            }
            bail!(message);
        }
        let mut source_tab_ids = located_panes
            .iter()
            .map(|(_, source_tab_id)| *source_tab_id)
            .collect::<Vec<_>>();
        source_tab_ids.sort_unstable();
        source_tab_ids.dedup();
        if let Err(error) = self.ensure_render_fence_tabs_are_available(&source_tab_ids) {
            let message = format!("cannot start break-multiple transaction: {error:#}");
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message.clone());
            }
            return Err(anyhow!(message));
        }

        let (mut tiled_panes_layout, floating_panes_layout) = self.default_layout.new_tab();
        let tab_index = self.get_new_tab_id();
        let swap_layouts = (
            self.default_layout.swap_tiled_layouts.clone(),
            self.default_layout.swap_floating_layouts.clone(),
        );
        if should_change_focus_to_new_tab {
            self.new_tab(
                tab_index,
                swap_layouts,
                None,
                Some(client_id),
                TabPlacement::Append,
            )?;
        } else {
            self.new_tab(tab_index, swap_layouts, None, None, TabPlacement::Append)?;
        }
        let tab = self.tabs.get_mut(&tab_index).with_context(err_context)?;
        if let Some(new_tab_name) = new_tab_name {
            tab.name = new_tab_name.clone();
        }
        // Hold the destination locally while extraction, admission and rollback
        // run. It is the unconditional recovery owner if a source disappears.
        let mut destination_tab = self.tabs.remove(&tab_index).with_context(err_context)?;
        let mut source_transactions = BTreeMap::new();
        for source_tab_id in &source_tab_ids {
            let source_tab = self
                .tabs
                .get_mut(source_tab_id)
                .with_context(|| format!("break source tab {source_tab_id} disappeared"))?;
            source_transactions.insert(*source_tab_id, TabTopologyTransaction::begin(source_tab));
        }
        let mut extracted_panes = vec![];
        let mut extracted_pane_ids = vec![];
        for (pane_id, source_tab_id) in located_panes {
            let extraction_result = (|| {
                let source_tab = self
                    .tabs
                    .get_mut(&source_tab_id)
                    .with_context(|| format!("break source tab {source_tab_id} disappeared"))?;
                let was_floating = source_tab.pane_id_is_floating(&pane_id);
                let original_geom = source_tab
                    .get_pane_with_id(pane_id)
                    .map(|pane| pane.position_and_size())
                    .with_context(|| {
                        format!("break source pane {pane_id:?} disappeared before extraction")
                    })?;
                let pane = source_tab.extract_pane(pane_id, true).with_context(|| {
                    format!("break source pane {pane_id:?} disappeared during extraction")
                })?;
                Ok::<_, anyhow::Error>(ExtractedBreakPane {
                    source_tab_id,
                    was_floating,
                    original_geom,
                    pane,
                })
            })();
            let extracted = match extraction_result {
                Ok(extracted) => extracted,
                Err(error) => {
                    let failures = self.rollback_break_source_transactions(
                        source_transactions,
                        extracted_panes,
                        &mut destination_tab,
                    );
                    self.tabs.insert(tab_index, destination_tab);
                    self.discard_pending_tab_after_layout_rejection(tab_index)
                        .non_fatal();
                    let message = format!(
                        "{error:#}{}",
                        if failures.is_empty() {
                            String::new()
                        } else {
                            format!("; rollback failures: {}", failures.join("; "))
                        }
                    );
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    bail!(message);
                },
            };
            extracted_pane_ids.push(pane_id);
            extracted_panes.push(extracted);
        }
        let mut inserted_panes = vec![];
        let mut extracted_panes = extracted_panes.into_iter();
        while let Some(mut extracted) = extracted_panes.next() {
            let run_instruction = extracted.pane.invoked_with().clone();
            let pane_id = extracted.pane.pid();
            let without_relayout = true;

            // we reset the pane geom here to screen size so that we won't have trouble adding it
            // temporarily to the new tab (eg. if it was stacked or had a fixed size), the size
            // will be adjusted before the next render, further down the pipeline, when we apply
            // the layout to this new tab
            let new_geom = PaneGeom::from(&self.size);
            extracted.pane.set_geom(new_geom);

            // here we pass None instead of the ClientId, because we do not want this pane to be
            // necessarily focused
            let rejected_pane = match destination_tab.try_add_tiled_pane_retaining_ownership(
                extracted.pane,
                pane_id,
                without_relayout,
                None,
            ) {
                Ok(rejected_pane) => rejected_pane,
                Err(error) => {
                    let mut panes_to_restore = vec![];
                    let mut restoration_failures = vec![];
                    if let Some(pane) = destination_tab.extract_pane(pane_id, true) {
                        panes_to_restore.push(ExtractedBreakPane {
                            source_tab_id: extracted.source_tab_id,
                            was_floating: extracted.was_floating,
                            original_geom: extracted.original_geom,
                            pane,
                        });
                    } else {
                        restoration_failures.push(format!(
                            "destination lost pane {pane_id:?} after failed admission"
                        ));
                    }
                    panes_to_restore.extend(extracted_panes);
                    for (inserted_id, source_tab_id, was_floating, original_geom) in
                        inserted_panes.into_iter().rev()
                    {
                        if let Some(pane) = destination_tab.extract_pane(inserted_id, true) {
                            panes_to_restore.push(ExtractedBreakPane {
                                source_tab_id,
                                was_floating,
                                original_geom,
                                pane,
                            });
                        } else {
                            restoration_failures.push(format!(
                                "destination lost inserted pane {inserted_id:?} before admission rollback"
                            ));
                        }
                    }
                    restoration_failures.extend(self.rollback_break_source_transactions(
                        source_transactions,
                        panes_to_restore,
                        &mut destination_tab,
                    ));
                    if !restoration_failures.is_empty() {
                        destination_tab
                            .activate_degraded_pending_layout()
                            .non_fatal();
                    }
                    self.tabs.insert(tab_index, destination_tab);
                    if restoration_failures.is_empty() {
                        self.discard_pending_tab_after_layout_rejection(tab_index)
                            .non_fatal();
                    }
                    let message = format!(
                        "failed to admit pane {pane_id:?} into break destination{}: {error:#}",
                        if restoration_failures.is_empty() {
                            String::new()
                        } else {
                            format!(
                                "; retained degraded ownership after {}",
                                restoration_failures.join("; ")
                            )
                        }
                    );
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    return Err(anyhow!(message)).with_context(err_context);
                },
            };
            if let Some(rejected_pane) = rejected_pane {
                extracted.pane = rejected_pane;
                let mut panes_to_restore = vec![extracted];
                panes_to_restore.extend(extracted_panes);

                let mut restoration_failures = vec![];
                for (inserted_id, source_tab_id, was_floating, original_geom) in
                    inserted_panes.into_iter().rev()
                {
                    if let Some(pane) = destination_tab.extract_pane(inserted_id, true) {
                        panes_to_restore.push(ExtractedBreakPane {
                            source_tab_id,
                            was_floating,
                            original_geom,
                            pane,
                        });
                    } else {
                        restoration_failures.push(format!(
                            "destination lost inserted pane {inserted_id:?} before rollback"
                        ));
                    }
                }
                restoration_failures.extend(self.rollback_break_source_transactions(
                    source_transactions,
                    panes_to_restore,
                    &mut destination_tab,
                ));

                let message = if restoration_failures.is_empty() {
                    format!(
                        "break destination tab {tab_index} has no room for pane {pane_id:?}; restored every extracted pane to its source"
                    )
                } else {
                    destination_tab
                        .activate_degraded_pending_layout()
                        .non_fatal();
                    format!(
                        "break destination tab {tab_index} has no room for pane {pane_id:?}; retained ownership with degraded recovery: {}",
                        restoration_failures.join("; ")
                    )
                };
                self.tabs.insert(tab_index, destination_tab);
                if restoration_failures.is_empty() {
                    self.discard_pending_tab_after_layout_rejection(tab_index)
                        .non_fatal();
                }
                if let Some(completion) = completion_tx.as_mut() {
                    completion.mark_failure(message.clone());
                }
                bail!(message);
            }
            inserted_panes.push((
                pane_id,
                extracted.source_tab_id,
                extracted.was_floating,
                extracted.original_geom,
            ));
            tiled_panes_layout.ignore_run_instruction(run_instruction);
        }
        self.tabs.insert(tab_index, destination_tab);
        let is_web_client = self
            .connected_clients
            .borrow()
            .get(&client_id)
            .copied()
            .unwrap_or(false);
        let transaction_id = self.reserve_layout_transaction_id();
        let target = LayoutTabOwner::capture(self, tab_index);
        let render_fenced_tabs = source_tab_ids
            .iter()
            .map(|source_tab_id| LayoutTabOwner::capture(self, *source_tab_id))
            .collect();
        let transaction = ActiveLayoutTransaction {
            kind: ScreenLayoutTransactionKind::BreakPane,
            targets: vec![target.clone()],
            created_pending_tabs: vec![target],
            render_fenced_tabs,
            tabs_to_close_after_commit: vec![],
            moved_original_panes: extracted_pane_ids.clone(),
            generation: None,
        };
        if let Err(error) = self.register_layout_transaction(transaction_id, transaction.clone()) {
            let mut destination_tab = self
                .tabs
                .remove(&tab_index)
                .context("break destination disappeared during registration rollback")?;
            let mut restoration_failures = vec![];
            let mut panes_to_restore = vec![];
            for (pane_id, source_tab_id, was_floating, original_geom) in
                inserted_panes.into_iter().rev()
            {
                if let Some(pane) = destination_tab.extract_pane(pane_id, true) {
                    panes_to_restore.push(ExtractedBreakPane {
                        source_tab_id,
                        was_floating,
                        original_geom,
                        pane,
                    });
                } else {
                    restoration_failures.push(format!(
                        "destination lost pane {pane_id:?} before registration rollback"
                    ));
                }
            }
            restoration_failures.extend(self.rollback_break_source_transactions(
                source_transactions,
                panes_to_restore,
                &mut destination_tab,
            ));
            if !restoration_failures.is_empty() {
                destination_tab
                    .activate_degraded_pending_layout()
                    .non_fatal();
            }
            self.tabs.insert(tab_index, destination_tab);
            if restoration_failures.is_empty() {
                self.discard_pending_tab_after_layout_rejection(tab_index)
                    .non_fatal();
            }
            let message = if restoration_failures.is_empty() {
                format!(
                    "failed to register break-multiple layout transaction {transaction_id}; restored every extracted pane: {error:#}"
                )
            } else {
                format!(
                    "failed to register break-multiple layout transaction {transaction_id}; retained exact ownership with degraded recovery ({}): {error:#}",
                    restoration_failures.join("; ")
                )
            };
            if let Some(completion) = completion_tx.as_mut() {
                completion.mark_failure(message.clone());
            }
            self.render(None).non_fatal();
            return Err(anyhow!(message));
        }
        for (source_tab_id, source_transaction) in source_transactions {
            if let Some(source_tab) = self.tabs.get_mut(&source_tab_id) {
                source_transaction.commit(source_tab);
            }
        }
        if let Some(destination_tab) = self.tabs.get_mut(&tab_index) {
            for pane_id in &extracted_pane_ids {
                if let Some(pane) = destination_tab.get_pane_with_id_mut(*pane_id) {
                    pane.commit_layout_transaction();
                }
            }
        }
        if let Some(completion) = completion_tx.as_mut() {
            completion.set_affected_tab_id(tab_index);
            if let Some(first_pane_id) = extracted_pane_ids.first() {
                completion.set_affected_pane_id(*first_pane_id);
            }
        }
        let instruction = PluginInstruction::NewTab(
            None,
            default_shell,
            Some(tiled_panes_layout),
            floating_panes_layout,
            tab_index,
            transaction_id,
            None,  // initial_panes
            false, // block_on_first_terminal
            should_change_focus_to_new_tab,
            (client_id, is_web_client),
            completion_tx,
            None,
        );
        if let Err(send_failure) = self.bus.senders.send_to_plugin_recover(instruction) {
            let (instruction, error) = send_failure.into_parts();
            let (mut recovered_completion, recovered_expected_kind) = match instruction {
                PluginInstruction::NewTab(
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    recovered_completion,
                    _,
                ) => (recovered_completion, true),
                _ => (None, false),
            };
            let transaction = self
                .active_layout_transactions
                .remove(&transaction_id)
                .context("break-multiple transaction disappeared during failed handoff")?;
            let message = if recovered_expected_kind {
                format!(
                    "failed to hand break-multiple-panes layout transaction {transaction_id} to Plugin: {error:#}"
                )
            } else {
                format!(
                    "Plugin handoff returned an unexpected instruction while rejecting break-multiple layout transaction {transaction_id}: {error:#}"
                )
            };
            if let Some(completion) = recovered_completion.as_mut() {
                completion.mark_failure(message.clone());
            }
            let mut no_pending_gate = HashSet::new();
            self.activate_degraded_break_tab(&transaction, &mut no_pending_gate)
                .with_context(|| {
                    format!("{message}; failed to preserve moved panes in degraded destination tab")
                })?;
            self.render(None).non_fatal();
            return Err(anyhow!(message));
        }
        Ok(BreakPaneTransfer {
            destination_tab_id: tab_index,
            source_tab_ids,
        })
    }
    pub fn break_pane_to_new_tab(
        &mut self,
        direction: Direction,
        client_id: ClientId,
    ) -> Result<()> {
        let err_context = || "failed break pane out of tab".to_string();
        if self.tabs.len() > 1 {
            let (active_pane_id, active_pane, pane_to_break_is_floating) = {
                let active_tab = self.get_active_tab_mut(client_id)?;
                let active_pane_id = active_tab
                    .get_active_pane_id(client_id)
                    .with_context(err_context)?;
                let pane_to_break_is_floating = active_tab.are_floating_panes_visible();
                let active_pane = active_tab
                    .extract_pane(active_pane_id, false)
                    .with_context(err_context)?;
                (active_pane_id, active_pane, pane_to_break_is_floating)
            };
            let update_mode_infos = false;
            match direction {
                Direction::Right | Direction::Down => {
                    self.switch_tab_next(None, update_mode_infos, client_id)?;
                },
                Direction::Left | Direction::Up => {
                    self.switch_tab_prev(None, update_mode_infos, client_id)?;
                },
            };
            let new_active_tab = self.get_active_tab_mut(client_id)?;

            if pane_to_break_is_floating {
                new_active_tab.show_floating_panes();
                new_active_tab.add_floating_pane(active_pane, active_pane_id, None, true)?;
            } else {
                new_active_tab.hide_floating_panes();
                new_active_tab.add_tiled_pane(
                    active_pane,
                    active_pane_id,
                    false,
                    Some(client_id),
                )?;
            }

            self.log_and_report_session_state()?;
        } else {
            let active_pane_id = {
                let active_tab = self.get_active_tab_mut(client_id)?;
                active_tab
                    .get_active_pane_id(client_id)
                    .with_context(err_context)?
            };
            self.bus
                .senders
                .send_to_background_jobs(BackgroundJob::DisplayPaneError(
                    vec![active_pane_id],
                    "No other tabs to add pane to!".into(),
                ))
                .with_context(err_context)?;
        }
        self.render(None)?;
        Ok(())
    }
    pub fn break_multiple_panes_to_tab_with_index(
        &mut self,
        pane_ids: Vec<PaneId>,
        tab_index: usize,
        should_change_focus_to_new_tab: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let all_tabs = self.get_tabs_mut();
        let has_tab_with_index = all_tabs.values().any(|t| t.position == tab_index);
        if !has_tab_with_index {
            log::error!("Cannot find tab with index: {tab_index}");
            return Ok(());
        }
        let mut extracted_panes = vec![];
        for pane_id in pane_ids {
            for tab in all_tabs.values_mut() {
                if tab.position == tab_index {
                    continue;
                }
                // here we pass None instead of the client_id we have because we do not need to
                // necessarily trigger a relayout for this tab
                let pane_was_floating = tab.pane_id_is_floating(&pane_id);
                if let Some(pane) = tab.extract_pane(pane_id, true) {
                    extracted_panes.push((pane_was_floating, pane));
                    break;
                }
            }
        }

        if should_change_focus_to_new_tab {
            self.go_to_tab(tab_index + 1, client_id)?;
        }
        if extracted_panes.is_empty() {
            // nothing to do here...
            return Ok(());
        }
        let screen_size = self.size;
        if let Some(new_active_tab) = self.get_indexed_tab_mut(tab_index) {
            for (pane_was_floating, mut pane) in extracted_panes {
                let pane_id = pane.pid();
                if pane_was_floating {
                    let floating_pane_coordinates = FloatingPaneCoordinates {
                        x: Some(PercentOrFixed::Fixed(pane.x())),
                        y: Some(PercentOrFixed::Fixed(pane.y())),
                        width: Some(PercentOrFixed::Fixed(pane.cols())),
                        height: Some(PercentOrFixed::Fixed(pane.rows())),
                        pinned: Some(pane.current_geom().is_pinned),
                        borderless: Some(pane.borderless()),
                    };
                    new_active_tab.add_floating_pane(
                        pane,
                        pane_id,
                        Some(floating_pane_coordinates),
                        false,
                    )?;
                } else {
                    // here we pass None instead of the ClientId, because we do not want this pane to be
                    // necessarily focused

                    // we reset the pane geom here to screen size so that we won't have trouble adding it
                    // temporarily to the new tab (eg. if it was stacked or had a fixed size), the size
                    // will be adjusted before the next render, further down the pipeline, when we apply
                    // the layout to this new tab
                    let new_geom = PaneGeom::from(&screen_size);
                    pane.set_geom(new_geom);

                    new_active_tab.add_tiled_pane(pane, pane_id, false, None)?;
                }
            }
        } else {
            log::error!("Could not find tab with index: {:?}", tab_index);
        }
        self.log_and_report_session_state()?;
        Ok(())
    }
    pub fn replace_pane(
        &mut self,
        new_pane_id: PaneId,
        hold_for_command: HoldForCommand,
        run: Option<Run>,
        pane_title: Option<InitialTitle>,
        close_replaced_pane: bool,
        client_id_tab_index_or_pane_id: ClientTabIndexOrPaneId,
    ) -> Result<()> {
        let suppress_pane = |tab: &mut Tab, pane_id: PaneId, new_pane_id: PaneId| {
            let _ = tab.suppress_pane_and_replace_with_pid(
                pane_id,
                new_pane_id,
                close_replaced_pane,
                run,
                None,
                None,
            );
            if let Some(pane_title) = pane_title {
                let _ = tab.rename_pane(pane_title.as_bytes().to_vec(), new_pane_id);
            }
            if let Some(hold_for_command) = hold_for_command {
                let is_first_run = true;
                tab.hold_pane(new_pane_id, None, is_first_run, hold_for_command)
            }
        };
        match client_id_tab_index_or_pane_id {
            ClientTabIndexOrPaneId::ClientId(client_id) => {
                active_tab!(self, client_id, |tab: &mut Tab| {
                    match tab.get_active_pane_id(client_id) {
                        Some(pane_id) => {
                            suppress_pane(tab, pane_id, new_pane_id);
                        },
                        None => {
                            log::error!(
                                "Failed to find active pane for client id: {:?}",
                                client_id
                            );
                        },
                    }
                });
            },
            ClientTabIndexOrPaneId::PaneId(pane_id) => {
                let tab_index = self
                    .tabs
                    .iter()
                    .find(|(_tab_index, tab)| tab.has_pane_with_pid(&pane_id))
                    .map(|(_tab_index, tab)| tab.position);
                match tab_index {
                    Some(tab_index) => {
                        if let Some(tab) =
                            self.tabs.iter_mut().find(|(_, t)| t.position == tab_index)
                        {
                            suppress_pane(tab.1, pane_id, new_pane_id);
                        }
                    },
                    None => {
                        log::error!("Could not find pane with id: {:?}", pane_id);
                    },
                };
            },
            ClientTabIndexOrPaneId::TabIndex(_tab_index) => {
                log::error!("Cannot replace pane with tab index");
            },
        }
        Ok(())
    }
    pub fn replace_pane_with_existing_pane(
        &mut self,
        pane_id_to_replace: PaneId,
        pane_id_of_existing_pane: PaneId,
        suppress_replaced_pane: bool,
        _completion_tx: Option<NotificationEnd>, // ends here
    ) {
        let Some(tab_index_of_pane_id_to_replace) = self
            .tabs
            .iter()
            .find(|(_tab_index, tab)| tab.has_pane_with_pid(&pane_id_to_replace))
            .map(|(_tab_index, tab)| tab.position)
        else {
            log::error!(
                "Could not find tab with pane_id: {:?} to replace",
                pane_id_to_replace
            );
            return;
        };
        let Some(tab_index_of_existing_pane) = self
            .tabs
            .iter()
            .find(|(_tab_index, tab)| tab.has_pane_with_pid(&pane_id_of_existing_pane))
            .map(|(_tab_index, tab)| tab.position)
        else {
            log::error!(
                "Could not find tab with pane_id: {:?} to be replaced by",
                pane_id_of_existing_pane
            );
            return;
        };
        let Some(extracted_pane_from_other_tab) = self
            .tabs
            .iter_mut()
            .find(|(_, t)| t.position == tab_index_of_existing_pane)
            .and_then(|(_, t)| t.extract_pane(pane_id_of_existing_pane, true))
        else {
            log::error!("Failed to find pane");
            return;
        };
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|(_, t)| t.position == tab_index_of_pane_id_to_replace)
        {
            if suppress_replaced_pane {
                tab.1.suppress_pane_and_replace_with_other_pane(
                    pane_id_to_replace,
                    extracted_pane_from_other_tab,
                    None,
                );
            } else {
                tab.1.close_pane_and_replace_with_other_pane(
                    pane_id_to_replace,
                    extracted_pane_from_other_tab,
                    None,
                );
            }
        }
        let _ = self.log_and_report_session_state();
    }
    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    pub fn reconfigure(
        &mut self,
        new_keybinds: Keybinds,
        new_default_mode: InputMode,
        theme: Styling,
        simplified_ui: bool,
        default_shell: Option<PathBuf>,
        pane_frames: bool,
        copy_command: Option<String>,
        copy_to_clipboard: Option<Clipboard>,
        copy_on_select: bool,
        auto_layout: bool,
        rounded_corners: bool,
        hide_session_name: bool,
        stacked_resize: bool,
        default_editor: Option<PathBuf>,
        advanced_mouse_actions: bool,
        mouse_hover_effects: bool,
        visual_bell: bool,
        focus_follows_mouse: bool,
        mouse_click_through: bool,
        client_id: ClientId,
    ) -> Result<()> {
        let should_support_arrow_fonts = !simplified_ui;

        // global configuration
        self.default_mode_info.update_theme(theme);
        self.default_mode_info
            .update_rounded_corners(rounded_corners);
        // `default_mode_info` is the fallback used by `change_mode` for
        // clients that don't yet have a per-client `mode_info` entry, so its
        // keybinds and base mode must be kept in sync with reconfigures.
        self.default_mode_info.update_keybinds(new_keybinds.clone());
        self.default_mode_info.update_default_mode(new_default_mode);
        self.default_shell = default_shell.clone().unwrap_or_else(get_default_shell);
        self.default_editor = default_editor.clone().or_else(get_default_editor);
        self.auto_layout = auto_layout;
        self.copy_options.command = copy_command.clone();
        self.copy_options.copy_on_select = copy_on_select;
        self.draw_pane_frames = pane_frames;
        self.advanced_mouse_actions = advanced_mouse_actions;
        self.mouse_hover_effects = mouse_hover_effects;
        self.visual_bell = visual_bell;
        self.focus_follows_mouse = focus_follows_mouse;
        self.mouse_click_through = mouse_click_through;
        self.default_mode_info
            .update_arrow_fonts(should_support_arrow_fonts);
        self.default_mode_info
            .update_hide_session_name(hide_session_name);
        {
            *self.stacked_resize.borrow_mut() = stacked_resize;
        }
        if let Some(copy_to_clipboard) = copy_to_clipboard {
            self.copy_options.clipboard = copy_to_clipboard;
        }
        for tab in self.tabs.values_mut() {
            tab.update_theme(theme);
            tab.update_rounded_corners(rounded_corners);
            tab.update_default_shell(default_shell.clone());
            tab.update_default_editor(self.default_editor.clone());
            tab.update_auto_layout(auto_layout);
            tab.update_copy_options(&self.copy_options);
            tab.set_pane_frames(pane_frames);
            tab.update_arrow_fonts(should_support_arrow_fonts);
            tab.update_advanced_mouse_actions(advanced_mouse_actions);
            tab.update_mouse_hover_effects(mouse_hover_effects);
            tab.update_focus_follows_mouse(focus_follows_mouse);
            tab.update_mouse_click_through(mouse_click_through);
        }

        // Clear hover state when disabled
        if !mouse_hover_effects {
            for tab in self.tabs.values_mut() {
                tab.clear_mouse_hover_state();
            }
        }

        // client specific configuration
        if self.connected_clients_contains(&client_id) {
            let mode_info = self
                .mode_info
                .entry(client_id)
                .or_insert_with(|| self.default_mode_info.clone());
            mode_info.update_keybinds(new_keybinds);
            mode_info.update_default_mode(new_default_mode);
            mode_info.update_theme(theme);
            mode_info.update_arrow_fonts(should_support_arrow_fonts);
            mode_info.update_hide_session_name(hide_session_name);
            for tab in self.tabs.values_mut() {
                tab.change_mode_info(mode_info.clone(), client_id);
                tab.mark_active_pane_for_rerender(client_id);
            }
        }

        // this needs to be done separately at the end because it applies some of the above changes
        // and propagates them to plugins
        for tab in self.tabs.values_mut() {
            tab.update_input_modes()?;
        }
        Ok(())
    }
    /// Apply a host-reported color-palette theme mode (CSI 2031 / DSR 997).
    ///
    /// This is the Phase 2 entry-point: it
    /// 1. de-duplicates against the last-known mode,
    /// 2. swaps the active palette to the configured `theme_dark` / `theme_light`
    ///    (when both are configured) by reusing the reconfigure propagation,
    /// 3. fans out an `Event::HostTerminalThemeChanged` plugin event,
    /// 4. forwards a `CSI ?997;{1|2}n` DSR onto the pty of every terminal pane
    ///    whose app opted in via `CSI ? 2031 h`.
    pub fn update_host_terminal_theme_mode(&mut self, mode: HostTerminalThemeMode) -> Result<()> {
        let err_context = || "Failed to update host terminal theme mode".to_string();

        // dedupe
        if self.host_terminal_theme_mode == Some(mode) {
            return Ok(());
        }
        self.host_terminal_theme_mode = Some(mode);

        // resolve target styling
        let resolved = match mode {
            HostTerminalThemeMode::Dark => self.host_theme_dark_styling,
            HostTerminalThemeMode::Light => self.host_theme_light_styling,
        };

        // theme propagation when both keys configured and the resolved
        // styling exists. (If only one of theme_dark/theme_light is set,
        // skip auto-switch; the static `theme` stays authoritative.)
        let auto_switch_enabled =
            self.host_theme_dark_styling.is_some() && self.host_theme_light_styling.is_some();
        if auto_switch_enabled {
            if let Some(theme) = resolved {
                self.default_mode_info.update_theme(theme);
                for tab in self.tabs.values_mut() {
                    tab.update_theme(theme);
                }
                // Iterate every connected client (active_tab_ids is the
                // canonical "who is connected" map). Iterating
                // self.mode_info.keys() would skip any client that has
                // never manually changed mode
                let client_ids: Vec<ClientId> = self.active_tab_ids.keys().copied().collect();
                let default_for_new = self.default_mode_info.clone();
                for client_id in client_ids {
                    let mode_info = self
                        .mode_info
                        .entry(client_id)
                        .or_insert_with(|| default_for_new.clone());
                    mode_info.update_theme(theme);
                    let mode_info_clone = mode_info.clone();
                    // Push the freshly-themed mode_info into every tab's
                    // per-client mode_info map. `update_input_modes` below
                    // reads from THAT map (not Screen's) when fanning out
                    // ModeUpdate to plugins, so without this step plugins
                    // receive ModeUpdate carrying the *old* style and the
                    // status-bar / tab-bar do not repaint. Also mark the
                    // active pane for rerender so terminal panes refresh
                    // their borders/title using the new palette.
                    for tab in self.tabs.values_mut() {
                        tab.change_mode_info(mode_info_clone.clone(), client_id);
                        tab.mark_active_pane_for_rerender(client_id);
                    }
                }
                for tab in self.tabs.values_mut() {
                    tab.update_input_modes().with_context(err_context)?;
                }
            } else {
                log::warn!(
                    "host theme auto-switch enabled but resolved styling missing for {:?}",
                    mode
                );
            }
        }

        // fan out the plugin event
        self.bus
            .senders
            .send_to_plugin(PluginInstruction::Update(vec![(
                None,
                None,
                Event::HostTerminalThemeChanged(mode),
            )]))
            .with_context(err_context)?;

        // forward DSR to opted-in terminal panes
        let mut pty_writes: Vec<(Vec<u8>, u32)> = vec![];
        for tab in self.tabs.values_mut() {
            for pane_id in tab.get_all_pane_ids() {
                if let PaneId::Terminal(terminal_id) = pane_id
                    && let Some(pane) = tab.get_pane_with_id_mut(pane_id)
                {
                    pane.push_color_palette_dsr(mode);
                    for bytes in pane.drain_messages_to_pty() {
                        pty_writes.push((bytes, terminal_id));
                    }
                }
            }
        }
        for (bytes, terminal_id) in pty_writes {
            let _ = self
                .bus
                .senders
                .send_to_pty_writer(PtyWriteInstruction::Write(bytes, terminal_id, None));
        }

        self.render(None)?;
        Ok(())
    }
    /// Apply a manual host-terminal theme mode change requested via the CLI or a
    /// keybinding. Surfaces a clear error to the CLI if the auto-switch gate
    /// (both `theme_dark` and `theme_light` configured) is not satisfied;
    /// otherwise delegates to `update_host_terminal_theme_mode`, which dedupes,
    /// repaints, fans out the plugin event, and forwards DSR to opted-in panes.
    pub fn apply_manual_host_terminal_theme_mode(
        &mut self,
        mode: HostTerminalThemeMode,
        completion_tx: &mut Option<NotificationEnd>,
    ) -> Result<()> {
        let auto_switch_enabled =
            self.host_theme_dark_styling.is_some() && self.host_theme_light_styling.is_some();
        if !auto_switch_enabled {
            if let Some(c) = completion_tx.as_mut() {
                c.set_exit_status(1);
                c.set_error_message(
                    "Manual theme switching requires both `theme_dark` and `theme_light` to be configured."
                        .to_string(),
                );
            }
            return Ok(());
        }
        self.update_host_terminal_theme_mode(mode)
    }
    pub fn toggle_pane_pinned(&mut self, client_id: ClientId) {
        active_tab_and_connected_client_id!(
            self,
            client_id,
            |tab: &mut Tab, client_id: ClientId| {
                tab.toggle_pane_pinned(client_id);
            }
        );
    }
    pub fn set_floating_pane_pinned(&mut self, pane_id: PaneId, should_be_pinned: bool) {
        let mut found = false;
        for tab in self.tabs.values_mut() {
            if tab.has_pane_with_pid(&pane_id) {
                tab.set_floating_pane_pinned(pane_id, should_be_pinned);
                found = true;
                break;
            }
        }
        if !found {
            log::error!(
                "Failed to find pane with id: {:?} to set as pinned",
                pane_id
            );
        }
    }
    pub fn stack_panes(&mut self, mut pane_ids_to_stack: Vec<PaneId>) -> Option<PaneId> {
        // if successful, returns the pane id of the last pane in the stack
        if pane_ids_to_stack.is_empty() {
            log::error!("Got an empty list of pane_ids to stack");
            return None;
        }
        let stack_size = pane_ids_to_stack.len();
        let root_pane_id = pane_ids_to_stack.remove(0);
        let last_pane_id = pane_ids_to_stack.last();
        let Some(root_tab_id) = self
            .tabs
            .iter()
            .find_map(|(tab_id, tab)| {
                if tab.has_pane_with_pid(&root_pane_id) {
                    Some(tab_id)
                } else {
                    None
                }
            })
            .copied()
        else {
            log::error!("Failed to find tab for root_pane_id: {:?}", root_pane_id);
            return None;
        };
        let root_pane_id_is_floating = self
            .tabs
            .get(&root_tab_id)
            .map(|t| t.pane_id_is_floating(&root_pane_id))
            .unwrap_or(false);

        if root_pane_id_is_floating && let Some(tab) = self.tabs.get_mut(&root_tab_id) {
            let _ = tab.toggle_pane_embed_or_floating_for_pane_id(root_pane_id, None);
        }

        let mut panes_to_stack = vec![];
        let target_tab_has_room_for_stack = self
            .tabs
            .get_mut(&root_tab_id)
            .map(|t| t.has_room_for_stack(root_pane_id, stack_size))
            .unwrap_or(false);
        if !target_tab_has_room_for_stack {
            log::error!("No room for stack with root pane id: {:?}", root_pane_id);
            return None;
        }

        for (tab_id, tab) in self.tabs.iter_mut() {
            if tab_id == &root_tab_id {
                // we do this before we extract panes so that the extraction won't trigger a
                // relayout according to the next swapped tiled pane
                tab.set_tiled_panes_damaged();
            }
            for pane_id in &pane_ids_to_stack {
                if tab.has_pane_with_pid(pane_id) {
                    match tab.extract_pane(*pane_id, false) {
                        Some(pane) => {
                            panes_to_stack.push(pane);
                        },
                        None => {
                            log::error!("Failed to extract pane: {:?}", pane_id);
                        },
                    }
                }
            }
        }
        if let Some(t) = self.tabs.get_mut(&root_tab_id) {
            t.stack_panes(root_pane_id, panes_to_stack)
        }
        last_pane_id.copied()
    }
    pub fn change_floating_panes_coordinates(
        &mut self,
        pane_ids_and_coordinates: Vec<(PaneId, FloatingPaneCoordinates)>,
    ) {
        for (pane_id, coordinates) in pane_ids_and_coordinates {
            for (_tab_id, tab) in self.tabs.iter_mut() {
                if tab.has_pane_with_pid(&pane_id) {
                    tab.change_floating_pane_coordinates(&pane_id, coordinates)
                        .non_fatal();
                    break;
                }
            }
        }
    }
    pub fn toggle_pane_borderless(&mut self, pane_id: PaneId) {
        for (_tab_id, tab) in self.tabs.iter_mut() {
            if tab.has_pane_with_pid(&pane_id) {
                tab.toggle_pane_borderless(&pane_id).non_fatal();
                break;
            }
        }
    }
    pub fn set_pane_borderless(&mut self, pane_id: PaneId, borderless: bool) {
        for (_tab_id, tab) in self.tabs.iter_mut() {
            if tab.has_pane_with_pid(&pane_id) {
                tab.set_pane_borderless(&pane_id, borderless).non_fatal();
                break;
            }
        }
    }
    pub fn handle_mouse_event(&mut self, event: MouseEvent, client_id: ClientId) {
        let is_bare_motion = event.event_type == MouseEventType::Motion
            && !event.left
            && !event.right
            && !event.middle
            && !event.wheel_up
            && !event.wheel_down;
        let active_pane_id_before = self
            .get_active_tab(client_id)
            .ok()
            .and_then(|tab| tab.get_active_pane_id(client_id));
        match self
            .get_active_tab_mut(client_id)
            .and_then(|tab| tab.handle_mouse_event(&event, client_id))
        {
            Ok(mouse_effect) => {
                let mut should_render = false;
                if let Some(pane_id) = mouse_effect.group_toggle
                    && self.advanced_mouse_actions
                {
                    self.toggle_pane_id_in_group(pane_id, &client_id);
                    should_render = true;
                }
                if let Some(pane_id) = mouse_effect.group_add
                    && self.advanced_mouse_actions
                {
                    self.add_pane_id_to_group(pane_id, &client_id);
                    should_render = true;
                }
                if mouse_effect.ungroup && self.advanced_mouse_actions {
                    self.clear_pane_group(&client_id);
                    should_render = true;
                }
                if mouse_effect.state_changed {
                    if !is_bare_motion {
                        let _ = self.log_and_report_session_state();
                    }
                    let active_pane_id_after = self
                        .get_active_tab(client_id)
                        .ok()
                        .and_then(|tab| tab.get_active_pane_id(client_id));
                    if active_pane_id_before.is_some()
                        && active_pane_id_before != active_pane_id_after
                    {
                        self.clear_bell_for_focused_pane(client_id);
                    }
                    should_render = true;
                }
                if !mouse_effect.leave_clipboard_message && !is_bare_motion {
                    let target_plugin_ids =
                        self.targeted_plugin_ids(client_id, EventType::InputReceived);
                    let plugin_updates: Vec<_> = target_plugin_ids
                        .into_iter()
                        .map(|pid| (Some(pid), Some(client_id), Event::InputReceived))
                        .collect();
                    if !plugin_updates.is_empty() {
                        let _ = self
                            .bus
                            .senders
                            .send_to_plugin(PluginInstruction::Update(plugin_updates));
                    }
                    should_render = true;
                }
                if should_render {
                    self.render(None).non_fatal();
                }
            },
            Err(e) => {
                log::error!("Failed to process MouseEvent: {}", e);
            },
        }
    }
    pub fn toggle_pane_in_group(&mut self, client_id: ClientId) -> Result<()> {
        let err_context = "Can't add pane to group";
        let active_tab = self
            .get_active_tab(client_id)
            .with_context(|| err_context)?;
        let active_pane_id = active_tab
            .get_active_pane_id(client_id)
            .with_context(|| err_context)?;
        self.toggle_pane_id_in_group(active_pane_id, &client_id);
        let _ = self.log_and_report_session_state();
        Ok(())
    }
    pub fn toggle_group_marking(&mut self, client_id: ClientId) -> Result<()> {
        let (was_marking_before, marking_pane_group_now) = {
            let mut currently_marking_pane_group = self.currently_marking_pane_group.borrow_mut();
            let previous_value = currently_marking_pane_group
                .remove(&client_id)
                .unwrap_or(false);
            let new_value = !previous_value;
            if new_value {
                currently_marking_pane_group.insert(client_id, true);
            }
            (previous_value, new_value)
        };
        if marking_pane_group_now {
            let active_pane_id = self.get_active_pane_id(&client_id);
            if let Some(active_pane_id) = active_pane_id {
                self.add_pane_id_to_group(active_pane_id, &client_id);
            }
        }
        let value_changed = was_marking_before != marking_pane_group_now;
        if value_changed {
            for tab in self.tabs.values_mut() {
                tab.update_input_modes()?;
            }
            let _ = self.log_and_report_session_state();
        }
        Ok(())
    }
    fn get_layout_metadata(
        &self,
        default_shell: Option<PathBuf>,
        tab_index: Option<usize>,
    ) -> SessionLayoutMetadata {
        let mut session_layout_metadata = SessionLayoutMetadata::new(self.default_layout.clone());
        if let Some(default_shell) = default_shell {
            session_layout_metadata.update_default_shell(default_shell);
        }
        let first_client_id = self.get_first_client_id();
        let active_tab_index =
            first_client_id.and_then(|client_id| self.active_tab_ids.get(&client_id));

        // Filter tabs based on optional tab_index parameter
        let tabs_to_process: Vec<_> = self
            .tabs
            .iter()
            .filter(|(idx, _)| tab_index.is_none_or(|target| **idx == target))
            .collect();

        for (tab_index, tab) in tabs_to_process {
            let tab_is_focused = active_tab_index == Some(tab_index);
            let hide_floating_panes = !tab.are_floating_panes_visible();
            let mut suppressed_panes = HashMap::new();
            for (triggering_pane_id, p) in tab.get_suppressed_panes() {
                suppressed_panes.insert(*triggering_pane_id, p);
            }

            let all_connected_clients: Vec<ClientId> = self
                .connected_clients
                .borrow()
                .keys()
                .copied()
                .filter(|c| self.active_tab_ids.get(c) == Some(tab_index))
                .collect();

            let mut active_pane_ids: HashMap<ClientId, Option<PaneId>> = HashMap::new();
            for connected_client_id in &all_connected_clients {
                active_pane_ids.insert(
                    *connected_client_id,
                    tab.get_active_pane_id(*connected_client_id),
                );
            }

            let tiled_panes: Vec<PaneLayoutMetadata> = tab
                .get_tiled_panes()
                .map(|(pane_id, p)| {
                    // here we look to see if this pane triggers any suppressed pane,
                    // and if so we take that suppressed pane - we do this because this
                    // is currently only the case the scrollback editing panes, and
                    // when dumping the layout we want the "real" pane and not the
                    // editor pane
                    match suppressed_panes.remove(pane_id) {
                        Some((is_scrollback_editor, suppressed_pane)) if *is_scrollback_editor => {
                            (suppressed_pane.pid(), suppressed_pane)
                        },
                        _ => (*pane_id, p),
                    }
                })
                .map(|(pane_id, p)| {
                    let focused_clients: Vec<ClientId> = active_pane_ids
                        .iter()
                        .filter_map(|(c_id, p_id)| {
                            p_id.and_then(|p_id| if p_id == pane_id { Some(*c_id) } else { None })
                        })
                        .collect();
                    let (default_fg, default_bg) = p.get_pane_default_colors();
                    PaneLayoutMetadata {
                        id: pane_id,
                        geom: p.position_and_size(),
                        cwd: None,
                        is_borderless: p.borderless(),
                        run: p.invoked_with().clone(),
                        title: p.custom_title(),
                        is_focused: !focused_clients.is_empty(),
                        pane_contents: if self.serialize_pane_viewport {
                            p.serialize(self.scrollback_lines_to_serialize)
                        } else {
                            None
                        },
                        focused_clients,
                        default_fg,
                        default_bg,
                    }
                })
                .collect();
            let floating_panes: Vec<PaneLayoutMetadata> = tab
                .get_floating_panes()
                .map(|(pane_id, p)| {
                    // here we look to see if this pane triggers any suppressed pane,
                    // and if so we take that suppressed pane - we do this because this
                    // is currently only the case the scrollback editing panes, and
                    // when dumping the layout we want the "real" pane and not the
                    // editor pane
                    match suppressed_panes.remove(pane_id) {
                        Some((is_scrollback_editor, suppressed_pane)) if *is_scrollback_editor => {
                            (suppressed_pane.pid(), suppressed_pane)
                        },
                        _ => (*pane_id, p),
                    }
                })
                .map(|(pane_id, p)| {
                    let focused_clients: Vec<ClientId> = active_pane_ids
                        .iter()
                        .filter_map(|(c_id, p_id)| {
                            p_id.and_then(|p_id| if p_id == pane_id { Some(*c_id) } else { None })
                        })
                        .collect();
                    let (default_fg, default_bg) = p.get_pane_default_colors();
                    PaneLayoutMetadata {
                        id: pane_id,
                        geom: p.position_and_size(),
                        cwd: None,
                        is_borderless: false, // floating panes are never borderless
                        run: p.invoked_with().clone(),
                        title: p.custom_title(),
                        is_focused: !focused_clients.is_empty(),
                        pane_contents: if self.serialize_pane_viewport {
                            p.serialize(self.scrollback_lines_to_serialize)
                        } else {
                            None
                        },
                        focused_clients,
                        default_fg,
                        default_bg,
                    }
                })
                .collect();
            session_layout_metadata.add_tab(
                tab.name.clone(),
                tab.instance_id.clone(),
                tab_is_focused,
                hide_floating_panes,
                tiled_panes,
                floating_panes,
            );
        }
        session_layout_metadata
    }
    fn update_plugin_loading_stage(
        &mut self,
        pid: u32,
        loading_indication: LoadingIndication,
    ) -> bool {
        let all_tabs = self.get_tabs_mut();
        let mut found_plugin = false;
        for tab in all_tabs.values_mut() {
            if tab.has_plugin(pid) {
                found_plugin = true;
                tab.update_plugin_loading_stage(pid, loading_indication);
                break;
            }
        }
        found_plugin
    }
    fn connected_clients_contains(&self, client_id: &ClientId) -> bool {
        self.connected_clients.borrow().contains_key(client_id)
    }
    fn get_client_pane_group(&self, client_id: &ClientId) -> HashSet<PaneId> {
        self.current_pane_group
            .borrow()
            .get_client_pane_group(client_id)
    }
    fn clear_pane_group(&mut self, client_id: &ClientId) {
        self.current_pane_group
            .borrow_mut()
            .clear_pane_group(client_id);
        self.currently_marking_pane_group
            .borrow_mut()
            .remove(client_id);
    }
    fn toggle_pane_id_in_group(&mut self, pane_id: PaneId, client_id: &ClientId) {
        {
            let mut pane_groups = self.current_pane_group.borrow_mut();
            pane_groups.toggle_pane_id_in_group(pane_id, self.size, client_id);
        }
        self.retain_only_existing_panes_in_pane_groups();
    }
    fn add_pane_id_to_group(&mut self, pane_id: PaneId, client_id: &ClientId) {
        {
            let mut pane_groups = self.current_pane_group.borrow_mut();
            pane_groups.add_pane_id_to_group(pane_id, self.size, client_id);
        }
        self.retain_only_existing_panes_in_pane_groups();
    }
    fn add_active_pane_to_group_if_marking(&mut self, client_id: &ClientId) {
        {
            if self
                .currently_marking_pane_group
                .borrow()
                .get(client_id)
                .copied()
                .unwrap_or(false)
            {
                let active_pane_id = self.get_active_pane_id(client_id);
                if let Some(active_pane_id) = active_pane_id {
                    self.add_pane_id_to_group(active_pane_id, client_id);
                }
            }
        }
        self.retain_only_existing_panes_in_pane_groups();
    }
    fn get_active_pane_id(&self, client_id: &ClientId) -> Option<PaneId> {
        let active_tab = self.get_active_tab(*client_id).ok()?;
        active_tab.get_active_pane_id(*client_id)
    }

    fn get_pane_info(&self, pane_id: PaneId) -> Option<PaneInfo> {
        // Search through all tabs to find the pane
        for tab in self.tabs.values() {
            if let Some(pane_info) = tab.get_pane_info(pane_id) {
                return Some(pane_info);
            }
        }
        None
    }

    fn get_tab_info(&self, tab_id: usize) -> Option<TabInfo> {
        // Look up tab by its stable ID
        self.tabs.get(&tab_id).map(|tab| {
            let all_focused_clients: Vec<ClientId> = self
                .active_tab_ids
                .iter()
                .filter(|(_c_id, active_tab_id)| **active_tab_id == tab.id)
                .map(|(c_id, _)| c_id)
                .copied()
                .collect();
            let (active_swap_layout_name, is_swap_layout_dirty) = tab.swap_layout_info();
            let tab_viewport = tab.get_viewport();
            let tab_display_area = tab.get_display_area();
            let selectable_tiled_panes_count = tab.get_selectable_tiled_panes_count();
            let selectable_floating_panes_count = tab.get_selectable_floating_panes_count();

            TabInfo {
                position: tab.position,
                name: tab.name.clone(),
                active: self.active_tab_ids.values().any(|i| i == &tab.id),
                panes_to_hide: tab.panes_to_hide_count(),
                is_fullscreen_active: tab.is_fullscreen_active(),
                is_sync_panes_active: tab.is_sync_panes_active(),
                are_floating_panes_visible: tab.are_floating_panes_visible(),
                other_focused_clients: all_focused_clients,
                active_swap_layout_name,
                is_swap_layout_dirty,
                viewport_rows: tab_viewport.rows,
                viewport_columns: tab_viewport.cols,
                display_area_rows: tab_display_area.rows,
                display_area_columns: tab_display_area.cols,
                selectable_tiled_panes_count,
                selectable_floating_panes_count,
                tab_id: tab.id,
                has_bell_notification: tab.tab_has_pending_bell
                    && !self.active_tab_ids.values().any(|i| i == &tab.id),
                is_flashing_bell: tab.tab_bell_flash
                    && !self.active_tab_ids.values().any(|i| i == &tab.id),
            }
        })
    }

    fn group_and_ungroup_panes(
        &mut self,
        pane_ids_to_group: Vec<PaneId>,
        pane_ids_to_ungroup: Vec<PaneId>,
        for_all_clients: bool,
        client_id: ClientId,
    ) {
        if for_all_clients {
            {
                let mut current_pane_group = self.current_pane_group.borrow_mut();
                current_pane_group.group_and_ungroup_panes_for_all_clients(
                    pane_ids_to_group,
                    pane_ids_to_ungroup,
                    self.size,
                );
            }
        } else {
            {
                let mut current_pane_group = self.current_pane_group.borrow_mut();
                current_pane_group.group_and_ungroup_panes(
                    pane_ids_to_group,
                    pane_ids_to_ungroup,
                    self.size,
                    &client_id,
                );
            }
        }
        self.retain_only_existing_panes_in_pane_groups();
        let _ = self.log_and_report_session_state();
    }
    fn retain_only_existing_panes_in_pane_groups(&mut self) {
        let clients_with_empty_group = {
            let mut clients_with_empty_group = vec![];
            let mut current_pane_group = { self.current_pane_group.borrow().clone_inner() };
            for (client_id, panes_in_group) in current_pane_group.iter_mut() {
                let all_tabs = self.get_tabs();
                panes_in_group.retain(|p_id| {
                    let mut found = false;
                    for tab in all_tabs.values() {
                        if tab.has_pane_with_pid(p_id) {
                            found = true;
                            break;
                        }
                    }
                    found
                });
                if panes_in_group.is_empty() {
                    clients_with_empty_group.push(*client_id)
                }
            }
            self.current_pane_group
                .borrow_mut()
                .override_groups_with(current_pane_group);
            clients_with_empty_group
        };
        for client_id in &clients_with_empty_group {
            self.currently_marking_pane_group
                .borrow_mut()
                .remove(client_id);
        }
        if !clients_with_empty_group.is_empty() {
            let all_tabs = self.get_tabs_mut();
            for tab in all_tabs.values_mut() {
                let _ = tab.update_input_modes();
            }
        }
    }
    fn update_active_pane_ids(&mut self) {
        let connected_clients: Vec<ClientId> =
            self.connected_clients.borrow().keys().copied().collect();
        for client_id in connected_clients {
            if let Some(active_pane_id) = self.get_active_pane_id(&client_id) {
                let active_pane_id: PaneId = active_pane_id;
                let history = self.pane_history.entry(client_id).or_default();
                history.retain(|e| e != &active_pane_id);
                history.push(active_pane_id);
            }
        }
    }
    fn subscribe_to_pane_renders(
        &mut self,
        subscriber_client_id: ClientId,
        pane_ids: Vec<zellij_utils::data::PaneId>,
        scrollback: Option<usize>,
        ansi: bool,
    ) {
        let mut previous_viewports = HashMap::new();
        let mut valid_pane_ids = HashSet::new();

        // Get a regular client ID for plugin pane content queries
        let regular_client_id = self
            .connected_clients
            .borrow()
            .keys()
            .find(|id| !self.watcher_clients.contains_key(id))
            .copied();

        for pane_id in &pane_ids {
            let server_pane_id: PaneId = (*pane_id).into();
            let mut found = false;

            for tab in self.tabs.values() {
                if let Some(pane) = tab.get_pane_with_id(server_pane_id) {
                    let get_full_scrollback = scrollback.is_some();
                    let max_lines = scrollback.and_then(|n| if n == 0 { None } else { Some(n) });

                    // For plugin panes, use a regular client_id; for terminal panes, None is fine
                    let query_client_id = match server_pane_id {
                        PaneId::Plugin(_) => regular_client_id,
                        PaneId::Terminal(_) => None,
                    };
                    let contents = if ansi {
                        pane.pane_contents_with_ansi(
                            query_client_id,
                            get_full_scrollback,
                            max_lines,
                        )
                    } else {
                        pane.pane_contents(query_client_id, get_full_scrollback, max_lines)
                    };

                    if let Some(os_input) = &self.bus.os_input {
                        let scrollback_data = if scrollback.is_some() {
                            Some(contents.lines_above_viewport.clone())
                        } else {
                            None
                        };
                        let _ = os_input.send_to_client(
                            subscriber_client_id,
                            ServerToClientMsg::PaneRenderUpdate {
                                pane_id: *pane_id,
                                viewport: contents.viewport.clone(),
                                scrollback: scrollback_data,
                                is_initial: true,
                            },
                        );
                    }

                    previous_viewports.insert(*pane_id, contents.viewport);
                    valid_pane_ids.insert(*pane_id);
                    found = true;
                    break;
                }
            }

            if !found && let Some(os_input) = &self.bus.os_input {
                let _ = os_input.send_to_client(
                    subscriber_client_id,
                    ServerToClientMsg::LogError {
                        lines: vec![format!("Pane {} not found", pane_id)],
                    },
                );
            }
        }

        if !valid_pane_ids.is_empty() {
            self.pane_render_subscribers.insert(
                subscriber_client_id,
                PaneRenderSubscription {
                    pane_ids: valid_pane_ids,
                    previous_viewports,
                    ansi,
                },
            );
        }
    }
    fn deliver_to_pane_subscribers_from_report(&mut self, report: &PaneRenderReport) {
        let Some(pane_map) = report.all_pane_contents.values().next() else {
            return;
        };
        let ansi_pane_map = report.all_pane_contents_with_ansi.values().next();
        self.deliver_subscriber_updates_from_map(pane_map, ansi_pane_map);
    }
    fn deliver_to_pane_subscribers_directly(&mut self) {
        // Collect unique pane IDs across all subscribers
        let all_subscribed_ids: HashSet<zellij_utils::data::PaneId> = self
            .pane_render_subscribers
            .values()
            .flat_map(|sub| sub.pane_ids.iter().copied())
            .collect();

        let has_ansi_subscribers = self.pane_render_subscribers.values().any(|s| s.ansi);

        // Query pane contents directly from tabs
        let mut pane_map: HashMap<zellij_utils::data::PaneId, PaneContents> = HashMap::new();
        let mut ansi_pane_map: HashMap<zellij_utils::data::PaneId, PaneContents> = HashMap::new();
        for pane_id in &all_subscribed_ids {
            let server_pane_id: PaneId = (*pane_id).into();
            for tab in self.tabs.values() {
                if let Some(pane) = tab.get_pane_with_id(server_pane_id) {
                    pane_map.insert(*pane_id, pane.pane_contents(None, false, None));
                    if has_ansi_subscribers {
                        ansi_pane_map
                            .insert(*pane_id, pane.pane_contents_with_ansi(None, false, None));
                    }
                    break;
                }
            }
        }

        let ansi_map_ref = if has_ansi_subscribers {
            Some(&ansi_pane_map)
        } else {
            None
        };
        self.deliver_subscriber_updates_from_map(&pane_map, ansi_map_ref);
    }
    fn deliver_subscriber_updates_from_map(
        &mut self,
        pane_map: &HashMap<zellij_utils::data::PaneId, PaneContents>,
        ansi_pane_map: Option<&HashMap<zellij_utils::data::PaneId, PaneContents>>,
    ) {
        // Collect updates to send, avoiding borrow conflicts
        let mut updates_to_send: Vec<(ClientId, ServerToClientMsg)> = Vec::new();
        let mut dead_subscribers: Vec<ClientId> = Vec::new();

        for (subscriber_id, subscription) in &self.pane_render_subscribers {
            let effective_map = if subscription.ansi {
                ansi_pane_map.unwrap_or(pane_map)
            } else {
                pane_map
            };

            for pane_id in &subscription.pane_ids {
                if let Some(contents) = effective_map.get(pane_id) {
                    let changed = subscription
                        .previous_viewports
                        .get(pane_id)
                        .map(|prev| prev != &contents.viewport)
                        .unwrap_or(true);

                    if changed {
                        updates_to_send.push((
                            *subscriber_id,
                            ServerToClientMsg::PaneRenderUpdate {
                                pane_id: *pane_id,
                                viewport: contents.viewport.clone(),
                                scrollback: None,
                                is_initial: false,
                            },
                        ));
                    }
                }
            }
        }

        // Send updates and track dead subscribers
        for (subscriber_id, msg) in &updates_to_send {
            if let Some(os_input) = &self.bus.os_input
                && os_input
                    .send_to_client(*subscriber_id, msg.clone())
                    .is_err()
            {
                dead_subscribers.push(*subscriber_id);
            }
        }

        // Update previous viewports for successful sends
        for (subscriber_id, msg) in updates_to_send {
            if dead_subscribers.contains(&subscriber_id) {
                continue;
            }
            if let ServerToClientMsg::PaneRenderUpdate {
                pane_id, viewport, ..
            } = msg
                && let Some(subscription) = self.pane_render_subscribers.get_mut(&subscriber_id)
            {
                subscription.previous_viewports.insert(pane_id, viewport);
            }
        }

        for id in dead_subscribers {
            self.pane_render_subscribers.remove(&id);
        }
    }
    fn notify_pane_closed_to_subscribers(&mut self, pane_id: zellij_utils::data::PaneId) {
        let mut dead_subscribers = Vec::new();

        for (subscriber_id, subscription) in &mut self.pane_render_subscribers {
            if subscription.pane_ids.remove(&pane_id) {
                if let Some(os_input) = &self.bus.os_input {
                    let _ = os_input.send_to_client(
                        *subscriber_id,
                        ServerToClientMsg::SubscribedPaneClosed { pane_id },
                    );
                }
                if subscription.pane_ids.is_empty() {
                    if let Some(os_input) = &self.bus.os_input {
                        let _ = os_input.send_to_client(
                            *subscriber_id,
                            ServerToClientMsg::Exit {
                                exit_reason: ExitReason::Normal,
                            },
                        );
                    }
                    dead_subscribers.push(*subscriber_id);
                }
            }
        }

        for id in dead_subscribers {
            self.pane_render_subscribers.remove(&id);
        }
    }
}

#[cfg(not(test))]
fn get_default_editor() -> Option<PathBuf> {
    std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .map(PathBuf::from)
        .ok()
}

#[cfg(test)]
fn get_default_editor() -> Option<PathBuf> {
    None
}

fn next_layout_generation(previous: Option<&DurableTabLayoutGeneration>) -> u64 {
    previous
        .map(|generation| generation.generation.wrapping_add(1).max(1))
        .unwrap_or(1)
}

pub(crate) fn reserve_new_durable_tab_layout_generation(
    generations: &mut HashMap<String, DurableTabLayoutGeneration>,
    tab_id: usize,
    tab_name: &str,
    tab_instance_id: &str,
    viewer_creation_fence: Option<ViewerCreationFence>,
) -> DurableTabLayoutGeneration {
    let normalized_token = tab_instance_id.to_ascii_lowercase();
    let generation = DurableTabLayoutGeneration {
        tab_id,
        tab_name: tab_name.to_owned(),
        tab_instance_id: normalized_token.clone(),
        generation: next_layout_generation(generations.get(&normalized_token)),
        viewer_creation_fence,
    };
    generations.insert(normalized_token, generation.clone());
    generation
}

pub(crate) fn reserve_durable_tab_layout_recovery(
    generations: &mut HashMap<String, DurableTabLayoutGeneration>,
    tab_id: usize,
    tab_name: &str,
    tab_instance_id: &str,
    viewer_creation_fence: Option<ViewerCreationFence>,
) -> Result<DurableTabLayoutGeneration, String> {
    let normalized_token = tab_instance_id.to_ascii_lowercase();
    if let Some(previous) = generations.get(&normalized_token)
        && (previous.tab_id != tab_id || previous.tab_name != tab_name)
    {
        return Err(format!(
            "durable tab recovery rejected an ABA replacement: token {} was reserved for tab {} '{}' but now resolves to tab {} '{}'",
            normalized_token, previous.tab_id, previous.tab_name, tab_id, tab_name
        ));
    }
    let generation = DurableTabLayoutGeneration {
        tab_id,
        tab_name: tab_name.to_owned(),
        tab_instance_id: normalized_token.clone(),
        generation: next_layout_generation(generations.get(&normalized_token)),
        viewer_creation_fence,
    };
    generations.insert(normalized_token, generation.clone());
    Ok(generation)
}

fn close_globally_stale_fenced_tab(
    screen: &mut Screen,
    generation: &DurableTabLayoutGeneration,
    writer_resource_ids: &[PaneId],
) -> Result<Vec<PaneId>> {
    let exact_owner_resource_ids = screen
        .get_tab_by_id(generation.tab_id)
        .filter(|tab| {
            tab.name == generation.tab_name
                && tab
                    .instance_id
                    .eq_ignore_ascii_case(&generation.tab_instance_id)
        })
        .map(Tab::get_all_pane_ids);
    if exact_owner_resource_ids.is_some() {
        screen.close_tab_by_id_excluding_pty_resources(
            generation.tab_id,
            &writer_resource_ids.iter().copied().collect(),
        )?;
    }
    Ok(exact_owner_resource_ids.unwrap_or_default())
}

fn verify_global_viewer_creation_fence(
    screen: &Screen,
    generation: &DurableTabLayoutGeneration,
) -> Result<(), ViewerCreationFenceRejection> {
    if let Some(fence) = generation.viewer_creation_fence.as_ref() {
        fence.verify_for_install(&screen.session_name, &generation.tab_name)?;
    }
    Ok(())
}

fn remove_layout_resources_from_screen(screen: &mut Screen, resource_ids: &[PaneId]) {
    for tab in screen.tabs.values_mut() {
        for resource_id in resource_ids {
            if tab.has_pane_with_pid(resource_id) {
                tab.close_pane(*resource_id, true, None);
            }
        }
    }
}

fn release_pending_layout_gate_if_ready(
    screen: &mut Screen,
    pending_tab_ids: &HashSet<usize>,
    pending_tab_switches: &mut HashSet<(usize, ClientId)>,
    pending_events_waiting_for_client: &mut Vec<ScreenInstruction>,
    pending_events_waiting_for_tab: &mut Vec<ScreenInstruction>,
) {
    if !pending_tab_ids.is_empty() {
        return;
    }
    for (tab_index, pending_client_id) in pending_tab_switches.drain() {
        screen
            .go_to_tab(tab_index + 1, pending_client_id)
            .non_fatal();
    }
    for event in pending_events_waiting_for_client.drain(..) {
        screen.bus.senders.send_to_screen(event).non_fatal();
    }
    for event in pending_events_waiting_for_tab.drain(..) {
        screen.bus.senders.send_to_screen(event).non_fatal();
    }
}

#[cfg(test)]
struct ViewerCreationPostInstallTestHook {
    installed: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}

#[cfg(test)]
static VIEWER_CREATION_POST_INSTALL_TEST_HOOKS: OnceLock<
    Mutex<HashMap<String, ViewerCreationPostInstallTestHook>>,
> = OnceLock::new();
#[cfg(test)]
static REJECT_AFTER_APPLY_PREPARE_TEST_TRANSACTIONS: OnceLock<Mutex<HashSet<LayoutTransactionId>>> =
    OnceLock::new();

#[cfg(test)]
pub(crate) fn reject_after_apply_prepare_for_test(transaction_id: LayoutTransactionId) {
    REJECT_AFTER_APPLY_PREPARE_TEST_TRANSACTIONS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .insert(transaction_id);
}

#[cfg(test)]
fn take_reject_after_apply_prepare_for_test(transaction_id: LayoutTransactionId) -> bool {
    REJECT_AFTER_APPLY_PREPARE_TEST_TRANSACTIONS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap()
        .remove(&transaction_id)
}

#[cfg(test)]
pub(crate) fn register_viewer_creation_post_install_test_hook(
    run_id: &str,
) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
    let (installed_tx, installed_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    VIEWER_CREATION_POST_INSTALL_TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(
            run_id.to_owned(),
            ViewerCreationPostInstallTestHook {
                installed: installed_tx,
                resume: resume_rx,
            },
        );
    (installed_rx, resume_tx)
}

#[cfg(test)]
fn pause_after_viewer_creation_install_for_test(generation: &DurableTabLayoutGeneration) {
    let Some(fence) = generation.viewer_creation_fence.as_ref() else {
        return;
    };
    let hook = VIEWER_CREATION_POST_INSTALL_TEST_HOOKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .remove(&fence.run_id);
    if let Some(hook) = hook {
        hook.installed.send(()).unwrap();
        hook.resume.recv().unwrap();
    }
}

pub(crate) fn durable_tab_layout_generation_is_current(
    screen: &Screen,
    generations: &HashMap<String, DurableTabLayoutGeneration>,
    generation: &DurableTabLayoutGeneration,
) -> bool {
    generations
        .get(&generation.tab_instance_id)
        .is_some_and(|current| current == generation)
        && screen.get_tab_by_id(generation.tab_id).is_some_and(|tab| {
            tab.name == generation.tab_name
                && tab
                    .instance_id
                    .eq_ignore_ascii_case(&generation.tab_instance_id)
        })
}

fn prepare_existing_tab_layout(tab_layout_info: &mut TabLayoutInfo, tab: &mut Tab) {
    if let Some(name) = tab_layout_info.tab_name.take() {
        tab.name = name;
    }
    let (tiled_to_ignore, floating_indices) = find_already_running_panes(
        &tab_layout_info.tiled_layout,
        &tab_layout_info.floating_layouts,
        tab,
    );
    for run_instruction in tiled_to_ignore {
        tab_layout_info
            .tiled_layout
            .ignore_run_instruction(run_instruction);
    }
    for index in floating_indices {
        if let Some(floating) = tab_layout_info.floating_layouts.get_mut(index) {
            floating.already_running = true;
        }
    }
}

fn layout_resource_ids(
    new_pane_pids: &[(u32, HoldForCommand)],
    new_floating_pane_pids: &[(u32, HoldForCommand)],
    new_plugin_ids: &HashMap<RunPluginOrAlias, Vec<u32>>,
) -> Vec<PaneId> {
    new_pane_pids
        .iter()
        .chain(new_floating_pane_pids)
        .map(|(id, _)| PaneId::Terminal(*id))
        .chain(
            new_plugin_ids
                .values()
                .flatten()
                .map(|id| PaneId::Plugin(*id)),
        )
        .collect()
}

fn find_already_running_panes(
    tiled_layout: &TiledPaneLayout,
    floating_layouts: &[FloatingPaneLayout],
    active_tab: &Tab,
) -> (Vec<Option<Run>>, Vec<usize>) {
    let mut layout_tiled_instructions = tiled_layout.extract_run_instructions();
    let running_tiled_instructions: Vec<Option<Run>> = active_tab
        .get_tiled_panes()
        .map(|(_, pane)| pane.invoked_with().clone())
        .collect();

    let mut tiled_to_ignore = Vec::new();
    for running_instr in running_tiled_instructions {
        if let Some(pos) = layout_tiled_instructions
            .iter()
            .position(|layout_instr| layout_instr == &running_instr)
        {
            layout_tiled_instructions.remove(pos);
            tiled_to_ignore.push(running_instr);
        }
    }

    let mut running_floating_instructions: Vec<Option<Run>> = active_tab
        .get_floating_panes()
        .map(|(_, pane)| pane.invoked_with().clone())
        .collect();

    let mut floating_indices = Vec::new();
    for (idx, floating_layout) in floating_layouts.iter().enumerate() {
        if let Some(pos) = running_floating_instructions
            .iter()
            .position(|instr| instr == &floating_layout.run)
        {
            running_floating_instructions.remove(pos);
            floating_indices.push(idx);
        }
    }

    (tiled_to_ignore, floating_indices)
}

// The box is here in order to make the
// NewClient enum smaller
#[allow(clippy::boxed_local)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn screen_thread_main(
    bus: Bus<ScreenInstruction>,
    max_panes: Option<usize>,
    client_attributes: ClientAttributes,
    config: Config,
    debug: bool,
    default_layout: Box<Layout>,
    has_clients_flag: Arc<AtomicBool>,
    session_name_override: Option<String>,
) -> Result<()> {
    // Resolve `theme_dark` / `theme_light` to concrete `Styling` from the
    // bundled themes BEFORE `config.options` is moved out below. These
    // populate Screen's auto-switch state at startup; runtime updates
    // continue to flow through `propagate_configuration_changes`.
    let host_theme_dark_styling = config
        .options
        .theme_dark
        .as_ref()
        .and_then(|name| config.themes.get_theme(name).map(|t| t.palette));
    let host_theme_light_styling = config
        .options
        .theme_light
        .as_ref()
        .and_then(|name| config.themes.get_theme(name).map(|t| t.palette));
    if config.options.theme_dark.is_some() && host_theme_dark_styling.is_none() {
        log::warn!(
            "theme_dark='{}' not found in themes; auto-theme switch disabled for dark.",
            config.options.theme_dark.as_deref().unwrap_or("?")
        );
    }
    if config.options.theme_light.is_some() && host_theme_light_styling.is_none() {
        log::warn!(
            "theme_light='{}' not found in themes; auto-theme switch disabled for light.",
            config.options.theme_light.as_deref().unwrap_or("?")
        );
    }

    let config_options = config.options;
    let arrow_fonts = !config_options.simplified_ui.unwrap_or_default();
    let draw_pane_frames = config_options.pane_frames.unwrap_or(true);
    let auto_layout = config_options.auto_layout.unwrap_or(true);
    let session_serialization = config_options.session_serialization.unwrap_or(true);
    let serialize_pane_viewport = config_options.serialize_pane_viewport.unwrap_or(false);
    let scrollback_lines_to_serialize = config_options.scrollback_lines_to_serialize;
    let session_is_mirrored = config_options.mirror_session.unwrap_or(false);
    let layout_dir = config_options.layout_dir;
    #[cfg(test)]
    let default_shell = config_options
        .default_shell
        .clone()
        .unwrap_or(PathBuf::from("/bin/sh"));
    #[cfg(not(test))]
    let default_shell = config_options
        .default_shell
        .clone()
        .unwrap_or_else(get_default_shell);
    let default_editor = config_options
        .scrollback_editor
        .clone()
        .or_else(get_default_editor);
    let default_layout_name = config_options
        .default_layout
        .map(|l| format!("{}", l.display()));
    let copy_options = CopyOptions::new(
        config_options.copy_command,
        config_options.copy_clipboard.unwrap_or_default(),
        config_options.copy_on_select.unwrap_or(true),
    );
    let web_server_ip = config_options
        .web_server_ip
        .unwrap_or_else(|| IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
    let web_server_port = config_options.web_server_port.unwrap_or(8082);
    let styled_underlines = config_options.styled_underlines.unwrap_or(true);
    let osc8_hyperlinks = config_options.osc8_hyperlinks.unwrap_or(true);
    let explicitly_disable_kitty_keyboard_protocol = config_options
        .support_kitty_keyboard_protocol
        .map(|e| !e) // this is due to the config options wording, if
        // "support_kitty_keyboard_protocol" is true,
        // explicitly_disable_kitty_keyboard_protocol is false and vice versa
        .unwrap_or(false); // by default, we try to support this if the terminal supports it and
    // the program running inside a pane requests it
    let stacked_resize = config_options.stacked_resize.unwrap_or(true);
    let web_clients_allowed = config_options
        .web_sharing
        .map(|s| s.web_clients_allowed())
        .unwrap_or(false);
    let web_sharing = config_options.web_sharing.unwrap_or_default();
    let advanced_mouse_actions = config_options.advanced_mouse_actions.unwrap_or(true);
    let mouse_hover_effects = config_options.mouse_hover_effects.unwrap_or(true);
    let visual_bell = config_options.visual_bell.unwrap_or(true);
    let focus_follows_mouse = config_options.focus_follows_mouse.unwrap_or(false);
    let mouse_click_through = config_options.mouse_click_through.unwrap_or(false);

    let mut mode_info = get_mode_info(
        config_options.default_mode.unwrap_or_default(),
        &client_attributes,
        PluginCapabilities {
            //  ¯\_(ツ)_/¯
            arrow_fonts: !arrow_fonts,
        },
        &config.keybinds,
        config_options.default_mode,
    );
    if let Some(session_name_override) = session_name_override {
        mode_info.session_name = Some(session_name_override);
    }

    let thread_senders = bus.senders.clone();
    let mut screen = Screen::new(
        bus,
        &client_attributes,
        max_panes,
        mode_info,
        draw_pane_frames,
        auto_layout,
        session_is_mirrored,
        copy_options,
        debug,
        default_layout,
        default_layout_name,
        default_shell,
        session_serialization,
        serialize_pane_viewport,
        scrollback_lines_to_serialize,
        styled_underlines,
        osc8_hyperlinks,
        arrow_fonts,
        layout_dir,
        explicitly_disable_kitty_keyboard_protocol,
        stacked_resize,
        default_editor,
        web_clients_allowed,
        web_sharing,
        advanced_mouse_actions,
        mouse_hover_effects,
        visual_bell,
        focus_follows_mouse,
        mouse_click_through,
        web_server_ip,
        web_server_port,
        has_clients_flag,
    );
    screen.host_theme_dark_styling = host_theme_dark_styling;
    screen.host_theme_light_styling = host_theme_light_styling;

    let mut pending_tab_ids: HashSet<usize> = HashSet::new();
    let mut durable_tab_layout_generations: HashMap<String, DurableTabLayoutGeneration> =
        HashMap::new();
    let mut pending_tab_switches: HashSet<(usize, ClientId)> = HashSet::new(); // usize is the
    // tab_index
    let mut pending_events_waiting_for_tab: Vec<ScreenInstruction> = vec![];
    let mut pending_events_waiting_for_client: Vec<ScreenInstruction> = vec![];
    let mut pending_events_waiting_for_pane: HashMap<PaneId, Vec<ScreenInstruction>> =
        HashMap::new();
    let mut plugin_loading_message_cache = HashMap::new();
    let mut keybind_intercepts = HashMap::new();
    loop {
        for (transaction_id, coordination) in screen.take_resolved_layout_reconciliations() {
            if let Err(error) = screen.reconcile_indeterminate_layout_transaction(
                transaction_id,
                coordination,
                &mut pending_tab_ids,
                &durable_tab_layout_generations,
                &mut pending_tab_switches,
                &mut pending_events_waiting_for_client,
                &mut pending_events_waiting_for_tab,
                &mut plugin_loading_message_cache,
            ) {
                log::error!(
                    "failed to finalize reconciled layout transaction {transaction_id}: {error:#}"
                );
            }
        }
        screen.retry_indeterminate_layout_transactions_in_background();
        screen.retry_pending_layout_cleanup_in_background();
        let (event, mut err_ctx) = screen
            .bus
            .recv()
            .context("failed to receive event on channel")?;
        err_ctx.add_call(ContextType::Screen((&event).into()));
        // here we start caching resizes, so that we'll send them in bulk at the end of each event
        // when this cache is Dropped, for more information, see the comments in PtyWriter
        let _resize_cache = ResizeCache::new(thread_senders.clone());

        match event {
            ScreenInstruction::PtyBytes(pid, vte_bytes) => {
                let all_tabs = screen.get_tabs_mut();
                let mut vte_bytes = Some(vte_bytes);
                for tab in all_tabs.values_mut() {
                    if tab.has_terminal_pid(pid) {
                        if let Some(bytes) = vte_bytes.take() {
                            tab.handle_pty_bytes(pid, bytes)
                                .context("failed to process pty bytes")?;
                        }
                        break;
                    }
                }
                if let Some(vte_bytes) = vte_bytes {
                    pending_events_waiting_for_pane
                        .entry(PaneId::Terminal(pid))
                        .or_default()
                        .push(ScreenInstruction::PtyBytes(pid, vte_bytes));
                }
                if screen.has_render_recipients() {
                    let _ = screen
                        .bus
                        .senders
                        .send_to_background_jobs(BackgroundJob::RenderToClients);
                }
            },
            ScreenInstruction::PluginBytes(mut plugin_render_assets) => {
                for plugin_render_asset in plugin_render_assets.iter_mut() {
                    let plugin_id = plugin_render_asset.plugin_id;
                    let client_id = plugin_render_asset.client_id;
                    let vte_bytes = plugin_render_asset.bytes.drain(..).collect();

                    let all_tabs = screen.get_tabs_mut();
                    for tab in all_tabs.values_mut() {
                        if tab.has_plugin(plugin_id) {
                            tab.handle_plugin_bytes(plugin_id, client_id, vte_bytes)
                                .context("failed to process plugin bytes")?;
                            break;
                        }
                    }
                    screen.render_blocker.remove_blocking_plugin(plugin_id);
                }
                screen.render(Some(plugin_render_assets))?;
            },
            ScreenInstruction::Render => {
                screen.render(None)?;
            },
            ScreenInstruction::LayoutMaintenanceWake => {},
            ScreenInstruction::RenderToClients => {
                // render_blocker.can_render() returning true means that either all pending plugins
                // (only those waiting for a new tab layout to be applied!) have been rendered or
                // that a 100ms timeout has been reached (more info in the RenderBlocker comment)
                if screen.render_blocker.can_render() {
                    screen.render_to_clients(&pending_tab_ids)?;
                } else {
                    screen.render(None)?;
                }
            },
            ScreenInstruction::NewPane(
                pid,
                initial_pane_title,
                hold_for_command,
                invoked_with,
                new_pane_placement,
                start_suppressed,
                client_or_tab_index,
                mut completion_tx,
                set_blocking,
            ) => {
                if let Some(c) = completion_tx.as_mut() {
                    c.set_affected_pane_id(pid)
                }

                let blocking_notification = if set_blocking { completion_tx } else { None };

                match client_or_tab_index {
                    ClientTabIndexOrPaneId::ClientId(client_id) => {
                        active_tab_and_connected_client_id_with_first_tab_fallback!(screen, client_id, |tab: &mut Tab, client_id: Option<ClientId>| {
                            tab.new_pane(pid,
                               initial_pane_title,
                               invoked_with,
                               start_suppressed,
                               true,
                               new_pane_placement,
                               client_id,
                               blocking_notification
                           )
                        }, ?);
                        if let Some(hold_for_command) = hold_for_command {
                            let is_first_run = true;
                            active_tab_and_connected_client_id_with_first_tab_fallback!(
                                screen,
                                client_id,
                                |tab: &mut Tab, _client_id: Option<ClientId>| tab.hold_pane(
                                    pid,
                                    None,
                                    is_first_run,
                                    hold_for_command
                                )
                            )
                        }
                    },
                    ClientTabIndexOrPaneId::TabIndex(tab_index) => {
                        // Some placements (directional split, stacked without a
                        // target pane) need a client_id to know which pane to
                        // split relative to. Only resolve one when required.
                        let needs_client_id = matches!(
                            new_pane_placement,
                            NewPanePlacement::Tiled {
                                direction: Some(_),
                                ..
                            } | NewPanePlacement::Stacked {
                                pane_id_to_stack_under: None,
                                ..
                            }
                        );
                        let client_id = if needs_client_id {
                            screen
                                .active_tab_ids
                                .iter()
                                .find(|(_, tid)| **tid == tab_index)
                                .map(|(cid, _)| *cid)
                                .or_else(|| screen.active_tab_ids.keys().next().copied())
                        } else {
                            None
                        };
                        if let Some(active_tab) = screen.tabs.get_mut(&tab_index) {
                            active_tab.new_pane(
                                pid,
                                initial_pane_title,
                                invoked_with,
                                start_suppressed,
                                true,
                                new_pane_placement,
                                client_id,
                                blocking_notification,
                            )?;
                            if let Some(hold_for_command) = hold_for_command {
                                let is_first_run = true;
                                active_tab.hold_pane(pid, None, is_first_run, hold_for_command);
                            }
                        } else {
                            log::error!("Tab index not found: {:?}", tab_index);
                        }
                    },
                    ClientTabIndexOrPaneId::PaneId(pane_id) => {
                        let mut found = false;
                        let all_tabs = screen.get_tabs_mut();
                        let should_focus_pane = false;
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                tab.new_pane(
                                    pid,
                                    initial_pane_title,
                                    invoked_with,
                                    start_suppressed,
                                    should_focus_pane,
                                    new_pane_placement,
                                    None,
                                    blocking_notification, // TODO: is this correct?
                                )?;
                                if let Some(hold_for_command) = hold_for_command {
                                    let is_first_run = true;
                                    tab.hold_pane(pid, None, is_first_run, hold_for_command);
                                }
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            log::error!(
                                "Failed to find tab containing pane with id: {:?}",
                                pane_id
                            );
                        }
                    },
                };
                if let Some(pending_events) = pending_events_waiting_for_pane.remove(&pid) {
                    for event in pending_events {
                        screen.bus.senders.send_to_screen(event).non_fatal();
                    }
                }
                screen.log_and_report_session_state()?;

                screen.render(None)?;
            },
            ScreenInstruction::OpenInPlaceEditor(pid, client_tab_index_or_pane_id) => {
                match client_tab_index_or_pane_id {
                    ClientTabIndexOrPaneId::ClientId(client_id) => {
                        active_tab!(screen, client_id, |tab: &mut Tab| tab
                            .replace_active_pane_with_editor_pane(pid, client_id), ?);
                        screen.log_and_report_session_state()?;
                    },
                    ClientTabIndexOrPaneId::TabIndex(_tab_index) => {
                        log::error!("Cannot OpenInPlaceEditor with a TabIndex");
                    },
                    ClientTabIndexOrPaneId::PaneId(pane_id_to_replace) => {
                        let mut found = false;
                        let all_tabs = screen.get_tabs_mut();
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id_to_replace) {
                                tab.replace_pane_with_editor_pane(pid, pane_id_to_replace)
                                    .non_fatal();
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            log::error!(
                                "Could not find pane with id {:?} to replace",
                                pane_id_to_replace
                            );
                        }
                    },
                }

                screen.render(None)?;
            },
            ScreenInstruction::TogglePaneEmbedOrFloating(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(screen, client_id, |tab: &mut Tab, client_id: ClientId| tab
                    .toggle_pane_embed_or_floating(client_id), ?);
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::ToggleFloatingPanes(client_id, default_shell, completion_tx) => {
                active_tab_and_connected_client_id!(screen, client_id, |tab: &mut Tab, client_id: ClientId| tab
                    .toggle_floating_panes(Some(client_id), default_shell, completion_tx), ?);
                screen.clear_bell_for_focused_pane(client_id);
                screen.log_and_report_session_state()?;

                screen.render(None)?;
            },
            ScreenInstruction::ShowFloatingPanes {
                client_id,
                tab_id,
                completion,
            } => {
                screen.show_floating_panes_in_tab(client_id, tab_id, completion)?;
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::HideFloatingPanes {
                client_id,
                tab_id,
                completion,
            } => {
                screen.hide_floating_panes_in_tab(client_id, tab_id, completion)?;
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::AreFloatingPanesVisible {
                client_id,
                tab_id,
                completion,
            } => {
                screen.are_floating_panes_visible_in_tab(client_id, tab_id, completion)?;
            },
            ScreenInstruction::WriteCharacter(
                key_with_modifier,
                raw_bytes,
                is_kitty_keyboard_protocol,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                if let Some(plugin_id) = keybind_intercepts.get(&client_id)
                    && let Some(key_with_modifier) = key_with_modifier
                {
                    let _ = screen
                        .bus
                        .senders
                        .send_to_plugin(PluginInstruction::Update(vec![(
                            Some(*plugin_id),
                            Some(client_id),
                            Event::InterceptedKeyPress(key_with_modifier),
                        )]));
                    continue;
                }
                let mut state_changed = false;
                let client_input_mode = screen.get_client_input_mode(client_id);
                match client_input_mode {
                    Some(InputMode::RenameTab) => {
                        if !(raw_bytes == BRACKETED_PASTE_BEGIN || raw_bytes == BRACKETED_PASTE_END)
                        {
                            screen.update_active_tab_name(raw_bytes, client_id)?;
                            state_changed = true;
                        }
                    },
                    _ => {
                        active_tab_and_connected_client_id!(
                            screen,
                            client_id,
                            |tab: &mut Tab, client_id: ClientId| {
                                match client_input_mode {
                                    Some(InputMode::EnterSearch) => {
                                        if !(raw_bytes == BRACKETED_PASTE_BEGIN
                                            || raw_bytes == BRACKETED_PASTE_END)
                                            && let Err(e) =
                                                tab.update_search_term(raw_bytes, client_id)
                                        {
                                            log::error!("{}", e);
                                        }
                                        state_changed = true;
                                    },
                                    Some(InputMode::RenamePane) => {
                                        if !(raw_bytes == BRACKETED_PASTE_BEGIN
                                            || raw_bytes == BRACKETED_PASTE_END)
                                        {
                                            if let Err(e) =
                                                tab.update_active_pane_name(raw_bytes, client_id)
                                            {
                                                log::error!("{}", e);
                                            }
                                            state_changed = true;
                                        }
                                    },
                                    _ => {
                                        let write_result = match tab.is_sync_panes_active() {
                                            true => tab.write_to_terminals_on_current_tab(
                                                &key_with_modifier,
                                                raw_bytes,
                                                is_kitty_keyboard_protocol,
                                                client_id,
                                            ),
                                            false => tab.write_to_active_terminal(
                                                &key_with_modifier,
                                                raw_bytes,
                                                is_kitty_keyboard_protocol,
                                                client_id,
                                            ),
                                        };
                                        if let Ok(true) = write_result {
                                            state_changed = true;
                                        }
                                    },
                                }
                            }
                        );
                    },
                };
                if state_changed {
                    screen.log_and_report_session_state()?;
                }
                screen.render(None)?;
            },
            ScreenInstruction::Resize(
                client_id,
                strategy,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.resize(client_id, strategy),
                    ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::SwitchFocus(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.focus_next_pane(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::FocusNextPane(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to change focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to change focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.focus_next_pane(client_id)
                    );
                    screen.render(None)?;
                }
            },
            ScreenInstruction::FocusPreviousPane(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to change focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to change focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.focus_previous_pane(client_id)
                    );
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusLeft(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.move_focus_left(client_id),
                        ?
                    );
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusLeftOrPreviousTab(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    screen.move_focus_left_or_previous_tab(client_id)?;
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusDown(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.move_focus_down(client_id),
                        ?
                    );
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusRight(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.move_focus_right(client_id),
                        ?
                    );
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusRightOrNextTab(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    screen.move_focus_right_or_next_tab(client_id)?;
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::MoveFocusUp(client_id, mut _completion_tx) => {
                if screen.get_first_client_id().is_none() {
                    log::error!("No connected clients to move focus for");
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message("No connected clients to move focus for".to_string());
                    }
                } else {
                    active_tab_and_connected_client_id!(
                        screen,
                        client_id,
                        |tab: &mut Tab, client_id: ClientId| tab.move_focus_up(client_id),
                        ?
                    );
                    screen.clear_bell_for_focused_pane(client_id);
                    screen.add_active_pane_to_group_if_marking(&client_id);
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::ClearScreen(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.clear_active_terminal_screen(
                        client_id,
                    ),
                    ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::DumpScreen(
                file,
                client_id,
                full,
                pane_id,
                mut completion_tx,
                cli_client_id,
                ansi,
                target_identity,
            ) => {
                let dump_result: Result<Option<String>> = (|| {
                    let mut dump_client_id = client_id;
                    let tab = if let Some(target) = target_identity.as_ref() {
                        if screen.session_incarnation != target.session_incarnation {
                            return Err(anyhow!(
                                "refusing dump: expected session incarnation {:?}, current {:?}",
                                target.session_incarnation,
                                screen.session_incarnation
                            ));
                        }
                        let tab = screen.tabs.get_mut(&target.tab_id).ok_or_else(|| {
                            anyhow!("refusing dump: tab ID {} no longer exists", target.tab_id)
                        })?;
                        if tab.instance_id != target.tab_instance_id {
                            return Err(anyhow!(
                                "refusing dump: tab ID {} instance changed from {:?} to {:?}",
                                target.tab_id,
                                target.tab_instance_id,
                                tab.instance_id
                            ));
                        }
                        if tab.name != target.tab_name {
                            return Err(anyhow!(
                                "refusing dump: tab ID {} name changed from {:?} to {:?}",
                                target.tab_id,
                                target.tab_name,
                                tab.name
                            ));
                        }
                        let pane_id = pane_id
                            .ok_or_else(|| anyhow!("typed dump requires an explicit pane ID"))?;
                        if !tab.has_pane_with_pid(&pane_id) {
                            return Err(anyhow!(
                                "refusing dump: pane {:?} does not belong to tab ID {}",
                                pane_id,
                                target.tab_id
                            ));
                        }
                        tab
                    } else if let Some(pane_id) = pane_id {
                        screen
                            .tabs
                            .values_mut()
                            .find(|tab| tab.has_pane_with_pid(&pane_id))
                            .ok_or_else(|| anyhow!("Pane with id {:?} not found", pane_id))?
                    } else {
                        // CLI actions can arrive under an ephemeral client ID
                        // that is not part of the interactive screen state.
                        // Preserve the historical behavior: resolve the first
                        // connected client rather than silently turning a
                        // valid untyped dump into an empty failure.
                        if screen.get_active_tab_mut(client_id).is_err() {
                            dump_client_id = screen
                                .get_first_client_id()
                                .ok_or_else(|| anyhow!("No connected clients to dump"))?;
                        }
                        screen.get_active_tab_mut(dump_client_id)?
                    };

                    if let Some(file_path) = file.as_ref() {
                        match pane_id {
                            Some(pane_id) if ansi => tab.dump_with_ansi_terminal_screen(
                                Some(file_path.clone()),
                                pane_id,
                                full,
                            )?,
                            Some(pane_id) => {
                                tab.dump_terminal_screen(Some(file_path.clone()), pane_id, full)?
                            },
                            None if ansi => tab.dump_with_ansi_active_terminal_screen(
                                Some(file_path.clone()),
                                dump_client_id,
                                full,
                            )?,
                            None => tab.dump_active_terminal_screen(
                                Some(file_path.clone()),
                                dump_client_id,
                                full,
                            )?,
                        }
                        Ok(None)
                    } else {
                        let dump = match pane_id {
                            Some(pane_id) if ansi => tab
                                .get_dump_with_ansi_terminal_screen(pane_id, full)
                                .ok_or_else(|| {
                                    anyhow!("pane {:?} has no dumpable terminal screen", pane_id)
                                })?,
                            Some(pane_id) => {
                                tab.get_dump_terminal_screen(pane_id, full).ok_or_else(|| {
                                    anyhow!("pane {:?} has no dumpable terminal screen", pane_id)
                                })?
                            },
                            None if ansi => {
                                tab.get_dump_with_ansi_active_terminal_screen(dump_client_id, full)
                            },
                            None => tab.get_dump_active_terminal_screen(dump_client_id, full),
                        };
                        Ok(Some(dump))
                    }
                })();

                match dump_result {
                    Ok(Some(dump)) => {
                        if let Err(error) =
                            screen.bus.senders.send_to_server(ServerInstruction::Log(
                                vec![dump],
                                cli_client_id.unwrap_or(client_id),
                                completion_tx,
                            ))
                        {
                            log::error!("Failed to return screen dump: {}", error);
                        }
                    },
                    Ok(None) => drop(completion_tx),
                    Err(error) => {
                        let error = dump_screen_error_message(&error);
                        log::error!("Failed to dump screen: {}", error);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.set_exit_status(1);
                            completion.set_error_message(error);
                        }
                        drop(completion_tx);
                    },
                }
            },
            ScreenInstruction::CopyPaneScrollback(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| {
                        let text = tab.get_dump_active_terminal_screen(client_id, true);
                        tab.copy_text_to_clipboard(&text)
                    },
                    ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::DumpLayout(default_shell, client_id, completion_tx) => {
                let err_context = || "Failed to dump layout".to_string();
                let session_layout_metadata = screen.get_layout_metadata(default_shell, None);
                screen
                    .bus
                    .senders
                    .send_to_plugin(PluginInstruction::DumpLayout(
                        session_layout_metadata,
                        client_id,
                        completion_tx,
                    ))
                    .with_context(err_context)?;
            },
            ScreenInstruction::ListClientsMetadata(default_shell, client_id, completion_tx) => {
                let err_context = || "Failed to dump layout".to_string();
                let session_layout_metadata = screen.get_layout_metadata(default_shell, None);
                screen
                    .bus
                    .senders
                    .send_to_plugin(PluginInstruction::ListClientsMetadata(
                        session_layout_metadata,
                        client_id,
                        completion_tx,
                    ))
                    .with_context(err_context)?;
            },
            ScreenInstruction::ListPanes {
                show_all,
                response_channel,
            } => {
                let err_context = || "Failed to list panes";
                let pane_entries = screen
                    .collect_pane_list(show_all)
                    .with_context(err_context)?;
                let _ = response_channel.send(pane_entries);
            },
            ScreenInstruction::ListTabs {
                client_id,
                response_channel,
            } => {
                let err_context = || "Failed to list tabs";
                let tab_infos = screen
                    .collect_tab_list(client_id)
                    .with_context(err_context)?;
                let _ = response_channel.send(tab_infos);
            },
            ScreenInstruction::GetCurrentTabInfo {
                client_id,
                response_channel,
            } => {
                let err_context = || "Failed to get current tab info";
                let tab_info = screen
                    .get_current_tab_info(client_id)
                    .with_context(err_context)?;
                let _ = response_channel.send(tab_info);
            },
            ScreenInstruction::DumpLayoutToPlugin {
                plugin_id,
                tab_index,
                response_channel,
            } => {
                let err_context = || "Failed to dump layout".to_string();
                let session_layout_metadata =
                    screen.get_layout_metadata(Some(screen.default_shell.clone()), tab_index);
                screen
                    .bus
                    .senders
                    .send_to_pty(PtyInstruction::DumpLayoutToPlugin {
                        session_layout_metadata,
                        plugin_id,
                        response_channel,
                    })
                    .with_context(err_context)
                    .non_fatal();
            },
            ScreenInstruction::GetFocusedPaneInfo {
                client_id,
                response_channel,
            } => {
                let response = match screen.active_tab_ids.get(&client_id) {
                    Some(&focused_tab_index) => match screen.get_active_pane_id(&client_id) {
                        Some(focused_pane_id) => GetFocusedPaneInfoResponse::Ok {
                            tab_index: focused_tab_index,
                            pane_id: focused_pane_id.into(),
                        },
                        None => GetFocusedPaneInfoResponse::Err(format!(
                            "No active pane found for client {:?}",
                            client_id
                        )),
                    },
                    None => GetFocusedPaneInfoResponse::Err(format!(
                        "Client {:?} not found in active_tab_indices",
                        client_id
                    )),
                };
                let _ = response_channel.send(response);
            },
            ScreenInstruction::GetPaneInfo {
                pane_id,
                response_channel,
            } => {
                let pane_info = screen.get_pane_info(pane_id);
                let _ = response_channel.send(pane_info);
            },
            ScreenInstruction::GetTabInfo {
                tab_id,
                response_channel,
            } => {
                let tab_info = screen.get_tab_info(tab_id);
                let _ = response_channel.send(tab_info);
            },
            ScreenInstruction::ListClientsToPlugin(plugin_id, client_id) => {
                let err_context = || "Failed to dump layout".to_string();
                let session_layout_metadata =
                    screen.get_layout_metadata(Some(screen.default_shell.clone()), None);
                screen
                    .bus
                    .senders
                    .send_to_pty(PtyInstruction::ListClientsToPlugin(
                        session_layout_metadata,
                        plugin_id,
                        client_id,
                    ))
                    .with_context(err_context)
                    .non_fatal();
            },
            ScreenInstruction::EditScrollback(client_id, ansi, completion_tx) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| {
                        if ansi {
                            tab.edit_scrollback_raw(client_id, completion_tx)
                        } else {
                            tab.edit_scrollback(client_id, completion_tx)
                        }
                    },
                    ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::GetPaneScrollback {
                pane_id,
                client_id,
                get_full_scrollback,
                response_channel,
            } => {
                let mut pane_contents: Option<PaneContents> = None;
                for tab in screen.get_tabs_mut().values() {
                    if let Some(pane) = tab.get_pane_with_id(pane_id) {
                        pane_contents =
                            Some(pane.pane_contents(Some(client_id), get_full_scrollback, None));
                        break;
                    }
                }
                // Send response back through channel
                let response = match pane_contents {
                    Some(contents) => PaneScrollbackResponse::Ok(contents),
                    None => {
                        log::warn!(
                            "Plugin requested scrollback for pane {:?} but pane was not found",
                            pane_id
                        );
                        PaneScrollbackResponse::Err(format!("Pane {:?} not found", pane_id))
                    },
                };
                if response_channel.send(response).is_err() {
                    // the plugin likely timed out and dropped the receiver
                    log::debug!(
                        "Plugin timed out before pane scrollback response was sent for pane {:?}",
                        pane_id
                    );
                }
            },
            ScreenInstruction::ScrollUp(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.scroll_active_terminal_up(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::MovePane(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneBackwards(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane_backwards(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneDown(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane_down(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneUp(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane_up(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneRight(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane_right(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneLeft(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.move_active_pane_left(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::ScrollUpAt(
                point,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .handle_scrollwheel_up(&point, 3, client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ScrollDown(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.scroll_active_terminal_down(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ScrollDownAt(
                point,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .handle_scrollwheel_down(&point, 3, client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToBottom(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_to_bottom(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToTop(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_to_top(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollUp(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_up_page(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollDown(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_down_page(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::HalfPageScrollUp(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_up_half_page(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::HalfPageScrollDown(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .scroll_active_terminal_down_half_page(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ClearScroll(client_id) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .clear_active_terminal_scroll(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::CloseFocusedPane(client_id, completion_tx) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.close_focused_pane(client_id, completion_tx), ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::SetSelectable(pid, selectable) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found_plugin = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pid) {
                        tab.set_pane_selectable(pid, selectable);
                        found_plugin = true;
                        break;
                    }
                }
                if !found_plugin {
                    pending_events_waiting_for_tab
                        .push(ScreenInstruction::SetSelectable(pid, selectable));
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::ShowPluginCursor(pid, client_id, cursor_position) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found_plugin = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_plugin(pid) {
                        tab.show_plugin_cursor(pid, client_id, cursor_position);
                        found_plugin = true;
                        break;
                    }
                }
                if !found_plugin {
                    pending_events_waiting_for_tab.push(ScreenInstruction::ShowPluginCursor(
                        pid,
                        client_id,
                        cursor_position,
                    ));
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::SetMouseSelectionSupport(pid, selection_support) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found_plugin = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pid) {
                        tab.set_mouse_selection_support(pid, selection_support);
                        found_plugin = true;
                        break;
                    }
                }
                if !found_plugin {
                    pending_events_waiting_for_tab.push(
                        ScreenInstruction::SetMouseSelectionSupport(pid, selection_support),
                    );
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::ClosePane(
                id,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                // waiting for it
                exit_status,
            ) => {
                match client_id {
                    Some(client_id) => {
                        active_tab!(screen, client_id, |tab: &mut Tab| tab.close_pane(
                            id,
                            false,
                            exit_status
                        ));
                    },
                    None => {
                        let mut found = false;
                        for tab in screen.tabs.values_mut() {
                            if tab.get_all_pane_ids().contains(&id) {
                                tab.close_pane(id, false, exit_status);
                                found = true;
                                break;
                            }
                        }
                        if !found {
                            pending_events_waiting_for_pane
                                .entry(id)
                                .or_default()
                                .push(ScreenInstruction::ClosePane(id, None, None, exit_status));
                        }
                    },
                }

                // Clean up PTY-side resources (async reader task, child PID mapping,
                // terminal_id_to_raw_fd entry). This is needed because the natural
                // child exit path (quit_cb) only sends ScreenInstruction::ClosePane
                // and never sends PtyInstruction::ClosePane. The handler in Pty is
                // idempotent, so this is safe even if ClosePane was already sent.
                let _ = screen
                    .bus
                    .senders
                    .send_to_pty(PtyInstruction::ClosePane(id, None));

                screen.log_and_report_session_state()?;
                screen.retain_only_existing_panes_in_pane_groups();
            },
            ScreenInstruction::HoldPane(id, exit_status, run_command) => {
                let is_first_run = false;
                let mut found = false;
                for tab in screen.tabs.values_mut() {
                    if tab.get_all_pane_ids().contains(&id) {
                        tab.hold_pane(id, exit_status, is_first_run, run_command.clone());
                        found = true;
                        break;
                    }
                }
                if !found {
                    pending_events_waiting_for_pane
                        .entry(id)
                        .or_default()
                        .push(ScreenInstruction::HoldPane(id, exit_status, run_command));
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UpdatePaneName(
                c,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.update_active_pane_name(c, client_id), ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UndoRenamePane(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.undo_active_rename_pane(client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::ToggleActiveTerminalFullscreen(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .toggle_active_pane_fullscreen(client_id)
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::TogglePaneFrames(
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.draw_pane_frames = !screen.draw_pane_frames;
                for tab in screen.tabs.values_mut() {
                    tab.set_pane_frames(screen.draw_pane_frames);
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::SwitchTabNext(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.switch_tab_next(None, true, client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::SwitchTabPrev(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.switch_tab_prev(None, true, client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::CloseTab(client_id, mut completion_tx) => {
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                }
                let result = screen
                    .close_tab(client_id)
                    .and_then(|_| screen.render(None));
                match result {
                    Ok(()) => {
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_success();
                        }
                    },
                    Err(error) => {
                        let message = format!("failed to close active tab: {error:#}");
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_failure(message.clone());
                        }
                        screen.log_and_report_session_state().non_fatal();
                        log::error!("{message}");
                    },
                }
            },
            ScreenInstruction::NewTab(
                cwd,
                default_shell,
                mut layout,
                floating_panes_layout,
                tab_name,
                (swap_tiled_layouts, swap_floating_layouts),
                initial_panes,
                block_on_first_terminal,
                should_change_focus_to_new_tab,
                placement,
                (client_id, is_web_client),
                mut completion_tx,
            ) => {
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                }
                let encoded_tab_instance_id = layout
                    .as_ref()
                    .and_then(|layout| layout.tab_instance_id.as_deref())
                    .map(str::to_owned);
                let decoded_tab_instance_id = encoded_tab_instance_id
                    .as_deref()
                    .map(ViewerCreationFence::decode_tab_instance_id)
                    .transpose()
                    .and_then(|decoded| {
                        let Some((token, fence)) = decoded else {
                            return Ok((None, None));
                        };
                        if let Some(fence) = fence.as_ref() {
                            fence.verify_current(
                                &screen.session_name,
                                tab_name.as_deref().unwrap_or_default(),
                            )?;
                        }
                        let durable_token = (token.len() == 32
                            && token.bytes().all(|byte| byte.is_ascii_hexdigit()))
                        .then(|| token.to_ascii_lowercase());
                        if fence.is_some() && durable_token.is_none() {
                            return Err(
                                "viewer creation fence has invalid durable token".to_owned()
                            );
                        }
                        Ok((durable_token, fence))
                    });
                let (restored_tab_instance_id, viewer_creation_fence) =
                    match decoded_tab_instance_id {
                        Ok(decoded) => decoded,
                        Err(message) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.set_exit_status(1);
                                completion.set_error_message(message.clone());
                            }
                            log::error!("{}", message);
                            continue;
                        },
                    };
                if viewer_creation_fence.is_some()
                    && let Some(layout) = layout.as_mut()
                {
                    // The receipt path is transport-only. From this point on,
                    // every runtime and persistence surface sees only the
                    // stable 32-hex durable token.
                    layout.tab_instance_id = restored_tab_instance_id.clone();
                }
                let restored_tab_instance_id = restored_tab_instance_id
                    .as_deref()
                    .filter(|instance_id| {
                        instance_id.len() == 32
                            && instance_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                    })
                    .map(str::to_ascii_lowercase);
                let reusable_tab_id = restored_tab_instance_id
                    .as_deref()
                    .map(|instance_id| {
                        screen.reusable_tab_id_for_instance(
                            instance_id,
                            tab_name.as_deref().unwrap_or_default(),
                        )
                    })
                    .unwrap_or(Ok(None));
                match reusable_tab_id {
                    Ok(Some(existing_tab_id)) => {
                        let restored_tab_instance_id =
                            restored_tab_instance_id.as_deref().unwrap_or_default();
                        let recovery = if block_on_first_terminal
                            || initial_panes
                                .as_ref()
                                .is_some_and(|panes| !panes.is_empty())
                        {
                            Err(
                                "durable tab recovery does not accept blocking or initial panes"
                                    .to_owned(),
                            )
                        } else if let Some(tiled_layout) = layout {
                            let existing_tab_name = screen
                                .tabs
                                .get(&existing_tab_id)
                                .map(|tab| tab.name.clone())
                                .ok_or_else(|| {
                                    format!(
                                        "durable tab {} disappeared before layout recovery",
                                        existing_tab_id
                                    )
                                });
                            existing_tab_name.and_then(|existing_tab_name| {
                                let live_identity_matches =
                                    screen.tabs.get(&existing_tab_id).is_some_and(|tab| {
                                        tab.name == existing_tab_name
                                            && tab
                                                .instance_id
                                                .eq_ignore_ascii_case(restored_tab_instance_id)
                                    });
                                if !live_identity_matches {
                                    return Err(format!(
                                        "durable tab {} changed identity before layout recovery",
                                        existing_tab_id
                                    ));
                                }
                                let generation = reserve_durable_tab_layout_recovery(
                                    &mut durable_tab_layout_generations,
                                    existing_tab_id,
                                    &existing_tab_name,
                                    restored_tab_instance_id,
                                    viewer_creation_fence.clone(),
                                )?;
                                let mut tab_layout_info = TabLayoutInfo {
                                    tab_index: existing_tab_id,
                                    tab_name: Some(existing_tab_name),
                                    tiled_layout,
                                    floating_layouts: floating_panes_layout,
                                    swap_tiled_layouts,
                                    swap_floating_layouts,
                                };
                                let tab =
                                    screen.tabs.get_mut(&existing_tab_id).ok_or_else(|| {
                                        format!(
                                            "durable tab {} disappeared before layout preparation",
                                            existing_tab_id
                                        )
                                    })?;
                                prepare_existing_tab_layout(&mut tab_layout_info, tab);
                                Ok((tab_layout_info, generation))
                            })
                        } else {
                            Err("durable tab recovery requires its original layout".to_owned())
                        };
                        match recovery {
                            Ok((tab_layout_info, generation)) => {
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.set_affected_tab_id(existing_tab_id);
                                }
                                pending_tab_ids.insert(existing_tab_id);
                                log::info!(
                                    "NewTab: recovering tab {} for durable instance {} at generation {}",
                                    existing_tab_id,
                                    restored_tab_instance_id,
                                    generation.generation
                                );
                                let transaction_id = screen.reserve_layout_transaction_id();
                                let target = LayoutTabOwner::capture(&screen, existing_tab_id);
                                let transaction = ActiveLayoutTransaction {
                                    kind: ScreenLayoutTransactionKind::DurableRecovery,
                                    targets: vec![target],
                                    created_pending_tabs: vec![],
                                    render_fenced_tabs: vec![],
                                    tabs_to_close_after_commit: vec![],
                                    moved_original_panes: vec![],
                                    generation: Some(generation.clone()),
                                };
                                if let Err(error) =
                                    screen.register_layout_transaction(transaction_id, transaction)
                                {
                                    pending_tab_ids.remove(&existing_tab_id);
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(format!("{error:#}"));
                                    }
                                    log::error!("{error:#}");
                                    continue;
                                }
                                let instruction = PluginInstruction::OverrideLayout(
                                    cwd,
                                    default_shell,
                                    vec![tab_layout_info],
                                    transaction_id,
                                    true,
                                    true,
                                    client_id,
                                    completion_tx,
                                    Some(Box::new(generation)),
                                );
                                if let Err(send_failure) =
                                    screen.bus.senders.send_to_plugin_recover(instruction)
                                {
                                    let (instruction, send_error) = send_failure.into_parts();
                                    let (mut recovered_completion, recovered_expected_kind) =
                                        match instruction {
                                            PluginInstruction::OverrideLayout(
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                recovered_completion,
                                                _,
                                            ) => (recovered_completion, true),
                                            _ => (None, false),
                                        };
                                    screen.active_layout_transactions.remove(&transaction_id);
                                    pending_tab_ids.remove(&existing_tab_id);
                                    let message = if recovered_expected_kind {
                                        format!(
                                            "failed to hand durable layout transaction {transaction_id} to Plugin: {send_error:#}"
                                        )
                                    } else {
                                        format!(
                                            "Plugin handoff returned an unexpected instruction while rejecting durable layout transaction {transaction_id}: {send_error:#}"
                                        )
                                    };
                                    if let Some(completion) = recovered_completion.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                    log::error!("{message}");
                                }
                            },
                            Err(message) => {
                                log::error!("{}", message);
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.set_exit_status(1);
                                    completion.set_error_message(message);
                                }
                            },
                        }
                    },
                    Ok(None) => {
                        let tab_index = screen.get_new_tab_id();
                        pending_tab_ids.insert(tab_index);
                        let client_id_for_new_tab = if should_change_focus_to_new_tab {
                            Some(client_id)
                        } else {
                            None
                        };
                        let resolved_swap_layouts = (
                            swap_tiled_layouts.unwrap_or_else(|| {
                                screen.default_layout.swap_tiled_layouts.clone()
                            }),
                            swap_floating_layouts.unwrap_or_else(|| {
                                screen.default_layout.swap_floating_layouts.clone()
                            }),
                        );
                        if let Err(error) = screen.new_tab(
                            tab_index,
                            resolved_swap_layouts,
                            tab_name.clone(),
                            client_id_for_new_tab,
                            placement,
                        ) {
                            pending_tab_ids.remove(&tab_index);
                            let message =
                                format!("failed to create pending tab {tab_index}: {error:#}");
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            log::error!("{message}");
                            continue;
                        }
                        let layout_generation = if let Some(restored_tab_instance_id) =
                            restored_tab_instance_id
                        {
                            let Some(tab) = screen.tabs.get_mut(&tab_index) else {
                                let message = format!("new durable tab {tab_index} disappeared");
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.mark_failure(message.clone());
                                }
                                pending_tab_ids.remove(&tab_index);
                                log::error!("{message}");
                                continue;
                            };
                            tab.instance_id = restored_tab_instance_id.clone();
                            Some(Box::new(reserve_new_durable_tab_layout_generation(
                                &mut durable_tab_layout_generations,
                                tab_index,
                                &tab.name,
                                &restored_tab_instance_id,
                                viewer_creation_fence.clone(),
                            )))
                        } else {
                            None
                        };
                        let transaction_id = screen.reserve_layout_transaction_id();
                        let target = LayoutTabOwner::capture(&screen, tab_index);
                        let transaction = ActiveLayoutTransaction {
                            kind: ScreenLayoutTransactionKind::NewTab,
                            targets: vec![target.clone()],
                            created_pending_tabs: vec![target],
                            render_fenced_tabs: vec![],
                            tabs_to_close_after_commit: vec![],
                            moved_original_panes: vec![],
                            generation: layout_generation.as_deref().cloned(),
                        };
                        if let Err(error) =
                            screen.register_layout_transaction(transaction_id, transaction)
                        {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(format!("{error:#}"));
                            }
                            screen
                                .discard_pending_tab_after_layout_rejection(tab_index)
                                .non_fatal();
                            pending_tab_ids.remove(&tab_index);
                            log::error!("{error:#}");
                            continue;
                        }
                        let instruction = PluginInstruction::NewTab(
                            cwd,
                            default_shell,
                            layout,
                            floating_panes_layout,
                            tab_index,
                            transaction_id,
                            initial_panes,
                            block_on_first_terminal,
                            should_change_focus_to_new_tab,
                            (client_id, is_web_client),
                            completion_tx,
                            layout_generation,
                        );
                        if let Err(send_failure) =
                            screen.bus.senders.send_to_plugin_recover(instruction)
                        {
                            let (instruction, send_error) = send_failure.into_parts();
                            let (mut recovered_completion, recovered_expected_kind) =
                                match instruction {
                                    PluginInstruction::NewTab(
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        _,
                                        recovered_completion,
                                        _,
                                    ) => (recovered_completion, true),
                                    _ => (None, false),
                                };
                            let transaction =
                                screen.active_layout_transactions.remove(&transaction_id);
                            if let Some(transaction) = transaction.as_ref() {
                                screen
                                    .discard_owned_pending_tabs(transaction, &mut pending_tab_ids);
                            }
                            let message = if recovered_expected_kind {
                                format!(
                                    "failed to hand layout transaction {transaction_id} to Plugin: {send_error:#}"
                                )
                            } else {
                                format!(
                                    "Plugin handoff returned an unexpected instruction while rejecting layout transaction {transaction_id}: {send_error:#}"
                                )
                            };
                            if let Some(completion) = recovered_completion.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            log::error!("{message}");
                        }
                    },
                    Err(message) => {
                        log::error!("{}", message);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.set_exit_status(1);
                            completion.set_error_message(message);
                        }
                    },
                }
            },
            ScreenInstruction::ApplyLayout(
                layout,
                floating_panes_layout,
                new_pane_pids,
                new_floating_pane_pids,
                new_plugin_ids,
                tab_id,
                should_change_focus_to_new_tab,
                (client_id, is_web_client),
                mut completion_tx,
                mut blocking_terminal,
                layout_generation,
                transaction_id,
            ) => {
                #[cfg(test)]
                let transaction_id = screen.resolve_legacy_test_layout_transaction_id(
                    transaction_id,
                    &[
                        ScreenLayoutTransactionKind::NewTab,
                        ScreenLayoutTransactionKind::BreakPane,
                    ],
                    &[tab_id],
                );
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                }
                if let Some((_, completion)) = blocking_terminal.as_mut() {
                    completion.require_explicit_resolution();
                }
                if let Some(indeterminate) = screen
                    .indeterminate_layout_transactions
                    .get(&transaction_id)
                {
                    let message = indeterminate.replay_rejection(transaction_id);
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    if let Some((_, completion)) = blocking_terminal.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    log::error!("{message}");
                    continue;
                }
                let raw_installed_resource_ids =
                    layout_resource_ids(&new_pane_pids, &new_floating_pane_pids, &new_plugin_ids);
                let mut installed_resource_ids = raw_installed_resource_ids.clone();
                installed_resource_ids.sort_unstable();
                installed_resource_ids.dedup();
                let mut expected_plugin_ids = new_plugin_ids
                    .values()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>();
                expected_plugin_ids.sort_unstable();
                expected_plugin_ids.dedup();
                let registered_owner = screen
                    .active_layout_transactions
                    .get(&transaction_id)
                    .cloned();
                if let Some(replay) = screen.replay_resolved_layout_transaction(
                    transaction_id,
                    &[
                        ScreenLayoutTransactionKind::NewTab,
                        ScreenLayoutTransactionKind::BreakPane,
                    ],
                    &[tab_id],
                    layout_generation.as_deref(),
                    &raw_installed_resource_ids,
                ) {
                    match replay {
                        Ok(ScreenLayoutDecision::Committed) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.set_affected_tab_id(tab_id);
                                if let Some(resource_id) = installed_resource_ids.first() {
                                    completion.set_affected_pane_id(*resource_id);
                                }
                                completion.mark_success();
                            }
                            if let Some((_, completion)) = blocking_terminal.as_mut() {
                                completion.mark_success();
                            }
                        },
                        Ok(ScreenLayoutDecision::CommittedWithCleanupDebt(message))
                        | Ok(ScreenLayoutDecision::CommittedWithPostCommitError(message))
                        | Ok(ScreenLayoutDecision::Rejected(message))
                        | Err(message) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            if let Some((_, completion)) = blocking_terminal.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                        },
                    }
                    continue;
                }
                if transaction_id != 0 && registered_owner.is_none() {
                    let message = format!(
                        "unknown layout transaction {transaction_id}; refusing to manufacture a Plugin/PTY resolution for an unowned completion"
                    );
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    if let Some((_, completion)) = blocking_terminal.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    log::error!("{message}");
                    continue;
                }
                // A newer generation of the same durable viewer will reuse and
                // heal this empty tab. Keep it pending so render GC cannot
                // delete the stable identity between the two writers.
                let mut preserve_pending_tab_on_rejection = false;
                let mut close_fenced_tab_on_rejection = false;
                let mut prepared_apply_layout = None;
                let mut validated_owner = None;
                let transaction_result: Result<()> = (|| {
                    if installed_resource_ids.len() != raw_installed_resource_ids.len() {
                        bail!(
                            "layout transaction {transaction_id} returned duplicate Apply resource ids: {raw_installed_resource_ids:?}"
                        );
                    }
                    if transaction_id != 0 {
                        validated_owner = Some(screen.validate_layout_transaction(
                            transaction_id,
                            &[
                                ScreenLayoutTransactionKind::NewTab,
                                ScreenLayoutTransactionKind::BreakPane,
                            ],
                            &[tab_id],
                            layout_generation.as_deref(),
                        )?);
                    }
                    if let Some(layout_generation) = layout_generation.as_ref()
                        && !durable_tab_layout_generation_is_current(
                            &screen,
                            &durable_tab_layout_generations,
                            layout_generation,
                        )
                    {
                        bail!(
                            "discarded stale durable tab layout generation {} for tab {} '{}'",
                            layout_generation.generation,
                            layout_generation.tab_id,
                            layout_generation.tab_name
                        );
                    }
                    if let Some(layout_generation) = layout_generation.as_ref()
                        && let Err(rejection) =
                            verify_global_viewer_creation_fence(&screen, layout_generation)
                    {
                        if rejection.should_close_exact_tab() {
                            close_fenced_tab_on_rejection = true;
                        } else {
                            preserve_pending_tab_on_rejection = true;
                        }
                        return Err(anyhow!(rejection.to_string()));
                    }

                    log::info!(
                        "ScreenInstruction::ApplyLayout: applying layout for tab {}",
                        tab_id
                    );
                    // tab_id is a stable identifier from NewTab instruction
                    if let Some(first_terminal_pane) = new_pane_pids.first() {
                        if let Some(c) = completion_tx.as_mut() {
                            c.set_affected_pane_id(PaneId::Terminal(first_terminal_pane.0))
                        }
                    } else if let Some(plugin_id) =
                        new_plugin_ids.values().next().and_then(|v| v.first())
                        && let Some(c) = completion_tx.as_mut()
                    {
                        c.set_affected_pane_id(PaneId::Plugin(*plugin_id))
                    }
                    // Set the affected tab ID for plugin API return value
                    if let Some(c) = completion_tx.as_mut() {
                        c.set_affected_tab_id(tab_id)
                    }
                    if !screen.tabs.contains_key(&tab_id) {
                        bail!("Tab with index {tab_id} not found. Cannot apply layout!");
                    }
                    prepared_apply_layout = Some(screen.prepare_apply_layout(
                        layout,
                        floating_panes_layout,
                        new_pane_pids.clone(),
                        new_floating_pane_pids,
                        new_plugin_ids.clone(),
                        tab_id,
                        should_change_focus_to_new_tab,
                        (client_id, is_web_client),
                        blocking_terminal.take(),
                    )?);
                    #[cfg(test)]
                    if take_reject_after_apply_prepare_for_test(transaction_id) {
                        bail!("injected rejection after Apply prepare");
                    }
                    #[cfg(test)]
                    if let Some(layout_generation) = layout_generation.as_ref() {
                        pause_after_viewer_creation_install_for_test(layout_generation);
                    }
                    if let Some(layout_generation) = layout_generation.as_ref()
                        && let Err(rejection) =
                            verify_global_viewer_creation_fence(&screen, layout_generation)
                    {
                        if rejection.should_close_exact_tab() {
                            close_fenced_tab_on_rejection = true;
                        } else {
                            preserve_pending_tab_on_rejection = true;
                        }
                        return Err(anyhow!(rejection.to_string()));
                    }

                    let prepared = prepared_apply_layout.as_ref().with_context(|| {
                        format!(
                            "prepared Apply transaction {transaction_id} disappeared before commit preflight"
                        )
                    })?;
                    let tab = screen.tabs.get(&tab_id).with_context(|| {
                        format!(
                            "prepared Apply target tab {tab_id} disappeared before commit preflight"
                        )
                    })?;
                    prepared.transaction.preflight_commit(tab)?;
                    Ok(())
                })();

                let reconciliation_intent = match transaction_result {
                    Ok(()) => LayoutReconciliationIntent::Activate,
                    Err(error) => LayoutReconciliationIntent::Reject(format!("{error:#}")),
                };
                let reconciliation_plan = LayoutReconciliationPlan {
                    intent: reconciliation_intent.clone(),
                    expected_plugin_ids: expected_plugin_ids.clone(),
                    resource_ids: installed_resource_ids.clone(),
                    preserve_pending_tab_on_rejection,
                    close_fenced_tab_on_rejection,
                    layout_generation: layout_generation.as_deref().cloned(),
                };
                let coordination = match &reconciliation_intent {
                    LayoutReconciliationIntent::Activate => coordinate_layout_activation(
                        &screen.bus.senders,
                        transaction_id,
                        &expected_plugin_ids,
                    ),
                    LayoutReconciliationIntent::Reject(rejection) => coordinate_layout_rejection(
                        &screen.bus.senders,
                        transaction_id,
                        &expected_plugin_ids,
                        rejection.clone(),
                    ),
                    LayoutReconciliationIntent::RejectByOwner(_) => {
                        unreachable!("Apply completion cannot originate by-owner rejection retry")
                    },
                    LayoutReconciliationIntent::PreparationFailure { .. } => {
                        unreachable!("Apply completion cannot originate preparation-failure retry")
                    },
                };
                let mut retire_active_transaction = true;
                let mut post_commit_error = None;
                let mut committed_blocking_terminal = None;
                match coordination {
                    LayoutCoordination::Commit => {
                        if let Some(prepared) = prepared_apply_layout.take() {
                            let screen_commit_succeeded = match screen
                                .commit_apply_layout_state(prepared)
                            {
                                Ok(mut committed) => {
                                    let cleanup = committed.effects.take_pending_cleanup();
                                    screen.retain_layout_cleanup(transaction_id, cleanup);
                                    committed_blocking_terminal =
                                        screen.emit_committed_apply_layout(committed);
                                    if let Some(owner) =
                                        validated_owner.as_ref().or(registered_owner.as_ref())
                                        && let Err(error) = screen
                                            .close_owned_tabs_after_layout_commit(
                                                transaction_id,
                                                owner,
                                            )
                                    {
                                        post_commit_error = Some(format!("{error:#}"));
                                    }
                                    screen.flush_layout_cleanup(transaction_id);
                                    true
                                },
                                Err(mut prepared) => {
                                    let message = format!(
                                        "layout transaction {transaction_id} activated externally but its preflighted Screen target tab {} disappeared; preserved the complete prepared owner as indeterminate",
                                        prepared.tab_id
                                    );
                                    prepared
                                        .transaction
                                        .mark_blocking_completion_failed(&message);
                                    screen.indeterminate_layout_transactions.insert(
                                        transaction_id,
                                        IndeterminatePreparedLayout::Apply {
                                            prepared,
                                            plan: reconciliation_plan.clone(),
                                        },
                                    );
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                    retire_active_transaction = false;
                                    log::error!("{message}");
                                    false
                                },
                            };
                            if screen_commit_succeeded {
                                let cleanup_decision =
                                    screen.pending_layout_cleanup_message(transaction_id);
                                if let Some(message) =
                                    post_commit_error.as_ref().or(cleanup_decision.as_ref())
                                {
                                    if let Some((_, mut completion)) =
                                        committed_blocking_terminal.take()
                                    {
                                        completion.mark_failure(message.clone());
                                    }
                                } else if let Some((terminal_id, completion)) =
                                    committed_blocking_terminal.take()
                                {
                                    let attachment_result =
                                        if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                                            tab.attach_blocking_layout_completion(
                                                terminal_id,
                                                completion,
                                            )
                                        } else {
                                            Err(completion)
                                        };
                                    if let Err(mut completion) = attachment_result {
                                        let message = format!(
                                            "layout transaction {transaction_id} committed but terminal {terminal_id} rejected blocking completion attachment"
                                        );
                                        completion.mark_failure(message.clone());
                                        post_commit_error = Some(message);
                                    }
                                }
                                if let Some(message) = post_commit_error.as_ref() {
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                } else if let Some(message) = cleanup_decision.as_ref() {
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                } else if let Some(completion) = completion_tx.as_mut() {
                                    completion.mark_success();
                                }
                                if let Some(owner) =
                                    validated_owner.as_ref().or(registered_owner.as_ref())
                                {
                                    screen.record_resolved_layout_transaction(
                                        transaction_id,
                                        owner,
                                        installed_resource_ids.clone(),
                                        post_commit_error.clone().map_or_else(
                                            || {
                                                cleanup_decision.clone().map_or(
                                                    ScreenLayoutDecision::Committed,
                                                    ScreenLayoutDecision::CommittedWithCleanupDebt,
                                                )
                                            },
                                            ScreenLayoutDecision::CommittedWithPostCommitError,
                                        ),
                                    );
                                }
                                if let Some(owner) =
                                    validated_owner.as_ref().or(registered_owner.as_ref())
                                {
                                    screen.retire_layout_transaction_from_pending_gate(
                                        transaction_id,
                                        owner,
                                        &mut pending_tab_ids,
                                    );
                                } else {
                                    pending_tab_ids.remove(&tab_id);
                                }
                                if pending_tab_ids.is_empty() {
                                    for (tab_index, pending_client_id) in
                                        pending_tab_switches.drain()
                                    {
                                        screen
                                            .go_to_tab(tab_index + 1, pending_client_id)
                                            .non_fatal();
                                    }
                                    if should_change_focus_to_new_tab
                                        && let Some(tab_position) =
                                            screen.get_tab_position_by_id(tab_id)
                                    {
                                        screen.go_to_tab(tab_position + 1, client_id).non_fatal();
                                    }
                                } else if should_change_focus_to_new_tab {
                                    let client_id_to_switch =
                                        if screen.active_tab_ids.contains_key(&client_id) {
                                            Some(client_id)
                                        } else {
                                            screen.active_tab_ids.keys().next().copied()
                                        };
                                    if let Some(client_id_to_switch) = client_id_to_switch
                                        && let Some(tab_position) =
                                            screen.get_tab_position_by_id(tab_id)
                                    {
                                        pending_tab_switches
                                            .insert((tab_position, client_id_to_switch));
                                    }
                                }

                                for plugin_ids in new_plugin_ids.values() {
                                    for plugin_id in plugin_ids {
                                        if let Some(loading_indication) =
                                            plugin_loading_message_cache.remove(plugin_id)
                                        {
                                            screen.update_plugin_loading_stage(
                                                *plugin_id,
                                                loading_indication,
                                            );
                                            screen.render(None).non_fatal();
                                        }
                                        screen.render_blocker.register_blocking_plugin(*plugin_id);
                                    }
                                }
                                for event in pending_events_waiting_for_client.drain(..) {
                                    screen.bus.senders.send_to_screen(event).non_fatal();
                                }
                                for event in pending_events_waiting_for_tab.drain(..) {
                                    screen.bus.senders.send_to_screen(event).non_fatal();
                                }
                                screen.render(None).non_fatal();
                                if let Some(os_input) = &mut screen.bus.os_input {
                                    for (connected_client_id, _is_web_client) in
                                        screen.connected_clients.borrow().iter()
                                    {
                                        log::info!(
                                            "ApplyLayout: sending QueryTerminalSize to client {}",
                                            connected_client_id
                                        );
                                        let _ = os_input.send_to_client(
                                            *connected_client_id,
                                            ServerToClientMsg::QueryTerminalSize,
                                        );
                                    }
                                }
                            }
                        } else {
                            let message = format!(
                                "prepared Apply transaction {transaction_id} disappeared after successful commit preflight; retaining its active owner for reconciliation"
                            );
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            retire_active_transaction = false;
                            log::error!("{message}");
                        }
                    },
                    LayoutCoordination::Rollback(message) => {
                        if let Some(prepared) = prepared_apply_layout.take() {
                            screen.rollback_prepared_apply_layout(prepared, &message);
                        }
                        for resource_id in &installed_resource_ids {
                            if let PaneId::Plugin(plugin_id) = resource_id {
                                plugin_loading_message_cache.remove(plugin_id);
                            }
                        }
                        remove_layout_resources_from_screen(&mut screen, &installed_resource_ids);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_failure(message.clone());
                        } else if let Some((_, completion)) = blocking_terminal.as_mut() {
                            completion.mark_failure(message.clone());
                        }
                        if let Some(owner) = validated_owner.as_ref().or(registered_owner.as_ref())
                        {
                            screen.record_resolved_layout_transaction(
                                transaction_id,
                                owner,
                                installed_resource_ids.clone(),
                                ScreenLayoutDecision::Rejected(message.clone()),
                            );
                        }
                        if close_fenced_tab_on_rejection
                            && let Some(layout_generation) = layout_generation.as_ref()
                        {
                            close_globally_stale_fenced_tab(
                                &mut screen,
                                layout_generation,
                                &installed_resource_ids,
                            )
                            .non_fatal();
                            pending_tab_ids.remove(&layout_generation.tab_id);
                        } else if !preserve_pending_tab_on_rejection {
                            if let Some(owner) = validated_owner.as_ref() {
                                if owner.kind == ScreenLayoutTransactionKind::BreakPane {
                                    if let Err(error) = screen
                                        .activate_degraded_break_tab(owner, &mut pending_tab_ids)
                                    {
                                        screen.retire_layout_transaction_from_pending_gate(
                                            transaction_id,
                                            owner,
                                            &mut pending_tab_ids,
                                        );
                                        log::error!(
                                            "layout transaction {transaction_id} could not activate its degraded break-pane destination safely: {error:#}"
                                        );
                                    } else {
                                        screen.render(None).non_fatal();
                                    }
                                } else {
                                    screen.discard_owned_pending_tabs(owner, &mut pending_tab_ids);
                                }
                            } else if transaction_id == 0 {
                                screen
                                    .discard_pending_tab_after_layout_rejection(tab_id)
                                    .non_fatal();
                                pending_tab_ids.remove(&tab_id);
                            } else if let Some(owner) = registered_owner.as_ref() {
                                screen.retire_layout_transaction_from_pending_gate(
                                    transaction_id,
                                    owner,
                                    &mut pending_tab_ids,
                                );
                            }
                        }
                        if let Some(owner) = validated_owner.as_ref().or(registered_owner.as_ref())
                        {
                            screen.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                owner,
                                &mut pending_tab_ids,
                            );
                        }
                        release_pending_layout_gate_if_ready(
                            &mut screen,
                            &pending_tab_ids,
                            &mut pending_tab_switches,
                            &mut pending_events_waiting_for_client,
                            &mut pending_events_waiting_for_tab,
                        );
                        log::warn!(
                            "layout transaction {transaction_id} finished rejected: {message}"
                        );
                    },
                    LayoutCoordination::Unknown(message) => {
                        let mut indeterminate = if let Some(prepared) = prepared_apply_layout.take()
                        {
                            IndeterminatePreparedLayout::Apply {
                                prepared,
                                plan: reconciliation_plan,
                            }
                        } else {
                            if let Some((_, completion)) = blocking_terminal.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            IndeterminatePreparedLayout::ResolutionOnly {
                                target_tab_ids: vec![tab_id],
                                plan: reconciliation_plan,
                            }
                        };
                        indeterminate.mark_blocking_completion_failed(&message);
                        screen
                            .indeterminate_layout_transactions
                            .insert(transaction_id, indeterminate);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_failure(message.clone());
                        }
                        if let Some(owner) = validated_owner.as_ref().or(registered_owner.as_ref())
                        {
                            // The active transaction and indeterminate ledger retain exact
                            // topology/render ownership while worker receipts are unknown.
                            // Do not also retain the global pending-event gate: doing so
                            // wedges unrelated tab events even though background
                            // reconciliation already owns the only unsafe continuation.
                            screen.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                owner,
                                &mut pending_tab_ids,
                            );
                        } else {
                            pending_tab_ids.remove(&tab_id);
                        }
                        release_pending_layout_gate_if_ready(
                            &mut screen,
                            &pending_tab_ids,
                            &mut pending_tab_switches,
                            &mut pending_events_waiting_for_client,
                            &mut pending_events_waiting_for_tab,
                        );
                        retire_active_transaction = false;
                        screen.log_and_report_session_state().non_fatal();
                        log::error!("{message}");
                    },
                }
                if retire_active_transaction && registered_owner.is_some() {
                    screen.active_layout_transactions.remove(&transaction_id);
                }
            },
            ScreenInstruction::LayoutPreparationFailed {
                transaction_id,
                tab_id,
                mut completion_tx,
                layout_generation,
                message,
                cleanup,
            } => {
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                    completion.mark_failure(message.clone());
                }
                let Some(owner) = screen
                    .active_layout_transactions
                    .get(&transaction_id)
                    .cloned()
                else {
                    log::warn!(
                        "ignoring duplicate or late preparation failure for resolved layout transaction {}: {}",
                        transaction_id,
                        message
                    );
                    continue;
                };
                if screen
                    .indeterminate_layout_transactions
                    .contains_key(&transaction_id)
                {
                    log::warn!(
                        "ignoring duplicate preparation failure while layout transaction {transaction_id} is already owned by background reconciliation"
                    );
                    continue;
                }
                let reported_tab_is_owned = tab_id.is_none_or(|reported_tab_id| {
                    owner
                        .targets
                        .iter()
                        .any(|target| target.tab_id == reported_tab_id)
                });
                let exact_failure = reported_tab_is_owned
                    && owner.generation_matches(layout_generation.as_deref())
                    && owner.exact_targets_are_current(&screen);
                if !exact_failure {
                    // The supplied tab/generation/cleanup payload is not
                    // authoritative, so never release its claimed IDs or mutate
                    // its claimed topology. The transaction id is authoritative:
                    // retain the render/pending fence and ask Plugin plus PTY to
                    // reject the resources they themselves own for that exact id.
                    // This turns a malformed producer result into a bounded,
                    // replayable reconciliation instead of an immortal owner.
                    let owner_tab_ids = owner
                        .pending_gate_owners()
                        .map(|owner| owner.tab_id)
                        .collect::<Vec<_>>();
                    let preserve_pending_tab_on_rejection = !owner
                        .exact_targets_are_current(&screen)
                        || owner_tab_ids.iter().any(|owner_tab_id| {
                            screen.tab_has_other_active_layout_owner(transaction_id, *owner_tab_id)
                        });
                    let rejection = format!(
                        "layout transaction {transaction_id} reported a mismatched preparation failure and was rejected by exact worker ownership: {message}"
                    );
                    let plan = LayoutReconciliationPlan {
                        intent: LayoutReconciliationIntent::RejectByOwner(rejection),
                        expected_plugin_ids: vec![],
                        resource_ids: vec![],
                        preserve_pending_tab_on_rejection,
                        close_fenced_tab_on_rejection: false,
                        layout_generation: owner.generation.clone(),
                    };
                    screen.indeterminate_layout_transactions.insert(
                        transaction_id,
                        IndeterminatePreparedLayout::ResolutionOnly {
                            target_tab_ids: owner_tab_ids,
                            plan,
                        },
                    );
                    screen.log_and_report_session_state().non_fatal();
                    log::error!(
                        "reconciling mismatched preparation failure for active layout transaction {} by exact Plugin/PTy ownership while retaining its pending gate: {}",
                        transaction_id,
                        message
                    );
                    continue;
                }
                let owner_tab_ids = owner
                    .targets
                    .iter()
                    .map(|target| target.tab_id)
                    .collect::<Vec<_>>();
                let superseded = owner_tab_ids.iter().any(|owner_tab_id| {
                    screen.tab_has_other_active_layout_owner(transaction_id, *owner_tab_id)
                }) || layout_generation.as_deref().is_some_and(|generation| {
                    !durable_tab_layout_generation_is_current(
                        &screen,
                        &durable_tab_layout_generations,
                        generation,
                    )
                });
                let cleanup_for_retry = cleanup.clone();
                if let Err(cleanup_error) = certify_layout_preparation_cleanup(
                    &screen.bus.senders,
                    transaction_id,
                    cleanup,
                    &message,
                ) {
                    if let LayoutPreparationCleanup::ReleasePluginReservation {
                        mut plugin_ids,
                        pty_cleanup_succeeded: true,
                    } = cleanup_for_retry
                    {
                        plugin_ids.sort_unstable();
                        plugin_ids.dedup();
                        let plan = LayoutReconciliationPlan {
                            intent: LayoutReconciliationIntent::PreparationFailure {
                                failure_message: message.clone(),
                                pty_cleanup_succeeded: true,
                            },
                            expected_plugin_ids: plugin_ids.clone(),
                            resource_ids: plugin_ids.into_iter().map(PaneId::Plugin).collect(),
                            preserve_pending_tab_on_rejection: superseded,
                            close_fenced_tab_on_rejection: false,
                            layout_generation: layout_generation.as_deref().cloned(),
                        };
                        screen.indeterminate_layout_transactions.insert(
                            transaction_id,
                            IndeterminatePreparedLayout::ResolutionOnly {
                                target_tab_ids: owner_tab_ids,
                                plan,
                            },
                        );
                        screen.retire_layout_transaction_from_pending_gate(
                            transaction_id,
                            &owner,
                            &mut pending_tab_ids,
                        );
                    }
                    release_pending_layout_gate_if_ready(
                        &mut screen,
                        &pending_tab_ids,
                        &mut pending_tab_switches,
                        &mut pending_events_waiting_for_client,
                        &mut pending_events_waiting_for_tab,
                    );
                    screen.log_and_report_session_state().non_fatal();
                    log::error!(
                        "quarantining layout transaction {transaction_id} after preparation failure because cleanup is not certified: {cleanup_error}"
                    );
                    continue;
                }

                if !superseded {
                    if owner.kind == ScreenLayoutTransactionKind::BreakPane {
                        if let Err(error) =
                            screen.activate_degraded_break_tab(&owner, &mut pending_tab_ids)
                        {
                            screen.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                &owner,
                                &mut pending_tab_ids,
                            );
                            log::error!(
                                "layout transaction {transaction_id} could not preserve its moved pane in a degraded destination: {error:#}"
                            );
                        } else {
                            screen.render(None).non_fatal();
                        }
                    } else {
                        screen.discard_owned_pending_tabs(&owner, &mut pending_tab_ids);
                        for owner_tab_id in owner_tab_ids {
                            pending_tab_ids.remove(&owner_tab_id);
                        }
                    }
                } else {
                    screen.retire_layout_transaction_from_pending_gate(
                        transaction_id,
                        &owner,
                        &mut pending_tab_ids,
                    );
                }
                release_pending_layout_gate_if_ready(
                    &mut screen,
                    &pending_tab_ids,
                    &mut pending_tab_switches,
                    &mut pending_events_waiting_for_client,
                    &mut pending_events_waiting_for_tab,
                );
                screen.active_layout_transactions.remove(&transaction_id);
                log::warn!(
                    "layout transaction {} failed during Plugin/PTY preparation: {}",
                    transaction_id,
                    message
                );
            },
            #[cfg(test)]
            ScreenInstruction::RetireLayoutTransactionsForTabForTest(tab_id) => {
                let transaction_ids = screen
                    .active_layout_transactions
                    .iter()
                    .filter_map(|(transaction_id, transaction)| {
                        transaction
                            .targets
                            .iter()
                            .any(|target| target.tab_id == tab_id)
                            .then_some(*transaction_id)
                    })
                    .collect::<Vec<_>>();
                for transaction_id in transaction_ids {
                    if let Some(transaction) = screen
                        .active_layout_transactions
                        .get(&transaction_id)
                        .cloned()
                    {
                        screen.retire_layout_transaction_from_pending_gate(
                            transaction_id,
                            &transaction,
                            &mut pending_tab_ids,
                        );
                        screen.active_layout_transactions.remove(&transaction_id);
                    }
                }
            },
            #[cfg(test)]
            ScreenInstruction::QueryLayoutTransactionStateForTest {
                transaction_id,
                response_channel,
            } => {
                let pending_gate = screen
                    .active_layout_transactions
                    .get(&transaction_id)
                    .is_some_and(|transaction| {
                        transaction
                            .pending_gate_owners()
                            .any(|target| pending_tab_ids.contains(&target.tab_id))
                    });
                let _ = response_channel.send((
                    screen
                        .active_layout_transactions
                        .contains_key(&transaction_id),
                    screen
                        .indeterminate_layout_transactions
                        .contains_key(&transaction_id),
                    pending_gate,
                ));
            },
            ScreenInstruction::GoToTab(
                tab_index,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                let client_id_to_switch = if let Some(cid) = client_id {
                    if screen.active_tab_ids.contains_key(&cid) {
                        Some(cid)
                    } else {
                        screen.active_tab_ids.keys().next().copied()
                    }
                } else {
                    None
                };
                match client_id_to_switch {
                    // we must make sure pending_tab_ids is empty because otherwise we cannot be
                    // sure this instruction is applied at the right time (eg. we might have a
                    // pending tab that will become not-pending after this instruction and change
                    // the client focus, which should have happened before this instruction and not
                    // after)
                    Some(client_id) if pending_tab_ids.is_empty() => {
                        screen.go_to_tab(tab_index as usize, client_id)?;
                        screen.render(None)?;
                    },
                    _ => {
                        if let Some(client_id) = client_id {
                            pending_tab_switches.insert((tab_index as usize, client_id));
                        }
                    },
                }
            },
            ScreenInstruction::GoToTabName(
                tab_name,
                default_shell,
                create,
                client_id,
                mut completion_tx,
            ) => {
                let swap_layouts = (
                    screen.default_layout.swap_tiled_layouts.clone(),
                    screen.default_layout.swap_floating_layouts.clone(),
                );
                let client_id = if let Some(cid) = client_id {
                    if screen.active_tab_ids.contains_key(&cid) {
                        Some(cid)
                    } else {
                        screen.active_tab_ids.keys().next().copied()
                    }
                } else {
                    None
                };
                if let Some(client_id) = client_id {
                    let is_web_client = screen
                        .connected_clients
                        .borrow()
                        .get(&client_id)
                        .copied()
                        .unwrap_or(false);
                    match screen.go_to_tab_name(tab_name.clone(), client_id) {
                        Ok(tab_exists) => {
                            screen.render(None)?;
                            if tab_exists {
                                // Tab already exists - find its ID and set in completion
                                if let Some(existing_tab) =
                                    screen.tabs.values().find(|t| t.name == tab_name)
                                    && let Some(c) = completion_tx.as_mut()
                                {
                                    c.set_affected_tab_id(existing_tab.id)
                                }
                            }
                            if create && !tab_exists {
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.require_explicit_resolution();
                                }
                                let tab_index = screen.get_new_tab_id();
                                let should_change_focus_to_new_tab = true;
                                if let Err(error) = screen.new_tab(
                                    tab_index,
                                    swap_layouts,
                                    Some(tab_name),
                                    Some(client_id),
                                    TabPlacement::Append,
                                ) {
                                    let message = format!(
                                        "failed to create tab {tab_index} from GoToTabName: {error:#}"
                                    );
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                    log::error!("{message}");
                                    continue;
                                }
                                let transaction_id = screen.reserve_layout_transaction_id();
                                let target = LayoutTabOwner::capture(&screen, tab_index);
                                let transaction = ActiveLayoutTransaction {
                                    kind: ScreenLayoutTransactionKind::NewTab,
                                    targets: vec![target.clone()],
                                    created_pending_tabs: vec![target],
                                    render_fenced_tabs: vec![],
                                    tabs_to_close_after_commit: vec![],
                                    moved_original_panes: vec![],
                                    generation: None,
                                };
                                if let Err(error) =
                                    screen.register_layout_transaction(transaction_id, transaction)
                                {
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(format!("{error:#}"));
                                    }
                                    screen
                                        .discard_pending_tab_after_layout_rejection(tab_index)
                                        .non_fatal();
                                    pending_tab_ids.remove(&tab_index);
                                    log::error!("{error:#}");
                                    continue;
                                }
                                pending_tab_ids.insert(tab_index);
                                let instruction = PluginInstruction::NewTab(
                                    None,
                                    default_shell,
                                    None,
                                    vec![],
                                    tab_index,
                                    transaction_id,
                                    None,  // initial_panes
                                    false, // block_on_first_terminal
                                    should_change_focus_to_new_tab,
                                    (client_id, is_web_client),
                                    completion_tx,
                                    None,
                                );
                                if let Err(send_failure) =
                                    screen.bus.senders.send_to_plugin_recover(instruction)
                                {
                                    let (instruction, send_error) = send_failure.into_parts();
                                    let (mut recovered_completion, recovered_expected_kind) =
                                        match instruction {
                                            PluginInstruction::NewTab(
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                _,
                                                recovered_completion,
                                                _,
                                            ) => (recovered_completion, true),
                                            _ => (None, false),
                                        };
                                    let transaction =
                                        screen.active_layout_transactions.remove(&transaction_id);
                                    if let Some(transaction) = transaction.as_ref() {
                                        screen.discard_owned_pending_tabs(
                                            transaction,
                                            &mut pending_tab_ids,
                                        );
                                    }
                                    let message = if recovered_expected_kind {
                                        format!(
                                            "failed to hand layout transaction {transaction_id} to Plugin: {send_error:#}"
                                        )
                                    } else {
                                        format!(
                                            "Plugin handoff returned an unexpected instruction while rejecting GoToTabName layout transaction {transaction_id}: {send_error:#}"
                                        )
                                    };
                                    if let Some(completion) = recovered_completion.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                    log::error!("{message}");
                                }
                                continue; // completion is owned by the plugin instruction
                            }
                            if !tab_exists && let Some(completion) = completion_tx.as_mut() {
                                completion.set_exit_status(1);
                                completion.set_error_message(format!(
                                    "Tab named {:?} not found",
                                    tab_name
                                ));
                            }
                        },
                        Err(error) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.set_exit_status(1);
                                completion.set_error_message(format!(
                                    "Failed to select tab named {:?}: {}",
                                    tab_name, error
                                ));
                            }
                        },
                    }
                } else if let Some(completion) = completion_tx.as_mut() {
                    completion.set_exit_status(1);
                    completion
                        .set_error_message("No connected clients to select a tab for".to_owned());
                }
            },
            ScreenInstruction::UpdateTabName(
                c,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.update_active_tab_name(c, client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::UndoRenameTab(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.undo_active_rename_tab(client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::MoveTabLeft(client_id, completion_tx) => {
                if pending_tab_ids.is_empty() {
                    screen.move_active_tab_to_left(client_id)?;
                    screen.render(None)?;
                } else {
                    // Defer execution, forward completion_tx
                    pending_events_waiting_for_tab
                        .push(ScreenInstruction::MoveTabLeft(client_id, completion_tx));
                }
            },
            ScreenInstruction::MoveTabRight(client_id, completion_tx) => {
                if pending_tab_ids.is_empty() {
                    screen.move_active_tab_to_right(client_id)?;
                    screen.render(None)?;
                } else {
                    // Defer execution, forward completion_tx
                    pending_events_waiting_for_tab
                        .push(ScreenInstruction::MoveTabRight(client_id, completion_tx));
                }
            },
            ScreenInstruction::TerminalResize(new_size) => {
                screen.resize_to_screen(new_size)?;
                screen.log_and_report_session_state()?; // update tabs so that the ui indication will be send to the plugins
                screen.render(None)?;
            },
            ScreenInstruction::RecomputeTabSize(client_id, new_size) => {
                screen.set_client_size(client_id, new_size);
                let active_tab_id = screen.active_tab_ids.get(&client_id).copied();
                if let Some(tab_id) = active_tab_id {
                    screen.recompute_tab_size(tab_id)?;
                    screen.log_and_report_session_state()?;
                    screen.render(None)?;
                }
            },
            ScreenInstruction::TerminalPixelDimensions(pixel_dimensions) => {
                screen.update_pixel_dimensions(pixel_dimensions);
            },
            ScreenInstruction::TerminalBackgroundColor(background_color_instruction) => {
                screen.update_terminal_background_color(background_color_instruction);
            },
            ScreenInstruction::TerminalForegroundColor(background_color_instruction) => {
                screen.update_terminal_foreground_color(background_color_instruction);
            },
            ScreenInstruction::TerminalColorRegisters(color_registers) => {
                screen.update_terminal_color_registers(color_registers);
            },
            ScreenInstruction::ForwardHostQuery { pane_id, query } => {
                screen.forward_host_query(pane_id, query);
            },
            ScreenInstruction::ForwardedReplyFromHost { token, reply_bytes } => {
                screen.handle_forwarded_reply_from_host(token, reply_bytes)?;
                // The handler's replay of `pending_pty_input` mutates the
                // grid. Without a render scheduling here the clients
                // keep displaying the pre-reply frame
                screen.render(None)?;
            },
            ScreenInstruction::ResumePaneAfterForward {
                pane_id,
                reply_bytes,
            } => {
                screen.resume_pane_after_forward(pane_id, reply_bytes)?;
                // Same rationale as ForwardedReplyFromHost above — the
                // resume's replay paints the grid, so we must schedule
                // a render before yielding back to the screen loop.
                screen.render(None)?;
            },
            ScreenInstruction::HostTerminalThemeChanged(mode) => {
                screen.update_host_terminal_theme_mode(mode)?;
            },
            ScreenInstruction::SetDarkTheme(mut completion_tx) => {
                screen.apply_manual_host_terminal_theme_mode(
                    HostTerminalThemeMode::Dark,
                    &mut completion_tx,
                )?;
            },
            ScreenInstruction::SetLightTheme(mut completion_tx) => {
                screen.apply_manual_host_terminal_theme_mode(
                    HostTerminalThemeMode::Light,
                    &mut completion_tx,
                )?;
            },
            ScreenInstruction::ToggleTheme(mut completion_tx) => {
                let next = match screen.host_terminal_theme_mode {
                    Some(HostTerminalThemeMode::Dark) => HostTerminalThemeMode::Light,
                    Some(HostTerminalThemeMode::Light) => HostTerminalThemeMode::Dark,
                    // No prior mode: treat as Dark and toggle to Light.
                    None => HostTerminalThemeMode::Light,
                };
                screen.apply_manual_host_terminal_theme_mode(next, &mut completion_tx)?;
            },
            ScreenInstruction::ChangeMode(
                input_mode,
                base_mode,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.change_mode(input_mode, base_mode, client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::ChangeModeForAllClients(
                input_mode,
                base_mode,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.change_mode_for_all_clients(input_mode, base_mode)?;
                screen.render(None)?;
            },
            ScreenInstruction::ToggleActiveSyncTab(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, _client_id: ClientId| tab.toggle_sync_panes_is_active()
                );
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::MouseEvent(
                event,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.handle_mouse_event(event, client_id);
            },
            ScreenInstruction::Copy(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab!(screen, client_id, |tab: &mut Tab| tab
                    .copy_selection(client_id), ?);
                screen.render(None)?;
            },
            ScreenInstruction::Exit => {
                break;
            },
            ScreenInstruction::ToggleTab(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.toggle_tab(client_id)?;
                screen.render(None)?;
            },
            ScreenInstruction::AddClient(
                client_id,
                is_web_client,
                client_size,
                tab_position_to_focus,
                pane_id_to_focus,
            ) => {
                // Record the client's viewport BEFORE add_client so that
                // add_client's internal recompute sees this client's size and
                // sizes the destination tab against all of its viewers.
                screen.set_client_size(client_id, client_size);
                screen.add_client(client_id, is_web_client)?;
                let pane_id = pane_id_to_focus.map(|(pane_id, is_plugin)| {
                    if is_plugin {
                        PaneId::Plugin(pane_id)
                    } else {
                        PaneId::Terminal(pane_id)
                    }
                });
                if let Some(pane_id) = pane_id {
                    screen.focus_pane_with_id(pane_id, true, false, client_id)?;
                } else if let Some(tab_position_to_focus) = tab_position_to_focus {
                    screen.go_to_tab(tab_position_to_focus, client_id)?;
                }
                for event in pending_events_waiting_for_client.drain(..) {
                    screen.bus.senders.send_to_screen(event).non_fatal();
                }
                screen.log_and_report_session_state()?;

                if is_web_client {
                    // we do this because
                    // we need to query the client for its size, and we must do it only after we've
                    // added it to our state.
                    //
                    // we have to do this specifically for web clients because the browser (as opposed
                    // to a traditional terminal) can only figure out its dimensions after we sent it relevant
                    // state (eg. font, which is controlled by our config and it needs to determine cell size)
                    if let Some(os_input) = &mut screen.bus.os_input {
                        let _ = os_input
                            .send_to_client(client_id, ServerToClientMsg::QueryTerminalSize);
                    }
                }

                screen.render(None)?;
            },
            ScreenInstruction::RemoveClient(client_id) => {
                screen.remove_client(client_id)?;
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::UpdateSearch(
                c,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.update_search_term(c, client_id), ?
                );
                screen.render(None)?;
            },
            ScreenInstruction::SearchDown(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.search_down(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::SearchUp(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.search_up(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::SearchToggleCaseSensitivity(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .toggle_search_case_sensitivity(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::SearchToggleWrap(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.toggle_search_wrap(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::SearchToggleWholeWord(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab.toggle_search_whole_words(client_id)
                );
                screen.render(None)?;
            },
            ScreenInstruction::AddRedPaneFrameColorOverride(pane_ids, error_text) => {
                let all_tabs = screen.get_tabs_mut();
                for pane_id in pane_ids {
                    for tab in all_tabs.values_mut() {
                        if tab.has_pane_with_pid(&pane_id) {
                            tab.add_red_pane_frame_color_override(pane_id, error_text.clone());
                            break;
                        }
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::AddHighlightPaneFrameColorOverride(pane_ids, error_text) => {
                let all_tabs = screen.get_tabs_mut();
                for pane_id in pane_ids {
                    for tab in all_tabs.values_mut() {
                        if tab.has_pane_with_pid(&pane_id) {
                            tab.add_highlight_pane_frame_color_override(
                                pane_id,
                                error_text.clone(),
                                None,
                            );
                            break;
                        }
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ClearPaneFrameColorOverride(pane_ids) => {
                let all_tabs = screen.get_tabs_mut();
                for pane_id in pane_ids {
                    for tab in all_tabs.values_mut() {
                        if tab.has_pane_with_pid(&pane_id) {
                            tab.clear_pane_frame_color_override(pane_id, None);
                            break;
                        }
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::SetTabBellFlash(tab_id, is_flashing) => {
                if let Some(tab) = screen.tabs.values_mut().find(|t| t.id == tab_id) {
                    tab.tab_bell_flash = is_flashing;
                    tab.clear_tab_bell_ring();
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::PreviousSwapLayout(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, _client_id: ClientId| tab.previous_swap_layout(),
                    ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::NextSwapLayout(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, _client_id: ClientId| tab.next_swap_layout(),
                    ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::OverrideLayout(
                cwd,
                default_shell,
                mut tab_layouts,
                retain_existing_terminal_panes,
                retain_existing_plugin_panes,
                apply_only_to_focused_tab,
                client_id,
                mut completion_tx,
            ) => {
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                }
                // Layouts identify tabs by display position. Convert those
                // positions to stable IDs before comparing, mutating or
                // creating tabs so a retired ID is never resurrected.
                if !apply_only_to_focused_tab
                    && let Err(error) = screen.assign_stable_tab_ids_to_layout(&mut tab_layouts)
                {
                    log::error!("Failed to validate override layout: {}", error);
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.set_exit_status(1);
                        completion.set_error_message(error.to_string());
                    }
                    drop(completion_tx);
                    continue;
                }

                // 1. Determine which tabs to close (exist but not in layout)
                let existing_tab_indices: HashSet<usize> = screen.tabs.keys().copied().collect();
                let layout_tab_indices: HashSet<usize> =
                    tab_layouts.iter().map(|tl| tl.tab_index).collect();
                let tabs_to_close: Vec<usize> = existing_tab_indices
                    .difference(&layout_tab_indices)
                    .copied()
                    .collect();

                // 2. Process each tab layout
                let mut processed_tab_layouts = Vec::new();

                if apply_only_to_focused_tab {
                    match screen.get_active_tab_mut(client_id) {
                        Ok(active_tab) => {
                            if tab_layouts.is_empty() {
                                let message = "No tab layouts found, cannot override.".to_owned();
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.mark_failure(message.clone());
                                }
                                log::error!("{message}");
                                continue;
                            }
                            let mut tab_layout_info = tab_layouts.remove(0);
                            tab_layout_info.tab_index = active_tab.id;
                            // Find already-running panes for this tab
                            let (tiled_to_ignore, floating_indices) = find_already_running_panes(
                                &tab_layout_info.tiled_layout,
                                &tab_layout_info.floating_layouts,
                                active_tab,
                            );

                            // Mark run instructions to ignore (prevents re-spawning)
                            for run_instruction in tiled_to_ignore {
                                tab_layout_info
                                    .tiled_layout
                                    .ignore_run_instruction(run_instruction);
                            }

                            for idx in floating_indices {
                                if let Some(f) = tab_layout_info.floating_layouts.get_mut(idx) {
                                    f.already_running = true;
                                }
                            }

                            processed_tab_layouts.push(tab_layout_info);
                        },
                        Err(e) => {
                            let message = format!("Failed to override layout of active tab: {e:#}");
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(message.clone());
                            }
                            log::error!("{message}");
                            continue;
                        },
                    }
                } else {
                    for mut tab_layout_info in tab_layouts {
                        // Get the tab by index
                        let tab = match screen.tabs.get_mut(&tab_layout_info.tab_index) {
                            Some(t) => t,
                            None => {
                                // no corresponding tab exists, we'll create it
                                processed_tab_layouts.push(tab_layout_info);
                                continue;
                            },
                        };

                        // Find already-running panes for this tab
                        let (tiled_to_ignore, floating_indices) = find_already_running_panes(
                            &tab_layout_info.tiled_layout,
                            &tab_layout_info.floating_layouts,
                            tab,
                        );

                        // Mark run instructions to ignore (prevents re-spawning)
                        for run_instruction in tiled_to_ignore {
                            tab_layout_info
                                .tiled_layout
                                .ignore_run_instruction(run_instruction);
                        }

                        for idx in floating_indices {
                            if let Some(f) = tab_layout_info.floating_layouts.get_mut(idx) {
                                f.already_running = true;
                            }
                        }

                        processed_tab_layouts.push(tab_layout_info);
                    }
                }

                // 3. Register exact ownership before Plugin can allocate a
                // single resource. Names and omitted-tab closures remain
                // deferred until the PTY commit ACK.
                let transaction_id = screen.reserve_layout_transaction_id();
                let targets = processed_tab_layouts
                    .iter()
                    .map(|layout| LayoutTabOwner::capture(&screen, layout.tab_index))
                    .collect();
                let tabs_to_close_after_commit = if apply_only_to_focused_tab {
                    vec![]
                } else {
                    tabs_to_close
                        .into_iter()
                        .map(|tab_id| LayoutTabOwner::capture(&screen, tab_id))
                        .collect()
                };
                let transaction = ActiveLayoutTransaction {
                    kind: ScreenLayoutTransactionKind::Override,
                    targets,
                    created_pending_tabs: vec![],
                    render_fenced_tabs: vec![],
                    tabs_to_close_after_commit,
                    moved_original_panes: vec![],
                    generation: None,
                };
                let pending_override_tab_ids = transaction
                    .targets
                    .iter()
                    .map(|target| target.tab_id)
                    .collect::<Vec<_>>();
                if let Err(error) = screen.register_layout_transaction(transaction_id, transaction)
                {
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(format!("{error:#}"));
                    }
                    log::error!("{error:#}");
                    continue;
                }
                pending_tab_ids.extend(pending_override_tab_ids);
                let instruction = PluginInstruction::OverrideLayout(
                    cwd,
                    default_shell,
                    processed_tab_layouts,
                    transaction_id,
                    retain_existing_terminal_panes,
                    retain_existing_plugin_panes,
                    client_id,
                    completion_tx,
                    None,
                );
                if let Err(send_failure) = screen.bus.senders.send_to_plugin_recover(instruction) {
                    let (instruction, send_error) = send_failure.into_parts();
                    let (mut recovered_completion, recovered_expected_kind) = match instruction {
                        PluginInstruction::OverrideLayout(
                            _,
                            _,
                            _,
                            _,
                            _,
                            _,
                            _,
                            recovered_completion,
                            _,
                        ) => (recovered_completion, true),
                        _ => (None, false),
                    };
                    if let Some(owner) = screen
                        .active_layout_transactions
                        .get(&transaction_id)
                        .cloned()
                    {
                        screen.retire_layout_transaction_from_pending_gate(
                            transaction_id,
                            &owner,
                            &mut pending_tab_ids,
                        );
                    }
                    screen.active_layout_transactions.remove(&transaction_id);
                    release_pending_layout_gate_if_ready(
                        &mut screen,
                        &pending_tab_ids,
                        &mut pending_tab_switches,
                        &mut pending_events_waiting_for_client,
                        &mut pending_events_waiting_for_tab,
                    );
                    let message = if recovered_expected_kind {
                        format!(
                            "failed to hand layout transaction {transaction_id} to Plugin: {send_error:#}"
                        )
                    } else {
                        format!(
                            "Plugin handoff returned an unexpected instruction while rejecting Override transaction {transaction_id}: {send_error:#}"
                        )
                    };
                    if let Some(completion) = recovered_completion.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    log::error!("{message}");
                }
            },
            ScreenInstruction::OverrideLayoutComplete(
                tab_results,
                retain_existing_terminal_panes,
                retain_existing_plugin_panes,
                client_id,
                mut completion_tx,
                layout_generation,
                transaction_id,
            ) => {
                #[cfg(test)]
                let transaction_id = screen.resolve_legacy_test_layout_transaction_id(
                    transaction_id,
                    &[
                        ScreenLayoutTransactionKind::Override,
                        ScreenLayoutTransactionKind::DurableRecovery,
                    ],
                    &tab_results
                        .iter()
                        .map(|result| result.tab_index)
                        .collect::<Vec<_>>(),
                );
                if let Some(completion) = completion_tx.as_mut() {
                    completion.require_explicit_resolution();
                }
                if let Some(indeterminate) = screen
                    .indeterminate_layout_transactions
                    .get(&transaction_id)
                {
                    let message = indeterminate.replay_rejection(transaction_id);
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    log::error!("{message}");
                    continue;
                }
                let raw_installed_resource_ids = tab_results
                    .iter()
                    .flat_map(|result| {
                        layout_resource_ids(
                            &result.new_terminal_pids,
                            &result.new_floating_pane_pids,
                            &result.plugin_ids,
                        )
                    })
                    .collect::<Vec<_>>();
                let mut installed_resource_ids = raw_installed_resource_ids.clone();
                installed_resource_ids.sort_unstable();
                installed_resource_ids.dedup();
                let mut expected_plugin_ids = tab_results
                    .iter()
                    .flat_map(|result| result.plugin_ids.values().flatten().copied())
                    .collect::<Vec<_>>();
                expected_plugin_ids.sort_unstable();
                expected_plugin_ids.dedup();
                let mut created_tab_ids = vec![];
                // See ApplyLayout: same-viewer supersession retires only this
                // writer, not the stable empty tab awaited by the next writer.
                let mut preserve_pending_tab_on_rejection = false;
                let mut close_fenced_tab_on_rejection = false;
                let mut prepared_override_layouts = vec![];
                let mut validated_owner = None;
                let registered_owner = screen
                    .active_layout_transactions
                    .get(&transaction_id)
                    .cloned();
                let transaction_target_ids = tab_results
                    .iter()
                    .map(|result| result.tab_index)
                    .collect::<Vec<_>>();
                if let Some(replay) = screen.replay_resolved_layout_transaction(
                    transaction_id,
                    &[
                        ScreenLayoutTransactionKind::Override,
                        ScreenLayoutTransactionKind::DurableRecovery,
                    ],
                    &transaction_target_ids,
                    layout_generation.as_deref(),
                    &raw_installed_resource_ids,
                ) {
                    match replay {
                        Ok(ScreenLayoutDecision::Committed) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_success();
                            }
                        },
                        Ok(ScreenLayoutDecision::CommittedWithCleanupDebt(message))
                        | Ok(ScreenLayoutDecision::CommittedWithPostCommitError(message))
                        | Ok(ScreenLayoutDecision::Rejected(message))
                        | Err(message) => {
                            if let Some(completion) = completion_tx.as_mut() {
                                completion.mark_failure(message);
                            }
                        },
                    }
                    continue;
                }
                if transaction_id != 0 && registered_owner.is_none() {
                    let message = format!(
                        "unknown layout transaction {transaction_id}; refusing to manufacture a Plugin/PTY resolution for an unowned Override completion"
                    );
                    if let Some(completion) = completion_tx.as_mut() {
                        completion.mark_failure(message.clone());
                    }
                    log::error!("{message}");
                    continue;
                }
                let transaction_result: Result<()> = (|| {
                    let mut unique_target_ids = transaction_target_ids.clone();
                    unique_target_ids.sort_unstable();
                    unique_target_ids.dedup();
                    if unique_target_ids.len() != transaction_target_ids.len() {
                        bail!(
                            "layout transaction {transaction_id} returned duplicate Override tab results: {transaction_target_ids:?}"
                        );
                    }
                    if installed_resource_ids.len() != raw_installed_resource_ids.len() {
                        bail!(
                            "layout transaction {transaction_id} returned duplicate Override resource ids: {raw_installed_resource_ids:?}"
                        );
                    }
                    if transaction_id != 0 {
                        validated_owner = Some(screen.validate_layout_transaction(
                            transaction_id,
                            &[
                                ScreenLayoutTransactionKind::Override,
                                ScreenLayoutTransactionKind::DurableRecovery,
                            ],
                            &transaction_target_ids,
                            layout_generation.as_deref(),
                        )?);
                    }
                    let fenced_result_is_exact =
                        layout_generation.as_ref().is_none_or(|generation| {
                            durable_tab_layout_generation_is_current(
                                &screen,
                                &durable_tab_layout_generations,
                                generation,
                            ) && matches!(
                                tab_results.as_slice(),
                                [result]
                                    if result.tab_index == generation.tab_id
                                        && result
                                            .tiled_layout
                                            .tab_instance_id
                                            .as_deref()
                                            .is_some_and(|token| token.eq_ignore_ascii_case(
                                                &generation.tab_instance_id
                                            ))
                            )
                        });
                    if !fenced_result_is_exact {
                        bail!(
                            "{}",
                            layout_generation.as_ref().map_or_else(
                                || "discarded malformed fenced layout result".to_owned(),
                                |generation| {
                                    format!(
                                        "discarded stale durable tab recovery generation {} for tab {} '{}'",
                                        generation.generation,
                                        generation.tab_id,
                                        generation.tab_name
                                    )
                                },
                            )
                        );
                    }
                    if let Some(layout_generation) = layout_generation.as_ref()
                        && let Err(rejection) =
                            verify_global_viewer_creation_fence(&screen, layout_generation)
                    {
                        if rejection.should_close_exact_tab() {
                            close_fenced_tab_on_rejection = true;
                        } else {
                            preserve_pending_tab_on_rejection = true;
                        }
                        return Err(anyhow!(rejection.to_string()));
                    }

                    // Process each tab result. A failure aborts the whole writer transaction.
                    for tab_result in tab_results {
                        let tab_index = tab_result.tab_index;
                        let restored_tab_instance_id = tab_result
                            .tiled_layout
                            .tab_instance_id
                            .as_deref()
                            .filter(|instance_id| {
                                instance_id.len() == 32
                                    && instance_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                            })
                            .map(str::to_ascii_lowercase);
                        if let Some(tab) = screen.tabs.get_mut(&tab_index) {
                            let new_tab_name = tab_result.tab_name.clone();
                            let mut transaction = tab
                                .begin_override_layout(
                                    tab_result.tiled_layout,
                                    tab_result.floating_layouts,
                                    tab_result.swap_tiled_layouts,
                                    tab_result.swap_floating_layouts,
                                    tab_result.new_terminal_pids,
                                    tab_result.new_floating_pane_pids,
                                    tab_result.plugin_ids,
                                    retain_existing_terminal_panes,
                                    retain_existing_plugin_panes,
                                    client_id,
                                    None,
                                )
                                .with_context(|| {
                                    format!("failed to override layout for tab {tab_index}")
                                })?;
                            transaction.defer_tab_name(new_tab_name);
                            prepared_override_layouts.push((tab_index, transaction));
                        } else {
                            // Tab doesn't exist - create it.
                            let new_tab_name = tab_result.tab_name.clone();
                            let swap_layouts = (
                                tab_result.swap_tiled_layouts.clone().unwrap_or_default(),
                                tab_result.swap_floating_layouts.clone().unwrap_or_default(),
                            );
                            screen
                                .new_tab(
                                    tab_index,
                                    swap_layouts,
                                    None,
                                    None,
                                    TabPlacement::Append,
                                )
                                .with_context(|| {
                                    format!(
                                        "failed to create tab {tab_index} during override completion"
                                    )
                                })?;
                            created_tab_ids.push(tab_index);
                            if let Some(restored_tab_instance_id) = restored_tab_instance_id
                                && let Some(tab) = screen.tabs.get_mut(&tab_index)
                            {
                                tab.instance_id = restored_tab_instance_id;
                            }
                            let created_owner = LayoutTabOwner::capture(&screen, tab_index);
                            if let Some(active_transaction) =
                                screen.active_layout_transactions.get_mut(&transaction_id)
                            {
                                if let Some(target) = active_transaction
                                    .targets
                                    .iter_mut()
                                    .find(|target| target.tab_id == tab_index)
                                {
                                    *target = created_owner.clone();
                                }
                                active_transaction.created_pending_tabs.push(created_owner);
                            }

                            let mut transaction = screen
                                .tabs
                                .get_mut(&tab_index)
                                .with_context(|| {
                                    format!(
                                        "new tab {tab_index} disappeared during override completion"
                                    )
                                })?
                                .begin_override_layout(
                                    tab_result.tiled_layout,
                                    tab_result.floating_layouts,
                                    tab_result.swap_tiled_layouts,
                                    tab_result.swap_floating_layouts,
                                    tab_result.new_terminal_pids,
                                    tab_result.new_floating_pane_pids,
                                    tab_result.plugin_ids,
                                    retain_existing_terminal_panes,
                                    retain_existing_plugin_panes,
                                    client_id,
                                    None,
                                )
                                .with_context(|| {
                                    format!("failed to override layout for new tab {tab_index}")
                                })?;
                            transaction.defer_tab_name(new_tab_name);
                            prepared_override_layouts.push((tab_index, transaction));
                        }
                    }

                    #[cfg(test)]
                    if let Some(layout_generation) = layout_generation.as_ref() {
                        pause_after_viewer_creation_install_for_test(layout_generation);
                    }
                    if let Some(layout_generation) = layout_generation.as_ref()
                        && let Err(rejection) =
                            verify_global_viewer_creation_fence(&screen, layout_generation)
                    {
                        if rejection.should_close_exact_tab() {
                            close_fenced_tab_on_rejection = true;
                        } else {
                            preserve_pending_tab_on_rejection = true;
                        }
                        return Err(anyhow!(rejection.to_string()));
                    }

                    if transaction_id != 0 {
                        validated_owner = Some(screen.validate_layout_transaction(
                            transaction_id,
                            &[
                                ScreenLayoutTransactionKind::Override,
                                ScreenLayoutTransactionKind::DurableRecovery,
                            ],
                            &transaction_target_ids,
                            layout_generation.as_deref(),
                        )?);
                    }
                    for (tab_id, transaction) in &prepared_override_layouts {
                        let tab = screen.tabs.get(tab_id).with_context(|| {
                            format!(
                                "prepared Override target tab {tab_id} disappeared before commit preflight"
                            )
                        })?;
                        transaction.preflight_commit(tab)?;
                    }
                    Ok(())
                })();

                let reconciliation_intent = match transaction_result {
                    Ok(()) => LayoutReconciliationIntent::Activate,
                    Err(error) => LayoutReconciliationIntent::Reject(format!("{error:#}")),
                };
                let reconciliation_plan = LayoutReconciliationPlan {
                    intent: reconciliation_intent.clone(),
                    expected_plugin_ids: expected_plugin_ids.clone(),
                    resource_ids: installed_resource_ids.clone(),
                    preserve_pending_tab_on_rejection,
                    close_fenced_tab_on_rejection,
                    layout_generation: layout_generation.as_deref().cloned(),
                };
                let coordination = match &reconciliation_intent {
                    LayoutReconciliationIntent::Activate => coordinate_layout_activation(
                        &screen.bus.senders,
                        transaction_id,
                        &expected_plugin_ids,
                    ),
                    LayoutReconciliationIntent::Reject(rejection) => coordinate_layout_rejection(
                        &screen.bus.senders,
                        transaction_id,
                        &expected_plugin_ids,
                        rejection.clone(),
                    ),
                    LayoutReconciliationIntent::RejectByOwner(_) => {
                        unreachable!(
                            "Override completion cannot originate by-owner rejection retry"
                        )
                    },
                    LayoutReconciliationIntent::PreparationFailure { .. } => {
                        unreachable!(
                            "Override completion cannot originate preparation-failure retry"
                        )
                    },
                };
                let mut retire_active_transaction = true;
                let mut post_commit_error = None;
                match coordination {
                    LayoutCoordination::Commit => {
                        match screen.commit_override_layout_state(std::mem::take(
                            &mut prepared_override_layouts,
                        )) {
                            CommittedOverrideLayout::Complete(mut committed_override_effects) => {
                                let mut cleanup = PendingTabLayoutCleanup::default();
                                for (_, effects) in &mut committed_override_effects {
                                    cleanup.append(effects.take_pending_cleanup());
                                }
                                screen.retain_layout_cleanup(transaction_id, cleanup);
                                for (tab_id, effects) in committed_override_effects {
                                    if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                                        if let Some((_, mut completion)) = effects.emit(tab) {
                                            let message = format!(
                                                "Override transaction {transaction_id} unexpectedly retained a blocking terminal completion"
                                            );
                                            completion.mark_failure(message.clone());
                                            post_commit_error = Some(message);
                                        }
                                    } else {
                                        log::error!(
                                            "committed Override target tab {tab_id} disappeared before infallible local effects"
                                        );
                                    }
                                }
                                if let Some(owner) =
                                    validated_owner.as_ref().or(registered_owner.as_ref())
                                    && let Err(error) = screen
                                        .close_owned_tabs_after_layout_commit(transaction_id, owner)
                                {
                                    post_commit_error = Some(format!("{error:#}"));
                                }
                                screen.flush_layout_cleanup(transaction_id);
                                let cleanup_decision =
                                    screen.pending_layout_cleanup_message(transaction_id);
                                if let Some(message) = post_commit_error.as_ref() {
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                } else if let Some(message) = cleanup_decision.as_ref() {
                                    if let Some(completion) = completion_tx.as_mut() {
                                        completion.mark_failure(message.clone());
                                    }
                                } else if let Some(completion) = completion_tx.as_mut() {
                                    completion.mark_success();
                                }
                                if let Some(owner) =
                                    validated_owner.as_ref().or(registered_owner.as_ref())
                                {
                                    screen.record_resolved_layout_transaction(
                                        transaction_id,
                                        owner,
                                        installed_resource_ids.clone(),
                                        post_commit_error.clone().map_or_else(
                                            || {
                                                cleanup_decision.clone().map_or(
                                                    ScreenLayoutDecision::Committed,
                                                    ScreenLayoutDecision::CommittedWithCleanupDebt,
                                                )
                                            },
                                            ScreenLayoutDecision::CommittedWithPostCommitError,
                                        ),
                                    );
                                }
                                if let Some(owner) =
                                    validated_owner.as_ref().or(registered_owner.as_ref())
                                {
                                    screen.retire_layout_transaction_from_pending_gate(
                                        transaction_id,
                                        owner,
                                        &mut pending_tab_ids,
                                    );
                                } else if let Some(layout_generation) = layout_generation.as_ref() {
                                    pending_tab_ids.remove(&layout_generation.tab_id);
                                }
                                if pending_tab_ids.is_empty() {
                                    for (tab_index, pending_client_id) in
                                        pending_tab_switches.drain()
                                    {
                                        screen
                                            .go_to_tab(tab_index + 1, pending_client_id)
                                            .non_fatal();
                                    }
                                }
                                for event in pending_events_waiting_for_client.drain(..) {
                                    screen.bus.senders.send_to_screen(event).non_fatal();
                                }
                                for event in pending_events_waiting_for_tab.drain(..) {
                                    screen.bus.senders.send_to_screen(event).non_fatal();
                                }
                                screen.log_and_report_session_state().non_fatal();
                                screen.render(None).non_fatal();
                            },
                            CommittedOverrideLayout::Indeterminate {
                                missing_tab_id,
                                mut committed_effects,
                                remaining_prepared,
                            } => {
                                let message = format!(
                                    "layout transaction {transaction_id} activated in Plugin and PTY but preflighted Override target tab {missing_tab_id} disappeared during Screen commit; preserved every remaining transaction as indeterminate"
                                );
                                let mut cleanup = PendingTabLayoutCleanup::default();
                                for (_, effects) in &mut committed_effects {
                                    cleanup.append(effects.take_pending_cleanup());
                                }
                                screen.retain_layout_cleanup(transaction_id, cleanup);
                                for (tab_id, effects) in committed_effects {
                                    if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                                        if let Some((_, mut completion)) = effects.emit(tab) {
                                            let message = format!(
                                                "partially committed Override transaction {transaction_id} unexpectedly retained a blocking terminal completion"
                                            );
                                            completion.mark_failure(message.clone());
                                            log::error!("{message}");
                                        }
                                    } else {
                                        log::error!(
                                            "partially committed Override target tab {tab_id} disappeared before local effects"
                                        );
                                    }
                                }
                                screen.flush_layout_cleanup(transaction_id);
                                let mut indeterminate = IndeterminatePreparedLayout::Override {
                                    prepared_layouts: remaining_prepared,
                                    created_tab_ids,
                                    plan: reconciliation_plan.clone(),
                                };
                                indeterminate.mark_blocking_completion_failed(&message);
                                screen
                                    .indeterminate_layout_transactions
                                    .insert(transaction_id, indeterminate);
                                if let Some(completion) = completion_tx.as_mut() {
                                    completion.mark_failure(message.clone());
                                }
                                retire_active_transaction = false;
                                screen.log_and_report_session_state().non_fatal();
                                log::error!("{message}");
                            },
                        }
                    },
                    LayoutCoordination::Rollback(message) => {
                        for (tab_id, transaction) in prepared_override_layouts.drain(..).rev() {
                            if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                                transaction.rollback(tab, &message);
                            }
                        }
                        let excluded_pty_resource_ids =
                            installed_resource_ids.iter().copied().collect();
                        for created_tab_id in created_tab_ids.iter().rev() {
                            if screen.tabs.contains_key(created_tab_id) {
                                screen
                                    .close_tab_by_id_excluding_pty_resources(
                                        *created_tab_id,
                                        &excluded_pty_resource_ids,
                                    )
                                    .non_fatal();
                            }
                        }
                        for resource_id in &installed_resource_ids {
                            if let PaneId::Plugin(plugin_id) = resource_id {
                                plugin_loading_message_cache.remove(plugin_id);
                            }
                        }
                        remove_layout_resources_from_screen(&mut screen, &installed_resource_ids);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_failure(message.clone());
                        }
                        if let Some(owner) = validated_owner.as_ref().or(registered_owner.as_ref())
                        {
                            screen.record_resolved_layout_transaction(
                                transaction_id,
                                owner,
                                installed_resource_ids.clone(),
                                ScreenLayoutDecision::Rejected(message.clone()),
                            );
                        }
                        if close_fenced_tab_on_rejection
                            && let Some(layout_generation) = layout_generation.as_ref()
                        {
                            close_globally_stale_fenced_tab(
                                &mut screen,
                                layout_generation,
                                &installed_resource_ids,
                            )
                            .non_fatal();
                            pending_tab_ids.remove(&layout_generation.tab_id);
                        } else if !preserve_pending_tab_on_rejection
                            && let Some(layout_generation) = layout_generation.as_ref()
                            && durable_tab_layout_generation_is_current(
                                &screen,
                                &durable_tab_layout_generations,
                                layout_generation,
                            )
                        {
                            pending_tab_ids.remove(&layout_generation.tab_id);
                        }
                        if let Some(owner) = validated_owner.as_ref().or(registered_owner.as_ref())
                        {
                            screen.retire_layout_transaction_from_pending_gate(
                                transaction_id,
                                owner,
                                &mut pending_tab_ids,
                            );
                        }
                        release_pending_layout_gate_if_ready(
                            &mut screen,
                            &pending_tab_ids,
                            &mut pending_tab_switches,
                            &mut pending_events_waiting_for_client,
                            &mut pending_events_waiting_for_tab,
                        );
                        log::warn!(
                            "layout transaction {transaction_id} finished rejected: {message}"
                        );
                    },
                    LayoutCoordination::Unknown(message) => {
                        let mut indeterminate = IndeterminatePreparedLayout::Override {
                            prepared_layouts: prepared_override_layouts,
                            created_tab_ids,
                            plan: reconciliation_plan,
                        };
                        indeterminate.mark_blocking_completion_failed(&message);
                        screen
                            .indeterminate_layout_transactions
                            .insert(transaction_id, indeterminate);
                        if let Some(completion) = completion_tx.as_mut() {
                            completion.mark_failure(message.clone());
                        }
                        if registered_owner.is_none()
                            && let Some(layout_generation) = layout_generation.as_ref()
                        {
                            pending_tab_ids.remove(&layout_generation.tab_id);
                            release_pending_layout_gate_if_ready(
                                &mut screen,
                                &pending_tab_ids,
                                &mut pending_tab_switches,
                                &mut pending_events_waiting_for_client,
                                &mut pending_events_waiting_for_tab,
                            );
                        }
                        retire_active_transaction = false;
                        screen.log_and_report_session_state().non_fatal();
                        log::error!("{message}");
                    },
                }
                if retire_active_transaction && registered_owner.is_some() {
                    screen.active_layout_transactions.remove(&transaction_id);
                }
            },
            ScreenInstruction::QueryTabNames(client_id, completion_tx) => {
                let tab_names = screen
                    .get_tabs_mut()
                    .values()
                    .map(|tab| tab.name.clone())
                    .collect::<Vec<String>>();
                screen.bus.senders.send_to_server(ServerInstruction::Log(
                    tab_names,
                    client_id,
                    completion_tx,
                ))?;
            },
            ScreenInstruction::NewTiledPluginPane(
                run_plugin,
                pane_title,
                skip_cache,
                cwd,
                client_id,
                completion_tx,
                explicit_tab_id,
            ) => {
                let tab_index = explicit_tab_id
                    .unwrap_or_else(|| *screen.active_tab_ids.values().next().unwrap_or(&1));
                let size = Size::default();
                let should_float = Some(false);
                let should_be_opened_in_place = false;
                screen
                    .bus
                    .senders
                    .send_to_pty(PtyInstruction::FillPluginCwd(
                        should_float,
                        should_be_opened_in_place,
                        false, // close_replaced_pane
                        pane_title,
                        run_plugin,
                        tab_index,
                        None,
                        client_id,
                        size,
                        skip_cache,
                        cwd,
                        None,
                        None,
                        completion_tx,
                    ))?;
            },
            ScreenInstruction::NewFloatingPluginPane(
                run_plugin,
                pane_title,
                skip_cache,
                cwd,
                floating_pane_coordinates,
                client_id,
                completion_tx,
                explicit_tab_id,
            ) => {
                let resolved_tab_index =
                    explicit_tab_id.or_else(|| screen.active_tab_ids.values().next().copied());
                match resolved_tab_index {
                    Some(tab_index) => {
                        let size = Size::default();
                        let should_float = Some(true);
                        let should_be_opened_in_place = false;
                        screen
                            .bus
                            .senders
                            .send_to_pty(PtyInstruction::FillPluginCwd(
                                should_float,
                                should_be_opened_in_place,
                                false, // close_replaced_pane
                                pane_title,
                                run_plugin,
                                tab_index,
                                None,
                                client_id,
                                size,
                                skip_cache,
                                cwd,
                                None,
                                floating_pane_coordinates,
                                completion_tx,
                            ))?;
                    },
                    None => {
                        log::error!(
                            "Could not find an active tab - is there at least 1 connected user?"
                        );
                    },
                }
            },
            ScreenInstruction::NewInPlacePluginPane(
                run_plugin,
                pane_title,
                pane_id_to_replace,
                skip_cache,
                close_replaced_pane,
                client_id,
                completion_tx,
                explicit_tab_id,
            ) => {
                let resolved_tab_index =
                    explicit_tab_id.or_else(|| screen.active_tab_ids.values().next().copied());
                match resolved_tab_index {
                    Some(tab_index) => {
                        let size = Size::default();
                        let should_float = None;
                        let should_be_in_place = true;
                        screen
                            .bus
                            .senders
                            .send_to_pty(PtyInstruction::FillPluginCwd(
                                should_float,
                                should_be_in_place,
                                close_replaced_pane,
                                pane_title,
                                run_plugin,
                                tab_index,
                                Some(pane_id_to_replace),
                                client_id,
                                size,
                                skip_cache,
                                None,
                                None,
                                None,
                                completion_tx,
                            ))?;
                    },
                    None => {
                        log::error!(
                            "Could not find an active tab - is there at least 1 connected user?"
                        );
                    },
                }
            },
            ScreenInstruction::StartOrReloadPluginPane(run_plugin, pane_title, completion_tx) => {
                let tab_index = screen.active_tab_ids.values().next().unwrap_or(&1);
                let size = Size::default();
                let should_float = Some(false);

                screen
                    .bus
                    .senders
                    .send_to_plugin(PluginInstruction::Reload(
                        should_float,
                        pane_title,
                        run_plugin,
                        *tab_index,
                        size,
                        completion_tx,
                    ))?;
            },
            ScreenInstruction::AddPlugin(
                should_float,
                should_be_in_place,
                close_replaced_pane,
                run_plugin_or_alias,
                pane_title,
                tab_index,
                plugin_id,
                pane_id_to_replace,
                cwd,
                start_suppressed,
                floating_pane_coordinates,
                should_focus_plugin,
                client_id,
                mut completion_tx,
            ) => {
                let mut new_pane_placement = NewPanePlacement::default();
                let maybe_should_float = should_float;
                let should_be_tiled = maybe_should_float.map(|f| !f).unwrap_or(false);
                let should_float = maybe_should_float.unwrap_or(false);
                if floating_pane_coordinates.is_some() || should_float {
                    new_pane_placement = NewPanePlacement::with_floating_pane_coordinates(
                        floating_pane_coordinates.clone(),
                    );
                }
                if should_be_tiled {
                    new_pane_placement = NewPanePlacement::Tiled {
                        direction: None,
                        borderless: None,
                    };
                }
                if should_be_in_place {
                    new_pane_placement = NewPanePlacement::with_pane_id_to_replace(
                        pane_id_to_replace.map(|id| id.into()),
                        close_replaced_pane,
                    );
                }
                if screen.active_tab_ids.is_empty() && tab_index.is_none() {
                    pending_events_waiting_for_client.push(ScreenInstruction::AddPlugin(
                        maybe_should_float,
                        should_be_in_place,
                        close_replaced_pane,
                        run_plugin_or_alias,
                        pane_title,
                        tab_index,
                        plugin_id,
                        pane_id_to_replace,
                        cwd,
                        start_suppressed,
                        floating_pane_coordinates,
                        should_focus_plugin,
                        client_id,
                        completion_tx,
                    ));
                    continue;
                }
                let pane_title = pane_title.unwrap_or_else(|| {
                    format!(
                        "({}) - {}",
                        cwd.map(|cwd| cwd.display().to_string())
                            .unwrap_or(".".to_owned()),
                        run_plugin_or_alias.location_string()
                    )
                });
                let run_plugin = Run::Plugin(run_plugin_or_alias);

                // Set affected pane ID for CLI client output
                if let Some(ref mut completion) = completion_tx {
                    completion.set_affected_pane_id(PaneId::Plugin(plugin_id));
                }

                if should_be_in_place {
                    if let Some(pane_id_to_replace) = pane_id_to_replace {
                        let client_tab_index_or_pane_id =
                            ClientTabIndexOrPaneId::PaneId(pane_id_to_replace);
                        screen.replace_pane(
                            PaneId::Plugin(plugin_id),
                            None,
                            Some(run_plugin),
                            Some(pane_title),
                            close_replaced_pane,
                            client_tab_index_or_pane_id,
                        )?;
                    } else if let Some(client_id) = client_id {
                        let client_tab_index_or_pane_id =
                            ClientTabIndexOrPaneId::ClientId(client_id);
                        screen.replace_pane(
                            PaneId::Plugin(plugin_id),
                            None,
                            Some(run_plugin),
                            Some(pane_title),
                            close_replaced_pane,
                            client_tab_index_or_pane_id,
                        )?;
                    } else {
                        log::error!(
                            "Must have pane id to replace or connected client_id if replacing a pane"
                        );
                    }
                } else if let Some(client_id) = client_id {
                    active_tab_and_connected_client_id!(screen, client_id, |active_tab: &mut Tab, _client_id: ClientId| {
                        active_tab.new_pane(
                            PaneId::Plugin(plugin_id),
                            Some(pane_title),
                            Some(run_plugin),
                            start_suppressed,
                            should_focus_plugin.unwrap_or(true),
                            new_pane_placement,
                            Some(client_id),
                            None,
                        )
                    }, ?);
                } else if let Some(active_tab) =
                    tab_index.and_then(|tab_index| screen.tabs.get_mut(&tab_index))
                {
                    active_tab.new_pane(
                        PaneId::Plugin(plugin_id),
                        Some(pane_title),
                        Some(run_plugin),
                        start_suppressed,
                        should_focus_plugin.unwrap_or(true),
                        new_pane_placement,
                        None,
                        None,
                    )?;
                } else {
                    log::error!("Tab index not found: {:?}", tab_index);
                }
                if let Some(loading_indication) = plugin_loading_message_cache.remove(&plugin_id) {
                    screen.update_plugin_loading_stage(plugin_id, loading_indication);
                    screen.render(None)?;
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UpdatePluginLoadingStage(pid, loading_indication) => {
                let found_plugin =
                    screen.update_plugin_loading_stage(pid, loading_indication.clone());
                if !found_plugin {
                    plugin_loading_message_cache.insert(pid, loading_indication);
                }
                screen.render(None)?;
            },
            ScreenInstruction::StartPluginLoadingIndication(pid, loading_indication) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_plugin(pid) {
                        tab.start_plugin_loading_indication(pid, loading_indication);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ProgressPluginLoadingOffset(pid) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_plugin(pid) {
                        tab.progress_plugin_loading_offset(pid);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::RequestStateUpdateForPlugins => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    tab.update_input_modes()?;
                }
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::LaunchOrFocusPlugin(
                run_plugin,
                should_float,
                move_to_focused_tab,
                should_open_in_place,
                close_replaced_pane,
                pane_id_to_replace,
                skip_cache,
                client_id,
                mut completion_tx,
                explicit_tab_id,
            ) => match pane_id_to_replace {
                Some(pane_id_to_replace) if should_open_in_place => {
                    let resolved_tab_index =
                        explicit_tab_id.or_else(|| screen.active_tab_ids.values().next().copied());
                    match resolved_tab_index {
                        Some(tab_index) => {
                            let size = Size::default();
                            screen
                                .bus
                                .senders
                                .send_to_pty(PtyInstruction::FillPluginCwd(
                                    Some(should_float),
                                    should_open_in_place,
                                    close_replaced_pane,
                                    None,
                                    run_plugin,
                                    tab_index,
                                    Some(pane_id_to_replace),
                                    client_id,
                                    size,
                                    skip_cache,
                                    None,
                                    None,
                                    None,
                                    completion_tx,
                                ))?;
                        },
                        None => {
                            log::error!(
                                "Could not find an active tab - is there at least 1 connected user?"
                            );
                        },
                    }
                },
                _ => {
                    let client_id = if screen.active_tab_ids.contains_key(&client_id) {
                        Some(client_id)
                    } else {
                        screen.get_first_client_id()
                    };
                    let client_id_and_focused_tab = client_id.and_then(|client_id| {
                        screen
                            .active_tab_ids
                            .get(&client_id)
                            .map(|tab_index| (*tab_index, client_id))
                    });
                    let resolved_tab_and_client = explicit_tab_id
                        .and_then(|tid| client_id.map(|cid| (tid, cid)))
                        .or(client_id_and_focused_tab);
                    match resolved_tab_and_client {
                        Some((tab_index, client_id)) => {
                            if screen.focus_plugin_pane(
                                &run_plugin,
                                should_float,
                                move_to_focused_tab,
                                should_open_in_place,
                                client_id,
                                &mut completion_tx,
                            )? {
                                screen.render(None)?;
                                screen.log_and_report_session_state()?;
                            } else {
                                screen
                                    .bus
                                    .senders
                                    .send_to_pty(PtyInstruction::FillPluginCwd(
                                        Some(should_float),
                                        should_open_in_place,
                                        close_replaced_pane,
                                        None,
                                        run_plugin,
                                        tab_index,
                                        None,
                                        client_id,
                                        Size::default(),
                                        skip_cache,
                                        None,
                                        None,
                                        None,
                                        completion_tx,
                                    ))?;
                            }
                        },
                        None => {
                            log::error!("No connected clients found - cannot load or focus plugin")
                        },
                    }
                },
            },
            ScreenInstruction::LaunchPlugin(
                run_plugin,
                should_float,
                should_open_in_place,
                close_replaced_pane,
                pane_id_to_replace,
                skip_cache,
                cwd,
                client_id,
                completion_tx,
                explicit_tab_id,
            ) => match pane_id_to_replace {
                Some(pane_id_to_replace) => {
                    let resolved_tab_index =
                        explicit_tab_id.or_else(|| screen.active_tab_ids.values().next().copied());
                    match resolved_tab_index {
                        Some(tab_index) => {
                            let size = Size::default();
                            screen
                                .bus
                                .senders
                                .send_to_pty(PtyInstruction::FillPluginCwd(
                                    Some(should_float),
                                    should_open_in_place,
                                    close_replaced_pane,
                                    None,
                                    run_plugin,
                                    tab_index,
                                    Some(pane_id_to_replace),
                                    client_id,
                                    size,
                                    skip_cache,
                                    cwd,
                                    None,
                                    None,
                                    completion_tx,
                                ))?;
                        },
                        None => {
                            log::error!(
                                "Could not find an active tab - is there at least 1 connected user?"
                            );
                        },
                    }
                },
                None => {
                    let client_id = if screen.active_tab_ids.contains_key(&client_id) {
                        Some(client_id)
                    } else {
                        screen.get_first_client_id()
                    };
                    let client_id_and_focused_tab = client_id.and_then(|client_id| {
                        screen
                            .active_tab_ids
                            .get(&client_id)
                            .map(|tab_index| (*tab_index, client_id))
                    });
                    let resolved_tab_and_client = explicit_tab_id
                        .and_then(|tid| client_id.map(|cid| (tid, cid)))
                        .or(client_id_and_focused_tab);
                    match resolved_tab_and_client {
                        Some((tab_index, client_id)) => {
                            screen
                                .bus
                                .senders
                                .send_to_pty(PtyInstruction::FillPluginCwd(
                                    Some(should_float),
                                    should_open_in_place,
                                    close_replaced_pane,
                                    None,
                                    run_plugin,
                                    tab_index,
                                    None,
                                    client_id,
                                    Size::default(),
                                    skip_cache,
                                    cwd,
                                    None,
                                    None,
                                    completion_tx,
                                ))?;
                        },
                        None => {
                            log::error!("No connected clients found - cannot load or focus plugin")
                        },
                    }
                },
            },
            ScreenInstruction::SuppressPane(pane_id, client_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_non_suppressed_pane_with_pid(&pane_id) {
                        tab.suppress_pane(pane_id, Some(client_id));
                        drop(screen.render(None));
                        break;
                    }
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UnsuppressPane(pane_id, should_float_if_hidden) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.unsuppress_pane(pane_id, should_float_if_hidden);
                        drop(screen.render(None));
                        break;
                    }
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UnsuppressOrExpandPane(pane_id, should_float_if_hidden) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.unsuppress_or_expand_pane(pane_id, should_float_if_hidden);
                        drop(screen.render(None));
                        break;
                    }
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::FocusPaneWithId(
                pane_id,
                should_float_if_hidden,
                should_be_in_place_if_hidden,
                client_id,
                mut completion_tx,
            ) => {
                let pane_exists = screen
                    .tabs
                    .iter()
                    .any(|(_, tab)| tab.has_pane_with_pid(&pane_id));
                if !pane_exists {
                    if let Some(c) = completion_tx.as_mut() {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                } else {
                    let already_focused = screen
                        .get_active_pane_id(&client_id)
                        .map(|active| active == pane_id)
                        .unwrap_or(false);
                    if already_focused {
                        if let Some(c) = completion_tx.as_mut() {
                            c.set_exit_status(1);
                            c.set_error_message(format!("Pane {:?} is already focused", pane_id));
                        }
                    } else {
                        screen.focus_pane_with_id(
                            pane_id,
                            should_float_if_hidden,
                            should_be_in_place_if_hidden,
                            client_id,
                        )?;
                        screen.clear_bell_for_pane_id(pane_id, client_id);
                        screen.log_and_report_session_state()?;
                    }
                }
            },
            ScreenInstruction::RenamePane(
                pane_id,
                new_name,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        match tab.rename_pane(new_name, pane_id) {
                            Ok(()) => drop(screen.render(None)),
                            Err(e) => log::error!("Failed to rename pane: {:?}", e),
                        }
                        break;
                    }
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::RenameActivePane(new_name, client_id, _completion_tx) => {
                active_tab_and_connected_client_id!(
                    screen,
                    client_id,
                    |tab: &mut Tab, client_id: ClientId| tab
                        .rename_active_pane(new_name, client_id),
                    ?
                );
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::RenameTab(
                tab_index,
                new_name,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                // tab_index here is 1-based user input representing display position
                let tab_position = tab_index.saturating_sub(1); // Convert to 0-based

                match screen.get_tab_by_position_mut(tab_position) {
                    Some(tab) => {
                        tab.name = String::from_utf8_lossy(&new_name).to_string();
                    },
                    None => {
                        log::error!("Failed to find tab at position: {}", tab_position);
                    },
                }
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::GoToTabWithId(tab_id, client_id, _completion_tx) => {
                let client_id_to_switch = client_id
                    .and_then(|cid| {
                        if screen.active_tab_ids.contains_key(&cid) {
                            Some(cid)
                        } else {
                            screen.active_tab_ids.keys().next().copied()
                        }
                    })
                    .or_else(|| screen.active_tab_ids.keys().next().copied());

                if let Some(client_id) = client_id_to_switch {
                    // Get the position from the ID
                    if let Some(tab_position) = screen.get_tab_position_by_id(tab_id) {
                        // switch_active_tab expects 0-based position
                        screen.switch_active_tab(tab_position, None, true, client_id)?;
                        screen.render(None)?;

                        screen
                            .tab_history
                            .entry(client_id)
                            .or_default()
                            .push(tab_id);
                    } else {
                        log::error!("Tab with ID {} not found", tab_id);
                    }
                }
            },
            ScreenInstruction::RenameTabWithId(tab_id, new_name, _completion_tx) => {
                // Use get_tab_by_id_mut() helper method
                if let Some(tab) = screen.get_tab_by_id_mut(tab_id) {
                    tab.name = String::from_utf8_lossy(&new_name).to_string();
                    screen.log_and_report_session_state()?;
                } else {
                    log::error!("Failed to find tab with ID: {}", tab_id);
                }
            },
            ScreenInstruction::CloseTabWithId(tab_id, mut completion_tx) => {
                if screen.get_tab_by_id(tab_id).is_some() {
                    if let Err(error) = screen.close_tab_by_id(tab_id) {
                        log::error!("Failed to close tab with ID {}: {}", tab_id, error);
                        if let Some(ref mut completion_tx) = completion_tx {
                            completion_tx.set_exit_status(1);
                            completion_tx.set_error_message(format!(
                                "Failed to close tab with ID {}: {}",
                                tab_id, error
                            ));
                        }
                    }
                } else {
                    log::error!("Failed to find tab with ID: {}", tab_id);
                    if let Some(ref mut completion_tx) = completion_tx {
                        completion_tx.set_exit_status(1);
                        completion_tx
                            .set_error_message(format!("Failed to find tab with ID: {}", tab_id));
                    }
                }
            },
            ScreenInstruction::CloseTabWithIdIfName(
                tab_id,
                expected_name,
                expected_session_incarnation,
                expected_tab_instance_id,
                mut completion_tx,
            ) => {
                if let Err(error) = screen.close_tab_by_id_if_name(
                    tab_id,
                    &expected_name,
                    &expected_session_incarnation,
                    &expected_tab_instance_id,
                ) {
                    log::error!(
                        "Failed to close tab with ID {} and expected name {:?}: {}",
                        tab_id,
                        expected_name,
                        error
                    );
                    if let Some(ref mut completion_tx) = completion_tx {
                        completion_tx.set_exit_status(1);
                        completion_tx.set_error_message(error.to_string());
                    }
                }
            },
            ScreenInstruction::CloseTabWithIdIfNameIfQuiescent(
                tab_id,
                expected_name,
                expected_session_incarnation,
                expected_tab_instance_id,
                mut completion_tx,
            ) => {
                if let Err(error) = screen.close_tab_by_id_if_name_if_quiescent(
                    tab_id,
                    &expected_name,
                    &expected_session_incarnation,
                    &expected_tab_instance_id,
                ) {
                    log::error!(
                        "GC-safe close refused for tab ID {} and expected name {:?}: {}",
                        tab_id,
                        expected_name,
                        error
                    );
                    if let Some(ref mut completion_tx) = completion_tx {
                        completion_tx.set_exit_status(1);
                        completion_tx.set_error_message(error.to_string());
                    }
                }
            },
            ScreenInstruction::BreakPanesToTabWithId {
                pane_ids,
                tab_id,
                should_change_focus_to_target_tab,
                client_id,
                mut completion_tx,
            } => {
                // Verify tab exists
                if screen.get_tab_by_id(tab_id).is_none() {
                    log::error!("Tab with ID {} not found", tab_id);
                    // Don't set affected_tab_id, it will remain None to signal failure
                } else {
                    // break_multiple_panes_to_tab_with_index uses tab ID
                    screen.break_multiple_panes_to_tab_with_index(
                        pane_ids,
                        tab_id,
                        should_change_focus_to_target_tab,
                        client_id,
                    )?;
                    // Set affected tab ID (tab_id is the ID here)
                    if let Some(c) = completion_tx.as_mut() {
                        c.set_affected_tab_id(tab_id)
                    }
                    let pane_group = screen.get_client_pane_group(&client_id);
                    if !pane_group.is_empty() {
                        let _ = screen.bus.senders.send_to_background_jobs(
                            BackgroundJob::HighlightPanesWithMessage(
                                pane_group.iter().copied().collect(),
                                "BROKEN OUT".to_owned(),
                            ),
                        );
                    }
                    screen.clear_pane_group(&client_id);
                }
            },
            ScreenInstruction::RequestPluginPermissions(plugin_id, plugin_permission) => {
                let all_tabs = screen.get_tabs_mut();
                let found = all_tabs.values_mut().any(|tab| {
                    if tab.has_plugin(plugin_id) {
                        tab.request_plugin_permissions(plugin_id, Some(plugin_permission.clone()));
                        true
                    } else {
                        false
                    }
                });

                if !found {
                    log::error!("PluginId '{}' not found - caching request", plugin_id);
                    pending_events_waiting_for_client.push(
                        ScreenInstruction::RequestPluginPermissions(plugin_id, plugin_permission),
                    );
                }
            },
            ScreenInstruction::BreakPane(default_shell, client_id, completion_tx) => {
                let default_layout = screen.default_layout.clone();
                match screen.break_pane(default_shell, default_layout, client_id, completion_tx) {
                    Ok(Some(transfer)) => {
                        pending_tab_ids.extend(transfer.pending_gate_tab_ids());
                        // Both the source extraction and the destination stay
                        // behind one transaction fence. A render request while
                        // worker ACKs are unresolved therefore preserves the
                        // last committed frame instead of making the pane
                        // disappear.
                        screen.render(None).non_fatal();
                    },
                    Ok(None) => {},
                    Err(error) => {
                        log::error!("{error:#}");
                    },
                }
            },
            ScreenInstruction::BreakPaneRight(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.break_pane_to_new_tab(Direction::Right, client_id)?;
            },
            ScreenInstruction::BreakPaneLeft(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.break_pane_to_new_tab(Direction::Left, client_id)?;
            },
            ScreenInstruction::UpdateSessionInfos(new_session_infos, resurrectable_sessions) => {
                screen.update_session_infos(new_session_infos, resurrectable_sessions)?;
            },
            ScreenInstruction::UpdateAvailableLayouts(layouts, errors) => {
                screen.update_available_layouts(layouts, errors);
            },
            ScreenInstruction::ReplacePane(
                new_pane_id,
                hold_for_command,
                pane_title,
                invoked_with,
                close_replaced_pane,
                client_id_tab_index_or_pane_id,
                mut completion_tx,
            ) => {
                if let Some(c) = completion_tx.as_mut() {
                    c.set_affected_pane_id(new_pane_id)
                }
                screen.replace_pane(
                    new_pane_id,
                    hold_for_command,
                    invoked_with,
                    pane_title,
                    close_replaced_pane,
                    client_id_tab_index_or_pane_id,
                )?;

                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::SerializeLayoutForResurrection => {
                if screen.session_serialization {
                    screen.dump_layout_to_hd()?;
                }
            },
            ScreenInstruction::SaveSession(_client_id, completion_tx) => {
                let err_context = || "Failed to save session";

                screen.update_active_pane_ids();
                let pane_manifest = screen.generate_and_report_pane_state()?;
                let tab_infos = screen.generate_and_report_tab_state()?;

                #[cfg(not(test))]
                let (available_layouts, _layout_errors) = Layout::list_available_layouts(
                    screen.layout_dir.clone(),
                    &screen.default_layout_name,
                );
                #[cfg(test)]
                let available_layouts = vec![];

                let creation_time = {
                    let sock_path = ZELLIJ_SOCK_DIR.join(&screen.session_name);
                    std::fs::metadata(&sock_path)
                        .ok()
                        .and_then(|f| f.created().ok().or_else(|| f.modified().ok()))
                        .and_then(|d| d.elapsed().ok())
                        .map(|d| Duration::from_secs(d.as_secs()))
                        .unwrap_or_default()
                };
                let session_info = SessionInfo {
                    name: screen.session_name.clone(),
                    tabs: tab_infos,
                    panes: pane_manifest,
                    connected_clients: screen.active_tab_ids.keys().len(),
                    is_current_session: true,
                    available_layouts,
                    web_clients_allowed: screen.web_sharing.web_clients_allowed(),
                    web_client_count: screen
                        .connected_clients
                        .borrow()
                        .iter()
                        .filter(|(_client_id, is_web_client)| **is_web_client)
                        .count(),
                    plugins: Default::default(),
                    tab_history: screen.tab_history.clone(),
                    pane_history: screen
                        .pane_history
                        .iter()
                        .map(|(k, v)| (*k, v.iter().map(|v| (*v).into()).collect()))
                        .collect(),
                    creation_time,
                };

                let session_layout_metadata = if screen.session_serialization {
                    screen.get_layout_metadata(Some(screen.default_shell.clone()), None)
                } else {
                    // Create empty metadata if serialization is disabled
                    SessionLayoutMetadata::new(screen.default_layout.clone())
                };
                let generation = reserve_session_state_generation(&screen.session_name)
                    .map_err(anyhow::Error::msg)?;

                screen
                    .bus
                    .senders
                    .send_to_pty(PtyInstruction::SaveSessionToDisk {
                        session_name: screen.session_name.clone(),
                        session_info,
                        session_layout_metadata,
                        generation,
                        completion_tx,
                    })
                    .with_context(err_context)?;
            },
            ScreenInstruction::RenameSession(
                name,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                if screen.peer_sessions_cache.contains_key(&name) {
                    let error_text = "A session by this name already exists.";
                    log::error!("{}", error_text);
                    if let Some(os_input) = &mut screen.bus.os_input {
                        let _ = os_input.send_to_client(
                            client_id,
                            ServerToClientMsg::LogError {
                                lines: vec![error_text.to_owned()],
                            },
                        );
                    }
                } else if screen.resurrectable_sessions_cache.contains_key(&name) {
                    let error_text =
                        "A resurrectable session by this name exists, cannot use this name.";
                    log::error!("{}", error_text);
                    if let Some(os_input) = &mut screen.bus.os_input {
                        let _ = os_input.send_to_client(
                            client_id,
                            ServerToClientMsg::LogError {
                                lines: vec![error_text.to_owned()],
                            },
                        );
                    }
                } else {
                    let err_context = || "Failed to rename session".to_string();
                    let old_session_name = screen.session_name.clone();

                    // update state
                    screen.session_name = name.clone();
                    screen.default_mode_info.session_name = Some(name.clone());
                    for (_client_id, mode_info) in screen.mode_info.iter_mut() {
                        mode_info.session_name = Some(name.clone());
                    }
                    for (_, tab) in screen.tabs.iter_mut() {
                        tab.rename_session(name.clone()).with_context(err_context)?;
                    }

                    // rename socket file
                    let old_socket_file_path = ZELLIJ_SOCK_DIR.join(&old_session_name);
                    let new_socket_file_path = ZELLIJ_SOCK_DIR.join(&name);
                    if let Err(e) = std::fs::rename(old_socket_file_path, new_socket_file_path) {
                        log::error!("Failed to rename ipc socket: {:?}", e);
                    }

                    // rename session_info folder (TODO: make this atomic, right now there is a
                    // chance background_jobs will re-create this folder before it knows the
                    // session was renamed)
                    let old_session_info_folder =
                        session_info_folder_for_session(&old_session_name);
                    let new_session_info_folder = session_info_folder_for_session(&name);
                    if let Err(e) =
                        std::fs::rename(old_session_info_folder, new_session_info_folder)
                    {
                        log::error!("Failed to rename session_info folder: {:?}", e);
                    }

                    // report
                    screen
                        .log_and_report_session_state()
                        .with_context(err_context)?;

                    // set the env variable
                    set_session_name(name.clone());
                    let connected_client_ids: Vec<ClientId> =
                        screen.active_tab_ids.keys().copied().collect();
                    for client_id in connected_client_ids {
                        if let Some(os_input) = &mut screen.bus.os_input {
                            let _ = os_input.send_to_client(
                                client_id,
                                ServerToClientMsg::RenamedSession { name: name.clone() },
                            );
                        }
                    }
                }
            },
            ScreenInstruction::Reconfigure(params) => {
                let ReconfigureParams {
                    client_id,
                    keybinds,
                    default_mode,
                    theme,
                    host_theme_dark,
                    host_theme_light,
                    simplified_ui,
                    default_shell,
                    pane_frames,
                    copy_to_clipboard,
                    copy_command,
                    copy_on_select,
                    auto_layout,
                    rounded_corners,
                    hide_session_name,
                    stacked_resize,
                    default_editor,
                    advanced_mouse_actions,
                    mouse_hover_effects,
                    visual_bell,
                    focus_follows_mouse,
                    mouse_click_through,
                } = *params;
                screen.host_theme_dark_styling = host_theme_dark;
                screen.host_theme_light_styling = host_theme_light;
                screen
                    .reconfigure(
                        keybinds,
                        default_mode,
                        theme,
                        simplified_ui,
                        default_shell,
                        pane_frames,
                        copy_command,
                        copy_to_clipboard,
                        copy_on_select,
                        auto_layout,
                        rounded_corners,
                        hide_session_name,
                        stacked_resize,
                        default_editor,
                        advanced_mouse_actions,
                        mouse_hover_effects,
                        visual_bell,
                        focus_follows_mouse,
                        mouse_click_through,
                        client_id,
                    )
                    .non_fatal();
            },
            ScreenInstruction::RerunCommandPane(terminal_pane_id, completion_tx) => {
                screen.rerun_command_pane_with_id(terminal_pane_id, completion_tx)
            },
            ScreenInstruction::ResizePaneWithId(resize, pane_id) => {
                screen.resize_pane_with_id(resize, pane_id)
            },
            ScreenInstruction::EditScrollbackForPaneWithId(pane_id, completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.edit_scrollback_for_pane_with_id(pane_id, completion_tx)
                            .non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::WriteToPaneId(bytes, pane_id, _completion) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.write_to_pane_id(&None, bytes, false, pane_id, None, None)
                            .non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::Paste(bytes, pane_id, client_id, _completion) => {
                match pane_id {
                    Some(pane_id) => {
                        let all_tabs = screen.get_tabs_mut();
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                tab.paste_to_pane_id(bytes, pane_id, _completion)
                                    .non_fatal();
                                break;
                            }
                        }
                    },
                    None => {
                        active_tab_and_connected_client_id!(
                            screen,
                            client_id,
                            |tab: &mut Tab, _client_id: ClientId| {
                                tab.paste_to_active_terminal(bytes, client_id, _completion)
                                    .non_fatal();
                            }
                        );
                    },
                }
                screen.render(None)?;
            },
            ScreenInstruction::SetPaneColor(pane_id, fg, bg, _completion) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.set_pane_color(pane_id, fg, bg).non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::WriteKeyToPaneId(
                key_with_modifier,
                bytes,
                is_kitty,
                pane_id,
                _completion,
            ) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.write_to_pane_id(
                            &key_with_modifier,
                            bytes,
                            is_kitty,
                            pane_id,
                            None, // client_id not needed for targeted write
                            None, // completion handled by instruction
                        )
                        .non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::CopyTextToClipboard(text, plugin_id) => {
                let plugin_pane_id = PaneId::Plugin(plugin_id);
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&plugin_pane_id) {
                        tab.copy_text_to_clipboard(&text)
                            .with_context(|| {
                                format!(
                                    "failed to copy text to clipboard from plugin {}",
                                    plugin_id
                                )
                            })
                            .non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::MovePaneWithPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.move_pane(pane_id);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::MovePaneWithPaneIdInDirection(pane_id, direction) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        match direction {
                            Direction::Down => tab.move_pane_down(pane_id),
                            Direction::Up => tab.move_pane_up(pane_id),
                            Direction::Left => tab.move_pane_left(pane_id),
                            Direction::Right => tab.move_pane_right(pane_id),
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ClearScreenForPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.clear_screen_for_pane_id(pane_id);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollUpInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_up(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling up"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollDownInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_down(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling down"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToTopInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_to_top(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling to top"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToBottomInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_to_bottom(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling to bottom"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollUpInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_page_up(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollDownInPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id {
                            tab.scroll_terminal_page_down(terminal_pane_id);
                        } else {
                            // this is because to do this with plugins, we need the client_id -
                            // which we do not have (yet?) in this context...
                            log::error!(
                                "Currently only terminal panes are supported for scrolling"
                            );
                        }
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::TogglePaneIdFullscreen(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.toggle_pane_fullscreen(pane_id);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::TogglePaneEmbedOrEjectForPaneId(pane_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.toggle_pane_embed_or_floating_for_pane_id(pane_id, None)
                            .non_fatal();
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::CloseTabWithIndex(tab_index) => {
                // tab_index here means display position (0-based) per API convention
                // Must map to stable ID for close_tab_by_id()

                if let Some(tab_id) = screen.get_tab_id_at_position(tab_index) {
                    screen.close_tab_by_id(tab_id).non_fatal();
                } else {
                    log::error!("Failed to find tab at position: {}", tab_index);
                }
            },
            ScreenInstruction::BreakPanesToNewTab {
                pane_ids,
                default_shell,
                should_change_focus_to_new_tab,
                new_tab_name,
                client_id,
                completion_tx,
            } => {
                match screen.break_multiple_panes_to_new_tab(
                    pane_ids,
                    default_shell,
                    should_change_focus_to_new_tab,
                    new_tab_name,
                    client_id,
                    completion_tx,
                ) {
                    Ok(transfer) => {
                        pending_tab_ids.extend(transfer.pending_gate_tab_ids());
                        screen.render(None).non_fatal();
                    },
                    Err(error) => {
                        log::error!("{error:#}");
                    },
                }
                // TODO: is this a race?
                let pane_group = screen.get_client_pane_group(&client_id);
                if !pane_group.is_empty() {
                    let _ = screen.bus.senders.send_to_background_jobs(
                        BackgroundJob::HighlightPanesWithMessage(
                            pane_group.iter().copied().collect(),
                            "BROKEN OUT".to_owned(),
                        ),
                    );
                }
                screen.clear_pane_group(&client_id);
            },
            ScreenInstruction::BreakPanesToTabWithIndex {
                pane_ids,
                tab_index,
                should_change_focus_to_new_tab,
                client_id,
                mut completion_tx,
            } => {
                // tab_index is the target tab ID
                screen.break_multiple_panes_to_tab_with_index(
                    pane_ids,
                    tab_index,
                    should_change_focus_to_new_tab,
                    client_id,
                )?;
                // Set affected tab ID (tab_index is the ID here)
                if let Some(c) = completion_tx.as_mut() {
                    c.set_affected_tab_id(tab_index)
                }
                let pane_group = screen.get_client_pane_group(&client_id);
                if !pane_group.is_empty() {
                    let _ = screen.bus.senders.send_to_background_jobs(
                        BackgroundJob::HighlightPanesWithMessage(
                            pane_group.iter().copied().collect(),
                            "BROKEN OUT".to_owned(),
                        ),
                    );
                }
                screen.clear_pane_group(&client_id);
            },
            ScreenInstruction::TogglePanePinned(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.toggle_pane_pinned(client_id);
            },
            ScreenInstruction::SetFloatingPanePinned(pane_id, should_be_pinned) => {
                screen.set_floating_pane_pinned(pane_id, should_be_pinned);
            },
            ScreenInstruction::StackPanes(
                pane_ids_to_stack,
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                if let Some(last_pane_id) = screen.stack_panes(pane_ids_to_stack) {
                    let _ = screen.focus_pane_with_id(last_pane_id, false, false, client_id);
                    let _ = screen.render(None);
                    let pane_group = screen.get_client_pane_group(&client_id);
                    if !pane_group.is_empty() {
                        let _ = screen.bus.senders.send_to_background_jobs(
                            BackgroundJob::HighlightPanesWithMessage(
                                pane_group.iter().copied().collect(),
                                "STACKED".to_owned(),
                            ),
                        );
                    }
                    screen.clear_pane_group(&client_id);
                }
            },
            ScreenInstruction::ChangeFloatingPanesCoordinates(
                pane_ids_and_coordinates,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.change_floating_panes_coordinates(pane_ids_and_coordinates);
                let _ = screen.render(None);
            },
            ScreenInstruction::TogglePaneBorderless(pane_id, _completion_tx) => {
                screen.toggle_pane_borderless(pane_id);
                let _ = screen.render(None);
            },
            ScreenInstruction::SetPaneBorderless(pane_id, borderless, _completion_tx) => {
                screen.set_pane_borderless(pane_id, borderless);
                let _ = screen.render(None);
            },
            ScreenInstruction::GroupAndUngroupPanes(
                pane_ids_to_group,
                pane_ids_to_ungroup,
                for_all_clients,
                client_id,
            ) => {
                screen.group_and_ungroup_panes(
                    pane_ids_to_group,
                    pane_ids_to_ungroup,
                    for_all_clients,
                    client_id,
                );
                let _ = screen.log_and_report_session_state();
            },
            ScreenInstruction::TogglePaneInGroup(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.toggle_pane_in_group(client_id).non_fatal();
            },
            ScreenInstruction::ToggleGroupMarking(
                client_id,
                _completion_tx, // the action ends here, dropping this will release anything
                                // waiting for it
            ) => {
                screen.toggle_group_marking(client_id).non_fatal();
            },
            ScreenInstruction::SessionSharingStatusChange(web_sharing) => {
                if web_sharing {
                    screen.web_sharing = WebSharing::On;
                } else {
                    screen.web_sharing = WebSharing::Off;
                }

                for tab in screen.tabs.values_mut() {
                    tab.update_web_sharing(screen.web_sharing);
                }
                let _ = screen.log_and_report_session_state();
                let _ = screen.render(None);
            },
            ScreenInstruction::HighlightAndUnhighlightPanes(
                pane_ids_to_highlight,
                pane_ids_to_unhighlight,
                client_id,
            ) => {
                {
                    let all_tabs = screen.get_tabs_mut();
                    for pane_id in pane_ids_to_highlight {
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                tab.add_highlight_pane_frame_color_override(
                                    pane_id,
                                    None,
                                    Some(client_id),
                                );
                            }
                        }
                    }
                    for pane_id in pane_ids_to_unhighlight {
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                tab.clear_pane_frame_color_override(pane_id, Some(client_id));
                            }
                        }
                    }
                    screen.render(None)?;
                }
                let _ = screen.log_and_report_session_state();
            },
            ScreenInstruction::FloatMultiplePanes(pane_ids_to_float, client_id) => {
                {
                    let all_tabs = screen.get_tabs_mut();
                    let mut ejected_panes_in_group = vec![];
                    for pane_id in pane_ids_to_float {
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                if !tab.pane_id_is_floating(&pane_id) {
                                    ejected_panes_in_group.push(pane_id);
                                    tab.toggle_pane_embed_or_floating_for_pane_id(
                                        pane_id,
                                        Some(client_id),
                                    )
                                    .non_fatal();
                                }
                                tab.show_floating_panes();
                            }
                        }
                    }
                    screen.render(None)?;
                    if !ejected_panes_in_group.is_empty() {
                        let _ = screen.bus.senders.send_to_background_jobs(
                            BackgroundJob::HighlightPanesWithMessage(
                                ejected_panes_in_group,
                                "EJECTED".to_owned(),
                            ),
                        );
                    }
                }
                let _ = screen.log_and_report_session_state();
            },
            ScreenInstruction::EmbedMultiplePanes(pane_ids_to_float, client_id) => {
                {
                    let all_tabs = screen.get_tabs_mut();
                    let mut embedded_panes_in_group = vec![];
                    for pane_id in pane_ids_to_float {
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                if tab.pane_id_is_floating(&pane_id) {
                                    embedded_panes_in_group.push(pane_id);
                                    tab.toggle_pane_embed_or_floating_for_pane_id(
                                        pane_id,
                                        Some(client_id),
                                    )
                                    .non_fatal();
                                }
                                tab.hide_floating_panes();
                            }
                        }
                    }
                    screen.render(None)?;
                    if !embedded_panes_in_group.is_empty() {
                        let _ = screen.bus.senders.send_to_background_jobs(
                            BackgroundJob::HighlightPanesWithMessage(
                                embedded_panes_in_group,
                                "EMBEDDED".to_owned(),
                            ),
                        );
                    }
                }
                let _ = screen.log_and_report_session_state();
            },
            ScreenInstruction::InterceptKeyPresses(plugin_id, client_id) => {
                keybind_intercepts.insert(client_id, plugin_id);
            },
            ScreenInstruction::ClearKeyPressesIntercepts(client_id) => {
                keybind_intercepts.remove(&client_id);
            },
            ScreenInstruction::ReplacePaneWithExistingPane(
                old_pane_id,
                new_pane_id,
                suppress_replaced_pane,
                completion_tx,
            ) => screen.replace_pane_with_existing_pane(
                old_pane_id,
                new_pane_id,
                suppress_replaced_pane,
                completion_tx,
            ),
            ScreenInstruction::AddWatcherClient(client_id, size) => {
                screen
                    .add_watcher_client(client_id)
                    .context("failed to add watcher client")?;
                screen.set_watcher_size(client_id, size);
                screen.render(None)?;
            },
            ScreenInstruction::RemoveWatcherClient(client_id) => {
                screen.remove_watcher_client(client_id);
            },
            ScreenInstruction::SetFollowedClient(client_id) => {
                screen
                    .set_followed_client(client_id)
                    .context("failed to set followed client")?;
            },
            ScreenInstruction::WatcherTerminalResize(client_id, size) => {
                screen.set_watcher_size(client_id, size);
                screen.render(None)?;
            },
            ScreenInstruction::ClearMouseHelpText(client_id) => {
                if let Ok(tab) = screen.get_active_tab_mut(client_id) {
                    tab.clear_mouse_help_text(client_id);
                    screen.render(None)?;
                }
            },
            ScreenInstruction::SetPluginRegexHighlights {
                pane_id,
                plugin_id,
                highlights,
            } => {
                let style = screen.style;
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.set_plugin_regex_highlights_for_pane(
                            pane_id, plugin_id, highlights, &style,
                        );
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ClearPluginHighlights { pane_id, plugin_id } => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.clear_plugin_highlights_for_pane(pane_id, plugin_id);
                        break;
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ClearAllPluginHighlights(plugin_id) => {
                let all_tabs = screen.get_tabs_mut();
                for tab in all_tabs.values_mut() {
                    tab.clear_all_plugin_highlights(plugin_id);
                }
                screen.render(None)?;
            },
            ScreenInstruction::DesktopNotificationResponse(raw_bytes, client_id) => {
                if let Some((terminal_id, app_wants_report, is_query, rewritten_bytes)) =
                    denormalize_notification_response(&raw_bytes)
                {
                    let pane_id = PaneId::Terminal(terminal_id);
                    // Write response to the pane if the app expects it:
                    // capability query answers (q flag) or activation reports (r flag)
                    if app_wants_report || is_query {
                        let all_tabs = screen.get_tabs_mut();
                        for tab in all_tabs.values_mut() {
                            if tab.has_pane_with_pid(&pane_id) {
                                tab.write_to_pane_id(
                                    &None,
                                    rewritten_bytes,
                                    false,
                                    pane_id,
                                    None,
                                    None,
                                )
                                .non_fatal();
                                break;
                            }
                        }
                    }
                    // Focus the pane on activation click (not on query responses)
                    if !is_query {
                        screen
                            .focus_pane_with_id(pane_id, false, false, client_id)
                            .non_fatal();
                    }
                    screen.render(None)?;
                    screen.log_and_report_session_state()?;
                }
            },
            ScreenInstruction::SubscribeToPaneRenders {
                client_id,
                pane_ids,
                scrollback,
                ansi,
            } => {
                screen.subscribe_to_pane_renders(client_id, pane_ids, scrollback, ansi);
            },
            ScreenInstruction::NotifyPaneClosedToSubscribers { pane_id } => {
                screen.notify_pane_closed_to_subscribers(pane_id);
            },
            ScreenInstruction::PluginSubscribedToAnsiPaneContents(has_subscribers) => {
                screen.plugins_need_ansi_pane_contents = has_subscribers;
            },
            ScreenInstruction::UpdateBackgroundPluginSubscriptions(
                plugin_id,
                client_id,
                subscriptions,
            ) => {
                if subscriptions.is_empty() {
                    screen
                        .background_plugin_subscriptions
                        .remove(&(plugin_id, client_id));
                } else {
                    screen
                        .background_plugin_subscriptions
                        .insert((plugin_id, client_id), subscriptions);
                }
            },
            ScreenInstruction::BroadcastModeUpdate(mode_info, target_client_id) => {
                screen.broadcast_mode_update(mode_info, target_client_id)?;
            },
            // Pane-targeting CLI handlers
            ScreenInstruction::ScrollUpWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.scroll_up_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollDownWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.scroll_down_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToTopWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.scroll_to_top_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ScrollToBottomWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.scroll_to_bottom_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollUpWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.page_scroll_up_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::PageScrollDownWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.page_scroll_down_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::HalfPageScrollUpWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.half_page_scroll_up_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::HalfPageScrollDownWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.half_page_scroll_down_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ResizeWithPaneId(pane_id, strategy, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.resize_by_pane_id(pane_id, strategy);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneWithPaneIdCli(pane_id, direction, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.move_pane_by_pane_id(pane_id, direction);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MovePaneBackwardsWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.move_pane_backwards_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::ClearScreenWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.clear_screen_by_pane_id(pane_id).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::EditScrollbackWithPaneId(pane_id, ansi, completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        if ansi {
                            tab.edit_scrollback_raw_for_pane_with_id(pane_id, completion_tx)
                                .non_fatal();
                        } else {
                            tab.edit_scrollback_for_pane_with_id(pane_id, completion_tx)
                                .non_fatal();
                        }
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                }
                screen.render(None)?;
            },
            ScreenInstruction::ToggleFullscreenWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.toggle_fullscreen_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::TogglePaneEmbedOrFloatingWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.toggle_pane_embed_or_floating_for_pane_id(pane_id, None)
                            .non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::CloseFocusWithPaneId(pane_id, completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.close_pane_by_pane_id(pane_id, completion_tx)
                            .non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::RenamePaneWithPaneId(pane_id, name, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.rename_pane_by_pane_id(pane_id, name).non_fatal();
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::UndoRenamePaneWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.undo_rename_pane_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::TogglePanePinnedWithPaneId(pane_id, mut _completion_tx) => {
                let all_tabs = screen.get_tabs_mut();
                let mut found = false;
                for tab in all_tabs.values_mut() {
                    if tab.has_pane_with_pid(&pane_id) {
                        tab.toggle_pane_pinned_by_pane_id(pane_id);
                        found = true;
                        break;
                    }
                }
                if !found {
                    log::error!("Pane with id {:?} not found", pane_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Pane with id {:?} not found", pane_id));
                    }
                }
            },
            // Tab-targeting CLI handlers
            ScreenInstruction::UndoRenameTabWithTabId(tab_id, mut _completion_tx) => {
                if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                    if tab.name != tab.prev_name {
                        tab.name = tab.prev_name.clone();
                        screen.log_and_report_session_state()?;
                    }
                } else {
                    log::error!("Tab with id {} not found", tab_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Tab with id {} not found", tab_id));
                    }
                }
                screen.render(None)?;
            },
            ScreenInstruction::ToggleActiveSyncTabWithTabId(tab_id, mut _completion_tx) => {
                if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                    tab.toggle_sync_panes_is_active();
                } else {
                    log::error!("Tab with id {} not found", tab_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Tab with id {} not found", tab_id));
                    }
                }
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::ToggleFloatingPanesWithTabId(
                tab_id,
                default_shell,
                mut completion_tx,
            ) => {
                if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                    // Pass None as client_id so that if a new floating pane must be spawned,
                    // it targets this tab (via TabIndex) rather than the focused tab of some client.
                    tab.toggle_floating_panes(None, default_shell, completion_tx)
                        .non_fatal();
                } else {
                    log::error!("Tab with id {} not found", tab_id);
                    if let Some(ref mut c) = completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Tab with id {} not found", tab_id));
                    }
                    drop(completion_tx);
                }
                screen.log_and_report_session_state()?;
                screen.render(None)?;
            },
            ScreenInstruction::PreviousSwapLayoutWithTabId(tab_id, mut _completion_tx) => {
                if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                    tab.previous_swap_layout().non_fatal();
                } else {
                    log::error!("Tab with id {} not found", tab_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Tab with id {} not found", tab_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::NextSwapLayoutWithTabId(tab_id, mut _completion_tx) => {
                if let Some(tab) = screen.tabs.get_mut(&tab_id) {
                    tab.next_swap_layout().non_fatal();
                } else {
                    log::error!("Tab with id {} not found", tab_id);
                    if let Some(ref mut c) = _completion_tx {
                        c.set_exit_status(1);
                        c.set_error_message(format!("Tab with id {} not found", tab_id));
                    }
                }
                screen.render(None)?;
                screen.log_and_report_session_state()?;
            },
            ScreenInstruction::MoveTabWithTabId(tab_id, direction, _completion_tx) => {
                if pending_tab_ids.is_empty() {
                    screen.move_tab_by_id(tab_id, direction)?;
                    screen.render(None)?;
                } else {
                    pending_events_waiting_for_tab.push(ScreenInstruction::MoveTabWithTabId(
                        tab_id,
                        direction,
                        _completion_tx,
                    ));
                }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod dump_screen_error_tests {
    use super::dump_screen_error_message;
    use crate::route::NotificationEnd;
    use anyhow::Context;
    use tokio::sync::oneshot;

    #[test]
    fn dump_screen_write_error_preserves_source_chain_in_completion_ack() {
        let source = std::io::Error::other("destination is a directory");
        let error = Err::<(), _>(source)
            .context("failed to write to file")
            .context("failed to dump pane Terminal(1) in tab 1")
            .unwrap_err();
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.set_exit_status(1);
        completion.set_error_message(dump_screen_error_message(&error));
        drop(completion);

        let result = rx.blocking_recv().unwrap();
        let message = result.error_message.unwrap();
        assert!(
            message.contains("failed to dump pane Terminal(1) in tab 1"),
            "{message}"
        );
        assert!(message.contains("failed to write to file"), "{message}");
        assert!(message.contains("destination is a directory"), "{message}");
    }
}

#[path = "./unit/screen_tests.rs"]
#[cfg(test)]
mod screen_tests;
