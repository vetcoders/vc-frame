use super::{
    LayoutPluginReceipt, LayoutPluginResolution, PinnedExecutor, PluginId, PluginInstruction,
};
use crate::global_async_runtime::get_tokio_runtime;
use crate::plugins::pipes::{
    PendingPipes, PipeStateChange, apply_pipe_message_to_plugin, pipes_to_block_or_unblock,
};
use crate::plugins::plugin_loader::PluginLoader;
use crate::plugins::plugin_map::{AtomicEvent, PluginEnv, PluginMap, RunningPlugin};

use crate::plugins::plugin_worker::MessageToWorker;
use crate::plugins::watch_filesystem::watch_filesystem;
use crate::plugins::zellij_exports::{wasi_read_string, wasi_write_object};
use highway::{HighwayHash, PortableHash};
use log::info;
use notify_debouncer_full::{Debouncer, FileIdMap, notify::RecommendedWatcher};
#[cfg(test)]
use std::sync::{Barrier, atomic::AtomicUsize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::mpsc::Sender;
use tokio_util::sync::CancellationToken;
use url::Url;
use wasmi::{Engine, Module};
use zellij_utils::consts::{ZELLIJ_CACHE_DIR, ZELLIJ_SESSION_CACHE_DIR, ZELLIJ_TMP_DIR};
use zellij_utils::data::{
    FloatingPaneCoordinates, InputMode, LayoutInfo, LayoutWithError, PaneContents,
    PaneRenderReport, PermissionStatus, PermissionType, PipeMessage, PipeSource,
};
use zellij_utils::downloader::Downloader;
use zellij_utils::input::keybinds::Keybinds;
use zellij_utils::input::permission::PermissionCache;
use zellij_utils::plugin_api::event::ProtobufEvent;

use prost::Message;

use crate::panes::PaneId;
use crate::plugins::plugin_map::RunningPluginAndSubscriptions;
use crate::{
    ClientId, ServerInstruction, background_jobs::BackgroundJob, pty::LayoutTransactionId,
    route::NotificationEnd, screen::ScreenInstruction, thread_bus::ThreadSenders,
    ui::loading_indication::LoadingIndication,
};
use zellij_utils::{
    data::{Event, EventType},
    errors::prelude::*,
    input::{
        command::TerminalAction,
        layout::{PluginUserConfiguration, RunPlugin, RunPluginLocation, RunPluginOrAlias},
        plugins::{PluginAliases, PluginConfig},
    },
    pane_size::Size,
};

/// On Windows, colons in URL strings (e.g. `zellij:tab-bar`, `file:///...`)
/// are illegal in path components. Replace them with underscores.
#[cfg(windows)]
fn make_plugin_url_path_safe(url: String) -> String {
    url.replace(':', "_")
}

#[cfg(not(windows))]
fn make_plugin_url_path_safe(url: String) -> String {
    url
}

#[derive(Debug, Clone)]
pub enum EventOrPipeMessage {
    Event(Box<Event>),
    PipeMessage(PipeMessage),
}

#[derive(Debug, Clone, Default)]
pub struct PluginRenderAsset {
    // TODO: naming
    pub client_id: ClientId,
    pub plugin_id: PluginId,
    pub bytes: Vec<u8>,
    pub cli_pipes: HashMap<String, PipeStateChange>,
}

impl PluginRenderAsset {
    pub fn new(plugin_id: PluginId, client_id: ClientId, bytes: Vec<u8>) -> Self {
        PluginRenderAsset {
            client_id,
            plugin_id,
            bytes,
            ..Default::default()
        }
    }
    pub fn with_pipes(mut self, cli_pipes: HashMap<String, PipeStateChange>) -> Self {
        self.cli_pipes = cli_pipes;
        self
    }
}

#[derive(Debug, Clone)]
pub struct LoadingContext {
    pub plugin_id: PluginId,
    pub client_id: ClientId,
    pub plugin_cwd: PathBuf,
    pub plugin_own_data_dir: PathBuf,
    pub plugin_own_cache_dir: PathBuf,
    pub plugin_config: PluginConfig,
    pub tab_index: Option<usize>,
    pub path_to_default_shell: PathBuf,
    pub session_env_vars: std::collections::BTreeMap<String, String>,
    pub default_shell: Option<TerminalAction>,
    pub layout_dir: Option<PathBuf>,
    pub default_mode: InputMode,
    pub keybinds: Keybinds,
    pub plugin_dir: PathBuf,
    pub size: Size,
}

impl LoadingContext {
    pub fn new(
        wasm_bridge: &WasmBridge,
        cwd: Option<PathBuf>,
        plugin_config: PluginConfig,
        plugin_id: PluginId,
        client_id: ClientId,
        tab_index: Option<usize>,
        size: Size,
    ) -> Self {
        let plugin_own_data_dir = ZELLIJ_SESSION_CACHE_DIR
            .join(make_plugin_url_path_safe(
                Url::from(&plugin_config.location).to_string(),
            ))
            .join(format!("{}-{}", plugin_id, client_id));
        let plugin_own_cache_dir = ZELLIJ_CACHE_DIR
            .join(make_plugin_url_path_safe(
                Url::from(&plugin_config.location).to_string(),
            ))
            .join("plugin_cache");
        let default_mode = wasm_bridge
            .base_modes
            .get(&client_id)
            .copied()
            .unwrap_or(wasm_bridge.default_mode);
        let keybinds = wasm_bridge
            .keybinds
            .get(&client_id)
            .cloned()
            .unwrap_or_else(|| wasm_bridge.default_keybinds.clone());

        LoadingContext {
            client_id,
            plugin_id,
            path_to_default_shell: wasm_bridge.path_to_default_shell.clone(),
            session_env_vars: wasm_bridge.session_env_vars.clone(),
            plugin_cwd: cwd.unwrap_or_else(|| wasm_bridge.zellij_cwd.clone()),
            default_shell: wasm_bridge.default_shell.clone(),
            layout_dir: wasm_bridge.layout_dir.clone(),
            keybinds,
            default_mode,
            plugin_own_data_dir,
            plugin_own_cache_dir,
            plugin_config,
            tab_index,
            plugin_dir: wasm_bridge.plugin_dir.clone(),
            size,
        }
    }
    pub fn update_plugin_path(&mut self, new_path: PathBuf) {
        self.plugin_config.path = new_path;
    }
}

pub type PluginCache = Arc<Mutex<HashMap<PathBuf, Module>>>;

const PLUGIN_EVENT_DIAGNOSTIC_SLOTS: usize = 256;
const PLUGIN_EVENT_DIAGNOSTIC_WINDOW_MS: u64 = 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PluginEventDiagnosticKind {
    SessionUpdate,
    PaneUpdate,
    PaneClosed,
    TabUpdate,
    Other,
}

impl PluginEventDiagnosticKind {
    fn from_event(event: &Event) -> Self {
        match event {
            Event::SessionUpdate(..) => Self::SessionUpdate,
            Event::PaneUpdate(..) => Self::PaneUpdate,
            Event::PaneClosed(..) => Self::PaneClosed,
            Event::TabUpdate(..) => Self::TabUpdate,
            _ => Self::Other,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SessionUpdate => "SessionUpdate",
            Self::PaneUpdate => "PaneUpdate",
            Self::PaneClosed => "PaneClosed",
            Self::TabUpdate => "TabUpdate",
            Self::Other => "Other",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::SessionUpdate => 0,
            Self::PaneUpdate => 1,
            Self::PaneClosed => 2,
            Self::TabUpdate => 3,
            Self::Other => 4,
        }
    }
}

#[derive(Default)]
struct PluginEventDiagnosticSlot {
    // Zero is the empty sentinel; stored identities are offset by one.
    owner: AtomicU64,
    window_started_ms: AtomicU64,
    dispatched: [AtomicU64; 5],
    rendered: [AtomicU64; 5],
    empty_rendered: [AtomicU64; 5],
}

struct PluginEventDiagnostics {
    slots: [PluginEventDiagnosticSlot; PLUGIN_EVENT_DIAGNOSTIC_SLOTS],
}

impl Default for PluginEventDiagnostics {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| PluginEventDiagnosticSlot::default()),
        }
    }
}

impl PluginEventDiagnostics {
    fn owner(plugin_id: PluginId, client_id: ClientId) -> u64 {
        (((plugin_id as u64) << 32) | client_id as u64).wrapping_add(1)
    }

    fn slot_index(owner: u64) -> usize {
        (owner ^ (owner >> 32)) as usize % PLUGIN_EVENT_DIAGNOSTIC_SLOTS
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    fn record(
        &self,
        plugin_id: PluginId,
        client_id: ClientId,
        event: &Event,
        rendered: bool,
        empty_rendered: bool,
    ) {
        if !log::log_enabled!(target: "vc_frame::plugin_event_rate", log::Level::Debug) {
            return;
        }
        self.record_at(
            plugin_id,
            client_id,
            PluginEventDiagnosticKind::from_event(event),
            rendered,
            empty_rendered,
            Self::now_ms(),
        );
    }

    fn record_at(
        &self,
        plugin_id: PluginId,
        client_id: ClientId,
        kind: PluginEventDiagnosticKind,
        rendered: bool,
        empty_rendered: bool,
        now_ms: u64,
    ) {
        let owner = Self::owner(plugin_id, client_id);
        let slot = &self.slots[Self::slot_index(owner)];
        let observed_owner = slot.owner.load(Ordering::Relaxed);
        if observed_owner == 0 {
            match slot
                .owner
                .compare_exchange(0, owner, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => slot.window_started_ms.store(now_ms, Ordering::Relaxed),
                Err(existing_owner) if existing_owner != owner => return,
                Err(_) => {},
            }
        } else if observed_owner != owner {
            // A fixed-size diagnostic table deliberately drops collisions instead
            // of allocating or locking in the plugin event hot path.
            return;
        }

        let index = kind.index();
        slot.dispatched[index].fetch_add(1, Ordering::Relaxed);
        if rendered {
            slot.rendered[index].fetch_add(1, Ordering::Relaxed);
        }
        if empty_rendered {
            slot.empty_rendered[index].fetch_add(1, Ordering::Relaxed);
        }

        let window_started_ms = slot.window_started_ms.load(Ordering::Relaxed);
        let elapsed_ms = now_ms.saturating_sub(window_started_ms);
        if elapsed_ms < PLUGIN_EVENT_DIAGNOSTIC_WINDOW_MS
            || slot
                .window_started_ms
                .compare_exchange(
                    window_started_ms,
                    now_ms,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_err()
        {
            return;
        }

        for event_kind in [
            PluginEventDiagnosticKind::SessionUpdate,
            PluginEventDiagnosticKind::PaneUpdate,
            PluginEventDiagnosticKind::PaneClosed,
            PluginEventDiagnosticKind::TabUpdate,
            PluginEventDiagnosticKind::Other,
        ] {
            let event_index = event_kind.index();
            let dispatched = slot.dispatched[event_index].swap(0, Ordering::Relaxed);
            let rendered = slot.rendered[event_index].swap(0, Ordering::Relaxed);
            let empty_rendered = slot.empty_rendered[event_index].swap(0, Ordering::Relaxed);
            if dispatched > 0 {
                log::debug!(
                    target: "vc_frame::plugin_event_rate",
                    "plugin_id={plugin_id} client_id={client_id} event={} dispatched={} rendered={} empty_rendered={} window_ms={elapsed_ms}",
                    event_kind.as_str(),
                    dispatched,
                    rendered,
                    empty_rendered,
                );
            }
        }
    }

    fn flush_plugin(&self, plugin_id: PluginId) {
        if !log::log_enabled!(target: "vc_frame::plugin_event_rate", log::Level::Debug) {
            return;
        }
        let now_ms = Self::now_ms();
        for slot in &self.slots {
            let encoded_owner = slot.owner.load(Ordering::Relaxed);
            if encoded_owner == 0
                || ((encoded_owner.wrapping_sub(1) >> 32) as PluginId) != plugin_id
            {
                continue;
            }
            let client_id = encoded_owner.wrapping_sub(1) as ClientId;
            let elapsed_ms = now_ms.saturating_sub(slot.window_started_ms.load(Ordering::Relaxed));
            for event_kind in [
                PluginEventDiagnosticKind::SessionUpdate,
                PluginEventDiagnosticKind::PaneUpdate,
                PluginEventDiagnosticKind::PaneClosed,
                PluginEventDiagnosticKind::TabUpdate,
                PluginEventDiagnosticKind::Other,
            ] {
                let event_index = event_kind.index();
                let dispatched = slot.dispatched[event_index].swap(0, Ordering::Relaxed);
                let rendered = slot.rendered[event_index].swap(0, Ordering::Relaxed);
                let empty_rendered = slot.empty_rendered[event_index].swap(0, Ordering::Relaxed);
                if dispatched > 0 {
                    log::debug!(
                        target: "vc_frame::plugin_event_rate",
                        "plugin_id={plugin_id} client_id={client_id} event={} dispatched={} rendered={} empty_rendered={} window_ms={elapsed_ms} final=true",
                        event_kind.as_str(),
                        dispatched,
                        rendered,
                        empty_rendered,
                    );
                }
            }
            slot.window_started_ms.store(0, Ordering::Relaxed);
            slot.owner.store(0, Ordering::Relaxed);
        }
    }

    #[cfg(test)]
    fn counts(
        &self,
        plugin_id: PluginId,
        client_id: ClientId,
        kind: PluginEventDiagnosticKind,
    ) -> (u64, u64, u64) {
        let owner = Self::owner(plugin_id, client_id);
        let slot = &self.slots[Self::slot_index(owner)];
        if slot.owner.load(Ordering::Relaxed) != owner {
            return (0, 0, 0);
        }
        let index = kind.index();
        (
            slot.dispatched[index].load(Ordering::Relaxed),
            slot.rendered[index].load(Ordering::Relaxed),
            slot.empty_rendered[index].load(Ordering::Relaxed),
        )
    }
}

const MAX_LAYOUT_PLUGIN_RECEIPT_TRANSACTIONS: usize = 512;
const MAX_LAYOUT_PLUGIN_CLEANUP_RECEIPTS: usize = 512;
const LAYOUT_PLUGIN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum LayoutPluginResolutionKind {
    Activate,
    Release,
    Compensate,
}

impl LayoutPluginResolution {
    fn kind(&self) -> LayoutPluginResolutionKind {
        match self {
            Self::Activate => LayoutPluginResolutionKind::Activate,
            Self::Release { .. } => LayoutPluginResolutionKind::Release,
            Self::Compensate { .. } => LayoutPluginResolutionKind::Compensate,
        }
    }
}

#[derive(Clone)]
pub(super) struct LayoutPluginReservationRequest {
    pub run_plugin: RunPlugin,
    pub tab_index: Option<usize>,
    pub size: Size,
    pub cwd: Option<PathBuf>,
    pub skip_cache: bool,
    pub client_id: ClientId,
}

#[derive(Clone)]
struct ReservedLayoutPlugin {
    plugin_id: PluginId,
    run_plugin: RunPlugin,
    plugin_config: PluginConfig,
    tab_index: Option<usize>,
    size: Size,
    cwd: Option<PathBuf>,
    skip_cache: bool,
    client_id: ClientId,
    cancellation: CancellationToken,
    activation_tracker: Arc<LayoutPluginActivationTracker>,
}

struct LayoutPluginActivationJob {
    plugin_executor: Arc<PinnedExecutor>,
    senders: ThreadSenders,
    plugin_map_for_cleanup: Arc<Mutex<PluginMap>>,
    transaction_id: LayoutTransactionId,
    plugin: ReservedLayoutPlugin,
    loading_context: LoadingContext,
    group_plugin_ids: Vec<PluginId>,
    cancellation: CancellationToken,
    activation_gate: Arc<LayoutPluginActivationGate>,
    activation_guards: LayoutPluginActivationGuards,
    #[cfg(test)]
    test_hooks: LayoutPluginTestHooks,
}

#[cfg(test)]
#[derive(Clone, Default)]
struct LayoutPluginTestHooks {
    load_starts: Arc<AtomicUsize>,
    before_load_gate: Option<Arc<LayoutPluginLoadTestGate>>,
}

#[cfg(test)]
struct LayoutPluginLoadTestGate {
    entered: Barrier,
    release: Barrier,
}

#[cfg(test)]
impl LayoutPluginLoadTestGate {
    fn new() -> Self {
        Self {
            entered: Barrier::new(2),
            release: Barrier::new(2),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayoutPluginTransactionState {
    Reserved,
    Activated,
    ActivationFailed,
}

#[derive(Default)]
struct LayoutPluginActivationTracker {
    active: Mutex<usize>,
    idle: Condvar,
}

impl LayoutPluginActivationTracker {
    fn begin(self: &Arc<Self>) -> LayoutPluginActivationGuard {
        *self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) += 1;
        LayoutPluginActivationGuard {
            tracker: self.clone(),
        }
    }

    fn wait_for_idle(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while *active != 0 {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (next_active, result) = self
                .idle
                .wait_timeout(active, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poison| poison.into_inner());
            active = next_active;
            if result.timed_out() && *active != 0 {
                return false;
            }
        }
        true
    }

    fn is_idle(&self) -> bool {
        *self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            == 0
    }

    #[cfg(test)]
    fn active_count(&self) -> usize {
        *self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

struct LayoutPluginActivationGuard {
    tracker: Arc<LayoutPluginActivationTracker>,
}

impl Drop for LayoutPluginActivationGuard {
    fn drop(&mut self) {
        let mut active = self
            .tracker
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *active = active.saturating_sub(1);
        if *active == 0 {
            self.tracker.idle.notify_all();
        }
    }
}

struct LayoutPluginActivationGuards {
    _plugin: LayoutPluginActivationGuard,
    _group: LayoutPluginActivationGuard,
}

impl LayoutPluginActivationGuards {
    fn begin(
        plugin_tracker: &Arc<LayoutPluginActivationTracker>,
        group_tracker: &Arc<LayoutPluginActivationTracker>,
    ) -> Self {
        Self {
            _plugin: plugin_tracker.begin(),
            _group: group_tracker.begin(),
        }
    }
}

#[derive(Default)]
struct LayoutPluginActivationGate {
    open: Mutex<bool>,
    opened: Condvar,
}

impl LayoutPluginActivationGate {
    fn wait(&self) {
        let mut open = self
            .open
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        while !*open {
            open = self
                .opened
                .wait(open)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    fn open(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = true;
        self.opened.notify_all();
    }
}

struct LayoutPluginReservation {
    plugins: Vec<ReservedLayoutPlugin>,
    state: LayoutPluginTransactionState,
    cancellation: CancellationToken,
    tracker: Arc<LayoutPluginActivationTracker>,
    activation_gate: Arc<LayoutPluginActivationGate>,
}

struct LayoutPluginCleanupDebt {
    requested_plugin_ids: Vec<PluginId>,
    remaining_plugin_ids: BTreeSet<PluginId>,
}

/// Ring-buffer cap for events cached on behalf of a plugin that has not
/// finished loading. Oldest entries are dropped first: a stuck or
/// crash-looping load must not retain unbounded broadcast history.
const MAX_CACHED_EVENTS_PER_PENDING_PLUGIN: usize = 4096;

pub struct WasmBridge {
    connected_clients: Arc<Mutex<Vec<ClientId>>>,
    senders: ThreadSenders,
    plugin_dir: PathBuf,
    plugin_map: Arc<Mutex<PluginMap>>,
    plugin_executor: Arc<PinnedExecutor>,
    event_diagnostics: Arc<PluginEventDiagnostics>,
    next_plugin_id: PluginId,
    plugin_ids_waiting_for_permission_request: HashSet<PluginId>,
    // Exact plugin/client targets parked by Screen's chrome lifecycle. Heavy
    // state payloads never cross into these WASM instances while hidden.
    parked_chrome_plugin_clients: HashSet<(PluginId, ClientId)>,
    cached_events_for_pending_plugins: HashMap<PluginId, Vec<EventOrPipeMessage>>,
    cached_resizes_for_pending_plugins: HashMap<PluginId, (usize, usize)>, // (rows, columns)
    cached_worker_messages: HashMap<PluginId, Vec<(ClientId, String, String, String)>>, // Vec<clientid,
    // worker_name,
    // message,
    // payload>
    loading_plugins: HashSet<(PluginId, RunPlugin)>, // tracks loading plugins without handles
    pending_plugin_reloads: HashSet<RunPlugin>,
    path_to_default_shell: PathBuf,
    watcher: Option<Debouncer<RecommendedWatcher, FileIdMap>>,
    zellij_cwd: PathBuf,
    session_env_vars: std::collections::BTreeMap<String, String>,
    default_shell: Option<TerminalAction>,
    cached_plugin_map:
        HashMap<RunPluginLocation, HashMap<PluginUserConfiguration, Vec<(PluginId, ClientId)>>>,
    pending_pipes: PendingPipes,
    layout_dir: Option<PathBuf>,
    available_layouts: Vec<LayoutInfo>,
    available_layout_errors: Vec<LayoutWithError>,
    default_mode: InputMode,
    default_keybinds: Keybinds,
    keybinds: HashMap<ClientId, Keybinds>,
    base_modes: HashMap<ClientId, InputMode>,
    downloader: Downloader,
    previous_pane_render_report: Option<PaneRenderReport>,
    pub last_session_save_time: Arc<Mutex<Option<u64>>>, // milliseconds since UNIX epoch
    layout_plugin_reservations: HashMap<LayoutTransactionId, LayoutPluginReservation>,
    layout_plugin_owners: HashMap<PluginId, LayoutTransactionId>,
    layout_plugin_receipts:
        BTreeMap<(LayoutTransactionId, LayoutPluginResolutionKind), LayoutPluginReceipt>,
    layout_plugin_cleanup_debts: HashMap<LayoutTransactionId, LayoutPluginCleanupDebt>,
    layout_plugin_cleanup_receipts: BTreeMap<LayoutTransactionId, Vec<PluginId>>,
    plugin_unload_debts: HashMap<PluginId, u32>,
    #[cfg(test)]
    layout_plugin_test_hooks: LayoutPluginTestHooks,
    #[cfg(test)]
    rejected_layout_plugin_releases: HashSet<LayoutTransactionId>,
}

impl WasmBridge {
    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    pub fn new(
        senders: ThreadSenders,
        engine: Engine,
        plugin_dir: PathBuf,
        path_to_default_shell: PathBuf,
        zellij_cwd: PathBuf,
        session_env_vars: std::collections::BTreeMap<String, String>,
        default_shell: Option<TerminalAction>,
        layout_dir: Option<PathBuf>,
        available_layouts: Vec<LayoutInfo>,
        available_layout_errors: Vec<LayoutWithError>,
        default_mode: InputMode,
        default_keybinds: Keybinds,
    ) -> Self {
        let plugin_map = Arc::new(Mutex::new(PluginMap::default()));
        let connected_clients: Arc<Mutex<Vec<ClientId>>> = Arc::new(Mutex::new(vec![]));
        let plugin_cache: Arc<Mutex<HashMap<PathBuf, Module>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let watcher = None;
        let downloader = Downloader::new(ZELLIJ_CACHE_DIR.to_path_buf());
        let max_threads = num_cpus::get().clamp(4, 16);
        let plugin_executor = Arc::new(PinnedExecutor::new(
            max_threads,
            &senders,
            &plugin_map,
            &connected_clients,
            &plugin_cache,
            &engine,
        ));
        WasmBridge {
            connected_clients,
            senders,
            plugin_dir,
            plugin_map,
            plugin_executor,
            event_diagnostics: Arc::new(PluginEventDiagnostics::default()),
            path_to_default_shell,
            watcher,
            next_plugin_id: 0,
            cached_events_for_pending_plugins: HashMap::new(),
            plugin_ids_waiting_for_permission_request: HashSet::new(),
            parked_chrome_plugin_clients: HashSet::new(),
            cached_resizes_for_pending_plugins: HashMap::new(),
            cached_worker_messages: HashMap::new(),
            loading_plugins: HashSet::new(),
            pending_plugin_reloads: HashSet::new(),
            zellij_cwd,
            session_env_vars,
            default_shell,
            cached_plugin_map: HashMap::new(),
            pending_pipes: Default::default(),
            layout_dir,
            available_layouts,
            available_layout_errors,
            default_mode,
            default_keybinds,
            keybinds: HashMap::new(),
            base_modes: HashMap::new(),
            downloader,
            previous_pane_render_report: None,
            last_session_save_time: Arc::new(Mutex::new(None)),
            layout_plugin_reservations: HashMap::new(),
            layout_plugin_owners: HashMap::new(),
            layout_plugin_receipts: BTreeMap::new(),
            layout_plugin_cleanup_debts: HashMap::new(),
            layout_plugin_cleanup_receipts: BTreeMap::new(),
            plugin_unload_debts: HashMap::new(),
            #[cfg(test)]
            layout_plugin_test_hooks: LayoutPluginTestHooks::default(),
            #[cfg(test)]
            rejected_layout_plugin_releases: HashSet::new(),
        }
    }

    pub fn reserve_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
        requests: Vec<LayoutPluginReservationRequest>,
    ) -> std::result::Result<Vec<PluginId>, String> {
        if self
            .layout_plugin_reservations
            .contains_key(&transaction_id)
            || self
                .layout_plugin_receipts
                .keys()
                .any(|(receipt_transaction_id, _)| receipt_transaction_id == &transaction_id)
            || self
                .layout_plugin_cleanup_debts
                .contains_key(&transaction_id)
            || self
                .layout_plugin_cleanup_receipts
                .contains_key(&transaction_id)
        {
            return Err(format!(
                "layout plugin transaction {transaction_id} is already reserved or resolved"
            ));
        }

        let mut plugins = Vec::with_capacity(requests.len());
        for (offset, request) in requests.into_iter().enumerate() {
            let plugin_config =
                PluginConfig::from_run_plugin(&request.run_plugin).ok_or_else(|| {
                    format!(
                        "failed to resolve layout plugin {} for transaction {transaction_id}",
                        request.run_plugin.location
                    )
                })?;
            let offset = u32::try_from(offset)
                .map_err(|_| format!("too many layout plugins in transaction {transaction_id}"))?;
            let plugin_id = self.next_plugin_id.checked_add(offset).ok_or_else(|| {
                format!("plugin id space exhausted for layout transaction {transaction_id}")
            })?;
            plugins.push(ReservedLayoutPlugin {
                plugin_id,
                run_plugin: request.run_plugin,
                plugin_config,
                tab_index: request.tab_index,
                size: request.size,
                cwd: request.cwd,
                skip_cache: request.skip_cache,
                client_id: request.client_id,
                cancellation: CancellationToken::new(),
                activation_tracker: Arc::new(LayoutPluginActivationTracker::default()),
            });
        }

        self.next_plugin_id =
            self.next_plugin_id
                .checked_add(u32::try_from(plugins.len()).map_err(|_| {
                    format!("too many layout plugins in transaction {transaction_id}")
                })?)
                .ok_or_else(|| {
                    format!("plugin id space exhausted for layout transaction {transaction_id}")
                })?;
        let plugin_ids = plugins
            .iter()
            .map(|plugin| plugin.plugin_id)
            .collect::<Vec<_>>();
        for plugin_id in &plugin_ids {
            self.layout_plugin_owners.insert(*plugin_id, transaction_id);
        }
        self.layout_plugin_reservations.insert(
            transaction_id,
            LayoutPluginReservation {
                plugins,
                state: LayoutPluginTransactionState::Reserved,
                cancellation: CancellationToken::new(),
                tracker: Arc::new(LayoutPluginActivationTracker::default()),
                activation_gate: Arc::new(LayoutPluginActivationGate::default()),
            },
        );
        Ok(plugin_ids)
    }

    pub fn resolve_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
        resolution: LayoutPluginResolution,
        expected_plugin_ids: Vec<PluginId>,
    ) -> std::result::Result<LayoutPluginReceipt, String> {
        let kind = resolution.kind();
        let mut expected_plugin_ids = expected_plugin_ids;
        expected_plugin_ids.sort_unstable();

        #[cfg(test)]
        if kind == LayoutPluginResolutionKind::Release
            && self.rejected_layout_plugin_releases.remove(&transaction_id)
        {
            return Err(format!(
                "injected layout plugin release failure for transaction {transaction_id}"
            ));
        }

        if let Some(receipt) = self
            .layout_plugin_receipts
            .get(&(transaction_id, kind))
            .cloned()
        {
            let mut receipt_plugin_ids = layout_plugin_receipt_ids(&receipt).to_vec();
            receipt_plugin_ids.sort_unstable();
            if receipt_plugin_ids == expected_plugin_ids {
                return Ok(receipt);
            }
            return Err(format!(
                "layout plugin transaction {transaction_id} replay conflict for {kind:?}: expected ids {expected_plugin_ids:?}, recorded ids {receipt_plugin_ids:?}"
            ));
        }

        let Some(reservation) = self.layout_plugin_reservations.get(&transaction_id) else {
            let compensates_retired_empty_activation = kind
                == LayoutPluginResolutionKind::Compensate
                && expected_plugin_ids.is_empty()
                && self
                    .layout_plugin_receipts
                    .get(&(transaction_id, LayoutPluginResolutionKind::Activate))
                    .is_some_and(|receipt| layout_plugin_receipt_ids(receipt).is_empty());
            if compensates_retired_empty_activation {
                let receipt = LayoutPluginReceipt::Compensated { plugin_ids: vec![] };
                self.record_layout_plugin_receipt(transaction_id, kind, receipt.clone());
                return Ok(receipt);
            }
            let resolved_kinds = self
                .layout_plugin_receipts
                .keys()
                .filter_map(|(receipt_transaction_id, receipt_kind)| {
                    (receipt_transaction_id == &transaction_id).then_some(*receipt_kind)
                })
                .collect::<Vec<_>>();
            return if resolved_kinds.is_empty() {
                Err(format!(
                    "unknown layout plugin transaction {transaction_id}"
                ))
            } else {
                Err(format!(
                    "layout plugin transaction {transaction_id} resolution conflict: already resolved as {resolved_kinds:?}, cannot resolve as {kind:?}"
                ))
            };
        };
        let mut reserved_plugin_ids = reservation
            .plugins
            .iter()
            .map(|plugin| plugin.plugin_id)
            .collect::<Vec<_>>();
        reserved_plugin_ids.sort_unstable();
        if reserved_plugin_ids != expected_plugin_ids {
            return Err(format!(
                "layout plugin transaction {transaction_id} id conflict: expected {expected_plugin_ids:?}, reserved {reserved_plugin_ids:?}"
            ));
        }

        let receipt = match resolution {
            LayoutPluginResolution::Activate => match reservation.state {
                LayoutPluginTransactionState::Reserved => {
                    self.activate_layout_plugins(transaction_id)?
                },
                LayoutPluginTransactionState::Activated => LayoutPluginReceipt::Activated {
                    plugin_ids: reserved_plugin_ids.clone(),
                },
                LayoutPluginTransactionState::ActivationFailed => {
                    return Err(format!(
                        "layout plugin transaction {transaction_id} cannot Activate from {:?}",
                        reservation.state
                    ));
                },
            },
            LayoutPluginResolution::Release { reason } => {
                if !matches!(
                    reservation.state,
                    LayoutPluginTransactionState::Reserved
                        | LayoutPluginTransactionState::ActivationFailed
                ) {
                    return Err(format!(
                        "layout plugin transaction {transaction_id} cannot Release from {:?}: {reason}",
                        reservation.state
                    ));
                }
                self.release_reserved_layout_plugins(transaction_id, reason)?
            },
            LayoutPluginResolution::Compensate { reason } => {
                if !matches!(
                    reservation.state,
                    LayoutPluginTransactionState::Activated
                        | LayoutPluginTransactionState::ActivationFailed
                ) {
                    return Err(format!(
                        "layout plugin transaction {transaction_id} cannot Compensate from {:?}: {reason}",
                        reservation.state
                    ));
                }
                self.compensate_layout_plugins(transaction_id, reason)?
            },
        };
        self.record_layout_plugin_receipt(transaction_id, kind, receipt.clone());
        Ok(receipt)
    }

    pub fn release_layout_plugins_by_transaction(
        &mut self,
        transaction_id: LayoutTransactionId,
        reason: String,
    ) -> std::result::Result<LayoutPluginReceipt, String> {
        if let Some(receipt) = self
            .layout_plugin_receipts
            .get(&(transaction_id, LayoutPluginResolutionKind::Release))
            .cloned()
        {
            return Ok(receipt);
        }

        let exact_plugin_ids = self
            .layout_plugin_reservations
            .get(&transaction_id)
            .map(|reservation| {
                reservation
                    .plugins
                    .iter()
                    .map(|plugin| plugin.plugin_id)
                    .collect()
            })
            .unwrap_or_default();
        self.resolve_layout_plugins(
            transaction_id,
            LayoutPluginResolution::Release { reason },
            exact_plugin_ids,
        )
    }

    #[cfg(test)]
    pub(super) fn reject_next_layout_plugin_release_for_test(
        &mut self,
        transaction_id: LayoutTransactionId,
    ) {
        self.rejected_layout_plugin_releases.insert(transaction_id);
    }

    pub fn cleanup_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
        mut plugin_ids: Vec<PluginId>,
    ) -> std::result::Result<Vec<PluginId>, String> {
        plugin_ids.sort_unstable();
        plugin_ids.dedup();

        if let Some(receipt_plugin_ids) = self.layout_plugin_cleanup_receipts.get(&transaction_id) {
            if receipt_plugin_ids == &plugin_ids {
                return Ok(receipt_plugin_ids.clone());
            }
            return Err(format!(
                "layout plugin cleanup transaction {transaction_id} replay conflict: requested ids {plugin_ids:?}, recorded ids {receipt_plugin_ids:?}"
            ));
        }

        // Ghost short-circuit: if none of the requested plugins still own
        // layout state, runtime map entries, or executor assignments, certify
        // cleanup immediately so CloseTab debt cannot probe dead ids forever.
        let runtime_plugin_ids = self
            .plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .plugin_ids();
        self.plugin_map.clear_poison();
        let any_live_plugin = plugin_ids.iter().any(|plugin_id| {
            self.layout_plugin_owners.contains_key(plugin_id)
                || runtime_plugin_ids.contains(plugin_id)
                || self.plugin_executor.has_assignment(*plugin_id)
        });
        if !any_live_plugin {
            self.layout_plugin_cleanup_debts.remove(&transaction_id);
            self.record_layout_plugin_cleanup_receipt(transaction_id, plugin_ids.clone());
            log::info!(
                "layout plugin cleanup transaction {transaction_id} certified ghost plugins {plugin_ids:?} as already gone"
            );
            return Ok(plugin_ids);
        }

        if let Some(debt) = self.layout_plugin_cleanup_debts.get(&transaction_id) {
            if debt.requested_plugin_ids != plugin_ids {
                return Err(format!(
                    "layout plugin cleanup transaction {transaction_id} retry conflict: requested ids {plugin_ids:?}, pending ids {:?}",
                    debt.requested_plugin_ids
                ));
            }
        } else {
            self.layout_plugin_cleanup_debts.insert(
                transaction_id,
                LayoutPluginCleanupDebt {
                    requested_plugin_ids: plugin_ids.clone(),
                    remaining_plugin_ids: plugin_ids.iter().copied().collect(),
                },
            );
        }
        self.cancel_complete_layout_plugin_groups_for_cleanup(transaction_id, &plugin_ids)?;

        let remaining_plugin_ids = self
            .layout_plugin_cleanup_debts
            .get(&transaction_id)
            .map(|debt| {
                debt.remaining_plugin_ids
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for plugin_id in remaining_plugin_ids {
            self.cleanup_layout_plugin(transaction_id, plugin_id)
                .map_err(|error| {
                    format!(
                        "layout plugin cleanup transaction {transaction_id} retained debt for plugin {plugin_id}: {error}"
                    )
                })?;
            if let Some(debt) = self.layout_plugin_cleanup_debts.get_mut(&transaction_id) {
                debt.remaining_plugin_ids.remove(&plugin_id);
            }
        }

        let debt = self
            .layout_plugin_cleanup_debts
            .get(&transaction_id)
            .ok_or_else(|| {
                format!(
                    "layout plugin cleanup transaction {transaction_id} lost its cleanup debt before receipt"
                )
            })?;
        if !debt.remaining_plugin_ids.is_empty() {
            return Err(format!(
                "layout plugin cleanup transaction {transaction_id} remains incomplete for ids {:?}",
                debt.remaining_plugin_ids
            ));
        }
        if self
            .layout_plugin_owners
            .keys()
            .any(|plugin_id| plugin_ids.contains(plugin_id))
        {
            return Err(format!(
                "layout plugin cleanup transaction {transaction_id} still has layout ownership after cleanup"
            ));
        }
        let runtime_plugin_ids = self
            .plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .plugin_ids();
        self.plugin_map.clear_poison();
        if plugin_ids
            .iter()
            .any(|plugin_id| runtime_plugin_ids.contains(plugin_id))
        {
            return Err(format!(
                "layout plugin cleanup transaction {transaction_id} still has runtime plugins after cleanup"
            ));
        }
        if plugin_ids
            .iter()
            .any(|plugin_id| self.plugin_executor.has_assignment(*plugin_id))
        {
            return Err(format!(
                "layout plugin cleanup transaction {transaction_id} still has executor assignments after cleanup"
            ));
        }

        self.layout_plugin_cleanup_debts.remove(&transaction_id);
        self.record_layout_plugin_cleanup_receipt(transaction_id, plugin_ids.clone());
        Ok(plugin_ids)
    }

    fn cancel_complete_layout_plugin_groups_for_cleanup(
        &self,
        cleanup_transaction_id: LayoutTransactionId,
        requested_plugin_ids: &[PluginId],
    ) -> std::result::Result<(), String> {
        let owner_transaction_ids = requested_plugin_ids
            .iter()
            .filter_map(|plugin_id| self.layout_plugin_owners.get(plugin_id).copied())
            .collect::<BTreeSet<_>>();
        for owner_transaction_id in owner_transaction_ids {
            let reservation = self
                .layout_plugin_reservations
                .get(&owner_transaction_id)
                .ok_or_else(|| {
                    format!(
                        "layout plugin owner transaction {owner_transaction_id} is missing reservation metadata"
                    )
                })?;
            if owner_transaction_id != cleanup_transaction_id
                && reservation.state == LayoutPluginTransactionState::Reserved
            {
                return Err(format!(
                    "plugins belong to active foreign layout transaction {owner_transaction_id}, which is still Reserved"
                ));
            }
            if reservation.tracker.is_idle() {
                continue;
            }
            let live_owner_ids = self
                .layout_plugin_owners
                .iter()
                .filter_map(|(plugin_id, transaction_id)| {
                    (transaction_id == &owner_transaction_id).then_some(*plugin_id)
                })
                .collect::<BTreeSet<_>>();
            if live_owner_ids
                .iter()
                .all(|plugin_id| requested_plugin_ids.contains(plugin_id))
            {
                reservation.cancellation.cancel();
                reservation.activation_gate.open();
            }
        }
        Ok(())
    }

    fn cleanup_layout_plugin(
        &mut self,
        cleanup_transaction_id: LayoutTransactionId,
        plugin_id: PluginId,
    ) -> std::result::Result<(), String> {
        let owner_transaction_id = self.layout_plugin_owners.get(&plugin_id).copied();
        let reserved_plugin = if let Some(owner_transaction_id) = owner_transaction_id {
            let reservation = self
                .layout_plugin_reservations
                .get(&owner_transaction_id)
                .ok_or_else(|| {
                    format!(
                        "plugin {plugin_id} has owner transaction {owner_transaction_id} without reservation metadata"
                    )
                })?;
            let reserved_plugin = reservation
                .plugins
                .iter()
                .find(|plugin| plugin.plugin_id == plugin_id)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "plugin {plugin_id} is missing from owner transaction {owner_transaction_id}"
                    )
                })?;
            if owner_transaction_id != cleanup_transaction_id
                && reservation.state == LayoutPluginTransactionState::Reserved
            {
                return Err(format!(
                    "plugin {plugin_id} belongs to active foreign layout transaction {owner_transaction_id}"
                ));
            }
            let activation_gate = reservation.activation_gate.clone();
            let tracker = reserved_plugin.activation_tracker.clone();
            reserved_plugin.cancellation.cancel();
            activation_gate.open();
            if !tracker.wait_for_idle(LAYOUT_PLUGIN_CLEANUP_TIMEOUT) {
                return Err(format!(
                    "plugin {plugin_id} owner transaction {owner_transaction_id} did not quiesce within {:?}",
                    LAYOUT_PLUGIN_CLEANUP_TIMEOUT
                ));
            }
            Some(reserved_plugin)
        } else {
            None
        };

        let has_runtime_plugin = self
            .plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .plugin_ids()
            .contains(&plugin_id);
        self.plugin_map.clear_poison();
        let has_executor_assignment = self.plugin_executor.has_assignment(plugin_id);
        if has_runtime_plugin && !has_executor_assignment {
            return Err(format!(
                "plugin {plugin_id} has runtime state without an executor assignment; engine-affine BeforeClose cannot be certified"
            ));
        }

        if has_executor_assignment {
            let completion = self
                .plugin_executor
                .try_execute_plugin_unload_with_completion(
                    plugin_id,
                    move |senders, plugin_map, _connected_clients, _plugin_cache, _engine| {
                        let plugins_to_cleanup = {
                            let mut plugin_map = plugin_map
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            plugin_map
                                .remove_plugins(plugin_id)
                                .into_iter()
                                .collect::<Vec<_>>()
                        };
                        plugin_map.clear_poison();
                        for ((plugin_id, client_id), (running_plugin, subscriptions, workers)) in
                            plugins_to_cleanup
                        {
                            if running_plugin
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .intercepting_key_presses()
                            {
                                let _ = senders.send_to_screen(
                                    ScreenInstruction::ClearKeyPressesIntercepts(client_id),
                                );
                            }
                            let _ = senders.send_to_screen(
                                ScreenInstruction::ClearAllPluginHighlights(plugin_id),
                            );
                            for worker_sender in workers.into_values() {
                                let _ = worker_sender.send(MessageToWorker::Exit);
                            }

                            let needs_before_close = subscriptions
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .contains(&EventType::BeforeClose);
                            if needs_before_close {
                                let mut running_plugin = running_plugin
                                    .lock()
                                    .unwrap_or_else(|poison| poison.into_inner());
                                if let Err(error) = apply_before_close_event_to_plugin(
                                    plugin_id,
                                    &mut running_plugin,
                                ) {
                                    log::error!("{error:?}");
                                    handle_plugin_crash(
                                        plugin_id,
                                        format!("{error:?}").replace("\n", "\n\r"),
                                        senders.clone(),
                                    );
                                }
                            }
                            let cache_dir = running_plugin
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .store
                                .data()
                                .plugin_own_data_dir
                                .clone();
                            if let Err(error) = std::fs::remove_dir_all(&cache_dir) {
                                log::error!(
                                    "Failed to remove cache dir for plugin {plugin_id}: {error:?}"
                                );
                            }
                        }
                    },
                )?;
            match completion.recv_timeout(LAYOUT_PLUGIN_CLEANUP_TIMEOUT) {
                Ok(Ok(())) => {},
                Ok(Err(error)) => return Err(error),
                Err(error) => {
                    return Err(format!(
                        "plugin {plugin_id} unload did not complete within {:?}: {error}",
                        LAYOUT_PLUGIN_CLEANUP_TIMEOUT
                    ));
                },
            }
        }

        let runtime_plugin_remains = self
            .plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .plugin_ids()
            .contains(&plugin_id);
        self.plugin_map.clear_poison();
        if runtime_plugin_remains {
            return Err(format!(
                "plugin {plugin_id} unload completed without removing runtime state"
            ));
        }
        if self.plugin_executor.has_assignment(plugin_id) {
            return Err(format!(
                "plugin {plugin_id} unload completed without removing its executor assignment"
            ));
        }

        self.cached_events_for_pending_plugins.remove(&plugin_id);
        self.cached_resizes_for_pending_plugins.remove(&plugin_id);
        self.cached_worker_messages.remove(&plugin_id);
        self.plugin_ids_waiting_for_permission_request
            .remove(&plugin_id);
        self.parked_chrome_plugin_clients
            .retain(|(parked_plugin_id, _)| parked_plugin_id != &plugin_id);
        self.loading_plugins
            .retain(|(loading_plugin_id, _)| loading_plugin_id != &plugin_id);
        self.cached_plugin_map.clear();
        let mut pipes_to_unblock = self.pending_pipes.unload_plugin(&plugin_id);
        for pipe_name in pipes_to_unblock.drain(..) {
            let _ = self
                .senders
                .send_to_server(ServerInstruction::UnblockCliPipeInput(pipe_name))
                .context("failed to unblock input pipe");
        }
        if let Some(reserved_plugin) = reserved_plugin {
            let loading_context = LoadingContext::new(
                self,
                reserved_plugin.cwd,
                reserved_plugin.plugin_config,
                reserved_plugin.plugin_id,
                reserved_plugin.client_id,
                reserved_plugin.tab_index,
                reserved_plugin.size,
            );
            remove_layout_plugin_data_dir(&loading_context.plugin_own_data_dir);
        }
        let _ = self
            .senders
            .send_to_background_jobs(BackgroundJob::StopPluginLoadingAnimation(plugin_id));
        if let Some(owner_transaction_id) = owner_transaction_id {
            if self.layout_plugin_owners.get(&plugin_id) != Some(&owner_transaction_id) {
                return Err(format!(
                    "plugin {plugin_id} changed owner before cleanup could retire transaction {owner_transaction_id}"
                ));
            }
            self.layout_plugin_owners.remove(&plugin_id);
            let has_live_owner = self
                .layout_plugin_owners
                .values()
                .any(|candidate_owner| candidate_owner == &owner_transaction_id);
            if !has_live_owner {
                self.layout_plugin_reservations
                    .remove(&owner_transaction_id);
            }
        }

        let plugin_list = self
            .plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .list_plugins();
        self.plugin_map.clear_poison();
        let _ = self
            .senders
            .send_to_background_jobs(BackgroundJob::ReportPluginList(plugin_list));
        self.notify_screen_of_ansi_subscription_change();
        Ok(())
    }

    fn record_layout_plugin_cleanup_receipt(
        &mut self,
        transaction_id: LayoutTransactionId,
        plugin_ids: Vec<PluginId>,
    ) {
        self.layout_plugin_cleanup_receipts
            .insert(transaction_id, plugin_ids);
        while self.layout_plugin_cleanup_receipts.len() > MAX_LAYOUT_PLUGIN_CLEANUP_RECEIPTS {
            let Some(oldest_transaction_id) =
                self.layout_plugin_cleanup_receipts.keys().next().copied()
            else {
                break;
            };
            self.layout_plugin_cleanup_receipts
                .remove(&oldest_transaction_id);
        }
    }

    fn cleanup_owned_layout_plugin_ids(
        &mut self,
        transaction_id: LayoutTransactionId,
        plugin_ids: &[PluginId],
    ) -> std::result::Result<(), String> {
        for plugin_id in plugin_ids {
            self.cleanup_layout_plugin(transaction_id, *plugin_id)
                .map_err(|error| {
                    format!(
                        "layout plugin transaction {transaction_id} retained cleanup debt for plugin {plugin_id}: {error}"
                    )
                })?;
        }
        Ok(())
    }

    fn activate_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
    ) -> std::result::Result<LayoutPluginReceipt, String> {
        let reservation = self
            .layout_plugin_reservations
            .get(&transaction_id)
            .ok_or_else(|| format!("unknown layout plugin transaction {transaction_id}"))?;
        let plugins = reservation.plugins.clone();
        let cancellation = reservation.cancellation.clone();
        let tracker = reservation.tracker.clone();
        let activation_gate = reservation.activation_gate.clone();
        let plugin_ids = plugins
            .iter()
            .map(|plugin| plugin.plugin_id)
            .collect::<Vec<_>>();

        for plugin in &plugins {
            self.cached_events_for_pending_plugins
                .insert(plugin.plugin_id, vec![]);
            self.cached_resizes_for_pending_plugins
                .insert(plugin.plugin_id, (plugin.size.rows, plugin.size.cols));
            self.loading_plugins
                .insert((plugin.plugin_id, plugin.run_plugin.clone()));
            let loading_indication = LoadingIndication::new(plugin.run_plugin.location.to_string());
            self.start_plugin_loading_indication(&[plugin.plugin_id], &loading_indication);

            if let Err(message) = self.schedule_reserved_layout_plugin(
                transaction_id,
                plugin.clone(),
                plugin_ids.clone(),
                cancellation.clone(),
                tracker.clone(),
                activation_gate.clone(),
            ) {
                cancellation.cancel();
                activation_gate.open();
                let cleanup_complete = tracker.wait_for_idle(LAYOUT_PLUGIN_CLEANUP_TIMEOUT);
                if !cleanup_complete {
                    if let Some(reservation) =
                        self.layout_plugin_reservations.get_mut(&transaction_id)
                    {
                        reservation.state = LayoutPluginTransactionState::ActivationFailed;
                    }
                    return Err(format!(
                        "layout plugin transaction {transaction_id} activation failed and cleanup did not quiesce: {message}"
                    ));
                }
                if let Err(cleanup_error) =
                    self.cleanup_owned_layout_plugin_ids(transaction_id, &plugin_ids)
                {
                    if let Some(reservation) =
                        self.layout_plugin_reservations.get_mut(&transaction_id)
                    {
                        reservation.state = LayoutPluginTransactionState::ActivationFailed;
                    }
                    return Err(format!(
                        "layout plugin transaction {transaction_id} activation failed and cleanup could not be certified: {message}; {cleanup_error}"
                    ));
                }
                return Ok(LayoutPluginReceipt::ActivationRolledBack {
                    plugin_ids,
                    message,
                });
            }
        }

        activation_gate.open();
        if plugin_ids.is_empty() {
            self.layout_plugin_reservations.remove(&transaction_id);
        } else if let Some(reservation) = self.layout_plugin_reservations.get_mut(&transaction_id) {
            reservation.state = LayoutPluginTransactionState::Activated;
        }
        Ok(LayoutPluginReceipt::Activated { plugin_ids })
    }

    fn release_reserved_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
        reason: String,
    ) -> std::result::Result<LayoutPluginReceipt, String> {
        let (plugin_ids, cancellation, tracker, activation_gate) = {
            let reservation = self
                .layout_plugin_reservations
                .get(&transaction_id)
                .ok_or_else(|| format!("unknown layout plugin transaction {transaction_id}"))?;
            (
                reservation
                    .plugins
                    .iter()
                    .map(|plugin| plugin.plugin_id)
                    .collect::<Vec<_>>(),
                reservation.cancellation.clone(),
                reservation.tracker.clone(),
                reservation.activation_gate.clone(),
            )
        };
        cancellation.cancel();
        activation_gate.open();
        if !tracker.wait_for_idle(LAYOUT_PLUGIN_CLEANUP_TIMEOUT) {
            return Err(format!(
                "layout plugin transaction {transaction_id} release did not quiesce within {:?}: {reason}",
                LAYOUT_PLUGIN_CLEANUP_TIMEOUT
            ));
        }
        self.cleanup_owned_layout_plugin_ids(transaction_id, &plugin_ids)?;
        if plugin_ids.is_empty() {
            self.layout_plugin_reservations.remove(&transaction_id);
        }
        log::debug!("released suspended layout plugins for transaction {transaction_id}: {reason}");
        Ok(LayoutPluginReceipt::Released { plugin_ids })
    }

    fn compensate_layout_plugins(
        &mut self,
        transaction_id: LayoutTransactionId,
        reason: String,
    ) -> std::result::Result<LayoutPluginReceipt, String> {
        let (plugin_ids, cancellation, tracker, activation_gate) = {
            let reservation = self
                .layout_plugin_reservations
                .get(&transaction_id)
                .ok_or_else(|| format!("unknown layout plugin transaction {transaction_id}"))?;
            (
                reservation
                    .plugins
                    .iter()
                    .map(|plugin| plugin.plugin_id)
                    .collect::<Vec<_>>(),
                reservation.cancellation.clone(),
                reservation.tracker.clone(),
                reservation.activation_gate.clone(),
            )
        };
        cancellation.cancel();
        activation_gate.open();
        if !tracker.wait_for_idle(LAYOUT_PLUGIN_CLEANUP_TIMEOUT) {
            return Err(format!(
                "layout plugin transaction {transaction_id} compensation did not quiesce within {:?}: {reason}",
                LAYOUT_PLUGIN_CLEANUP_TIMEOUT
            ));
        }
        self.cleanup_owned_layout_plugin_ids(transaction_id, &plugin_ids)?;
        log::debug!("compensated layout plugins for transaction {transaction_id}: {reason}");
        Ok(LayoutPluginReceipt::Compensated { plugin_ids })
    }

    pub fn handle_layout_plugin_activation_failure(
        &mut self,
        transaction_id: LayoutTransactionId,
        mut plugin_ids: Vec<PluginId>,
        message: String,
    ) {
        plugin_ids.sort_unstable();
        let Some(reservation) = self.layout_plugin_reservations.get_mut(&transaction_id) else {
            return;
        };
        let mut reserved_plugin_ids = reservation
            .plugins
            .iter()
            .map(|plugin| plugin.plugin_id)
            .collect::<Vec<_>>();
        reserved_plugin_ids.sort_unstable();
        if reserved_plugin_ids != plugin_ids {
            log::error!(
                "ignored layout plugin activation failure for transaction {transaction_id}: ids {plugin_ids:?} do not match reserved ids {reserved_plugin_ids:?}"
            );
            return;
        }
        reservation.cancellation.cancel();
        reservation.activation_gate.open();
        reservation.state = LayoutPluginTransactionState::ActivationFailed;
        let tracker = reservation.tracker.clone();
        if !tracker.wait_for_idle(LAYOUT_PLUGIN_CLEANUP_TIMEOUT) {
            log::error!(
                "layout plugin transaction {transaction_id} failed but did not quiesce for cleanup: {message}"
            );
            return;
        }
        if let Err(error) = self.cleanup_owned_layout_plugin_ids(transaction_id, &plugin_ids) {
            log::error!(
                "layout plugin transaction {transaction_id} activation failure retained cleanup debt: {message}; {error}"
            );
            return;
        }
        self.record_layout_plugin_receipt(
            transaction_id,
            LayoutPluginResolutionKind::Compensate,
            LayoutPluginReceipt::Compensated {
                plugin_ids: plugin_ids.clone(),
            },
        );
        log::error!(
            "layout plugin transaction {transaction_id} activation failed and was fully compensated: {message}"
        );
    }

    fn record_layout_plugin_receipt(
        &mut self,
        transaction_id: LayoutTransactionId,
        kind: LayoutPluginResolutionKind,
        receipt: LayoutPluginReceipt,
    ) {
        self.layout_plugin_receipts
            .insert((transaction_id, kind), receipt);
        while self
            .layout_plugin_receipts
            .keys()
            .map(|(transaction_id, _)| *transaction_id)
            .collect::<BTreeSet<_>>()
            .len()
            > MAX_LAYOUT_PLUGIN_RECEIPT_TRANSACTIONS
        {
            let Some(oldest_transaction_id) = self
                .layout_plugin_receipts
                .keys()
                .next()
                .map(|(transaction_id, _)| *transaction_id)
            else {
                break;
            };
            self.layout_plugin_receipts
                .retain(|(transaction_id, _), _| transaction_id != &oldest_transaction_id);
        }
    }

    fn schedule_reserved_layout_plugin(
        &self,
        transaction_id: LayoutTransactionId,
        plugin: ReservedLayoutPlugin,
        group_plugin_ids: Vec<PluginId>,
        cancellation: CancellationToken,
        tracker: Arc<LayoutPluginActivationTracker>,
        activation_gate: Arc<LayoutPluginActivationGate>,
    ) -> std::result::Result<(), String> {
        if cancellation.is_cancelled() {
            return Err(format!(
                "layout plugin transaction {transaction_id} was cancelled before enqueue"
            ));
        }

        let mut loading_context = LoadingContext::new(
            self,
            plugin.cwd.clone(),
            plugin.plugin_config.clone(),
            plugin.plugin_id,
            plugin.client_id,
            plugin.tab_index,
            plugin.size,
        );
        let needs_download = matches!(plugin.plugin_config.location, RunPluginLocation::Remote(_));
        let activation_guards =
            LayoutPluginActivationGuards::begin(&plugin.activation_tracker, &tracker);
        #[cfg(test)]
        let test_hooks = self.layout_plugin_test_hooks.clone();

        if needs_download {
            let plugin_cancellation = plugin.cancellation.clone();
            let downloader = self.downloader.clone();
            let plugin_executor = self.plugin_executor.clone();
            let senders = self.senders.clone();
            let plugin_map = self.plugin_map.clone();
            get_tokio_runtime().spawn(async move {
                activation_gate.wait();
                if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                    return;
                }
                let mut loading_indication =
                    LoadingIndication::new(plugin.run_plugin.location.to_string());
                let RunPluginLocation::Remote(url) = &plugin.plugin_config.location else {
                    return;
                };
                let file_name: String = PortableHash::default()
                    .hash128(url.as_bytes())
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let plugin_data_dir = loading_context.plugin_own_data_dir.clone();
                let download_result = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = plugin_cancellation.cancelled() => return,
                    result = downloader.download(url, Some(&file_name)) => result,
                };
                match download_result {
                    Ok(_) => loading_context.update_plugin_path(ZELLIJ_CACHE_DIR.join(&file_name)),
                    Err(error) => {
                        cancellation.cancel();
                        let plugin_list =
                            remove_layout_plugin_group_from_map(&plugin_map, &group_plugin_ids);
                        remove_layout_plugin_data_dir(&plugin_data_dir);
                        drop(activation_guards);
                        notify_layout_plugin_group_cleanup(
                            &senders,
                            &group_plugin_ids,
                            plugin_list,
                        );
                        handle_plugin_loading_failure(
                            &senders,
                            plugin.plugin_id,
                            &mut loading_indication,
                            &error,
                            Some(plugin.client_id),
                        );
                        report_layout_plugin_activation_failure(
                            &senders,
                            transaction_id,
                            group_plugin_ids,
                            format!("remote plugin download failed: {error:#}"),
                        );
                        return;
                    },
                }
                if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                    return;
                }
                let result = enqueue_reserved_layout_plugin(LayoutPluginActivationJob {
                    plugin_executor,
                    senders: senders.clone(),
                    plugin_map_for_cleanup: plugin_map.clone(),
                    transaction_id,
                    plugin,
                    loading_context,
                    group_plugin_ids: group_plugin_ids.clone(),
                    cancellation: cancellation.clone(),
                    activation_gate,
                    activation_guards,
                    #[cfg(test)]
                    test_hooks,
                });
                if let Err(message) = result {
                    cancellation.cancel();
                    remove_layout_plugin_data_dir(&plugin_data_dir);
                    cleanup_layout_plugin_group_shared(&senders, &plugin_map, &group_plugin_ids);
                    report_layout_plugin_activation_failure(
                        &senders,
                        transaction_id,
                        group_plugin_ids,
                        message,
                    );
                }
            });
            Ok(())
        } else {
            enqueue_reserved_layout_plugin(LayoutPluginActivationJob {
                plugin_executor: self.plugin_executor.clone(),
                senders: self.senders.clone(),
                plugin_map_for_cleanup: self.plugin_map.clone(),
                transaction_id,
                plugin,
                loading_context,
                group_plugin_ids,
                cancellation,
                activation_gate,
                activation_guards,
                #[cfg(test)]
                test_hooks,
            })
        }
    }

    pub fn load_plugin(
        &mut self,
        run: &Option<RunPlugin>,
        tab_index: Option<usize>,
        size: Size,
        cwd: Option<PathBuf>,
        skip_cache: bool,
        client_id: Option<ClientId>,
    ) -> Result<(PluginId, ClientId)> {
        let _err_context = move || "failed to load plugin".to_string();

        let client_id = client_id
            .and_then(|client_id| {
                // first attempt to use a connected client (because this might be a cli_client that
                // should not get plugins) and only if none is connected, load a "dummy" plugin for
                // the cli client
                let connected_clients = self.connected_clients.lock().unwrap();
                if connected_clients.contains(&client_id) {
                    Some(client_id)
                } else {
                    None
                }
            })
            .or_else(|| {
                // if no client id was provided, try to use the first connected client
                self.connected_clients
                    .lock()
                    .unwrap()
                    .iter()
                    .next()
                    .copied()
            })
            .or(client_id) // if we got here, this is likely a cli client with no other clients
            // connected, or loading a background plugin on app start, we use the provided client id as a dummy to load the
            // plugin anyway
            .with_context(
                || "Plugins must have a client id, none was provided and none are connected",
            )?;

        let plugin_id = self.next_plugin_id;

        match run {
            Some(run) => {
                let plugin = match PluginConfig::from_run_plugin(run) {
                    Some(plugin) => plugin,
                    None => {
                        self.next_plugin_id += 1;
                        let mut loading_indication =
                            LoadingIndication::new(run.location.to_string());
                        handle_plugin_loading_failure(
                            &self.senders,
                            plugin_id,
                            &mut loading_indication,
                            format!("Failed to resolve plugin: {}", run.location),
                            Some(client_id),
                        );
                        return Ok((plugin_id, client_id));
                    },
                };
                let plugin_name = run.location.to_string();

                self.cached_events_for_pending_plugins
                    .insert(plugin_id, vec![]);
                self.cached_resizes_for_pending_plugins
                    .insert(plugin_id, (size.rows, size.cols));
                self.loading_plugins.insert((plugin_id, run.clone()));

                // Clone for threaded contexts
                let plugin_executor = self.plugin_executor.clone();
                let senders = self.senders.clone();
                let zellij_cwd = cwd.unwrap_or_else(|| self.zellij_cwd.clone());

                // Check if we need to download (async I/O required)
                let needs_download = matches!(plugin.location, RunPluginLocation::Remote(_));

                let mut loading_context = LoadingContext::new(
                    self,
                    Some(zellij_cwd.clone()),
                    plugin.clone(), // TODO: rename to plugin_config
                    plugin_id,
                    client_id,
                    tab_index,
                    size,
                );

                if needs_download {
                    let downloader = self.downloader.clone();
                    get_tokio_runtime().spawn(async move {
                        let _ = senders.send_to_background_jobs(
                            BackgroundJob::AnimatePluginLoading(plugin_id),
                        );
                        let mut loading_indication = LoadingIndication::new(plugin_name.clone());

                        if let RunPluginLocation::Remote(url) = &plugin.location {
                            let file_name: String = PortableHash::default()
                                .hash128(url.as_bytes())
                                .iter()
                                .map(ToString::to_string)
                                .collect();

                            match downloader.download(url, Some(&file_name)).await {
                                Ok(_) => loading_context
                                    .update_plugin_path(ZELLIJ_CACHE_DIR.join(&file_name)),
                                Err(e) => {
                                    handle_plugin_loading_failure(
                                        &senders,
                                        plugin_id,
                                        &mut loading_indication,
                                        e,
                                        Some(client_id),
                                    );
                                    return;
                                },
                            }
                        }

                        plugin_executor.execute_plugin_load(
                            plugin_id,
                            move |senders: ThreadSenders,
                                  plugin_map: Arc<Mutex<PluginMap>>,
                                  connected_clients: Arc<Mutex<Vec<ClientId>>>,
                                  plugin_cache: PluginCache,
                                  engine| {
                                let mut plugin_map = plugin_map.lock().unwrap();
                                match PluginLoader::new(
                                    skip_cache,
                                    loading_context,
                                    senders.clone(),
                                    engine.clone(),
                                    plugin_cache.clone(),
                                    &mut plugin_map,
                                    connected_clients.clone(),
                                )
                                .start_plugin()
                                {
                                    Ok(_) => {
                                        let plugin_list = plugin_map.list_plugins();
                                        handle_plugin_successful_loading(
                                            &senders,
                                            plugin_id,
                                            plugin_list,
                                        );
                                    },
                                    Err(e) => handle_plugin_loading_failure(
                                        &senders,
                                        plugin_id,
                                        &mut loading_indication,
                                        e,
                                        Some(client_id),
                                    ),
                                }

                                let _ =
                                    senders.send_to_plugin(PluginInstruction::ApplyCachedEvents {
                                        plugin_ids: vec![plugin_id],
                                        done_receiving_permissions: false,
                                    });
                            },
                        );
                    });
                } else {
                    let _ = senders
                        .send_to_background_jobs(BackgroundJob::AnimatePluginLoading(plugin_id));
                    let mut loading_indication = LoadingIndication::new(plugin_name.clone());

                    self.plugin_executor.execute_plugin_load(
                        plugin_id,
                        move |senders,
                              plugin_map,
                              connected_clients,
                              plugin_cache: PluginCache,
                              engine: Engine| {
                            let mut plugin_map = plugin_map.lock().unwrap();
                            match PluginLoader::new(
                                skip_cache,
                                loading_context,
                                senders.clone(),
                                engine.clone(),
                                plugin_cache.clone(),
                                &mut plugin_map,
                                connected_clients.clone(),
                            )
                            .start_plugin()
                            {
                                Ok(_) => {
                                    let plugin_list = plugin_map.list_plugins();
                                    handle_plugin_successful_loading(
                                        &senders,
                                        plugin_id,
                                        plugin_list,
                                    );
                                },
                                Err(e) => handle_plugin_loading_failure(
                                    &senders,
                                    plugin_id,
                                    &mut loading_indication,
                                    e,
                                    Some(client_id),
                                ),
                            }

                            let _ = senders.send_to_plugin(PluginInstruction::ApplyCachedEvents {
                                plugin_ids: vec![plugin_id],
                                done_receiving_permissions: false,
                            });
                        },
                    );
                }

                self.next_plugin_id += 1;
            },
            None => {
                self.next_plugin_id += 1;
                let mut loading_indication = LoadingIndication::new(format!("{}", plugin_id));
                handle_plugin_loading_failure(
                    &self.senders,
                    plugin_id,
                    &mut loading_indication,
                    "Failed to resolve plugin alias",
                    None,
                );
            },
        }
        Ok((plugin_id, client_id))
    }
    pub fn unload_plugin(&mut self, plugin_id: PluginId) -> Result<()> {
        info!("Bye from plugin {}", &plugin_id);
        let cleanup_transaction_id = self
            .layout_plugin_owners
            .get(&plugin_id)
            .copied()
            .unwrap_or_default();
        self.plugin_unload_debts.entry(plugin_id).or_insert(0);

        match self.cleanup_layout_plugin(cleanup_transaction_id, plugin_id) {
            Ok(()) => {
                self.event_diagnostics.flush_plugin(plugin_id);
                self.plugin_unload_debts.remove(&plugin_id);
            },
            Err(error) => {
                let attempt = self
                    .plugin_unload_debts
                    .get(&plugin_id)
                    .copied()
                    .unwrap_or_default()
                    .saturating_add(1);
                self.plugin_unload_debts.insert(plugin_id, attempt);
                self.schedule_plugin_unload_retry(plugin_id, attempt);
                log::error!(
                    "plugin {plugin_id} unload retained completion debt after attempt {attempt}: {error}"
                );
            },
        }
        // An unload failure is durable debt, not a reason to terminate the Plugin
        // instruction loop. The retry keeps the exact plugin id and its layout
        // ownership remains intact until completion-aware cleanup succeeds.
        Ok(())
    }

    fn schedule_plugin_unload_retry(&self, plugin_id: PluginId, attempt: u32) {
        let exponent = attempt.saturating_sub(1).min(5);
        let delay = Duration::from_millis(50_u64.saturating_mul(1_u64 << exponent));
        let senders = self.senders.clone();
        get_tokio_runtime().spawn(async move {
            tokio::time::sleep(delay).await;
            if let Err(error) = senders.send_to_plugin(PluginInstruction::Unload(plugin_id)) {
                log::error!(
                    "failed to schedule retry for plugin {plugin_id} unload completion debt: {error:#}"
                );
            }
        });
    }

    pub fn reload_plugin_with_id(&mut self, plugin_id: u32) -> Result<()> {
        let Some(run_plugin) = self.run_plugin_of_plugin_id(plugin_id) else {
            log::error!("Failed to find plugin with id: {}", plugin_id);
            return Ok(());
        };

        let (rows, columns) = self.size_of_plugin_id(plugin_id).unwrap_or((0, 0));
        self.cached_events_for_pending_plugins
            .insert(plugin_id, vec![]);
        self.cached_resizes_for_pending_plugins
            .insert(plugin_id, (rows, columns));

        let mut loading_indication = LoadingIndication::new(run_plugin.location.to_string());
        self.start_plugin_loading_indication(&[plugin_id], &loading_indication);
        self.loading_plugins.insert((plugin_id, run_plugin.clone()));

        let plugin_executor = self.plugin_executor.clone();

        let Some(first_client_id) = self.get_first_client_id() else {
            log::error!("No connected clients, cannot reload plugin.");
            return Ok(());
        };
        let Some(plugin_config) = self.plugin_config_of_plugin_id(plugin_id) else {
            log::error!("Could not find running plugin with id: {}", plugin_id);
            return Ok(());
        };
        let tab_index = self.tab_index_of_plugin_id(plugin_id);
        let Some(size) = self.size_of_plugin_id(plugin_id) else {
            log::error!(
                "Could not find size of running plugin with id: {}",
                plugin_id
            );
            return Ok(());
        };
        let size = Size {
            rows: size.0,
            cols: size.1,
        };

        let cwd = self.cwd_of_plugin_id(plugin_id);

        let loading_context = LoadingContext::new(
            self,
            cwd,
            plugin_config,
            plugin_id,
            first_client_id,
            tab_index,
            size,
        );

        plugin_executor.execute_for_plugin(
            plugin_id,
            move |senders, plugin_map, connected_clients, plugin_cache, engine| {
                let skip_cache = true; // we want to explicitly reload the plugin
                let mut plugin_map = plugin_map.lock().unwrap();
                match PluginLoader::new(
                    skip_cache,
                    loading_context,
                    senders.clone(),
                    engine.clone(),
                    plugin_cache.clone(),
                    &mut plugin_map,
                    connected_clients.clone(),
                )
                .start_plugin()
                {
                    Ok(_) => {
                        let plugin_list = plugin_map.list_plugins();
                        handle_plugin_successful_loading(&senders, plugin_id, plugin_list);
                    },
                    Err(e) => handle_plugin_loading_failure(
                        &senders,
                        plugin_id,
                        &mut loading_indication,
                        e,
                        Some(first_client_id),
                    ),
                }
                let _ = senders.send_to_plugin(PluginInstruction::ApplyCachedEvents {
                    plugin_ids: vec![plugin_id],
                    done_receiving_permissions: false,
                });
            },
        );
        Ok(())
    }
    pub fn reload_plugin(&mut self, run_plugin: &RunPlugin) -> Result<()> {
        if self.plugin_is_currently_being_loaded(&run_plugin.location) {
            self.pending_plugin_reloads.insert(run_plugin.clone());
            return Ok(());
        }

        let plugin_ids = self
            .all_plugin_ids_for_plugin_location(&run_plugin.location, &run_plugin.configuration)?;
        for plugin_id in &plugin_ids {
            self.reload_plugin_with_id(*plugin_id)?;
        }
        Ok(())
    }
    pub fn add_client(&mut self, client_id: ClientId) -> Result<()> {
        if self.client_is_connected(&client_id) {
            return Ok(());
        }

        let mut new_plugins = HashSet::new();
        for plugin_id in self.plugin_map.lock().unwrap().plugin_ids() {
            new_plugins.insert(plugin_id);
        }
        for plugin_id in new_plugins {
            let Some(run_plugin) = self.run_plugin_of_plugin_id(plugin_id) else {
                log::error!("Failed to find plugin with id: {}", plugin_id);
                return Ok(());
            };

            let (rows, columns) = self.size_of_plugin_id(plugin_id).unwrap_or((0, 0));
            self.cached_events_for_pending_plugins
                .insert(plugin_id, vec![]);
            self.cached_resizes_for_pending_plugins
                .insert(plugin_id, (rows, columns));

            let loading_indication = LoadingIndication::new(run_plugin.location.to_string());
            self.start_plugin_loading_indication(&[plugin_id], &loading_indication);
            self.loading_plugins.insert((plugin_id, run_plugin.clone()));

            let plugin_executor = self.plugin_executor.clone();

            let Some(plugin_config) = self.plugin_config_of_plugin_id(plugin_id) else {
                log::error!("Could not find running plugin with id: {}", plugin_id);
                return Ok(());
            };
            let tab_index = self.tab_index_of_plugin_id(plugin_id);
            let Some(size) = self.size_of_plugin_id(plugin_id) else {
                log::error!(
                    "Could not find size of running plugin with id: {}",
                    plugin_id
                );
                return Ok(());
            };
            let size = Size {
                rows: size.0,
                cols: size.1,
            };

            let cwd = self.cwd_of_plugin_id(plugin_id);

            let loading_context = LoadingContext::new(
                self,
                cwd,
                plugin_config,
                plugin_id,
                client_id,
                tab_index,
                size,
            );

            plugin_executor.execute_for_plugin(
                plugin_id,
                move |senders, plugin_map, connected_clients, plugin_cache, engine| {
                    let skip_cache = false;
                    let mut plugin_map = plugin_map.lock().unwrap();
                    match PluginLoader::new(
                        skip_cache,
                        loading_context,
                        senders.clone(),
                        engine.clone(),
                        plugin_cache.clone(),
                        &mut plugin_map,
                        connected_clients.clone(),
                    )
                    .without_connected_clients()
                    .start_plugin()
                    {
                        Ok(_) => {
                            let _ = senders
                                .send_to_screen(ScreenInstruction::RequestStateUpdateForPlugins);
                            let _ = senders.send_to_background_jobs(
                                BackgroundJob::StopPluginLoadingAnimation(plugin_id),
                            );
                            let _ = senders.send_to_plugin(PluginInstruction::ApplyCachedEvents {
                                plugin_ids: vec![plugin_id],
                                done_receiving_permissions: false,
                            });
                        },
                        Err(e) => {
                            log::error!("Failed to load plugin for new client: {}", e);
                        },
                    }
                },
            )
        }
        self.connected_clients.lock().unwrap().push(client_id);
        Ok(())
    }
    pub fn resize_plugin(
        &mut self,
        pid: PluginId,
        new_columns: usize,
        new_rows: usize,
        shutdown_sender: Sender<()>,
    ) -> Result<()> {
        let err_context = move || format!("failed to resize plugin {pid}");

        let plugins_to_resize: Vec<(PluginId, ClientId, Arc<Mutex<RunningPlugin>>)> = self
            .plugin_map
            .lock()
            .unwrap()
            .running_plugins()
            .iter()
            .filter(|&(plugin_id, _client_id, _running_plugin)| {
                !self
                    .cached_resizes_for_pending_plugins
                    .contains_key(plugin_id)
            })
            .cloned()
            .collect();
        for (plugin_id, client_id, running_plugin) in plugins_to_resize {
            if plugin_id == pid {
                let event_id = running_plugin
                    .lock()
                    .unwrap()
                    .next_event_id(AtomicEvent::Resize);
                // Execute directly on pinned thread (no async I/O needed for resize/render)
                self.plugin_executor.execute_for_plugin(plugin_id, {
                    // let senders = self.senders.clone();
                    let running_plugin = running_plugin.clone();
                    let _s = shutdown_sender.clone();
                    move |senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                        let mut running_plugin = running_plugin.lock().unwrap();
                        let _s = _s; // guard to allow the task to complete before cleanup/shutdown
                        if running_plugin.apply_event_id(AtomicEvent::Resize, event_id) {
                            let old_rows = running_plugin.rows;
                            let old_columns = running_plugin.columns;
                            running_plugin.rows = new_rows;
                            running_plugin.columns = new_columns;

                            // in the below conditional, we check if event_id == 0 so that we'll
                            // make sure to always render on the first resize event
                            if old_rows != new_rows || old_columns != new_columns || event_id == 0 {
                                let rendered_bytes = running_plugin
                                    .instance
                                    .clone()
                                    .get_typed_func::<(i32, i32), ()>(
                                        &mut running_plugin.store,
                                        "render",
                                    )
                                    .and_then(|render| {
                                        render.call(
                                            &mut running_plugin.store,
                                            (new_rows as i32, new_columns as i32),
                                        )
                                    })
                                    .map_err(|e| anyhow!(e))
                                    .and_then(|_| {
                                        wasi_read_string(running_plugin.store.data())
                                            .map_err(|e| anyhow!(e))
                                    })
                                    .with_context(err_context);
                                match rendered_bytes {
                                    Ok(rendered_bytes) => {
                                        let plugin_render_asset = PluginRenderAsset::new(
                                            plugin_id,
                                            client_id,
                                            rendered_bytes.as_bytes().to_vec(),
                                        );
                                        senders
                                            .send_to_screen(ScreenInstruction::PluginBytes(vec![
                                                plugin_render_asset,
                                            ]))
                                            .unwrap();
                                    },
                                    Err(e) => log::error!("{}", e),
                                }
                            }
                        }
                    }
                });
            }
        }
        for (plugin_id, current_size) in self.cached_resizes_for_pending_plugins.iter_mut() {
            if *plugin_id == pid {
                current_size.0 = new_rows;
                current_size.1 = new_columns;
            }
        }
        Ok(())
    }
    pub fn update_plugins(
        &mut self,
        mut updates: Vec<(Option<PluginId>, Option<ClientId>, Event)>,
        shutdown_sender: Sender<()>,
    ) -> Result<()> {
        let plugins_to_update: Vec<RunningPluginAndSubscriptions> = self
            .plugin_map
            .lock()
            .unwrap()
            .running_plugins_and_subscriptions();

        // Execute each plugin update on its respective pinned thread.
        // Snapshot each plugin's subscriptions ONCE per call — locking and
        // cloning them inside the events × plugins product turned a build's
        // FileSystemUpdate burst into a lock-storm on the plugin thread.
        let plugin_subscription_snapshots: Vec<_> = plugins_to_update
            .iter()
            .map(|(_, _, _, subscriptions)| subscriptions.lock().unwrap().clone())
            .collect();
        let plugin_executor = self.plugin_executor.clone();
        let event_diagnostics = self.event_diagnostics.clone();
        for (pid, cid, event) in updates.iter() {
            let (pid, cid) = (*pid, *cid);
            self.update_parked_chrome_target(pid, cid, event);
            let refreshable_status_bar_state =
                Self::is_refreshable_status_bar_state(pid, cid, event);
            // FIXME: This is very janky... Maybe I should write my own macro for Event -> EventType?
            let Ok(event_type) = EventType::from_str(&event.to_string()) else {
                continue;
            };
            for ((plugin_id, client_id, running_plugin, _), subs) in
                plugins_to_update.iter().zip(&plugin_subscription_snapshots)
            {
                if self.is_parked_chrome_state_payload(*plugin_id, *client_id, event) {
                    continue;
                }
                if (!self
                    .cached_events_for_pending_plugins
                    .contains_key(plugin_id)
                    || refreshable_status_bar_state)
                    && (subs.contains(&event_type)
                        || event_type == EventType::PermissionRequestResult)
                    && Self::message_is_directed_at_plugin(pid, cid, plugin_id, client_id)
                {
                    // Execute directly on pinned thread (no async I/O needed for event processing)
                    plugin_executor.execute_for_plugin(*plugin_id, {
                        let plugin_id = *plugin_id;
                        let client_id = *client_id;
                        let running_plugin = running_plugin.clone();
                        let event = event.clone();
                        let _s = shutdown_sender.clone();
                        let plugin_subs = subs.clone();
                        let event_diagnostics = event_diagnostics.clone();
                        move |senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                            let _s = _s; // guard to allow the task to complete before cleanup/shutdown
                            let mut running_plugin = running_plugin.lock().unwrap();
                            let mut plugin_render_assets = vec![];
                            match apply_event_to_plugin(
                                plugin_id,
                                client_id,
                                &mut running_plugin,
                                &event,
                                &mut plugin_render_assets,
                                senders.clone(),
                                &plugin_subs,
                            ) {
                                Ok((rendered, empty_rendered)) => {
                                    event_diagnostics.record(
                                        plugin_id,
                                        client_id,
                                        &event,
                                        rendered,
                                        empty_rendered,
                                    );
                                    let _ = senders.send_to_screen(ScreenInstruction::PluginBytes(
                                        plugin_render_assets,
                                    ));
                                },
                                Err(e) => {
                                    log::error!("{:?}", e);

                                    // https://stackoverflow.com/questions/66450942/in-rust-is-there-a-way-to-make-literal-newlines-in-r-using-windows-c
                                    let stringified_error =
                                        format!("{:?}", e).replace("\n", "\n\r");

                                    handle_plugin_crash(
                                        plugin_id,
                                        stringified_error,
                                        senders.clone(),
                                    );
                                },
                            }
                        }
                    });
                }
            }
        }

        // loop once more to update the cached events for the pending plugins (probably currently
        // being loaded, we'll send them these events when they load)
        for (pid, cid, event) in updates.drain(..) {
            if Self::is_refreshable_status_bar_state(pid, cid, &event) {
                // These are current-state signals, not edge-triggered events.
                // Deliver them directly to an already-ready exact target even
                // while another client instance is loading, but never put them
                // in the shared per-plugin cache. A newly loaded status bar
                // starts idle and requests a fresh server snapshot on success.
                continue;
            }
            for (plugin_id, cached_events) in self.cached_events_for_pending_plugins.iter_mut() {
                if pid.is_none() || pid.as_ref() == Some(plugin_id) {
                    // Keep the newest events — a stuck or crash-looping load
                    // must not accumulate unbounded broadcast history
                    // (FileSystemUpdate bursts, 1Hz SessionUpdate snapshots)
                    // while it waits.
                    if cached_events.len() >= MAX_CACHED_EVENTS_PER_PENDING_PLUGIN {
                        cached_events.remove(0);
                    }
                    cached_events.push(EventOrPipeMessage::Event(Box::new(event.clone())));
                }
            }
        }
        Ok(())
    }
    pub fn get_plugin_cwd(&self, plugin_id: PluginId, client_id: ClientId) -> Option<PathBuf> {
        self.plugin_map
            .lock()
            .unwrap()
            .running_plugins()
            .iter()
            .find_map(|(p_id, c_id, running_plugin)| {
                if p_id == &plugin_id && c_id == &client_id {
                    let plugin_cwd = running_plugin
                        .lock()
                        .unwrap()
                        .store
                        .data()
                        .plugin_cwd
                        .clone();
                    Some(plugin_cwd)
                } else {
                    None
                }
            })
    }
    pub fn change_plugin_host_dir(
        &mut self,
        new_host_dir: PathBuf,
        plugin_id_to_update: PluginId,
        client_id_to_update: ClientId,
    ) -> Result<()> {
        let plugins_to_change: Vec<RunningPluginAndSubscriptions> = self
            .plugin_map
            .lock()
            .unwrap()
            .running_plugins_and_subscriptions()
            .to_vec();

        // Execute directly on pinned thread (no async I/O needed for directory check/change)
        self.plugin_executor
            .execute_for_plugin(plugin_id_to_update, {
                move |senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                    match new_host_dir.try_exists() {
                        Ok(false) => {
                            log::error!(
                                "Failed to change folder to {},: folder does not exist",
                                new_host_dir.display()
                            );
                            let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                Some(plugin_id_to_update),
                                Some(client_id_to_update),
                                Event::FailedToChangeHostFolder(Some(format!(
                                    "Folder {} does not exist",
                                    new_host_dir.display()
                                ))),
                            )]));
                            return;
                        },
                        Err(e) => {
                            log::error!(
                                "Failed to change folder to {},: {}",
                                new_host_dir.display(),
                                e
                            );
                            let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                Some(plugin_id_to_update),
                                Some(client_id_to_update),
                                Event::FailedToChangeHostFolder(Some(e.to_string())),
                            )]));
                            return;
                        },
                        _ => {},
                    }
                    for (plugin_id, client_id, running_plugin, _subscriptions) in &plugins_to_change
                    {
                        if plugin_id == &plugin_id_to_update && client_id == &client_id_to_update {
                            let mut running_plugin = running_plugin.lock().unwrap();
                            let plugin_env = running_plugin.store.data_mut();
                            let stdin_pipe = plugin_env.stdin_pipe.clone();
                            let stdout_pipe = plugin_env.stdout_pipe.clone();
                            let wasi_ctx = PluginLoader::create_wasi_ctx(
                                &new_host_dir,
                                &plugin_env.plugin_own_data_dir,
                                &plugin_env.plugin_own_cache_dir,
                                &ZELLIJ_TMP_DIR,
                                &plugin_env.plugin.location.to_string(),
                                plugin_env.plugin_id,
                                stdin_pipe.clone(),
                                stdout_pipe.clone(),
                            );
                            match wasi_ctx {
                                Ok(wasi_ctx) => {
                                    drop(std::mem::replace(&mut plugin_env.wasi_ctx, wasi_ctx));
                                    plugin_env.plugin_cwd = new_host_dir.clone();

                                    let _ =
                                        senders.send_to_plugin(PluginInstruction::Update(vec![(
                                            Some(*plugin_id),
                                            Some(*client_id),
                                            Event::HostFolderChanged(new_host_dir.clone()),
                                        )]));
                                },
                                Err(e) => {
                                    let _ =
                                        senders.send_to_plugin(PluginInstruction::Update(vec![(
                                            Some(*plugin_id),
                                            Some(*client_id),
                                            Event::FailedToChangeHostFolder(Some(e.to_string())),
                                        )]));
                                    log::error!("Failed to create wasi ctx: {}", e);
                                },
                            }
                        }
                    }
                }
            });
        Ok(())
    }
    pub fn pipe_messages(
        &mut self,
        messages: Vec<(Option<PluginId>, Option<ClientId>, PipeMessage)>,
        shutdown_sender: Sender<()>,
        mut notification_end: Option<NotificationEnd>,
    ) -> Result<()> {
        let plugins_to_update: Vec<RunningPluginAndSubscriptions> = self
            .plugin_map
            .lock()
            .unwrap()
            .running_plugins_and_subscriptions()
            .iter()
            .filter(
                |&(plugin_id, _client_id, _running_plugin, _subscriptions)| {
                    !&self
                        .cached_events_for_pending_plugins
                        .contains_key(plugin_id)
                },
            )
            .cloned()
            .collect();

        // Execute each pipe message on its respective plugin's pinned thread
        let plugin_executor = self.plugin_executor.clone();
        for (message_pid, message_cid, pipe_message) in messages.clone().into_iter() {
            for (plugin_id, client_id, running_plugin, _subscriptions) in &plugins_to_update {
                if Self::message_is_directed_at_plugin(
                    message_pid,
                    message_cid,
                    plugin_id,
                    client_id,
                ) {
                    if let PipeSource::Cli(pipe_id) = &pipe_message.source {
                        self.pending_pipes
                            .mark_being_processed(pipe_id, plugin_id, client_id);
                    }
                    // Execute directly on pinned thread (no async I/O needed for pipe message processing)
                    plugin_executor.execute_for_plugin(*plugin_id, {
                        let running_plugin = running_plugin.clone();
                        let pipe_message = pipe_message.clone();
                        let plugin_id = *plugin_id;
                        let client_id = *client_id;
                        let _s = shutdown_sender.clone();
                        let notification_end = notification_end.take();
                        move |senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                            let mut running_plugin = running_plugin.lock().unwrap();
                            let mut plugin_render_assets = vec![];
                            let _s = _s; // guard to allow the task to complete before cleanup/shutdown
                            match apply_pipe_message_to_plugin(
                                plugin_id,
                                client_id,
                                &mut running_plugin,
                                &pipe_message,
                                &mut plugin_render_assets,
                                &senders,
                            ) {
                                Ok(()) => {
                                    let _ = senders.send_to_screen(ScreenInstruction::PluginBytes(
                                        plugin_render_assets,
                                    ));
                                },
                                Err(e) => {
                                    log::error!("{:?}", e);

                                    // https://stackoverflow.com/questions/66450942/in-rust-is-there-a-way-to-make-literal-newlines-in-rust-using-windows
                                    let stringified_error =
                                        format!("{:?}", e).replace("\n", "\n\r");

                                    handle_plugin_crash(
                                        plugin_id,
                                        stringified_error,
                                        senders.clone(),
                                    );
                                },
                            }
                            drop(notification_end);
                        }
                    });
                }
            }
            let all_connected_clients: Vec<ClientId> = self
                .connected_clients
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect();
            for (plugin_id, cached_events) in self.cached_events_for_pending_plugins.iter_mut() {
                if message_pid.is_none() || message_pid.as_ref() == Some(plugin_id) {
                    if cached_events.len() >= MAX_CACHED_EVENTS_PER_PENDING_PLUGIN {
                        cached_events.remove(0);
                    }
                    cached_events.push(EventOrPipeMessage::PipeMessage(pipe_message.clone()));
                    if let PipeSource::Cli(pipe_id) = &pipe_message.source {
                        for client_id in &all_connected_clients {
                            if Self::message_is_directed_at_plugin(
                                message_pid,
                                message_cid,
                                plugin_id,
                                client_id,
                            ) {
                                self.pending_pipes
                                    .mark_being_processed(pipe_id, plugin_id, client_id);
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    pub fn apply_cached_events(
        &mut self,
        plugin_ids: Vec<PluginId>,
        done_receiving_permissions: bool,
        shutdown_sender: Sender<()>,
    ) -> Result<()> {
        let mut applied_plugin_paths = HashSet::new();
        for plugin_id in plugin_ids {
            if !done_receiving_permissions
                && self
                    .plugin_ids_waiting_for_permission_request
                    .contains(&plugin_id)
            {
                continue;
            }
            self.plugin_ids_waiting_for_permission_request
                .remove(&plugin_id);
            self.apply_cached_events_and_resizes_for_plugin(plugin_id, shutdown_sender.clone())?;
            if let Some(run_plugin) = self.run_plugin_of_loading_plugin_id(plugin_id) {
                applied_plugin_paths.insert(run_plugin.clone());
            }
            self.loading_plugins
                .retain(|(p_id, _run_plugin)| p_id != &plugin_id);
            self.clear_plugin_map_cache();
        }
        for run_plugin in applied_plugin_paths.drain() {
            if self.pending_plugin_reloads.remove(&run_plugin) {
                let _ = self.reload_plugin(&run_plugin);
            }
        }
        Ok(())
    }
    pub fn remove_client(&mut self, client_id: ClientId) {
        self.parked_chrome_plugin_clients
            .retain(|(_, parked_client_id)| parked_client_id != &client_id);
        self.connected_clients
            .lock()
            .unwrap()
            .retain(|c| c != &client_id);

        // Remove client from cached pane render report
        if let Some(ref mut prev_report) = self.previous_pane_render_report {
            prev_report.all_pane_contents.remove(&client_id);
        }
    }

    fn get_changed_panes_per_client(
        &self,
        new_contents: &HashMap<ClientId, HashMap<zellij_utils::data::PaneId, PaneContents>>,
        previous_contents: Option<
            &HashMap<ClientId, HashMap<zellij_utils::data::PaneId, PaneContents>>,
        >,
    ) -> HashMap<ClientId, HashMap<zellij_utils::data::PaneId, PaneContents>> {
        let mut result: HashMap<ClientId, HashMap<zellij_utils::data::PaneId, PaneContents>> =
            HashMap::new();

        // First report - return everything grouped by client
        let Some(prev_contents) = previous_contents else {
            for (client_id, panes) in new_contents {
                result.insert(*client_id, panes.clone());
            }
            return result;
        };

        // Compare each client's panes
        for (client_id, new_panes) in new_contents {
            let mut client_panes: HashMap<zellij_utils::data::PaneId, PaneContents> =
                HashMap::new();
            for (pane_id, new_pane_contents) in new_panes {
                let has_changed = prev_contents
                    .get(client_id)
                    .and_then(|prev_panes| prev_panes.get(pane_id))
                    .map(|prev_pane_contents| {
                        prev_pane_contents.viewport != new_pane_contents.viewport
                            || prev_pane_contents.selected_text != new_pane_contents.selected_text
                    })
                    .unwrap_or(true);
                if has_changed {
                    client_panes.insert(*pane_id, new_pane_contents.clone());
                }
            }
            if !client_panes.is_empty() {
                result.insert(*client_id, client_panes);
            }
        }
        result
    }

    pub fn handle_pane_render_report(
        &mut self,
        pane_render_report: PaneRenderReport,
        shutdown_sender: Sender<()>,
    ) -> Result<()> {
        // Plain content (existing behavior)
        let changed_panes_per_client = self.get_changed_panes_per_client(
            &pane_render_report.all_pane_contents,
            self.previous_pane_render_report
                .as_ref()
                .map(|r| &r.all_pane_contents),
        );
        for (client_id, client_panes) in changed_panes_per_client {
            let updates = vec![(None, Some(client_id), Event::PaneRenderReport(client_panes))];
            self.update_plugins(updates, shutdown_sender.clone())?;
        }

        // ANSI content (new behavior)
        if !pane_render_report.all_pane_contents_with_ansi.is_empty() {
            let changed_ansi_panes_per_client = self.get_changed_panes_per_client(
                &pane_render_report.all_pane_contents_with_ansi,
                self.previous_pane_render_report
                    .as_ref()
                    .map(|r| &r.all_pane_contents_with_ansi),
            );
            for (client_id, client_panes) in changed_ansi_panes_per_client {
                let updates = vec![(
                    None,
                    Some(client_id),
                    Event::PaneRenderReportWithAnsi(client_panes),
                )];
                self.update_plugins(updates, shutdown_sender.clone())?;
            }
        }

        self.previous_pane_render_report = Some(pane_render_report);
        Ok(())
    }

    pub fn notify_screen_of_ansi_subscription_change(&self) {
        let any_plugin_needs_ansi = {
            let mut plugin_map = self.plugin_map.lock().unwrap();
            plugin_map
                .running_plugins_and_subscriptions()
                .iter()
                .any(|(_, _, _, subs)| {
                    subs.lock()
                        .unwrap()
                        .contains(&EventType::PaneRenderReportWithAnsi)
                })
        };
        let _ = self
            .senders
            .send_to_screen(ScreenInstruction::PluginSubscribedToAnsiPaneContents(
                any_plugin_needs_ansi,
            ));
    }

    pub fn notify_screen_of_background_plugin_subscriptions(
        &self,
        plugin_id: PluginId,
        client_id: ClientId,
        events: HashSet<EventType>,
    ) {
        // Check if this plugin is a background plugin (tab_index == None)
        let is_background = {
            let mut plugin_map = self.plugin_map.lock().unwrap();
            plugin_map
                .running_plugins_and_subscriptions()
                .iter()
                .any(|(pid, cid, rp, _)| {
                    *pid == plugin_id
                        && *cid == client_id
                        && rp.lock().unwrap().store.data().tab_index.is_none()
                })
        };
        if is_background {
            let _ = self.senders.send_to_screen(
                ScreenInstruction::UpdateBackgroundPluginSubscriptions(
                    plugin_id, client_id, events,
                ),
            );
        }
    }

    pub fn send_initial_keybinds_to_plugin(&self, plugin_id: PluginId, client_id: ClientId) {
        let keybinds = {
            let mut plugin_map = self.plugin_map.lock().unwrap();
            plugin_map
                .running_plugins_and_subscriptions()
                .iter()
                .find(|(pid, cid, _, _)| *pid == plugin_id && *cid == client_id)
                .map(|(_, _, rp, _)| rp.lock().unwrap().store.data().keybinds.to_keybinds_vec())
        };
        if let Some(keybinds) = keybinds {
            let _ = self.senders.send_to_plugin(PluginInstruction::Update(vec![(
                Some(plugin_id),
                Some(client_id),
                Event::InitialKeybinds(keybinds),
            )]));
        }
    }

    pub fn cleanup(&mut self) {
        self.loading_plugins.clear();

        let plugin_ids = self.plugin_map.lock().unwrap().plugin_ids();
        for plugin_id in &plugin_ids {
            drop(self.unload_plugin(*plugin_id));
        }
        if let Some(watcher) = self.watcher.take() {
            watcher.stop_nonblocking();
        }
    }
    pub fn run_plugin_of_loading_plugin_id(&self, plugin_id: PluginId) -> Option<&RunPlugin> {
        self.loading_plugins
            .iter()
            .find(|(p_id, _run_plugin)| p_id == &plugin_id)
            .map(|(_p_id, run_plugin)| run_plugin)
    }
    pub fn run_plugin_of_plugin_id(&self, plugin_id: PluginId) -> Option<RunPlugin> {
        self.plugin_map
            .lock()
            .unwrap()
            .run_plugin_of_plugin_id(plugin_id)
    }

    pub fn reconfigure(
        &mut self,
        client_id: ClientId,
        keybinds: Option<Keybinds>,
        default_mode: Option<InputMode>,
        default_shell: Option<TerminalAction>,
        layout_dir: Option<PathBuf>,
    ) -> Result<()> {
        let plugins_to_reconfigure: Vec<(PluginId, Arc<Mutex<RunningPlugin>>)> = self
            .plugin_map
            .lock()
            .unwrap()
            .running_plugins()
            .iter()
            .cloned()
            .filter_map(|(plugin_id, c_id, running_plugin)| {
                if c_id == client_id {
                    Some((plugin_id, running_plugin.clone()))
                } else {
                    None
                }
            })
            .collect();
        if let Some(default_mode) = default_mode.as_ref() {
            self.base_modes.insert(client_id, *default_mode);
        }
        if let Some(keybinds) = keybinds.as_ref() {
            self.keybinds.insert(client_id, keybinds.clone());
        }
        self.default_shell = default_shell.clone();
        self.layout_dir = layout_dir.clone();
        // Collect plugins subscribed to InitialKeybinds for post-reconfigure notification
        let plugins_subscribed_to_initial_keybinds: Vec<PluginId> = if keybinds.is_some() {
            self.plugin_map
                .lock()
                .unwrap()
                .running_plugins_and_subscriptions()
                .iter()
                .filter(|(_, cid, _, subs)| {
                    *cid == client_id && subs.lock().unwrap().contains(&EventType::InitialKeybinds)
                })
                .map(|(pid, _, _, _)| *pid)
                .collect()
        } else {
            vec![]
        };

        for (plugin_id, running_plugin) in plugins_to_reconfigure {
            self.plugin_executor.execute_for_plugin(plugin_id, {
                let running_plugin = running_plugin.clone();
                let keybinds = keybinds.clone();
                let default_shell = default_shell.clone();
                let layout_dir = layout_dir.clone();
                move |_senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                    let mut running_plugin = running_plugin.lock().unwrap();
                    if let Some(keybinds) = keybinds {
                        running_plugin.update_keybinds(keybinds);
                    }
                    if let Some(default_mode) = default_mode {
                        running_plugin.update_default_mode(default_mode);
                    }
                    running_plugin.update_default_shell(default_shell);
                    running_plugin.update_layout_dir(layout_dir);
                }
            });
        }
        // Send InitialKeybinds to subscribed plugins after reconfiguration
        for plugin_id in plugins_subscribed_to_initial_keybinds {
            self.send_initial_keybinds_to_plugin(plugin_id, client_id);
        }
        Ok(())
    }
    fn apply_cached_events_and_resizes_for_plugin(
        &mut self,
        plugin_id: PluginId,
        shutdown_sender: Sender<()>,
    ) -> Result<()> {
        let err_context = || "Failed to apply cached events to plugin".to_string();
        if let Some(events_or_pipe_messages) =
            self.cached_events_for_pending_plugins.remove(&plugin_id)
        {
            let all_connected_clients: Vec<ClientId> = self
                .connected_clients
                .lock()
                .unwrap()
                .iter()
                .copied()
                .collect();
            for client_id in &all_connected_clients {
                if let Some((running_plugin, subscriptions)) = self
                    .plugin_map
                    .lock()
                    .unwrap()
                    .get_running_plugin_and_subscriptions(plugin_id, *client_id)
                {
                    let subs = subscriptions.lock().unwrap().clone();
                    let target_is_parked = self
                        .parked_chrome_plugin_clients
                        .contains(&(plugin_id, *client_id));
                    let event_diagnostics = self.event_diagnostics.clone();
                    self.plugin_executor.execute_for_plugin(plugin_id, {
                        let running_plugin = running_plugin.clone();
                        let client_id = *client_id;
                        let _s = shutdown_sender.clone();
                        let events_or_pipe_messages = events_or_pipe_messages.clone();
                        move |senders, _plugin_map, _connected_clients, _plugin_cache, _engine| {
                            let _s = _s; // guard to allow the task to complete before cleanup/shutdown
                            for event_or_pipe_message in events_or_pipe_messages {
                                match event_or_pipe_message {
                                    EventOrPipeMessage::Event(event) => {
                                        let event = *event;
                                        if target_is_parked
                                            && matches!(
                                                event,
                                                Event::SessionUpdate(..) | Event::PaneUpdate(..)
                                            )
                                        {
                                            continue;
                                        }
                                        match EventType::from_str(&event.to_string())
                                            .with_context(err_context)
                                        {
                                            Ok(event_type) => {
                                                if !subs.contains(&event_type) {
                                                    continue;
                                                }
                                                let mut running_plugin =
                                                    running_plugin.lock().unwrap();
                                                let mut plugin_render_assets = vec![];
                                                match apply_event_to_plugin(
                                                    plugin_id,
                                                    client_id,
                                                    &mut running_plugin,
                                                    &event,
                                                    &mut plugin_render_assets,
                                                    senders.clone(),
                                                    &subs,
                                                ) {
                                                    Ok((rendered, empty_rendered)) => {
                                                        event_diagnostics.record(
                                                            plugin_id,
                                                            client_id,
                                                            &event,
                                                            rendered,
                                                            empty_rendered,
                                                        );
                                                        let _ = senders.send_to_screen(
                                                            ScreenInstruction::PluginBytes(
                                                                plugin_render_assets,
                                                            ),
                                                        );
                                                    },
                                                    Err(e) => {
                                                        log::error!("{}", e);
                                                    },
                                                }
                                            },
                                            Err(e) => {
                                                log::error!("Failed to apply event: {:?}", e);
                                            },
                                        }
                                    },
                                    EventOrPipeMessage::PipeMessage(pipe_message) => {
                                        let mut running_plugin = running_plugin.lock().unwrap();
                                        let mut plugin_render_assets = vec![];

                                        match apply_pipe_message_to_plugin(
                                            plugin_id,
                                            client_id,
                                            &mut running_plugin,
                                            &pipe_message,
                                            &mut plugin_render_assets,
                                            &senders,
                                        ) {
                                            Ok(()) => {
                                                let _ = senders.send_to_screen(
                                                    ScreenInstruction::PluginBytes(
                                                        plugin_render_assets,
                                                    ),
                                                );
                                            },
                                            Err(e) => {
                                                log::error!("{:?}", e);

                                                // https://stackoverflow.com/questions/66450942/in-rust-is-there-a-way-to-make-literal-newlines-in-r-using-windows-c
                                                let stringified_error =
                                                    format!("{:?}", e).replace("\n", "\n\r");

                                                handle_plugin_crash(
                                                    plugin_id,
                                                    stringified_error,
                                                    senders.clone(),
                                                );
                                            },
                                        }
                                    },
                                }
                            }
                        }
                    });
                }
            }
        }
        if let Some((rows, columns)) = self.cached_resizes_for_pending_plugins.remove(&plugin_id) {
            self.resize_plugin(plugin_id, columns, rows, shutdown_sender.clone())?;
        }
        self.apply_cached_worker_messages(plugin_id)?;
        Ok(())
    }
    pub fn apply_cached_worker_messages(&mut self, plugin_id: PluginId) -> Result<()> {
        if let Some(mut messages) = self.cached_worker_messages.remove(&plugin_id) {
            let mut worker_messages: HashMap<(ClientId, String), Vec<(String, String)>> =
                HashMap::new();
            for (client_id, worker_name, message, payload) in messages.drain(..) {
                worker_messages
                    .entry((client_id, worker_name))
                    .or_default()
                    .push((message, payload));
            }
            for ((client_id, worker_name), messages) in worker_messages.drain() {
                self.post_messages_to_plugin_worker(plugin_id, client_id, worker_name, messages)?;
            }
        }
        Ok(())
    }
    fn plugin_is_currently_being_loaded(&self, plugin_location: &RunPluginLocation) -> bool {
        self.loading_plugins
            .iter()
            .any(|(_plugin_id, run_plugin)| &run_plugin.location == plugin_location)
    }
    fn plugin_id_of_loading_plugin(
        &self,
        plugin_location: &RunPluginLocation,
        plugin_configuration: &PluginUserConfiguration,
    ) -> Option<PluginId> {
        self.loading_plugins
            .iter()
            .find_map(|(plugin_id, run_plugin)| {
                if &run_plugin.location == plugin_location
                    && &run_plugin.configuration == plugin_configuration
                {
                    Some(*plugin_id)
                } else {
                    None
                }
            })
    }
    fn all_plugin_ids_for_plugin_location(
        &self,
        plugin_location: &RunPluginLocation,
        plugin_configuration: &PluginUserConfiguration,
    ) -> Result<Vec<PluginId>> {
        self.plugin_map
            .lock()
            .unwrap()
            .all_plugin_ids_for_plugin_location(plugin_location, plugin_configuration)
    }
    pub fn all_plugin_and_client_ids_for_plugin_location(
        &mut self,
        plugin_location: &RunPluginLocation,
        plugin_configuration: &PluginUserConfiguration,
    ) -> Vec<(PluginId, Option<ClientId>)> {
        if self.cached_plugin_map.is_empty() {
            self.cached_plugin_map = self.plugin_map.lock().unwrap().clone_plugin_assets();
        }
        match self
            .cached_plugin_map
            .get(plugin_location)
            .and_then(|m| m.get(plugin_configuration))
        {
            Some(plugin_and_client_ids) => plugin_and_client_ids
                .iter()
                .map(|(plugin_id, client_id)| (*plugin_id, Some(*client_id)))
                .collect(),
            None => vec![],
        }
    }
    pub fn all_plugin_ids(&self) -> Vec<(PluginId, ClientId)> {
        self.plugin_map.lock().unwrap().all_plugin_ids()
    }
    fn size_of_plugin_id(&self, plugin_id: PluginId) -> Option<(usize, usize)> {
        // (rows/colums)
        self.plugin_map
            .lock()
            .unwrap()
            .get_running_plugin(plugin_id, None)
            .map(|r| {
                let r = r.lock().unwrap();
                (r.rows, r.columns)
            })
    }
    fn cwd_of_plugin_id(&self, plugin_id: PluginId) -> Option<PathBuf> {
        self.plugin_map
            .lock()
            .unwrap()
            .get_running_plugin(plugin_id, None)
            .map(|r| {
                let r = r.lock().unwrap();
                r.store.data().plugin_cwd.clone()
            })
    }
    fn plugin_config_of_plugin_id(&self, plugin_id: PluginId) -> Option<PluginConfig> {
        self.plugin_map
            .lock()
            .unwrap()
            .get_running_plugin(plugin_id, None)
            .map(|r| {
                let r = r.lock().unwrap();
                r.store.data().plugin.clone()
            })
    }
    fn tab_index_of_plugin_id(&self, plugin_id: PluginId) -> Option<usize> {
        self.plugin_map
            .lock()
            .unwrap()
            .get_running_plugin(plugin_id, None)
            .and_then(|r| {
                let r = r.lock().unwrap();
                r.store.data().tab_index
            })
    }
    fn start_plugin_loading_indication(
        &self,
        plugin_ids: &[PluginId],
        loading_indication: &LoadingIndication,
    ) {
        for plugin_id in plugin_ids {
            let _ = self
                .senders
                .send_to_screen(ScreenInstruction::StartPluginLoadingIndication(
                    *plugin_id,
                    loading_indication.clone(),
                ));
            let _ = self
                .senders
                .send_to_background_jobs(BackgroundJob::AnimatePluginLoading(*plugin_id));
        }
    }
    pub fn post_messages_to_plugin_worker(
        &mut self,
        plugin_id: PluginId,
        client_id: ClientId,
        worker_name: String,
        mut messages: Vec<(String, String)>,
    ) -> Result<()> {
        let worker =
            self.plugin_map
                .lock()
                .unwrap()
                .worker_sender(plugin_id, client_id, &worker_name);
        match worker {
            Some(worker) => {
                for (message, payload) in messages.drain(..) {
                    if let Err(e) = worker.send(MessageToWorker::Message(message, payload)) {
                        log::error!("Failed to send message to worker: {:?}", e);
                    }
                }
            },
            None => {
                log::warn!("Worker {worker_name} not found, caching messages");
                for (message, payload) in messages.drain(..) {
                    self.cached_worker_messages
                        .entry(plugin_id)
                        .or_default()
                        .push((client_id, worker_name.clone(), message, payload));
                }
            },
        }
        Ok(())
    }
    pub fn start_fs_watcher_if_not_started(&mut self) {
        if self.watcher.is_none() {
            self.watcher = match watch_filesystem(self.senders.clone(), &self.zellij_cwd) {
                Ok(watcher) => Some(watcher),
                Err(e) => {
                    log::error!("Failed to watch filesystem: {:?}", e);
                    None
                },
            };
        }
    }
    pub fn cache_plugin_permissions(
        &mut self,
        plugin_id: PluginId,
        client_id: Option<ClientId>,
        permissions: Vec<PermissionType>,
        status: PermissionStatus,
        cache_path: Option<PathBuf>,
    ) -> Result<()> {
        let err_context = || format!("Failed to write plugin permission {plugin_id}");

        let running_plugin = self
            .plugin_map
            .lock()
            .unwrap()
            .get_running_plugin(plugin_id, client_id)
            .ok_or_else(|| anyhow!("Failed to get running plugin"))?;

        let mut running_plugin = running_plugin.lock().unwrap();

        let permissions = if status == PermissionStatus::Granted {
            permissions
        } else {
            vec![]
        };

        running_plugin
            .store
            .data_mut()
            .set_permissions(HashSet::from_iter(permissions.clone()));

        let mut permission_cache = PermissionCache::from_path_or_default(cache_path);
        permission_cache.cache(
            running_plugin.store.data().plugin.location.to_string(),
            permissions,
        );

        permission_cache.write_to_file().with_context(err_context)
    }
    pub fn cache_plugin_events(&mut self, plugin_id: PluginId) {
        self.plugin_ids_waiting_for_permission_request
            .insert(plugin_id);
        self.cached_events_for_pending_plugins
            .entry(plugin_id)
            .or_default();
    }

    // gets all running plugins details matching this run_plugin, if none are running, loads one and
    // returns its details
    #[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
    pub fn get_or_load_plugins(
        &mut self,
        run_plugin_or_alias: RunPluginOrAlias,
        size: Size,
        cwd: Option<PathBuf>,
        skip_cache: bool,
        should_float: bool,
        should_be_open_in_place: bool,
        pane_title: Option<String>,
        pane_id_to_replace: Option<PaneId>,
        cli_client_id: Option<ClientId>,
        floating_pane_coordinates: Option<FloatingPaneCoordinates>,
        should_focus: bool,
    ) -> Vec<(PluginId, Option<ClientId>)> {
        let run_plugin = run_plugin_or_alias.get_run_plugin();
        match run_plugin {
            Some(run_plugin) => {
                let all_plugin_ids = self.all_plugin_and_client_ids_for_plugin_location(
                    &run_plugin.location,
                    &run_plugin.configuration,
                );
                if all_plugin_ids.is_empty() {
                    if let Some(loading_plugin_id) = self.plugin_id_of_loading_plugin(
                        &run_plugin.location,
                        &run_plugin.configuration,
                    ) {
                        return vec![(loading_plugin_id, None)];
                    }
                    match self.load_plugin(
                        &Some(run_plugin),
                        None,
                        size,
                        cwd.clone(),
                        skip_cache,
                        cli_client_id,
                    ) {
                        Ok((plugin_id, client_id)) => {
                            let start_suppressed = false;
                            drop(self.senders.send_to_screen(ScreenInstruction::AddPlugin(
                                Some(should_float),
                                should_be_open_in_place,
                                false, // close_replaced_pane
                                run_plugin_or_alias,
                                pane_title,
                                None,
                                plugin_id,
                                pane_id_to_replace,
                                cwd,
                                start_suppressed,
                                floating_pane_coordinates,
                                Some(should_focus),
                                Some(client_id),
                                None,
                            )));
                            vec![(plugin_id, Some(client_id))]
                        },
                        Err(e) => {
                            log::error!("Failed to load plugin: {e}");
                            if let Some(cli_client_id) = cli_client_id {
                                let _ = self.senders.send_to_server(ServerInstruction::LogError(
                                    vec![format!("Failed to log plugin: {e}")],
                                    cli_client_id,
                                    None,
                                ));
                            }
                            vec![]
                        },
                    }
                } else {
                    all_plugin_ids
                }
            },
            None => {
                log::error!("Plugin not found for alias");
                vec![]
            },
        }
    }
    pub fn clear_plugin_map_cache(&mut self) {
        self.cached_plugin_map.clear();
    }
    // returns the pipe names to unblock
    pub fn update_cli_pipe_state(
        &mut self,
        pipe_state_changes: Vec<PluginRenderAsset>,
    ) -> Vec<String> {
        let mut pipe_names_to_unblock = vec![];
        for pipe_state_change in pipe_state_changes {
            let client_id = pipe_state_change.client_id;
            let plugin_id = pipe_state_change.plugin_id;
            for (cli_pipe_name, pipe_state_change) in pipe_state_change.cli_pipes {
                pipe_names_to_unblock.append(&mut self.pending_pipes.update_pipe_state_change(
                    &cli_pipe_name,
                    pipe_state_change,
                    &plugin_id,
                    &client_id,
                ));
            }
        }
        let pipe_names_to_unblock =
            pipe_names_to_unblock
                .into_iter()
                .fold(HashSet::new(), |mut acc, p| {
                    acc.insert(p);
                    acc
                });
        pipe_names_to_unblock.into_iter().collect()
    }
    fn message_is_directed_at_plugin(
        message_pid: Option<PluginId>,
        message_cid: Option<ClientId>,
        plugin_id: &PluginId,
        client_id: &ClientId,
    ) -> bool {
        message_pid.is_none() && message_cid.is_none()
            || (message_pid.is_none() && message_cid == Some(*client_id))
            || (message_cid.is_none() && message_pid == Some(*plugin_id))
            || (message_cid == Some(*client_id) && message_pid == Some(*plugin_id))
    }
    fn is_refreshable_status_bar_state(
        message_pid: Option<PluginId>,
        message_cid: Option<ClientId>,
        event: &Event,
    ) -> bool {
        message_pid.is_some()
            && message_cid.is_some()
            && matches!(event,
                Event::CustomMessage(message, _)
                    if message == crate::screen::VC_FLEET_LIVE_COUNT_MESSAGE
                        || message == crate::screen::VC_STATUS_BAR_VISIBILITY_MESSAGE)
    }
    fn update_parked_chrome_target(
        &mut self,
        message_pid: Option<PluginId>,
        message_cid: Option<ClientId>,
        event: &Event,
    ) {
        let (Some(plugin_id), Some(client_id)) = (message_pid, message_cid) else {
            return;
        };
        match event {
            Event::CustomMessage(message, payload)
                if message == crate::screen::VC_STATUS_BAR_VISIBILITY_MESSAGE =>
            {
                match payload.as_str() {
                    "false" => {
                        self.parked_chrome_plugin_clients
                            .insert((plugin_id, client_id));
                    },
                    "true" => {
                        self.parked_chrome_plugin_clients
                            .remove(&(plugin_id, client_id));
                    },
                    _ => {},
                }
            },
            Event::CustomMessage(message, _)
                if message == crate::screen::VC_FLEET_LIVE_COUNT_MESSAGE =>
            {
                self.parked_chrome_plugin_clients
                    .remove(&(plugin_id, client_id));
            },
            _ => {},
        }
    }
    fn is_parked_chrome_state_payload(
        &self,
        plugin_id: PluginId,
        client_id: ClientId,
        event: &Event,
    ) -> bool {
        self.parked_chrome_plugin_clients
            .contains(&(plugin_id, client_id))
            && matches!(event, Event::SessionUpdate(..) | Event::PaneUpdate(..))
    }
    pub fn client_is_connected(&self, client_id: &ClientId) -> bool {
        self.connected_clients.lock().unwrap().contains(client_id)
    }
    pub fn get_first_client_id(&self) -> Option<ClientId> {
        self.connected_clients
            .lock()
            .unwrap()
            .iter()
            .next()
            .copied()
    }
    pub fn update_available_layouts(
        &mut self,
        layouts: Vec<LayoutInfo>,
        errors: Vec<LayoutWithError>,
    ) {
        // Diff with existing layouts
        if self.available_layouts != layouts || self.available_layout_errors != errors {
            // Update the stored layouts
            self.available_layouts = layouts.clone();
            self.available_layout_errors = errors.clone();

            // Notify all plugins of the change
            let _ = self.senders.send_to_plugin(PluginInstruction::Update(vec![(
                None, // Broadcast to all plugins
                None, // Broadcast to all clients
                Event::AvailableLayoutInfo(layouts, errors),
            )]));
        }
    }
    pub fn state_update_for_plugin(&self, plugin_id: PluginId) {
        let _ = self.senders.send_to_plugin(PluginInstruction::Update(vec![(
            Some(plugin_id),
            None,
            Event::AvailableLayoutInfo(
                self.available_layouts.clone(),
                self.available_layout_errors.clone(),
            ),
        )]));
    }
    pub fn detect_and_notify_plugin_config_changes(
        &mut self,
        new_plugins: &PluginAliases,
        shutdown_send: Sender<()>,
    ) -> Result<()> {
        let err_context = || "failed to detect plugin config changes";

        // Get all running plugins
        let running_plugins = self.plugin_map.lock().unwrap().running_plugins();

        for (plugin_id, client_id, running_plugin) in running_plugins {
            let running_plugin = running_plugin.lock().unwrap();
            let plugin_env = &running_plugin.store.data();
            let current_config = &plugin_env.plugin.initial_userspace_configuration;
            let plugin_location = &plugin_env.plugin.location;

            // Look up this plugin in the new config by location
            // Note: PluginAliases is HashMap<String, RunPlugin>, so we need to iterate
            let new_config_for_location = new_plugins
                .aliases
                .values()
                .find(|run_plugin| &run_plugin.location == plugin_location)
                .map(|run_plugin| &run_plugin.configuration);

            if let Some(new_config) = new_config_for_location {
                // Compare configurations - only fire event if changed
                if current_config != new_config {
                    drop(running_plugin); // Release lock before sending

                    let event = Event::PluginConfigurationChanged(new_config.inner().clone());
                    let updates = vec![(Some(plugin_id), Some(client_id), event)];
                    self.update_plugins(updates, shutdown_send.clone())
                        .with_context(err_context)?;
                }
            }
        }

        Ok(())
    }
}

fn layout_plugin_receipt_ids(receipt: &LayoutPluginReceipt) -> &[PluginId] {
    match receipt {
        LayoutPluginReceipt::Activated { plugin_ids }
        | LayoutPluginReceipt::Released { plugin_ids }
        | LayoutPluginReceipt::Compensated { plugin_ids }
        | LayoutPluginReceipt::ActivationRolledBack { plugin_ids, .. } => plugin_ids,
    }
}

fn enqueue_reserved_layout_plugin(
    activation_job: LayoutPluginActivationJob,
) -> std::result::Result<(), String> {
    #[cfg(test)]
    let test_hooks = activation_job.test_hooks.clone();
    let LayoutPluginActivationJob {
        plugin_executor,
        senders,
        plugin_map_for_cleanup,
        transaction_id,
        plugin,
        loading_context,
        group_plugin_ids,
        cancellation,
        activation_gate,
        activation_guards,
        ..
    } = activation_job;
    let plugin_cancellation = plugin.cancellation.clone();
    if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
        return Err(format!(
            "layout plugin transaction {transaction_id} was cancelled before enqueue"
        ));
    }

    let plugin_id = plugin.plugin_id;
    let client_id = plugin.client_id;
    let skip_cache = plugin.skip_cache;
    let plugin_name = plugin.run_plugin.location.to_string();
    let panic_senders = senders.clone();
    let panic_plugin_map = plugin_map_for_cleanup.clone();
    let panic_group_plugin_ids = group_plugin_ids.clone();
    let panic_cancellation = cancellation.clone();
    let plugin_data_dir = loading_context.plugin_own_data_dir.clone();
    let panic_plugin_data_dir = plugin_data_dir.clone();
    plugin_executor.try_execute_plugin_load(
        plugin_id,
        move |senders, plugin_map, connected_clients, plugin_cache, engine| {
            let activation_guards = activation_guards;
            activation_gate.wait();
            if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                remove_layout_plugin_data_dir(&plugin_data_dir);
                drop(activation_guards);
                return;
            }

            let mut loading_indication = LoadingIndication::new(plugin_name);
            let load_result = {
                let mut plugin_map = plugin_map
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                    None
                } else {
                    #[cfg(test)]
                    if let Some(gate) = &test_hooks.before_load_gate {
                        gate.entered.wait();
                        gate.release.wait();
                    }
                    if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                        None
                    } else {
                        #[cfg(test)]
                        test_hooks.load_starts.fetch_add(1, Ordering::SeqCst);
                        let result = PluginLoader::new(
                            skip_cache,
                            loading_context,
                            senders.clone(),
                            engine,
                            plugin_cache,
                            &mut plugin_map,
                            connected_clients,
                        )
                        .start_plugin();
                        if result.is_err()
                            || cancellation.is_cancelled()
                            || plugin_cancellation.is_cancelled()
                        {
                            let ids_to_remove = if cancellation.is_cancelled() {
                                group_plugin_ids.as_slice()
                            } else {
                                std::slice::from_ref(&plugin_id)
                            };
                            for plugin_id in ids_to_remove {
                                plugin_map.remove_plugins(*plugin_id);
                            }
                        }
                        Some(result)
                    }
                }
            };
            plugin_map.clear_poison();

            if load_result.is_none()
                || cancellation.is_cancelled()
                || plugin_cancellation.is_cancelled()
            {
                let ids_to_remove = if cancellation.is_cancelled() {
                    group_plugin_ids.as_slice()
                } else {
                    std::slice::from_ref(&plugin_id)
                };
                let plugin_list = remove_layout_plugin_group_from_map(&plugin_map, ids_to_remove);
                remove_layout_plugin_data_dir(&plugin_data_dir);
                drop(activation_guards);
                notify_layout_plugin_group_cleanup(&senders, ids_to_remove, plugin_list);
                return;
            }

            match load_result.expect("checked as Some above") {
                Ok(()) => {
                    if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                        let ids_to_remove = if cancellation.is_cancelled() {
                            group_plugin_ids.as_slice()
                        } else {
                            std::slice::from_ref(&plugin_id)
                        };
                        let plugin_list =
                            remove_layout_plugin_group_from_map(&plugin_map, ids_to_remove);
                        remove_layout_plugin_data_dir(&plugin_data_dir);
                        drop(activation_guards);
                        notify_layout_plugin_group_cleanup(&senders, ids_to_remove, plugin_list);
                        return;
                    }
                    let plugin_list = plugin_map
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .list_plugins();
                    plugin_map.clear_poison();
                    drop(activation_guards);
                    if cancellation.is_cancelled() || plugin_cancellation.is_cancelled() {
                        let ids_to_remove = if cancellation.is_cancelled() {
                            group_plugin_ids.as_slice()
                        } else {
                            std::slice::from_ref(&plugin_id)
                        };
                        cleanup_layout_plugin_group_shared(&senders, &plugin_map, ids_to_remove);
                        remove_layout_plugin_data_dir(&plugin_data_dir);
                        return;
                    }
                    handle_plugin_successful_loading(&senders, plugin_id, plugin_list);
                    let mut followup_instructions =
                        vec![PluginInstruction::RequestStateUpdateForPlugin(plugin_id)];
                    if !cancellation.is_cancelled() && !plugin_cancellation.is_cancelled() {
                        followup_instructions.push(PluginInstruction::ApplyCachedEvents {
                            plugin_ids: vec![plugin_id],
                            done_receiving_permissions: false,
                        });
                    }
                    send_plugin_instructions_off_pinned_executor(&senders, followup_instructions);
                },
                Err(error) => {
                    cancellation.cancel();
                    let plugin_list =
                        remove_layout_plugin_group_from_map(&plugin_map, &group_plugin_ids);
                    remove_layout_plugin_data_dir(&plugin_data_dir);
                    drop(activation_guards);
                    notify_layout_plugin_group_cleanup(&senders, &group_plugin_ids, plugin_list);
                    handle_plugin_loading_failure(
                        &senders,
                        plugin_id,
                        &mut loading_indication,
                        &error,
                        Some(client_id),
                    );
                    report_layout_plugin_activation_failure(
                        &senders,
                        transaction_id,
                        group_plugin_ids,
                        format!("plugin load failed: {error:#}"),
                    );
                },
            }
        },
        move |panic_message| {
            panic_cancellation.cancel();
            remove_layout_plugin_data_dir(&panic_plugin_data_dir);
            cleanup_layout_plugin_group_shared(
                &panic_senders,
                &panic_plugin_map,
                &panic_group_plugin_ids,
            );
            let mut loading_indication =
                LoadingIndication::new(format!("layout plugin {plugin_id}"));
            handle_plugin_loading_failure(
                &panic_senders,
                plugin_id,
                &mut loading_indication,
                &panic_message,
                Some(client_id),
            );
            report_layout_plugin_activation_failure(
                &panic_senders,
                transaction_id,
                panic_group_plugin_ids,
                format!("plugin executor panicked: {panic_message}"),
            );
        },
    )
}

fn cleanup_layout_plugin_group_shared(
    senders: &ThreadSenders,
    plugin_map: &Arc<Mutex<PluginMap>>,
    plugin_ids: &[PluginId],
) {
    let plugin_list = remove_layout_plugin_group_from_map(plugin_map, plugin_ids);
    notify_layout_plugin_group_cleanup(senders, plugin_ids, plugin_list);
}

fn remove_layout_plugin_group_from_map(
    plugin_map: &Arc<Mutex<PluginMap>>,
    plugin_ids: &[PluginId],
) -> BTreeMap<PluginId, RunPlugin> {
    let plugin_list = {
        let mut plugin_map_guard = plugin_map
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for plugin_id in plugin_ids {
            plugin_map_guard.remove_plugins(*plugin_id);
        }
        plugin_map_guard.list_plugins()
    };
    plugin_map.clear_poison();
    plugin_list
}

fn notify_layout_plugin_group_cleanup(
    senders: &ThreadSenders,
    plugin_ids: &[PluginId],
    plugin_list: BTreeMap<PluginId, RunPlugin>,
) {
    for plugin_id in plugin_ids {
        let _ =
            senders.send_to_background_jobs(BackgroundJob::StopPluginLoadingAnimation(*plugin_id));
    }
    let _ = senders.send_to_background_jobs(BackgroundJob::ReportPluginList(plugin_list));
    let _ = senders.send_to_screen(ScreenInstruction::RequestStateUpdateForPlugins);
}

fn remove_layout_plugin_data_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        log::error!(
            "failed to remove layout plugin data dir {}: {error}",
            path.display()
        );
    }
}

fn report_layout_plugin_activation_failure(
    senders: &ThreadSenders,
    transaction_id: LayoutTransactionId,
    plugin_ids: Vec<PluginId>,
    message: String,
) {
    send_plugin_instructions_off_pinned_executor(
        senders,
        vec![PluginInstruction::LayoutPluginActivationFailed {
            transaction_id,
            plugin_ids,
            message,
        }],
    );
}

fn send_plugin_instructions_off_pinned_executor(
    senders: &ThreadSenders,
    instructions: Vec<PluginInstruction>,
) {
    let senders = senders.clone();
    get_tokio_runtime().spawn_blocking(move || {
        for instruction in instructions {
            if let Err(error) = senders.send_to_plugin(instruction) {
                log::error!(
                    "failed to deliver layout plugin follow-up outside the pinned executor: {error:#}"
                );
                break;
            }
        }
    });
}

fn handle_plugin_successful_loading(
    senders: &ThreadSenders,
    plugin_id: PluginId,
    plugin_list: BTreeMap<PluginId, RunPlugin>,
) {
    let _ = senders.send_to_background_jobs(BackgroundJob::StopPluginLoadingAnimation(plugin_id));
    let _ = senders.send_to_screen(ScreenInstruction::RequestStateUpdateForPlugins);
    let _ = senders.send_to_background_jobs(BackgroundJob::ReportPluginList(plugin_list));
}

fn handle_plugin_loading_failure(
    senders: &ThreadSenders,
    plugin_id: PluginId,
    loading_indication: &mut LoadingIndication,
    error: impl std::fmt::Debug,
    client_id: Option<ClientId>,
) {
    log::error!("{:?}", error);
    let _ = senders.send_to_background_jobs(BackgroundJob::StopPluginLoadingAnimation(plugin_id));
    loading_indication.indicate_loading_error(format!("{:?}", error));
    let _ = senders.send_to_screen(ScreenInstruction::UpdatePluginLoadingStage(
        plugin_id,
        loading_indication.clone(),
    ));
    if let Some(client_id) = client_id {
        let _ = senders.send_to_server(ServerInstruction::LogError(
            vec![format!("{:?}", error)],
            client_id,
            None,
        ));
    }
}

// TODO: move to permissions?
fn check_event_permission(
    plugin_env: &PluginEnv,
    event: &Event,
) -> (PermissionStatus, Option<PermissionType>) {
    if plugin_env.plugin.is_builtin() {
        // built-in plugins can do all the things because they're part of the application and
        // there's no use to deny them anything
        return (PermissionStatus::Granted, None);
    }
    let permission = match event {
        Event::ModeUpdate(..)
        | Event::TabUpdate(..)
        | Event::PaneUpdate(..)
        | Event::SessionUpdate(..)
        | Event::CopyToClipboard(..)
        | Event::SystemClipboardFailure
        | Event::CommandPaneOpened(..)
        | Event::CommandPaneExited(..)
        | Event::PaneClosed(..)
        | Event::EditPaneOpened(..)
        | Event::EditPaneExited(..)
        | Event::FailedToWriteConfigToDisk(..)
        | Event::CommandPaneReRun(..)
        | Event::CwdChanged(..)
        | Event::CommandChanged(..)
        | Event::AvailableLayoutInfo(..)
        | Event::PluginConfigurationChanged(..)
        | Event::HighlightClicked { .. }
        | Event::InputReceived => PermissionType::ReadApplicationState,
        Event::WebServerStatus(..) => PermissionType::StartWebServer,
        Event::PaneRenderReport(..) | Event::PaneRenderReportWithAnsi(..) => {
            PermissionType::ReadPaneContents
        },
        Event::UserAction(..) => PermissionType::InterceptInput,
        _ => return (PermissionStatus::Granted, None),
    };

    if let Some(permissions) = plugin_env.permissions.lock().unwrap().as_ref()
        && permissions.contains(&permission)
    {
        return (PermissionStatus::Granted, None);
    }

    (PermissionStatus::Denied, Some(permission))
}

fn apply_event_to_plugin(
    plugin_id: PluginId,
    client_id: ClientId,
    running_plugin: &mut RunningPlugin,
    event: &Event,
    plugin_render_assets: &mut Vec<PluginRenderAsset>,
    senders: ThreadSenders,
    plugin_subscriptions: &HashSet<EventType>,
) -> Result<(bool, bool)> {
    let instance = &running_plugin.instance;
    let rows = running_plugin.rows;
    let columns = running_plugin.columns;

    let err_context = || format!("Failed to apply event to plugin {plugin_id}");
    let mut rendered_event = false;
    let mut empty_rendered_event = false;
    match check_event_permission(running_plugin.store.data(), event) {
        (PermissionStatus::Granted, _) => {
            let mut event = event.clone();
            if let Event::ModeUpdate(mode_info) = &mut event {
                mode_info.base_mode = Some(running_plugin.store.data().default_mode);
                if plugin_subscriptions.contains(&EventType::InitialKeybinds) {
                    // Plugin caches keybindings via InitialKeybinds — send lightweight ModeUpdate
                    mode_info.keybinds = vec![];
                } else {
                    // Legacy plugin — send full keybindings as before
                    mode_info.keybinds = running_plugin.store.data().keybinds.to_keybinds_vec();
                }
            }
            let protobuf_event: Result<ProtobufEvent, _> = event.clone().try_into();
            match protobuf_event {
                Ok(protobuf_event) => {
                    let update = instance
                        .get_typed_func::<(), i32>(&mut running_plugin.store, "update")
                        .with_context(err_context)?;
                    wasi_write_object(running_plugin.store.data(), &protobuf_event.encode_to_vec())
                        .with_context(err_context)?;
                    let should_render = update
                        .call(&mut running_plugin.store, ())
                        .with_context(err_context)?;
                    let mut should_render = should_render == 1;
                    if let Event::PermissionRequestResult(..) = event {
                        // we always render in this case, otherwise the request permission screen stays on
                        // screen
                        should_render = true;
                    }
                    if rows > 0 && columns > 0 && should_render {
                        let rendered_bytes = instance
                            .get_typed_func::<(i32, i32), ()>(&mut running_plugin.store, "render")
                            .and_then(|render| {
                                render
                                    .call(&mut running_plugin.store, (rows as i32, columns as i32))
                            })
                            .map_err(|e| anyhow!(e))
                            .and_then(|_| {
                                wasi_read_string(running_plugin.store.data())
                                    .map_err(|e| anyhow!(e))
                            })
                            .with_context(err_context)?;
                        rendered_event = true;
                        empty_rendered_event = rendered_bytes.is_empty();
                        let pipes_to_block_or_unblock =
                            pipes_to_block_or_unblock(running_plugin, None);
                        let plugin_render_asset = PluginRenderAsset::new(
                            plugin_id,
                            client_id,
                            rendered_bytes.as_bytes().to_vec(),
                        )
                        .with_pipes(pipes_to_block_or_unblock);
                        plugin_render_assets.push(plugin_render_asset);
                    } else {
                        // This is a bit of a hack to get around the fact that plugins are allowed not to
                        // render and still unblock CLI pipes
                        let pipes_to_block_or_unblock =
                            pipes_to_block_or_unblock(running_plugin, None);
                        let plugin_render_asset =
                            PluginRenderAsset::new(plugin_id, client_id, vec![])
                                .with_pipes(pipes_to_block_or_unblock);
                        let _ = senders
                            .send_to_plugin(PluginInstruction::UnblockCliPipes(vec![
                                plugin_render_asset,
                            ]))
                            .context("failed to unblock input pipe");
                    }
                },
                Err(e) => {
                    log::error!("Failed to convert to protobuf: {:?}", e);
                },
            }
        },
        (PermissionStatus::Denied, permission) => {
            log::error!(
                "PluginId '{}' permission '{}' is not allowed - Event '{:?}' denied",
                plugin_id,
                permission
                    .map(|p| p.to_string())
                    .unwrap_or("UNKNOWN".to_owned()),
                EventType::from_str(&event.to_string()).with_context(err_context)?
            );
        },
    }
    Ok((rendered_event, empty_rendered_event))
}

pub fn handle_plugin_crash(plugin_id: PluginId, message: String, senders: ThreadSenders) {
    let mut loading_indication = LoadingIndication::new("Panic!".to_owned());
    loading_indication.indicate_loading_error(message);
    let _ = senders.send_to_screen(ScreenInstruction::UpdatePluginLoadingStage(
        plugin_id,
        loading_indication,
    ));
}

pub fn apply_before_close_event_to_plugin(
    plugin_id: PluginId,
    running_plugin: &mut RunningPlugin,
) -> Result<()> {
    let instance = &running_plugin.instance;

    let err_context = || format!("Failed to apply event to plugin {plugin_id}");
    let event = Event::BeforeClose;
    let protobuf_event: ProtobufEvent = event
        .clone()
        .try_into()
        .map_err(|e| anyhow!("Failed to convert to protobuf: {:?}", e))?;
    let update = instance
        .get_typed_func::<(), i32>(&mut running_plugin.store, "update")
        .with_context(err_context)?;
    wasi_write_object(running_plugin.store.data(), &protobuf_event.encode_to_vec())
        .with_context(err_context)?;
    let _should_render = update
        .call(&mut running_plugin.store, ())
        .with_context(err_context)?;
    // Terminal cleanup unblocks all pending pipes for this plugin directly on
    // the WasmBridge thread after this executor receipt resolves. Sending a
    // message back into the bounded Plugin channel here could deadlock the
    // pinned executor behind its own full input queue.
    Ok(())
}

#[cfg(test)]
mod layout_plugin_transaction_tests {
    use super::*;
    use zellij_utils::input::layout::RunPlugin;

    fn test_bridge(max_threads: usize) -> WasmBridge {
        test_bridge_with_senders(max_threads, ThreadSenders::default())
    }

    fn test_bridge_with_senders(max_threads: usize, senders: ThreadSenders) -> WasmBridge {
        let engine = Engine::default();
        let plugin_dir = tempfile::tempdir().unwrap().path().to_path_buf();
        let zellij_cwd = tempfile::tempdir().unwrap().path().to_path_buf();
        let mut bridge = WasmBridge::new(
            senders,
            engine.clone(),
            plugin_dir,
            PathBuf::from("/bin/sh"),
            zellij_cwd,
            BTreeMap::new(),
            None,
            None,
            vec![],
            vec![],
            InputMode::Normal,
            Keybinds::default(),
        );
        let plugin_cache = Arc::new(Mutex::new(HashMap::new()));
        bridge.plugin_executor = Arc::new(PinnedExecutor::new(
            max_threads,
            &bridge.senders,
            &bridge.plugin_map,
            &bridge.connected_clients,
            &plugin_cache,
            &engine,
        ));
        bridge
    }

    fn local_request(client_id: ClientId) -> LayoutPluginReservationRequest {
        let run_plugin = RunPlugin::from_url(&format!(
            "file:{}/vc-frame-missing-layout-plugin-{client_id}.wasm",
            std::env::temp_dir().display()
        ))
        .unwrap();
        LayoutPluginReservationRequest {
            run_plugin,
            tab_index: Some(1),
            size: Size::default(),
            cwd: None,
            skip_cache: false,
            client_id,
        }
    }

    fn remote_request(client_id: ClientId) -> LayoutPluginReservationRequest {
        LayoutPluginReservationRequest {
            run_plugin: RunPlugin::from_url(
                "https://10.255.255.1/vc-frame-cancelled-layout-plugin.wasm",
            )
            .unwrap(),
            tab_index: Some(1),
            size: Size::default(),
            cwd: None,
            skip_cache: false,
            client_id,
        }
    }

    #[test]
    fn event_diagnostics_count_dispatches_and_renders_without_dynamic_slots() {
        let diagnostics = PluginEventDiagnostics::default();

        for _ in 0..120 {
            diagnostics.record_at(
                7,
                3,
                PluginEventDiagnosticKind::PaneUpdate,
                false,
                false,
                100,
            );
        }
        for _ in 0..4 {
            diagnostics.record_at(
                7,
                3,
                PluginEventDiagnosticKind::SessionUpdate,
                true,
                false,
                100,
            );
        }
        diagnostics.record_at(
            7,
            3,
            PluginEventDiagnosticKind::SessionUpdate,
            true,
            true,
            100,
        );

        assert_eq!(
            diagnostics.counts(7, 3, PluginEventDiagnosticKind::PaneUpdate),
            (120, 0, 0)
        );
        assert_eq!(
            diagnostics.counts(7, 3, PluginEventDiagnosticKind::SessionUpdate),
            (5, 5, 1)
        );
    }

    #[test]
    fn event_diagnostics_drop_fixed_table_collisions_instead_of_reassigning() {
        let diagnostics = PluginEventDiagnostics::default();
        diagnostics.record_at(
            1,
            2,
            PluginEventDiagnosticKind::PaneUpdate,
            false,
            false,
            100,
        );
        diagnostics.record_at(
            1 + PLUGIN_EVENT_DIAGNOSTIC_SLOTS as PluginId,
            2,
            PluginEventDiagnosticKind::PaneUpdate,
            true,
            false,
            100,
        );

        assert_eq!(
            diagnostics.counts(1, 2, PluginEventDiagnosticKind::PaneUpdate),
            (1, 0, 0)
        );
        assert_eq!(
            diagnostics.counts(
                1 + PLUGIN_EVENT_DIAGNOSTIC_SLOTS as PluginId,
                2,
                PluginEventDiagnosticKind::PaneUpdate,
            ),
            (0, 0, 0)
        );
    }

    #[test]
    fn refreshable_status_bar_state_bypasses_the_pending_plugin_cache() {
        let mut bridge = test_bridge(1);
        let plugin_id = 77;
        let client_id = 2;
        bridge
            .cached_events_for_pending_plugins
            .insert(plugin_id, vec![]);
        let (shutdown_sender, _shutdown_receiver) = tokio::sync::mpsc::channel(1);

        bridge
            .update_plugins(
                vec![
                    (
                        Some(plugin_id),
                        Some(client_id),
                        Event::CustomMessage(
                            crate::screen::VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
                            "4".to_owned(),
                        ),
                    ),
                    (
                        Some(plugin_id),
                        Some(client_id),
                        Event::CustomMessage(
                            crate::screen::VC_STATUS_BAR_VISIBILITY_MESSAGE.to_owned(),
                            "false".to_owned(),
                        ),
                    ),
                ],
                shutdown_sender,
            )
            .unwrap();

        assert!(
            bridge
                .cached_events_for_pending_plugins
                .get(&plugin_id)
                .unwrap()
                .is_empty(),
            "refreshable state must be regenerated after load, not trapped in a shared cache"
        );
        assert!(!WasmBridge::is_refreshable_status_bar_state(
            Some(plugin_id),
            None,
            &Event::CustomMessage(
                crate::screen::VC_STATUS_BAR_VISIBILITY_MESSAGE.to_owned(),
                "false".to_owned(),
            ),
        ));
    }

    #[test]
    fn exact_chrome_lifecycle_parks_heavy_state_payloads() {
        let mut bridge = test_bridge(1);
        let plugin_id = 77;
        let client_id = 2;

        bridge.update_parked_chrome_target(
            Some(plugin_id),
            Some(client_id),
            &Event::CustomMessage(
                crate::screen::VC_STATUS_BAR_VISIBILITY_MESSAGE.to_owned(),
                "false".to_owned(),
            ),
        );
        assert!(bridge.is_parked_chrome_state_payload(
            plugin_id,
            client_id,
            &Event::PaneUpdate(Default::default()),
        ));
        assert!(bridge.is_parked_chrome_state_payload(
            plugin_id,
            client_id,
            &Event::SessionUpdate(vec![], vec![]),
        ));

        bridge.update_parked_chrome_target(
            Some(plugin_id),
            Some(client_id),
            &Event::CustomMessage(
                crate::screen::VC_FLEET_LIVE_COUNT_MESSAGE.to_owned(),
                "1".to_owned(),
            ),
        );
        assert!(!bridge.is_parked_chrome_state_payload(
            plugin_id,
            client_id,
            &Event::SessionUpdate(vec![], vec![]),
        ));
    }

    #[test]
    fn reservation_allocates_exact_ids_without_runtime_work() {
        let mut bridge = test_bridge(1);
        let request = local_request(9101);
        let plugin_config = PluginConfig::from_run_plugin(&request.run_plugin).unwrap();
        let ids = bridge
            .reserve_layout_plugins(1001, vec![request.clone()])
            .unwrap();

        assert_eq!(ids, vec![0]);
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(bridge.cached_events_for_pending_plugins.is_empty());
        assert!(bridge.cached_resizes_for_pending_plugins.is_empty());
        assert!(bridge.loading_plugins.is_empty());
        assert!(!bridge.plugin_executor.has_assignment(0));
        let loading_context = LoadingContext::new(
            &bridge,
            None,
            plugin_config,
            0,
            request.client_id,
            request.tab_index,
            request.size,
        );
        assert!(
            !loading_context.plugin_own_data_dir.exists(),
            "reservation must not create plugin filesystem state"
        );
        assert_eq!(
            bridge
                .layout_plugin_reservations
                .get(&1001)
                .unwrap()
                .tracker
                .active_count(),
            0
        );
    }

    #[test]
    fn exact_cleanup_waits_for_unload_completion_and_removes_all_ownership() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1101;
        let ids = bridge
            .reserve_layout_plugins(
                transaction_id,
                vec![local_request(9201), local_request(9202)],
            )
            .unwrap();
        for plugin_id in &ids {
            bridge.plugin_executor.register_plugin(*plugin_id);
        }

        let receipt = bridge
            .cleanup_layout_plugins(transaction_id, vec![ids[1], ids[0]])
            .unwrap();

        assert_eq!(receipt, ids);
        assert!(
            receipt
                .iter()
                .all(|plugin_id| !bridge.layout_plugin_owners.contains_key(plugin_id))
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(
            !bridge
                .layout_plugin_cleanup_debts
                .contains_key(&transaction_id)
        );
        assert!(
            receipt
                .iter()
                .all(|plugin_id| !bridge.plugin_executor.has_assignment(*plugin_id))
        );
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
    }

    #[test]
    fn foreign_active_owner_and_conflicting_retry_preserve_cleanup_debt() {
        let mut bridge = test_bridge(1);
        let owner_transaction_id = 1102;
        let cleanup_transaction_id = 2102;
        let ids = bridge
            .reserve_layout_plugins(
                owner_transaction_id,
                vec![local_request(9203), local_request(9204)],
            )
            .unwrap();

        assert!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, ids.clone())
                .unwrap_err()
                .contains("active foreign layout transaction")
        );
        assert!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, vec![ids[0]])
                .unwrap_err()
                .contains("retry conflict")
        );
        assert!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, vec![ids[0], 999])
                .unwrap_err()
                .contains("retry conflict")
        );
        assert!(
            bridge
                .layout_plugin_reservations
                .contains_key(&owner_transaction_id)
        );
        assert_eq!(
            bridge
                .layout_plugin_cleanup_debts
                .get(&cleanup_transaction_id)
                .unwrap()
                .remaining_plugin_ids,
            ids.iter().copied().collect::<BTreeSet<_>>()
        );
        assert!(
            ids.iter()
                .all(|plugin_id| bridge.layout_plugin_owners.get(plugin_id)
                    == Some(&owner_transaction_id))
        );

        bridge
            .layout_plugin_reservations
            .get_mut(&owner_transaction_id)
            .unwrap()
            .state = LayoutPluginTransactionState::Activated;
        assert_eq!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, ids.clone())
                .unwrap(),
            ids
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&owner_transaction_id)
        );
    }

    #[test]
    fn foreign_activated_cleanup_cancels_before_waiting_for_quiescence() {
        let mut bridge = test_bridge(1);
        let owner_transaction_id = 1106;
        let cleanup_transaction_id = 2106;
        let ids = bridge
            .reserve_layout_plugins(owner_transaction_id, vec![local_request(9209)])
            .unwrap();
        let (tracker, cancellation) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&owner_transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            (
                reservation.tracker.clone(),
                reservation.plugins[0].cancellation.clone(),
            )
        };
        let activation_guard = tracker.begin();
        let release_after_cancellation = std::thread::spawn(move || {
            for _ in 0..500 {
                if cancellation.is_cancelled() {
                    drop(activation_guard);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("cleanup did not cancel the foreign activation before waiting");
        });

        assert_eq!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, ids.clone())
                .unwrap(),
            ids
        );
        release_after_cancellation.join().unwrap();
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
    }

    #[test]
    fn foreign_group_cleanup_cancels_all_loaders_before_group_wait() {
        let mut bridge = test_bridge(1);
        let owner_transaction_id = 1107;
        let cleanup_transaction_id = 2107;
        let ids = bridge
            .reserve_layout_plugins(
                owner_transaction_id,
                vec![local_request(9210), local_request(9211)],
            )
            .unwrap();
        let (tracker, cancellation) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&owner_transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            (
                reservation.tracker.clone(),
                reservation.cancellation.clone(),
            )
        };
        let first_guard = tracker.begin();
        let second_guard = tracker.begin();
        let first_cancellation = cancellation.clone();
        let first_loader = std::thread::spawn(move || {
            for _ in 0..500 {
                if first_cancellation.is_cancelled() {
                    drop(first_guard);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("first loader was not cancelled before group wait");
        });
        let second_loader = std::thread::spawn(move || {
            for _ in 0..500 {
                if cancellation.is_cancelled() {
                    drop(second_guard);
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("second loader was not cancelled before group wait");
        });

        assert_eq!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, ids.clone())
                .unwrap(),
            ids
        );
        first_loader.join().unwrap();
        second_loader.join().unwrap();
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&owner_transaction_id)
        );
    }

    #[test]
    fn cleanup_accepts_general_non_reserved_runtime_plugin() {
        let mut bridge = test_bridge(1);
        let cleanup_transaction_id = 2105;
        let plugin_id = 777;
        let request = local_request(9208);
        bridge.plugin_executor.register_plugin(plugin_id);
        bridge
            .cached_events_for_pending_plugins
            .insert(plugin_id, vec![]);
        bridge
            .cached_resizes_for_pending_plugins
            .insert(plugin_id, (10, 20));
        bridge
            .loading_plugins
            .insert((plugin_id, request.run_plugin));

        assert_eq!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, vec![plugin_id])
                .unwrap(),
            vec![plugin_id]
        );
        assert!(!bridge.plugin_executor.has_assignment(plugin_id));
        assert!(!bridge.layout_plugin_owners.contains_key(&plugin_id));
        assert!(
            !bridge
                .cached_events_for_pending_plugins
                .contains_key(&plugin_id)
        );
        assert!(
            !bridge
                .cached_resizes_for_pending_plugins
                .contains_key(&plugin_id)
        );
        assert!(
            bridge
                .loading_plugins
                .iter()
                .all(|(loading_plugin_id, _)| loading_plugin_id != &plugin_id)
        );
    }

    #[test]
    fn partial_cleanup_error_retains_only_unfinished_debt_for_retry() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1103;
        let ids = bridge
            .reserve_layout_plugins(
                transaction_id,
                vec![local_request(9205), local_request(9206)],
            )
            .unwrap();
        for plugin_id in &ids {
            bridge.plugin_executor.register_plugin(*plugin_id);
        }
        bridge
            .plugin_executor
            .reject_next_enqueue_for_plugin(ids[1]);

        let error = bridge
            .cleanup_layout_plugins(transaction_id, ids.clone())
            .unwrap_err();

        assert!(error.contains(&format!("retained debt for plugin {}", ids[1])));
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert_eq!(
            bridge.layout_plugin_owners.get(&ids[1]),
            Some(&transaction_id)
        );
        assert!(!bridge.plugin_executor.has_assignment(ids[0]));
        assert!(bridge.plugin_executor.has_assignment(ids[1]));
        assert_eq!(
            bridge
                .layout_plugin_cleanup_debts
                .get(&transaction_id)
                .unwrap()
                .remaining_plugin_ids,
            BTreeSet::from([ids[1]])
        );

        assert_eq!(
            bridge
                .cleanup_layout_plugins(transaction_id, ids.clone())
                .unwrap(),
            ids
        );
        assert!(
            !bridge
                .layout_plugin_cleanup_debts
                .contains_key(&transaction_id)
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
    }

    #[test]
    fn duplicate_ids_and_lost_cleanup_ack_replay_the_same_bounded_receipt() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1104;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9207)])
            .unwrap();
        bridge.plugin_executor.register_plugin(ids[0]);

        let first_receipt = bridge
            .cleanup_layout_plugins(transaction_id, vec![ids[0], ids[0]])
            .unwrap();
        let replayed_receipt = bridge
            .cleanup_layout_plugins(transaction_id, vec![ids[0], ids[0]])
            .unwrap();

        assert_eq!(first_receipt, ids);
        assert_eq!(replayed_receipt, first_receipt);
        assert_eq!(
            bridge.layout_plugin_cleanup_receipts.get(&transaction_id),
            Some(&first_receipt)
        );
        assert!(
            bridge
                .cleanup_layout_plugins(transaction_id, vec![ids[0], 999])
                .unwrap_err()
                .contains("replay conflict")
        );
    }

    #[test]
    fn cleanup_receipt_cache_is_bounded_to_512_transactions() {
        let mut bridge = test_bridge(1);
        for transaction_id in 1..=513 {
            bridge
                .cleanup_layout_plugins(transaction_id, vec![])
                .unwrap();
        }

        assert_eq!(
            bridge.layout_plugin_cleanup_receipts.len(),
            MAX_LAYOUT_PLUGIN_CLEANUP_RECEIPTS
        );
        assert!(!bridge.layout_plugin_cleanup_receipts.contains_key(&1));
        assert!(bridge.layout_plugin_cleanup_receipts.contains_key(&513));
    }

    #[test]
    fn release_is_side_effect_free_replayable_and_conflicts_with_activate() {
        let mut bridge = test_bridge(1);
        let ids = bridge
            .reserve_layout_plugins(1002, vec![local_request(9102)])
            .unwrap();
        let resolution = LayoutPluginResolution::Release {
            reason: "PTY rejected prepare".to_owned(),
        };
        let receipt = bridge
            .resolve_layout_plugins(1002, resolution.clone(), ids.clone())
            .unwrap();
        assert_eq!(
            receipt,
            LayoutPluginReceipt::Released {
                plugin_ids: ids.clone()
            }
        );
        assert_eq!(
            bridge
                .resolve_layout_plugins(1002, resolution, ids.clone())
                .unwrap(),
            receipt
        );
        assert!(
            bridge
                .resolve_layout_plugins(1002, LayoutPluginResolution::Activate, ids)
                .unwrap_err()
                .contains("resolution conflict")
        );
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(bridge.cached_events_for_pending_plugins.is_empty());
        assert!(bridge.loading_plugins.is_empty());
    }

    #[test]
    fn activation_failed_can_be_released_with_a_terminal_receipt() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1013;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9114)])
            .unwrap();
        bridge
            .layout_plugin_reservations
            .get_mut(&transaction_id)
            .unwrap()
            .state = LayoutPluginTransactionState::ActivationFailed;
        bridge.plugin_executor.register_plugin(ids[0]);

        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Release {
                        reason: "activation enqueue failed".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Released {
                plugin_ids: ids.clone()
            }
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert!(!bridge.plugin_executor.has_assignment(ids[0]));
    }

    #[test]
    fn async_activation_failure_fully_compensates_and_retires_owner() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1014;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9115)])
            .unwrap();
        bridge
            .layout_plugin_reservations
            .get_mut(&transaction_id)
            .unwrap()
            .state = LayoutPluginTransactionState::Activated;
        bridge.plugin_executor.register_plugin(ids[0]);
        bridge
            .cached_events_for_pending_plugins
            .insert(ids[0], vec![]);
        bridge
            .cached_resizes_for_pending_plugins
            .insert(ids[0], (10, 20));

        bridge.handle_layout_plugin_activation_failure(
            transaction_id,
            ids.clone(),
            "async loader failed".to_owned(),
        );

        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert!(!bridge.plugin_executor.has_assignment(ids[0]));
        assert!(
            !bridge
                .cached_events_for_pending_plugins
                .contains_key(&ids[0])
        );
        assert!(
            !bridge
                .cached_resizes_for_pending_plugins
                .contains_key(&ids[0])
        );
        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Compensate {
                        reason: "Screen observed PTY rollback".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated { plugin_ids: ids }
        );
    }

    #[test]
    fn expected_id_mismatch_is_rejected_without_resolving_reservation() {
        let mut bridge = test_bridge(1);
        let ids = bridge
            .reserve_layout_plugins(1003, vec![local_request(9103)])
            .unwrap();
        let error = bridge
            .resolve_layout_plugins(
                1003,
                LayoutPluginResolution::Release {
                    reason: "mismatch".to_owned(),
                },
                vec![99],
            )
            .unwrap_err();
        assert!(error.contains("id conflict"));
        assert!(bridge.layout_plugin_reservations.contains_key(&1003));
        assert!(bridge.layout_plugin_receipts.is_empty());
        assert!(
            bridge
                .resolve_layout_plugins(
                    1003,
                    LayoutPluginResolution::Release {
                        reason: "correct retry".to_owned(),
                    },
                    ids,
                )
                .is_ok()
        );
    }

    #[test]
    fn release_by_transaction_recovers_from_a_wrong_hint_and_replays_exact_owner_ids() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1019;
        let ids = bridge
            .reserve_layout_plugins(
                transaction_id,
                vec![local_request(9122), local_request(9123)],
            )
            .unwrap();
        let wrong_external_plugin_hint = vec![u32::MAX];

        assert!(
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Release {
                        reason: "mismatched preparation hint".to_owned(),
                    },
                    wrong_external_plugin_hint,
                )
                .unwrap_err()
                .contains("id conflict")
        );

        let released = bridge
            .release_layout_plugins_by_transaction(
                transaction_id,
                "release exact owner reservation".to_owned(),
            )
            .unwrap();
        assert_eq!(
            released,
            LayoutPluginReceipt::Released {
                plugin_ids: ids.clone()
            }
        );
        assert_eq!(
            bridge
                .release_layout_plugins_by_transaction(
                    transaction_id,
                    "idempotent replay with a different reason".to_owned(),
                )
                .unwrap(),
            released
        );
        assert!(
            ids.iter()
                .all(|plugin_id| !bridge.layout_plugin_owners.contains_key(plugin_id))
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
    }

    #[test]
    fn release_by_transaction_rejects_activated_reservation_without_compensating_it() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1020;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9124)])
            .unwrap();
        let cancellation = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            reservation.cancellation.clone()
        };

        let error = bridge
            .release_layout_plugins_by_transaction(
                transaction_id,
                "preparation failure arrived after activation".to_owned(),
            )
            .unwrap_err();

        assert!(error.contains("cannot Release from Activated"));
        assert!(!cancellation.is_cancelled());
        assert_eq!(
            bridge.layout_plugin_owners.get(&ids[0]),
            Some(&transaction_id)
        );
        assert!(
            bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(
            !bridge
                .layout_plugin_receipts
                .contains_key(&(transaction_id, LayoutPluginResolutionKind::Release,))
        );
    }

    #[test]
    fn activate_and_compensate_replay_while_cross_kind_resolution_conflicts() {
        let mut bridge = test_bridge(1);
        bridge.reserve_layout_plugins(1008, vec![]).unwrap();
        let activated = bridge
            .resolve_layout_plugins(1008, LayoutPluginResolution::Activate, vec![])
            .unwrap();
        assert_eq!(
            bridge
                .resolve_layout_plugins(1008, LayoutPluginResolution::Activate, vec![])
                .unwrap(),
            activated
        );
        assert!(
            !bridge.layout_plugin_reservations.contains_key(&1008),
            "empty activation must retire its ownerless reservation"
        );
        assert!(
            bridge
                .resolve_layout_plugins(
                    1008,
                    LayoutPluginResolution::Release {
                        reason: "invalid after activation".to_owned(),
                    },
                    vec![],
                )
                .unwrap_err()
                .contains("resolution conflict")
        );
        let compensate = LayoutPluginResolution::Compensate {
            reason: "empty activated transaction".to_owned(),
        };
        let compensated = bridge
            .resolve_layout_plugins(1008, compensate.clone(), vec![])
            .unwrap();
        assert_eq!(
            bridge
                .resolve_layout_plugins(1008, compensate, vec![])
                .unwrap(),
            compensated
        );
    }

    #[test]
    fn receipt_cache_retains_all_kinds_for_512_transactions() {
        let mut bridge = test_bridge(1);
        for transaction_id in 1..=513 {
            bridge
                .reserve_layout_plugins(transaction_id, vec![])
                .unwrap();
            bridge
                .resolve_layout_plugins(transaction_id, LayoutPluginResolution::Activate, vec![])
                .unwrap();
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Compensate {
                        reason: "bounded receipt test".to_owned(),
                    },
                    vec![],
                )
                .unwrap();
        }
        assert_eq!(
            bridge.layout_plugin_receipts.len(),
            MAX_LAYOUT_PLUGIN_RECEIPT_TRANSACTIONS * 2
        );
        assert!(
            bridge
                .layout_plugin_receipts
                .keys()
                .all(|(transaction_id, _)| transaction_id != &1)
        );
    }

    #[test]
    fn live_activated_reservation_reconstructs_an_evicted_activation_receipt() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1016;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9117)])
            .unwrap();
        bridge
            .layout_plugin_reservations
            .get_mut(&transaction_id)
            .unwrap()
            .state = LayoutPluginTransactionState::Activated;
        bridge.record_layout_plugin_receipt(
            transaction_id,
            LayoutPluginResolutionKind::Activate,
            LayoutPluginReceipt::Activated {
                plugin_ids: ids.clone(),
            },
        );
        bridge
            .layout_plugin_receipts
            .remove(&(transaction_id, LayoutPluginResolutionKind::Activate));

        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Activate,
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Activated { plugin_ids: ids }
        );
        assert!(
            bridge
                .layout_plugin_receipts
                .contains_key(&(transaction_id, LayoutPluginResolutionKind::Activate))
        );
    }

    #[test]
    fn partial_enqueue_failure_rolls_back_only_after_all_work_quiesces() {
        let mut bridge = test_bridge(1);
        let ids = bridge
            .reserve_layout_plugins(1004, vec![local_request(9104), local_request(9105)])
            .unwrap();
        let tracker = bridge
            .layout_plugin_reservations
            .get(&1004)
            .unwrap()
            .tracker
            .clone();
        bridge
            .plugin_executor
            .reject_next_enqueue_for_plugin(ids[1]);

        let receipt = bridge
            .resolve_layout_plugins(1004, LayoutPluginResolution::Activate, ids.clone())
            .unwrap();
        assert!(matches!(
            receipt,
            LayoutPluginReceipt::ActivationRolledBack { .. }
        ));
        assert_eq!(tracker.active_count(), 0);
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(bridge.cached_events_for_pending_plugins.is_empty());
        assert!(bridge.loading_plugins.is_empty());
        assert!(
            ids.iter()
                .all(|plugin_id| !bridge.plugin_executor.has_assignment(*plugin_id))
        );
        assert!(!bridge.layout_plugin_reservations.contains_key(&1004));
    }

    #[test]
    fn compensation_cancels_job_waiting_for_plugin_map_before_loader_starts() {
        let mut bridge = test_bridge(1);
        let gate = Arc::new(LayoutPluginLoadTestGate::new());
        bridge.layout_plugin_test_hooks.before_load_gate = Some(gate.clone());
        let load_starts = bridge.layout_plugin_test_hooks.load_starts.clone();
        let ids = bridge
            .reserve_layout_plugins(1010, vec![local_request(9111)])
            .unwrap();

        assert!(matches!(
            bridge
                .resolve_layout_plugins(1010, LayoutPluginResolution::Activate, ids.clone())
                .unwrap(),
            LayoutPluginReceipt::Activated { .. }
        ));
        gate.entered.wait();
        let release_gate = gate.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release_gate.release.wait();
        });

        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    1010,
                    LayoutPluginResolution::Compensate {
                        reason: "cancel while waiting for plugin map".to_owned(),
                    },
                    ids,
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated {
                plugin_ids: vec![0]
            }
        );
        release.join().unwrap();
        assert_eq!(
            load_starts.load(Ordering::SeqCst),
            0,
            "cancelled job must re-check its token immediately before PluginLoader"
        );
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
    }

    #[test]
    fn ordinary_unload_during_in_flight_activation_removes_all_residue() {
        let mut bridge = test_bridge(1);
        let gate = Arc::new(LayoutPluginLoadTestGate::new());
        bridge.layout_plugin_test_hooks.before_load_gate = Some(gate.clone());
        let load_starts = bridge.layout_plugin_test_hooks.load_starts.clone();
        let transaction_id = 1011;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9112)])
            .unwrap();
        let (tracker, plugin_data_dir) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get(&transaction_id)
                .unwrap();
            let plugin = &reservation.plugins[0];
            let loading_context = LoadingContext::new(
                &bridge,
                plugin.cwd.clone(),
                plugin.plugin_config.clone(),
                plugin.plugin_id,
                plugin.client_id,
                plugin.tab_index,
                plugin.size,
            );
            (
                reservation.tracker.clone(),
                loading_context.plugin_own_data_dir,
            )
        };
        std::fs::create_dir_all(&plugin_data_dir).unwrap();
        std::fs::write(plugin_data_dir.join("in-flight-residue"), b"must disappear").unwrap();

        bridge
            .resolve_layout_plugins(
                transaction_id,
                LayoutPluginResolution::Activate,
                ids.clone(),
            )
            .unwrap();
        gate.entered.wait();
        let release_gate = gate.clone();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            release_gate.release.wait();
        });

        bridge.unload_plugin(ids[0]).unwrap();

        release.join().unwrap();
        assert!(tracker.wait_for_idle(Duration::from_secs(2)));
        assert_eq!(load_starts.load(Ordering::SeqCst), 0);
        assert!(!plugin_data_dir.exists());
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(bridge.cached_events_for_pending_plugins.is_empty());
        assert!(bridge.cached_resizes_for_pending_plugins.is_empty());
        assert!(bridge.cached_worker_messages.is_empty());
        assert!(bridge.loading_plugins.is_empty());
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(!bridge.plugin_executor.has_assignment(ids[0]));
    }

    #[test]
    fn full_plugin_self_channel_cannot_block_compensation_quiescence() {
        let (plugin_sender, plugin_receiver) = zellij_utils::channels::bounded(1);
        let senders = ThreadSenders {
            to_plugin: Some(zellij_utils::channels::SenderWithContext::new(
                plugin_sender,
            )),
            ..Default::default()
        };
        let mut bridge = test_bridge_with_senders(1, senders);
        bridge
            .senders
            .send_to_plugin(PluginInstruction::Exit)
            .unwrap();
        let ids = bridge
            .reserve_layout_plugins(1012, vec![local_request(9113)])
            .unwrap();
        let tracker = bridge
            .layout_plugin_reservations
            .get(&1012)
            .unwrap()
            .tracker
            .clone();

        bridge
            .resolve_layout_plugins(1012, LayoutPluginResolution::Activate, ids.clone())
            .unwrap();
        assert!(
            tracker.wait_for_idle(Duration::from_secs(2)),
            "tracker must quiesce before a bounded self-send"
        );
        let compensation_started = Instant::now();
        assert!(matches!(
            bridge
                .resolve_layout_plugins(
                    1012,
                    LayoutPluginResolution::Compensate {
                        reason: "full self channel".to_owned(),
                    },
                    ids,
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated { .. }
        ));
        assert!(
            compensation_started.elapsed() < Duration::from_secs(1),
            "compensation must not wait for a worker blocked on its own channel"
        );
        drop(plugin_receiver);
    }

    #[test]
    fn remote_activation_can_be_compensated_without_late_resurrection() {
        let mut bridge = test_bridge(1);
        let ids = bridge
            .reserve_layout_plugins(1005, vec![remote_request(9106)])
            .unwrap();
        assert_eq!(
            bridge
                .resolve_layout_plugins(1005, LayoutPluginResolution::Activate, ids.clone())
                .unwrap(),
            LayoutPluginReceipt::Activated {
                plugin_ids: ids.clone()
            }
        );
        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    1005,
                    LayoutPluginResolution::Compensate {
                        reason: "screen commit failed".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated {
                plugin_ids: ids.clone()
            }
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(
            ids.iter()
                .all(|plugin_id| !bridge.plugin_executor.has_assignment(*plugin_id))
        );
        assert!(!bridge.layout_plugin_reservations.contains_key(&1005));
    }

    #[test]
    fn queued_activation_is_cancelled_before_load_and_never_resurrects() {
        let mut bridge = test_bridge(1);
        let blocker = Arc::new(std::sync::Barrier::new(2));
        let blocker_in_job = blocker.clone();
        bridge.plugin_executor.register_plugin(999);
        bridge
            .plugin_executor
            .try_execute_for_plugin(999, move |_s, _p, _c, _ca, _e| {
                blocker_in_job.wait();
            })
            .unwrap();
        for _ in 0..100 {
            if bridge.plugin_executor.jobs_in_flight_for_plugin(999) == Some(1) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let ids = bridge
            .reserve_layout_plugins(1007, vec![local_request(9108)])
            .unwrap();
        assert!(matches!(
            bridge
                .resolve_layout_plugins(1007, LayoutPluginResolution::Activate, ids.clone())
                .unwrap(),
            LayoutPluginReceipt::Activated { .. }
        ));
        let release_blocker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            blocker.wait();
        });
        assert!(matches!(
            bridge
                .resolve_layout_plugins(
                    1007,
                    LayoutPluginResolution::Compensate {
                        reason: "cancel queued activation".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated { .. }
        ));
        release_blocker.join().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert!(
            ids.iter()
                .all(|plugin_id| !bridge.plugin_executor.has_assignment(*plugin_id))
        );
    }

    #[test]
    fn ordinary_unload_retires_only_its_owner_and_last_owner_retires_transaction() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1009;
        let ids = bridge
            .reserve_layout_plugins(
                transaction_id,
                vec![local_request(9109), local_request(9110)],
            )
            .unwrap();
        let (first_cancellation, sibling_cancellation) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            (
                reservation.plugins[0].cancellation.clone(),
                reservation.plugins[1].cancellation.clone(),
            )
        };

        bridge.unload_plugin(ids[0]).unwrap();

        assert!(first_cancellation.is_cancelled());
        assert!(!sibling_cancellation.is_cancelled());
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert_eq!(
            bridge.layout_plugin_owners.get(&ids[1]),
            Some(&transaction_id)
        );
        assert!(
            bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );

        bridge.unload_plugin(ids[1]).unwrap();

        assert!(sibling_cancellation.is_cancelled());
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[1]));
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
    }

    #[test]
    fn partial_cleanup_uses_per_plugin_tracker_without_waiting_for_active_sibling() {
        let mut bridge = test_bridge(1);
        let owner_transaction_id = 1017;
        let cleanup_transaction_id = 2106;
        let ids = bridge
            .reserve_layout_plugins(
                owner_transaction_id,
                vec![local_request(9118), local_request(9119)],
            )
            .unwrap();
        let (
            target_cancellation,
            sibling_cancellation,
            target_tracker,
            sibling_tracker,
            group_tracker,
        ) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&owner_transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            (
                reservation.plugins[0].cancellation.clone(),
                reservation.plugins[1].cancellation.clone(),
                reservation.plugins[0].activation_tracker.clone(),
                reservation.plugins[1].activation_tracker.clone(),
                reservation.tracker.clone(),
            )
        };
        let sibling_plugin_guard = sibling_tracker.begin();
        let sibling_group_guard = group_tracker.begin();

        let cleanup_started = Instant::now();
        assert_eq!(
            bridge
                .cleanup_layout_plugins(cleanup_transaction_id, vec![ids[0]])
                .unwrap(),
            vec![ids[0]]
        );
        assert!(
            cleanup_started.elapsed() < Duration::from_secs(1),
            "cleanup of one plugin must not wait for an unrelated sibling activation"
        );
        assert!(target_cancellation.is_cancelled());
        assert!(!sibling_cancellation.is_cancelled());
        assert_eq!(target_tracker.active_count(), 0);
        assert_eq!(sibling_tracker.active_count(), 1);
        assert_eq!(group_tracker.active_count(), 1);
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert_eq!(
            bridge.layout_plugin_owners.get(&ids[1]),
            Some(&owner_transaction_id)
        );

        drop(sibling_plugin_guard);
        drop(sibling_group_guard);
        bridge.unload_plugin(ids[1]).unwrap();
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[1]));
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&owner_transaction_id)
        );
    }

    #[test]
    fn group_compensation_cancels_and_waits_for_every_plugin_tracker() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1018;
        let ids = bridge
            .reserve_layout_plugins(
                transaction_id,
                vec![local_request(9120), local_request(9121)],
            )
            .unwrap();
        let (group_cancellation, group_tracker, plugin_trackers) = {
            let reservation = bridge
                .layout_plugin_reservations
                .get_mut(&transaction_id)
                .unwrap();
            reservation.state = LayoutPluginTransactionState::Activated;
            (
                reservation.cancellation.clone(),
                reservation.tracker.clone(),
                reservation
                    .plugins
                    .iter()
                    .map(|plugin| plugin.activation_tracker.clone())
                    .collect::<Vec<_>>(),
            )
        };
        let group_guards = (0..ids.len())
            .map(|_| group_tracker.begin())
            .collect::<Vec<_>>();
        let plugin_guards = plugin_trackers
            .iter()
            .map(|tracker| tracker.begin())
            .collect::<Vec<_>>();
        let cancellation_for_worker = group_cancellation.clone();
        let release_guards = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !cancellation_for_worker.is_cancelled() {
                assert!(
                    Instant::now() < deadline,
                    "group compensation never cancelled the active jobs"
                );
                std::thread::yield_now();
            }
            drop(plugin_guards);
            drop(group_guards);
        });

        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    transaction_id,
                    LayoutPluginResolution::Compensate {
                        reason: "test whole-group cancellation".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated {
                plugin_ids: ids.clone()
            }
        );
        release_guards.join().unwrap();
        assert!(group_cancellation.is_cancelled());
        assert_eq!(group_tracker.active_count(), 0);
        assert!(
            plugin_trackers
                .iter()
                .all(|tracker| tracker.active_count() == 0)
        );
        assert!(
            ids.iter()
                .all(|plugin_id| !bridge.layout_plugin_owners.contains_key(plugin_id))
        );
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
    }

    #[test]
    fn ordinary_unload_enqueue_failure_retains_owner_and_retries_exact_debt() {
        let mut bridge = test_bridge(1);
        let transaction_id = 1015;
        let ids = bridge
            .reserve_layout_plugins(transaction_id, vec![local_request(9116)])
            .unwrap();
        bridge
            .layout_plugin_reservations
            .get_mut(&transaction_id)
            .unwrap()
            .state = LayoutPluginTransactionState::Activated;
        bridge.plugin_executor.register_plugin(ids[0]);
        bridge
            .plugin_executor
            .reject_next_enqueue_for_plugin(ids[0]);

        bridge.unload_plugin(ids[0]).unwrap();

        assert_eq!(
            bridge.layout_plugin_owners.get(&ids[0]),
            Some(&transaction_id),
            "an enqueue failure must not retire layout ownership"
        );
        assert!(
            bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(bridge.plugin_executor.has_assignment(ids[0]));
        assert_eq!(bridge.plugin_unload_debts.get(&ids[0]), Some(&1));

        bridge.unload_plugin(ids[0]).unwrap();

        assert!(!bridge.plugin_unload_debts.contains_key(&ids[0]));
        assert!(!bridge.layout_plugin_owners.contains_key(&ids[0]));
        assert!(
            !bridge
                .layout_plugin_reservations
                .contains_key(&transaction_id)
        );
        assert!(!bridge.plugin_executor.has_assignment(ids[0]));
    }

    #[test]
    fn failed_in_flight_load_cancels_group_and_never_publishes_plugin() {
        let mut bridge = test_bridge(1);
        let ids = bridge
            .reserve_layout_plugins(1006, vec![local_request(9107)])
            .unwrap();
        let tracker = bridge
            .layout_plugin_reservations
            .get(&1006)
            .unwrap()
            .tracker
            .clone();
        assert!(matches!(
            bridge
                .resolve_layout_plugins(1006, LayoutPluginResolution::Activate, ids.clone())
                .unwrap(),
            LayoutPluginReceipt::Activated { .. }
        ));
        assert!(tracker.wait_for_idle(Duration::from_secs(5)));
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
        assert_eq!(
            bridge
                .resolve_layout_plugins(
                    1006,
                    LayoutPluginResolution::Compensate {
                        reason: "load failure cleanup".to_owned(),
                    },
                    ids.clone(),
                )
                .unwrap(),
            LayoutPluginReceipt::Compensated {
                plugin_ids: ids.clone()
            }
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(bridge.plugin_map.lock().unwrap().plugin_ids().is_empty());
    }
}
