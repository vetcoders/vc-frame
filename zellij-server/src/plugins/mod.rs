mod pinned_executor;
mod pipes;
mod plugin_loader;
mod plugin_map;
mod plugin_worker;
mod wasm_bridge;
mod watch_filesystem;
mod zellij_exports;
use log::info;

pub use pinned_executor::PinnedExecutor;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::PathBuf,
    time::Duration,
};
use wasmi::Engine;

use crate::panes::PaneId;
use crate::route::NotificationEnd;
use crate::screen::{DurableTabLayoutGeneration, LayoutPreparationCleanup, ScreenInstruction};
use crate::session_layout_metadata::SessionLayoutMetadata;
use crate::{
    ClientId, ServerInstruction,
    pty::{LayoutTransactionId, PtyInstruction},
    thread_bus::Bus,
};
use zellij_utils::data::PaneRenderReport;
use zellij_utils::input::layout::TabLayoutInfo;

pub use wasm_bridge::PluginRenderAsset;
use wasm_bridge::{LayoutPluginReservationRequest, WasmBridge};

use zellij_utils::{
    channels,
    data::{
        ClientInfo, CommandOrPlugin, Event, EventType, FloatingPaneCoordinates, InputMode,
        LayoutInfo, LayoutWithError, MessageToPlugin, PermissionStatus, PermissionType,
        PipeMessage, PipeSource, WebServerStatus,
    },
    errors::{ContextType, PluginContext, prelude::*},
    input::{
        actions::Action,
        command::TerminalAction,
        keybinds::Keybinds,
        layout::{FloatingPaneLayout, Layout, Run, RunPlugin, RunPluginOrAlias, TiledPaneLayout},
        plugins::PluginAliases,
    },
    pane_size::Size,
    session_serialization,
};

pub type PluginId = u32;

/// Explicitly separates a pane's layout identity from the WASM runtime that
/// supplies its surface. Ordinary plugin panes use the same id for both. A
/// session-manager projector owns a distinct `pane_id` and forwards render,
/// input, and resize work to `runtime_plugin_id`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PluginPaneId {
    pub pane_id: PluginId,
    pub runtime_plugin_id: PluginId,
}

impl PluginPaneId {
    pub fn direct(plugin_id: PluginId) -> Self {
        Self {
            pane_id: plugin_id,
            runtime_plugin_id: plugin_id,
        }
    }

    pub fn projector(pane_id: PluginId, runtime_plugin_id: PluginId) -> Self {
        Self {
            pane_id,
            runtime_plugin_id,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutPluginResolution {
    Activate,
    Release { reason: String },
    Compensate { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutPluginReceipt {
    Activated {
        plugin_ids: Vec<PluginId>,
    },
    Released {
        plugin_ids: Vec<PluginId>,
    },
    Compensated {
        plugin_ids: Vec<PluginId>,
    },
    ActivationRolledBack {
        plugin_ids: Vec<PluginId>,
        message: String,
    },
}

#[derive(Clone, Debug)]
pub struct DumpSessionLayoutResponse {
    pub layout_result: Result<String, String>,
    pub metadata: Option<zellij_utils::data::LayoutMetadata>,
}

#[derive(Clone, Debug)]
pub enum PluginInstruction {
    Load(
        Option<bool>,   // should float
        bool,           // should be opened in place
        bool,           // close_replaced_pane
        Option<String>, // pane title
        RunPluginOrAlias,
        Option<usize>,  // tab index
        Option<PaneId>, // pane id to replace if this is to be opened "in-place"
        ClientId,
        Size,
        Option<PathBuf>,  // cwd
        Option<PluginId>, // the focused plugin id if relevant
        bool,             // skip cache
        Option<bool>,     // should focus plugin
        Option<FloatingPaneCoordinates>,
        Option<NotificationEnd>, // completion signal
    ),
    LoadBackgroundPlugin(RunPluginOrAlias, ClientId),
    Update(Vec<(Option<PluginId>, Option<ClientId>, Event)>), // Focused plugin / broadcast, client_id, event data
    Unload(PluginId),                                         // plugin_id
    Reload(
        Option<bool>,   // should float
        Option<String>, // pane title
        RunPluginOrAlias,
        usize, // tab index
        Size,
        Option<NotificationEnd>,
    ),
    ReloadPluginWithId(u32),
    Resize(PluginId, usize, usize), // plugin_id, columns, rows
    AddClient(ClientId),
    RemoveClient(ClientId),
    NewTab(
        Option<PathBuf>,
        Option<TerminalAction>,
        Option<TiledPaneLayout>,
        Vec<FloatingPaneLayout>,
        usize,                        // tab_id
        LayoutTransactionId,          // allocated by Screen before any layout resource
        Option<Vec<CommandOrPlugin>>, // initial_panes
        bool,                         // block_on_first_terminal
        bool,                         // should change focus to new tab
        (ClientId, bool),             // bool -> is_web_client
        Option<NotificationEnd>,      // completion signal
        Option<Box<DurableTabLayoutGeneration>>,
    ),
    OverrideLayout(
        Option<PathBuf>,        // cwd
        Option<TerminalAction>, // default_shell
        Vec<TabLayoutInfo>,     // layouts for each tab
        LayoutTransactionId,    // allocated by Screen before any layout resource
        bool,                   // retain_existing_terminal_panes
        bool,                   // retain_existing_plugin_panes
        ClientId,
        Option<NotificationEnd>,
        Option<Box<DurableTabLayoutGeneration>>,
    ),
    ResolveLayoutPlugins {
        transaction_id: LayoutTransactionId,
        resolution: LayoutPluginResolution,
        expected_plugin_ids: Vec<PluginId>,
        ack: channels::Sender<std::result::Result<LayoutPluginReceipt, String>>,
    },
    ReleaseLayoutPluginsByTransaction {
        transaction_id: LayoutTransactionId,
        reason: String,
        ack: channels::Sender<std::result::Result<LayoutPluginReceipt, String>>,
    },
    CleanupLayoutPlugins {
        transaction_id: LayoutTransactionId,
        plugin_ids: Vec<PluginId>,
        ack: channels::Sender<std::result::Result<Vec<PluginId>, String>>,
    },
    LayoutPluginActivationFailed {
        transaction_id: LayoutTransactionId,
        plugin_ids: Vec<PluginId>,
        message: String,
    },
    #[cfg(test)]
    RejectNextLayoutPluginReleaseForTest(LayoutTransactionId),
    ApplyCachedEvents {
        plugin_ids: Vec<PluginId>,
        done_receiving_permissions: bool,
    },
    ApplyCachedWorkerMessages(PluginId),
    PostMessagesToPluginWorker(
        PluginId,
        ClientId,
        String, // worker name
        Vec<(
            String, // serialized message name
            String, // serialized payload
        )>,
    ),
    PostMessageToPlugin(
        PluginId,
        ClientId,
        String, // serialized message
        String, // serialized payload
    ),
    PluginSubscribedToEvents(PluginId, ClientId, HashSet<EventType>),
    PermissionRequestResult(
        PluginId,
        Option<ClientId>,
        Vec<PermissionType>,
        PermissionStatus,
        Option<PathBuf>,
    ),
    DumpLayout(SessionLayoutMetadata, ClientId, Option<NotificationEnd>),
    ListClientsMetadata(SessionLayoutMetadata, ClientId, Option<NotificationEnd>),
    DumpLayoutToPlugin {
        session_layout_metadata: SessionLayoutMetadata,
        plugin_id: PluginId,
        response_channel: crossbeam::channel::Sender<DumpSessionLayoutResponse>,
    },
    LogLayoutToHd {
        session_name: String,
        generation: u64,
        session_layout_metadata: SessionLayoutMetadata,
    },
    CliPipe {
        pipe_id: String,
        name: String,
        payload: Option<String>,
        plugin: Option<String>,
        args: Option<BTreeMap<String, String>>,
        configuration: Option<BTreeMap<String, String>>,
        floating: Option<bool>,
        pane_id_to_replace: Option<PaneId>,
        pane_title: Option<String>,
        cwd: Option<PathBuf>,
        skip_cache: bool,
        cli_client_id: ClientId,
    },
    KeybindPipe {
        name: String,
        payload: Option<String>,
        plugin: Option<String>,
        args: Option<BTreeMap<String, String>>,
        configuration: Option<BTreeMap<String, String>>,
        floating: Option<bool>,
        pane_id_to_replace: Option<PaneId>,
        pane_title: Option<String>,
        cwd: Option<PathBuf>,
        skip_cache: bool,
        cli_client_id: ClientId,
        plugin_and_client_id: Option<(u32, ClientId)>,
        notification_end: Option<NotificationEnd>,
    },
    CachePluginEvents {
        plugin_id: PluginId,
    },
    MessageFromPlugin {
        source_plugin_id: u32,
        message: MessageToPlugin,
    },
    UnblockCliPipes(Vec<PluginRenderAsset>),
    Reconfigure {
        client_id: ClientId,
        keybinds: Option<Keybinds>,
        default_mode: Option<InputMode>,
        default_shell: Option<TerminalAction>,
        layout_dir: Option<PathBuf>,
        was_written_to_disk: bool,
    },
    FailedToWriteConfigToDisk {
        file_path: Option<PathBuf>,
    },
    WatchFilesystem,
    ListClientsToPlugin(SessionLayoutMetadata, PluginId, ClientId),
    ChangePluginHostDir(PathBuf, PluginId, ClientId),
    WebServerStarted(String), // String -> the base url of the web server
    FailedToStartWebServer(String),
    PaneRenderReport(PaneRenderReport),
    UserInput {
        client_id: ClientId,
        action: Action,
        terminal_id: Option<u32>,
        cli_client_id: Option<ClientId>,
    },
    LayoutListUpdate(Vec<LayoutInfo>, Vec<LayoutWithError>),
    RequestStateUpdateForPlugin(PluginId),
    UpdateSessionSaveTime(u64), // u64 = milliseconds since UNIX epoch
    GetLastSessionSaveTime {
        response_channel: crossbeam::channel::Sender<Option<u64>>,
    },
    DetectPluginConfigChanges(PluginAliases),
    HighlightClicked {
        plugin_id: u32,
        client_id: ClientId,
        pane_id: PaneId,
        pattern: String,
        matched_string: String,
        context: BTreeMap<String, String>,
    },
    Exit,
}

impl From<&PluginInstruction> for PluginContext {
    fn from(plugin_instruction: &PluginInstruction) -> Self {
        match *plugin_instruction {
            PluginInstruction::Load(..) => PluginContext::Load,
            PluginInstruction::LoadBackgroundPlugin(..) => PluginContext::LoadBackgroundPlugin,
            PluginInstruction::Update(..) => PluginContext::Update,
            PluginInstruction::Unload(..) => PluginContext::Unload,
            PluginInstruction::Reload(..) => PluginContext::Reload,
            PluginInstruction::ReloadPluginWithId(..) => PluginContext::ReloadPluginWithId,
            PluginInstruction::Resize(..) => PluginContext::Resize,
            PluginInstruction::Exit => PluginContext::Exit,
            PluginInstruction::AddClient(_) => PluginContext::AddClient,
            PluginInstruction::RemoveClient(_) => PluginContext::RemoveClient,
            PluginInstruction::NewTab(..) => PluginContext::NewTab,
            PluginInstruction::OverrideLayout(..) => PluginContext::OverrideLayout,
            PluginInstruction::ResolveLayoutPlugins { .. }
            | PluginInstruction::ReleaseLayoutPluginsByTransaction { .. }
            | PluginInstruction::CleanupLayoutPlugins { .. }
            | PluginInstruction::LayoutPluginActivationFailed { .. } => PluginContext::Update,
            #[cfg(test)]
            PluginInstruction::RejectNextLayoutPluginReleaseForTest(..) => PluginContext::Update,
            PluginInstruction::ApplyCachedEvents { .. } => PluginContext::ApplyCachedEvents,
            PluginInstruction::ApplyCachedWorkerMessages(..) => {
                PluginContext::ApplyCachedWorkerMessages
            },
            PluginInstruction::PostMessagesToPluginWorker(..) => {
                PluginContext::PostMessageToPluginWorker
            },
            PluginInstruction::PostMessageToPlugin(..) => PluginContext::PostMessageToPlugin,
            PluginInstruction::PluginSubscribedToEvents(..) => {
                PluginContext::PluginSubscribedToEvents
            },
            PluginInstruction::PermissionRequestResult(..) => {
                PluginContext::PermissionRequestResult
            },
            PluginInstruction::DumpLayout(..) => PluginContext::DumpLayout,
            PluginInstruction::ListClientsMetadata(..) => PluginContext::ListClientsMetadata,
            PluginInstruction::LogLayoutToHd { .. } => PluginContext::LogLayoutToHd,
            PluginInstruction::CliPipe { .. } => PluginContext::CliPipe,
            PluginInstruction::CachePluginEvents { .. } => PluginContext::CachePluginEvents,
            PluginInstruction::MessageFromPlugin { .. } => PluginContext::MessageFromPlugin,
            PluginInstruction::UnblockCliPipes { .. } => PluginContext::UnblockCliPipes,
            PluginInstruction::WatchFilesystem => PluginContext::WatchFilesystem,
            PluginInstruction::KeybindPipe { .. } => PluginContext::KeybindPipe,
            PluginInstruction::DumpLayoutToPlugin { .. } => PluginContext::DumpLayoutToPlugin,
            PluginInstruction::Reconfigure { .. } => PluginContext::Reconfigure,
            PluginInstruction::FailedToWriteConfigToDisk { .. } => {
                PluginContext::FailedToWriteConfigToDisk
            },
            PluginInstruction::ListClientsToPlugin(..) => PluginContext::ListClientsToPlugin,
            PluginInstruction::ChangePluginHostDir(..) => PluginContext::ChangePluginHostDir,
            PluginInstruction::WebServerStarted(..) => PluginContext::WebServerStarted,
            PluginInstruction::FailedToStartWebServer(..) => PluginContext::FailedToStartWebServer,
            PluginInstruction::PaneRenderReport(..) => PluginContext::PaneRenderReport,
            PluginInstruction::UserInput { .. } => PluginContext::UserInput,
            PluginInstruction::LayoutListUpdate(..) => PluginContext::LayoutListUpdate,
            PluginInstruction::RequestStateUpdateForPlugin(..) => {
                PluginContext::RequestStateUpdateForPlugin
            },
            PluginInstruction::UpdateSessionSaveTime(..) => PluginContext::UpdateSessionSaveTime,
            PluginInstruction::GetLastSessionSaveTime { .. } => {
                PluginContext::GetLastSessionSaveTime
            },
            PluginInstruction::DetectPluginConfigChanges(..) => {
                PluginContext::DetectPluginConfigChanges
            },
            PluginInstruction::HighlightClicked { .. } => PluginContext::HighlightClicked,
        }
    }
}

#[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
pub(crate) fn plugin_thread_main(
    bus: Bus<PluginInstruction>,
    engine: Engine,
    data_dir: PathBuf,
    mut layout: Box<Layout>,
    layout_dir: Option<PathBuf>,
    available_layouts: Vec<LayoutInfo>,
    available_layout_errors: Vec<LayoutWithError>,
    path_to_default_shell: PathBuf,
    zellij_cwd: PathBuf,
    session_env_vars: std::collections::BTreeMap<String, String>,
    default_shell: Option<TerminalAction>,
    plugin_aliases: PluginAliases,
    default_mode: InputMode,
    default_keybinds: Keybinds,
    background_plugins: Vec<RunPluginOrAlias>,
    // the client id that started the session,
    // we need it here because the thread's own list of connected clients might not yet be updated
    // on session start when we need to load the background plugins, and so we must have an
    // explicit client_id that has started the session
    initiating_client_id: ClientId,
) -> Result<()> {
    info!("Wasm main thread starts");
    let plugin_dir = data_dir.join("plugins/");
    let plugin_global_data_dir = plugin_dir.join("data");
    layout.populate_plugin_aliases_in_layout(&plugin_aliases);

    // use this channel to ensure that tasks spawned from this thread terminate before exiting
    // https://tokio.rs/tokio/topics/shutdown#waiting-for-things-to-finish-shutting-down
    let (shutdown_send, mut shutdown_receive) = tokio::sync::mpsc::channel::<()>(1);

    let mut wasm_bridge = WasmBridge::new(
        bus.senders.clone(),
        engine,
        plugin_dir,
        path_to_default_shell,
        zellij_cwd.clone(),
        session_env_vars,
        default_shell,
        layout_dir,
        available_layouts,
        available_layout_errors,
        default_mode,
        default_keybinds,
    );

    for run_plugin_or_alias in background_plugins {
        load_background_plugin(
            run_plugin_or_alias,
            &mut wasm_bridge,
            &bus,
            &plugin_aliases,
            initiating_client_id,
        );
    }

    loop {
        let (event, mut err_ctx) = match bus.recv() {
            Ok(event) => event,
            Err(error) => {
                log::error!("Plugin instruction channel disconnected: {error}");
                break;
            },
        };
        err_ctx.add_call(ContextType::Plugin((&event).into()));
        match event {
            PluginInstruction::Load(
                should_float,
                should_be_open_in_place,
                close_replaced_pane,
                pane_title,
                mut run_plugin_or_alias,
                tab_index,
                pane_id_to_replace,
                client_id,
                size,
                cwd,
                focused_plugin_id,
                skip_cache,
                should_focus_plugin,
                floating_pane_coordinates,
                completion_tx,
            ) => {
                run_plugin_or_alias.populate_run_plugin_if_needed(&plugin_aliases);
                let cwd = run_plugin_or_alias.get_initial_cwd().or(cwd).or_else(|| {
                    if let Some(plugin_id) = focused_plugin_id {
                        wasm_bridge.get_plugin_cwd(plugin_id, client_id)
                    } else {
                        None
                    }
                });
                let run_plugin = run_plugin_or_alias.get_run_plugin();
                let start_suppressed = false;
                match wasm_bridge.load_plugin(
                    &run_plugin,
                    tab_index,
                    size,
                    cwd.clone(),
                    skip_cache,
                    Some(client_id),
                ) {
                    Ok((plugin_id, client_id)) => {
                        drop(bus.senders.send_to_screen(ScreenInstruction::AddPlugin(
                            should_float,
                            should_be_open_in_place,
                            close_replaced_pane,
                            run_plugin_or_alias,
                            pane_title,
                            tab_index,
                            plugin_id,
                            pane_id_to_replace,
                            cwd.clone(),
                            start_suppressed,
                            floating_pane_coordinates,
                            should_focus_plugin,
                            Some(client_id),
                            completion_tx,
                        )));

                        drop(bus.senders.send_to_pty(PtyInstruction::ReportPluginCwd(
                            plugin_id,
                            cwd.unwrap_or_else(|| zellij_cwd.clone()),
                        )));
                    },
                    Err(e) => {
                        log::error!("Failed to load plugin: {e}");
                    },
                }
            },
            PluginInstruction::LoadBackgroundPlugin(run_plugin_or_alias, client_id) => {
                load_background_plugin(
                    run_plugin_or_alias,
                    &mut wasm_bridge,
                    &bus,
                    &plugin_aliases,
                    client_id,
                );
            },
            PluginInstruction::Update(updates) => {
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
            },
            PluginInstruction::Unload(pid) => {
                wasm_bridge.unload_plugin(pid)?;
            },
            PluginInstruction::Reload(
                should_float,
                pane_title,
                mut run_plugin_or_alias,
                tab_index,
                size,
                completion_tx,
            ) => {
                run_plugin_or_alias.populate_run_plugin_if_needed(&plugin_aliases);
                match run_plugin_or_alias.get_run_plugin() {
                    Some(run_plugin) => {
                        match wasm_bridge.reload_plugin(&run_plugin) {
                            Ok(_) => {
                                let _ = bus
                                    .senders
                                    .send_to_server(ServerInstruction::UnblockInputThread);
                            },
                            Err(err) => match err.downcast_ref::<ZellijError>() {
                                Some(ZellijError::PluginDoesNotExist) => {
                                    log::warn!(
                                        "Plugin {} not found, starting it instead",
                                        run_plugin.location
                                    );
                                    // we intentionally do not provide the client_id here because it belongs to
                                    // the cli who spawned the command and is not an existing client_id
                                    let skip_cache = true; // when reloading we always skip cache
                                    let start_suppressed = false;
                                    match wasm_bridge.load_plugin(
                                        &Some(run_plugin),
                                        Some(tab_index),
                                        size,
                                        None,
                                        skip_cache,
                                        None,
                                    ) {
                                        Ok((plugin_id, _client_id)) => {
                                            let should_be_open_in_place = false;
                                            drop(bus.senders.send_to_screen(
                                                ScreenInstruction::AddPlugin(
                                                    should_float,
                                                    should_be_open_in_place,
                                                    false, // close_replaced_pane
                                                    run_plugin_or_alias,
                                                    pane_title,
                                                    Some(tab_index),
                                                    plugin_id,
                                                    None,
                                                    None,
                                                    start_suppressed,
                                                    None,
                                                    None,
                                                    None,
                                                    completion_tx,
                                                ),
                                            ));
                                        },
                                        Err(e) => {
                                            log::error!("Failed to load plugin: {e}");
                                        },
                                    };
                                },
                                _ => {
                                    return Err(err);
                                },
                            },
                        }
                    },
                    None => {
                        log::error!("Failed to find plugin info for: {:?}", run_plugin_or_alias);
                    },
                }
            },
            PluginInstruction::ReloadPluginWithId(plugin_id) => {
                wasm_bridge.reload_plugin_with_id(plugin_id).non_fatal();
            },
            PluginInstruction::Resize(pid, new_columns, new_rows) => {
                wasm_bridge.resize_plugin(pid, new_columns, new_rows, shutdown_send.clone())?;
            },
            PluginInstruction::AddClient(client_id) => {
                wasm_bridge.add_client(client_id)?;
            },
            PluginInstruction::RemoveClient(client_id) => {
                wasm_bridge.remove_client(client_id);
            },
            PluginInstruction::NewTab(
                cwd,
                terminal_action,
                mut tab_layout,
                mut floating_panes_layout,
                tab_id,
                transaction_id,
                initial_panes,
                block_on_first_terminal,
                should_change_focus_to_new_tab,
                (client_id, is_web_client),
                completion_tx,
                layout_generation,
            ) => {
                // prefer connected clients so as to avoid opening plugins in the background for
                // CLI clients unless no-one else is connected
                let client_id = if wasm_bridge.client_is_connected(&client_id) {
                    client_id
                } else if let Some(first_client_id) = wasm_bridge.get_first_client_id() {
                    first_client_id
                } else {
                    client_id
                };

                tab_layout = tab_layout.or_else(|| Some(layout.new_tab().0));

                // Match initial_panes plugins to empty slots in the layout
                if let Some(ref initial_panes_vec) = initial_panes
                    && let Some(ref mut tiled_layout) = tab_layout
                {
                    for initial_pane in initial_panes_vec.iter() {
                        if let CommandOrPlugin::Plugin(run_plugin_or_alias) = initial_pane
                            && !tiled_layout.replace_next_empty_slot_with_run(Run::Plugin(
                                run_plugin_or_alias.clone(),
                            ))
                        {
                            log::warn!("More initial_panes provided than empty slots available");
                            break;
                        }
                        // Skip CommandOrPlugin::Command entries (handled by pty thread)
                    }
                }
                if let Some(t) = tab_layout.as_mut() {
                    t.populate_plugin_aliases_in_layout(&plugin_aliases);
                    if let Some(cwd) = cwd.as_ref() {
                        t.add_cwd_to_layout(cwd);
                    }
                }
                floating_panes_layout.iter_mut().for_each(|f| {
                    if let Some(f) = f.run.as_mut() {
                        f.populate_run_plugin_if_needed(&plugin_aliases)
                    }
                });
                let extracted_run_instructions = tab_layout
                    .clone()
                    .unwrap_or_else(|| layout.new_tab().0)
                    .extract_run_instructions();
                let size = Size::default();
                let floating_panes_layout = if floating_panes_layout.is_empty() {
                    layout.new_tab().1
                } else {
                    floating_panes_layout
                };
                let mut extracted_floating_plugins: Vec<Option<Run>> = floating_panes_layout
                    .iter()
                    .filter(|f| !f.already_running)
                    .map(|f| f.run.clone())
                    .collect();
                let mut all_run_instructions = extracted_run_instructions;
                all_run_instructions.append(&mut extracted_floating_plugins);

                let mut plugin_reservation_error = None;
                let mut planned_plugins = vec![];
                for run_instruction in all_run_instructions {
                    if let Some(Run::Plugin(run_plugin_or_alias)) = run_instruction {
                        let Some(run_plugin) = run_plugin_or_alias.get_run_plugin() else {
                            plugin_reservation_error = Some(anyhow!(
                                "failed to resolve layout plugin {:?} for tab {}",
                                run_plugin_or_alias,
                                tab_id
                            ));
                            break;
                        };
                        let plugin_cwd = run_plugin_or_alias
                            .get_initial_cwd()
                            .or_else(|| cwd.clone());
                        planned_plugins.push((
                            run_plugin_or_alias,
                            LayoutPluginReservationRequest {
                                run_plugin,
                                tab_index: Some(tab_id),
                                size,
                                cwd: plugin_cwd,
                                skip_cache: false,
                                client_id,
                            },
                        ));
                    }
                }
                if let Some(error) = plugin_reservation_error {
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        Some(tab_id),
                        completion_tx,
                        layout_generation,
                        error.to_string(),
                        LayoutPreparationCleanup::Resolved,
                    );
                    continue;
                }
                let reservation_requests = planned_plugins
                    .iter()
                    .map(|(_, request)| request.clone())
                    .collect();
                let reserved_plugin_ids = match wasm_bridge
                    .reserve_layout_plugins(transaction_id, reservation_requests)
                {
                    Ok(plugin_ids) => plugin_ids,
                    Err(message) => {
                        reject_layout_preparation(
                            &bus,
                            transaction_id,
                            Some(tab_id),
                            completion_tx,
                            layout_generation,
                            message,
                            LayoutPreparationCleanup::Resolved,
                        );
                        continue;
                    },
                };
                if let Err(message) =
                    register_layout_plugin_projectors(&wasm_bridge, &bus, transaction_id)
                {
                    let cleanup = match wasm_bridge
                        .release_layout_plugins_by_transaction(transaction_id, message.clone())
                    {
                        Ok(_) => LayoutPreparationCleanup::Resolved,
                        Err(_) => LayoutPreparationCleanup::ReleasePluginReservation {
                            plugin_ids: reserved_plugin_ids.clone(),
                            pty_cleanup_succeeded: true,
                        },
                    };
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        Some(tab_id),
                        completion_tx,
                        layout_generation,
                        message,
                        cleanup,
                    );
                    continue;
                }
                let mut plugin_ids: HashMap<RunPluginOrAlias, Vec<PluginId>> = HashMap::new();
                for ((run_plugin_or_alias, _), plugin_id) in
                    planned_plugins.into_iter().zip(reserved_plugin_ids)
                {
                    plugin_ids
                        .entry(run_plugin_or_alias)
                        .or_default()
                        .push(plugin_id);
                }
                let plugin_ids_for_handoff_failure = plugin_ids.clone();
                let instruction = PtyInstruction::NewTab(
                    cwd,
                    terminal_action,
                    Box::new(tab_layout),
                    floating_panes_layout,
                    tab_id,
                    transaction_id,
                    plugin_ids,
                    initial_panes,
                    block_on_first_terminal,
                    should_change_focus_to_new_tab,
                    (client_id, is_web_client),
                    completion_tx,
                    layout_generation,
                );
                if let Err(send_failure) = bus.senders.send_to_pty_recover(instruction) {
                    let (instruction, handoff_error) = send_failure.into_parts();
                    let (tab_id, transaction_id, plugin_ids, mut completion_tx, layout_generation) =
                        match instruction {
                            PtyInstruction::NewTab(
                                _,
                                _,
                                _,
                                _,
                                tab_id,
                                transaction_id,
                                plugin_ids,
                                _,
                                _,
                                _,
                                _,
                                completion_tx,
                                layout_generation,
                            ) => (
                                tab_id,
                                transaction_id,
                                plugin_ids,
                                completion_tx,
                                layout_generation,
                            ),
                            _ => {
                                let (release_error, cleanup) = release_layout_plugin_reservation(
                                    &mut wasm_bridge,
                                    transaction_id,
                                    &plugin_ids_for_handoff_failure,
                                    anyhow!(
                                        "Plugin -> PTY handoff returned an unexpected instruction for layout transaction {transaction_id}: {handoff_error:#}"
                                    ),
                                );
                                let message = release_error.to_string();
                                reject_layout_preparation(
                                    &bus,
                                    transaction_id,
                                    Some(tab_id),
                                    None,
                                    None,
                                    message,
                                    cleanup,
                                );
                                continue;
                            },
                        };
                    let error = handoff_error.context(format!(
                        "layout transaction {transaction_id} failed Plugin -> PTY handoff"
                    ));
                    let (release_error, cleanup) = release_layout_plugin_reservation(
                        &mut wasm_bridge,
                        transaction_id,
                        &plugin_ids,
                        error,
                    );
                    let message = release_error.to_string();
                    mark_layout_completion_failed(completion_tx.as_mut(), &message);
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        Some(tab_id),
                        completion_tx,
                        layout_generation,
                        message,
                        cleanup,
                    );
                }
            },
            PluginInstruction::OverrideLayout(
                cwd,
                default_shell,
                tab_layouts,
                transaction_id,
                retain_existing_terminal_panes,
                retain_existing_plugin_panes,
                client_id,
                completion_tx,
                layout_generation,
            ) => {
                // 1. Prefer connected clients over CLI clients
                let client_id = if wasm_bridge.client_is_connected(&client_id) {
                    client_id
                } else if let Some(first_client_id) = wasm_bridge.get_first_client_id() {
                    first_client_id
                } else {
                    client_id
                };

                // 2. Process each tab layout and build one transaction-wide,
                // side-effect-free reservation plan.
                let mut tab_layouts_with_plugin_keys = Vec::new();
                let mut reservation_requests = Vec::new();
                let mut plugin_reservation_error = None;
                for mut tab_layout_info in tab_layouts {
                    // Populate plugin aliases in layouts
                    tab_layout_info
                        .tiled_layout
                        .populate_plugin_aliases_in_layout(&plugin_aliases);
                    tab_layout_info.floating_layouts.iter_mut().for_each(|f| {
                        if let Some(r) = f.run.as_mut() {
                            r.populate_run_plugin_if_needed(&plugin_aliases)
                        }
                    });

                    // Extract run instructions from tiled layout
                    let extracted_run_instructions =
                        tab_layout_info.tiled_layout.extract_run_instructions();

                    // Extract run instructions from floating layouts (excluding already_running)
                    let extracted_floating_plugins: Vec<Option<Run>> = tab_layout_info
                        .floating_layouts
                        .iter()
                        .filter(|f| !f.already_running)
                        .map(|f| f.run.clone())
                        .collect();

                    // Combine all run instructions
                    let mut all_run_instructions = extracted_run_instructions;
                    all_run_instructions.extend(extracted_floating_plugins);

                    let mut plugin_keys = Vec::new();
                    let size = Size::default();

                    for run_instruction in all_run_instructions {
                        if let Some(Run::Plugin(run_plugin_or_alias)) = run_instruction {
                            let Some(run_plugin) = run_plugin_or_alias.get_run_plugin() else {
                                plugin_reservation_error = Some(anyhow!(
                                    "failed to resolve layout plugin {:?} for recovered tab {}",
                                    run_plugin_or_alias,
                                    tab_layout_info.tab_index
                                ));
                                break;
                            };
                            let plugin_cwd = run_plugin_or_alias.get_initial_cwd();
                            plugin_keys.push(run_plugin_or_alias);
                            reservation_requests.push(LayoutPluginReservationRequest {
                                run_plugin,
                                tab_index: Some(tab_layout_info.tab_index),
                                size,
                                cwd: plugin_cwd,
                                skip_cache: false,
                                client_id,
                            });
                        }
                    }

                    tab_layouts_with_plugin_keys.push((tab_layout_info, plugin_keys));
                    if plugin_reservation_error.is_some() {
                        break;
                    }
                }

                if let Some(error) = plugin_reservation_error {
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        layout_generation
                            .as_ref()
                            .map(|generation| generation.tab_id),
                        completion_tx,
                        layout_generation,
                        error.to_string(),
                        LayoutPreparationCleanup::Resolved,
                    );
                    continue;
                }
                let reserved_plugin_ids = match wasm_bridge
                    .reserve_layout_plugins(transaction_id, reservation_requests)
                {
                    Ok(plugin_ids) => plugin_ids,
                    Err(message) => {
                        reject_layout_preparation(
                            &bus,
                            transaction_id,
                            layout_generation
                                .as_ref()
                                .map(|generation| generation.tab_id),
                            completion_tx,
                            layout_generation,
                            message,
                            LayoutPreparationCleanup::Resolved,
                        );
                        continue;
                    },
                };
                if let Err(message) =
                    register_layout_plugin_projectors(&wasm_bridge, &bus, transaction_id)
                {
                    let cleanup = match wasm_bridge
                        .release_layout_plugins_by_transaction(transaction_id, message.clone())
                    {
                        Ok(_) => LayoutPreparationCleanup::Resolved,
                        Err(_) => LayoutPreparationCleanup::ReleasePluginReservation {
                            plugin_ids: reserved_plugin_ids.clone(),
                            pty_cleanup_succeeded: true,
                        },
                    };
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        layout_generation
                            .as_ref()
                            .map(|generation| generation.tab_id),
                        completion_tx,
                        layout_generation,
                        message,
                        cleanup,
                    );
                    continue;
                }
                let mut reserved_plugin_ids = reserved_plugin_ids.into_iter();
                let mut tab_layouts_with_plugin_ids = Vec::new();
                for (tab_layout_info, plugin_keys) in tab_layouts_with_plugin_keys {
                    let mut plugin_ids = HashMap::new();
                    for run_plugin_or_alias in plugin_keys {
                        let plugin_id = reserved_plugin_ids.next().expect(
                            "layout plugin reservation must return one id per planned plugin",
                        );
                        plugin_ids
                            .entry(run_plugin_or_alias)
                            .or_insert_with(Vec::new)
                            .push(plugin_id);
                    }
                    tab_layouts_with_plugin_ids.push((tab_layout_info, plugin_ids));
                }
                debug_assert!(reserved_plugin_ids.next().is_none());
                // 3. Send to pty thread with all tab layouts and their plugin IDs
                let plugin_ids_for_handoff_failure = tab_layouts_with_plugin_ids
                    .iter()
                    .map(|(_, plugin_ids)| plugin_ids.clone())
                    .collect::<Vec<_>>();
                let instruction = PtyInstruction::OverrideLayout(
                    cwd,
                    default_shell,
                    tab_layouts_with_plugin_ids,
                    transaction_id,
                    retain_existing_terminal_panes,
                    retain_existing_plugin_panes,
                    client_id,
                    completion_tx,
                    layout_generation,
                );
                if let Err(send_failure) = bus.senders.send_to_pty_recover(instruction) {
                    let (instruction, handoff_error) = send_failure.into_parts();
                    let (
                        tab_layouts_with_plugin_ids,
                        transaction_id,
                        mut completion_tx,
                        layout_generation,
                    ) = match instruction {
                        PtyInstruction::OverrideLayout(
                            _,
                            _,
                            tab_layouts_with_plugin_ids,
                            transaction_id,
                            _,
                            _,
                            _,
                            completion_tx,
                            layout_generation,
                        ) => (
                            tab_layouts_with_plugin_ids,
                            transaction_id,
                            completion_tx,
                            layout_generation,
                        ),
                        _ => {
                            let plugin_id_maps =
                                plugin_ids_for_handoff_failure.iter().collect::<Vec<_>>();
                            let (release_error, cleanup) = release_layout_plugin_reservation_maps(
                                &mut wasm_bridge,
                                transaction_id,
                                &plugin_id_maps,
                                anyhow!(
                                    "Plugin -> PTY handoff returned an unexpected instruction for Override transaction {transaction_id}: {handoff_error:#}"
                                ),
                            );
                            let message = release_error.to_string();
                            reject_layout_preparation(
                                &bus,
                                transaction_id,
                                None,
                                None,
                                None,
                                message,
                                cleanup,
                            );
                            continue;
                        },
                    };
                    let all_plugin_ids = tab_layouts_with_plugin_ids
                        .iter()
                        .map(|(_, plugin_ids)| plugin_ids)
                        .collect::<Vec<_>>();
                    let error = handoff_error.context(format!(
                        "layout transaction {transaction_id} failed Plugin -> PTY handoff"
                    ));
                    let (release_error, cleanup) = release_layout_plugin_reservation_maps(
                        &mut wasm_bridge,
                        transaction_id,
                        &all_plugin_ids,
                        error,
                    );
                    let message = release_error.to_string();
                    mark_layout_completion_failed(completion_tx.as_mut(), &message);
                    reject_layout_preparation(
                        &bus,
                        transaction_id,
                        layout_generation
                            .as_ref()
                            .map(|generation| generation.tab_id),
                        completion_tx,
                        layout_generation,
                        message,
                        cleanup,
                    );
                }
            },
            PluginInstruction::ResolveLayoutPlugins {
                transaction_id,
                resolution,
                expected_plugin_ids,
                ack,
            } => {
                let result = wasm_bridge.resolve_layout_plugins(
                    transaction_id,
                    resolution,
                    expected_plugin_ids,
                );
                let _ = ack.send(result);
            },
            PluginInstruction::ReleaseLayoutPluginsByTransaction {
                transaction_id,
                reason,
                ack,
            } => {
                let result =
                    wasm_bridge.release_layout_plugins_by_transaction(transaction_id, reason);
                let _ = ack.send(result);
            },
            PluginInstruction::CleanupLayoutPlugins {
                transaction_id,
                plugin_ids,
                ack,
            } => {
                let result = wasm_bridge.cleanup_layout_plugins(transaction_id, plugin_ids);
                let _ = ack.send(result);
            },
            PluginInstruction::LayoutPluginActivationFailed {
                transaction_id,
                plugin_ids,
                message,
            } => {
                wasm_bridge.handle_layout_plugin_activation_failure(
                    transaction_id,
                    plugin_ids,
                    message,
                );
            },
            #[cfg(test)]
            PluginInstruction::RejectNextLayoutPluginReleaseForTest(transaction_id) => {
                wasm_bridge.reject_next_layout_plugin_release_for_test(transaction_id);
            },
            PluginInstruction::ApplyCachedEvents {
                plugin_ids,
                done_receiving_permissions,
            } => {
                wasm_bridge.apply_cached_events(
                    plugin_ids,
                    done_receiving_permissions,
                    shutdown_send.clone(),
                )?;
            },
            PluginInstruction::ApplyCachedWorkerMessages(plugin_id) => {
                wasm_bridge.apply_cached_worker_messages(plugin_id)?;
            },
            PluginInstruction::PostMessagesToPluginWorker(
                plugin_id,
                client_id,
                worker_name,
                messages,
            ) => {
                wasm_bridge.post_messages_to_plugin_worker(
                    plugin_id,
                    client_id,
                    worker_name,
                    messages,
                )?;
            },
            PluginInstruction::PostMessageToPlugin(plugin_id, client_id, message, payload) => {
                let updates = vec![(
                    Some(plugin_id),
                    Some(client_id),
                    Event::CustomMessage(message, payload),
                )];
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
            },
            PluginInstruction::PluginSubscribedToEvents(plugin_id, client_id, events) => {
                wasm_bridge.notify_screen_of_ansi_subscription_change();
                wasm_bridge.notify_screen_of_background_plugin_subscriptions(
                    plugin_id,
                    client_id,
                    events.clone(),
                );
                if events.contains(&EventType::InitialKeybinds) {
                    wasm_bridge.send_initial_keybinds_to_plugin(plugin_id, client_id);
                }
            },
            PluginInstruction::PermissionRequestResult(
                plugin_id,
                client_id,
                permissions,
                status,
                cache_path,
            ) => {
                if let Err(e) = wasm_bridge.cache_plugin_permissions(
                    plugin_id,
                    client_id,
                    permissions,
                    status,
                    cache_path,
                ) {
                    log::error!("{}", e);
                }

                let updates = vec![(
                    Some(plugin_id),
                    client_id,
                    Event::PermissionRequestResult(status),
                )];
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
                let done_receiving_permissions = true;
                wasm_bridge.apply_cached_events(
                    vec![plugin_id],
                    done_receiving_permissions,
                    shutdown_send.clone(),
                )?;
            },
            PluginInstruction::DumpLayout(
                mut session_layout_metadata,
                client_id,
                completion_tx,
            ) => {
                populate_session_layout_metadata(
                    &mut session_layout_metadata,
                    &wasm_bridge,
                    &plugin_aliases,
                    None,
                );
                drop(bus.senders.send_to_pty(PtyInstruction::DumpLayout(
                    session_layout_metadata,
                    client_id,
                    completion_tx,
                )));
            },
            PluginInstruction::ListClientsMetadata(
                mut session_layout_metadata,
                client_id,
                completion_tx,
            ) => {
                populate_session_layout_metadata(
                    &mut session_layout_metadata,
                    &wasm_bridge,
                    &plugin_aliases,
                    None,
                );
                drop(bus.senders.send_to_pty(PtyInstruction::ListClientsMetadata(
                    session_layout_metadata,
                    client_id,
                    completion_tx,
                )));
            },
            PluginInstruction::DumpLayoutToPlugin {
                mut session_layout_metadata,
                plugin_id,
                response_channel,
            } => {
                populate_session_layout_metadata(
                    &mut session_layout_metadata,
                    &wasm_bridge,
                    &plugin_aliases,
                    Some(plugin_id),
                );

                let layout_metadata = session_layout_metadata.to_layout_metadata();

                match session_serialization::serialize_session_layout(
                    session_layout_metadata.into(),
                ) {
                    Ok((layout, _pane_contents)) => {
                        // send synchronous response
                        let response = DumpSessionLayoutResponse {
                            layout_result: Ok(layout.clone()),
                            metadata: Some(layout_metadata),
                        };
                        let _ = response_channel.send(response);

                        // send CustomMessage to plugin (backwards compatibility, should get rid of
                        // this on API version upgrade)
                        let updates = vec![(
                            Some(plugin_id),
                            None,
                            Event::CustomMessage("session_layout".to_owned(), layout),
                        )];
                        wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
                    },
                    Err(e) => {
                        let error_msg = e.to_string();
                        let response = DumpSessionLayoutResponse {
                            layout_result: Err(error_msg.clone()),
                            metadata: None,
                        };
                        let _ = response_channel.send(response);

                        let updates = vec![(
                            Some(plugin_id),
                            None,
                            Event::CustomMessage("session_layout_error".to_owned(), error_msg),
                        )];
                        wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
                    },
                }
            },
            PluginInstruction::ListClientsToPlugin(
                mut session_layout_metadata,
                plugin_id,
                client_id,
            ) => {
                populate_session_layout_metadata(
                    &mut session_layout_metadata,
                    &wasm_bridge,
                    &plugin_aliases,
                    Some(plugin_id),
                );
                let mut clients_metadata = session_layout_metadata.all_clients_metadata();
                let mut client_list_for_plugin = vec![];
                let default_editor = session_layout_metadata.default_editor.clone();
                for (client_metadata_id, client_metadata) in clients_metadata.iter_mut() {
                    let is_current_client = client_metadata_id == &client_id;
                    client_list_for_plugin.push(ClientInfo::new(
                        *client_metadata_id,
                        client_metadata.get_pane_id().into(),
                        client_metadata.stringify_command(&default_editor),
                        is_current_client,
                    ));
                }
                let updates = vec![(
                    Some(plugin_id),
                    Some(client_id),
                    Event::ListClients(client_list_for_plugin),
                )];
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
            },
            PluginInstruction::LogLayoutToHd {
                session_name,
                generation,
                mut session_layout_metadata,
            } => {
                populate_session_layout_metadata(
                    &mut session_layout_metadata,
                    &wasm_bridge,
                    &plugin_aliases,
                    None,
                );
                drop(bus.senders.send_to_pty(PtyInstruction::LogLayoutToHd {
                    session_name,
                    generation,
                    session_layout_metadata,
                }));
            },
            PluginInstruction::CliPipe {
                pipe_id,
                name,
                payload,
                plugin,
                args,
                configuration,
                floating,
                pane_id_to_replace,
                pane_title,
                cwd,
                skip_cache,
                cli_client_id,
            } => {
                let should_float = floating.unwrap_or(true);
                let mut pipe_messages = vec![];
                let floating_pane_coordinates = None; // TODO: do we want to allow this?
                match plugin {
                    Some(plugin_url) => {
                        // send to specific plugin(s)
                        pipe_to_specific_plugins(
                            PipeSource::Cli(pipe_id.clone()),
                            &plugin_url,
                            &configuration,
                            &cwd,
                            skip_cache,
                            should_float,
                            &pane_id_to_replace,
                            &pane_title,
                            Some(cli_client_id),
                            &mut pipe_messages,
                            &name,
                            &payload,
                            &args,
                            &bus,
                            &mut wasm_bridge,
                            &plugin_aliases,
                            floating_pane_coordinates,
                            None,
                        );
                    },
                    None => {
                        // no specific destination, send to all plugins
                        pipe_to_all_plugins(
                            PipeSource::Cli(pipe_id.clone()),
                            &name,
                            &payload,
                            &args,
                            &mut wasm_bridge,
                            &mut pipe_messages,
                        );
                    },
                }
                wasm_bridge.pipe_messages(pipe_messages, shutdown_send.clone(), None)?;
            },
            PluginInstruction::KeybindPipe {
                name,
                payload,
                plugin,
                args,
                configuration,
                floating,
                pane_id_to_replace,
                pane_title,
                cwd,
                skip_cache,
                cli_client_id,
                plugin_and_client_id,
                notification_end,
            } => {
                let should_float = floating.unwrap_or(true);
                let mut pipe_messages = vec![];
                let floating_pane_coordinates = None; // TODO: do we want to allow this?
                if let Some((plugin_id, client_id)) = plugin_and_client_id {
                    let is_private = true;
                    pipe_messages.push((
                        Some(plugin_id),
                        Some(client_id),
                        PipeMessage::new(PipeSource::Keybind, name, &payload, &args, is_private),
                    ));
                } else {
                    match plugin {
                        Some(plugin_url) => {
                            // send to specific plugin(s)
                            pipe_to_specific_plugins(
                                PipeSource::Keybind,
                                &plugin_url,
                                &configuration,
                                &cwd,
                                skip_cache,
                                should_float,
                                &pane_id_to_replace,
                                &pane_title,
                                Some(cli_client_id),
                                &mut pipe_messages,
                                &name,
                                &payload,
                                &args,
                                &bus,
                                &mut wasm_bridge,
                                &plugin_aliases,
                                floating_pane_coordinates,
                                None,
                            );
                        },
                        None => {
                            // no specific destination, send to all plugins
                            pipe_to_all_plugins(
                                PipeSource::Keybind,
                                &name,
                                &payload,
                                &args,
                                &mut wasm_bridge,
                                &mut pipe_messages,
                            );
                        },
                    }
                }
                wasm_bridge.pipe_messages(
                    pipe_messages,
                    shutdown_send.clone(),
                    notification_end,
                )?;
            },
            PluginInstruction::CachePluginEvents { plugin_id } => {
                wasm_bridge.cache_plugin_events(plugin_id);
            },
            PluginInstruction::MessageFromPlugin {
                source_plugin_id,
                message,
            } => {
                let mut pipe_messages = vec![];
                let skip_cache = message
                    .new_plugin_args
                    .as_ref()
                    .map(|n| n.skip_cache)
                    .unwrap_or(false);
                let should_float = message
                    .new_plugin_args
                    .as_ref()
                    .and_then(|n| n.should_float)
                    .unwrap_or(true);
                let pane_title = message
                    .new_plugin_args
                    .as_ref()
                    .and_then(|n| n.pane_title.clone());
                let pane_id_to_replace = message
                    .new_plugin_args
                    .as_ref()
                    .and_then(|n| n.pane_id_to_replace);
                let floating_pane_coordinates = message.floating_pane_coordinates;
                match (message.plugin_url, message.destination_plugin_id) {
                    (Some(plugin_url), None) => {
                        // send to specific plugin(s)
                        pipe_to_specific_plugins(
                            PipeSource::Plugin(source_plugin_id),
                            &plugin_url,
                            &Some(message.plugin_config),
                            &None,
                            skip_cache,
                            should_float,
                            &pane_id_to_replace.map(|p| p.into()),
                            &pane_title,
                            None,
                            &mut pipe_messages,
                            &message.message_name,
                            &message.message_payload,
                            &Some(message.message_args),
                            &bus,
                            &mut wasm_bridge,
                            &plugin_aliases,
                            floating_pane_coordinates,
                            message.new_plugin_args.and_then(|n| n.should_focus),
                        );
                    },
                    (None, Some(destination_plugin_id)) => {
                        let is_private = true;
                        pipe_messages.push((
                            Some(destination_plugin_id),
                            None,
                            PipeMessage::new(
                                PipeSource::Plugin(source_plugin_id),
                                message.message_name,
                                &message.message_payload,
                                &Some(message.message_args),
                                is_private,
                            ),
                        ));
                    },
                    (Some(plugin_url), Some(destination_plugin_id)) => {
                        log::warn!(
                            "Message contains both a destination plugin url: {plugin_url} and a destination plugin id: {destination_plugin_id}, ignoring the url and prioritizing the id"
                        );
                        let is_private = true;
                        pipe_messages.push((
                            Some(destination_plugin_id),
                            None,
                            PipeMessage::new(
                                PipeSource::Plugin(source_plugin_id),
                                message.message_name,
                                &message.message_payload,
                                &Some(message.message_args),
                                is_private,
                            ),
                        ));
                    },
                    (None, None) => {
                        // send to all plugins
                        pipe_to_all_plugins(
                            PipeSource::Plugin(source_plugin_id),
                            &message.message_name,
                            &message.message_payload,
                            &Some(message.message_args),
                            &mut wasm_bridge,
                            &mut pipe_messages,
                        );
                    },
                }
                wasm_bridge.pipe_messages(pipe_messages, shutdown_send.clone(), None)?;
            },
            PluginInstruction::UnblockCliPipes(pipes_to_unblock) => {
                let pipes_to_unblock = wasm_bridge.update_cli_pipe_state(pipes_to_unblock);
                for pipe_name in pipes_to_unblock {
                    let _ = bus
                        .senders
                        .send_to_server(ServerInstruction::UnblockCliPipeInput(pipe_name))
                        .context("failed to unblock input pipe");
                }
            },
            PluginInstruction::Reconfigure {
                client_id,
                keybinds,
                default_mode,
                default_shell,
                layout_dir,
                was_written_to_disk,
            } => {
                wasm_bridge
                    .reconfigure(client_id, keybinds, default_mode, default_shell, layout_dir)
                    .non_fatal();
                // TODO: notify plugins that this happened so that they can eg. rebind temporary keys that
                // were lost
                if was_written_to_disk {
                    let updates = vec![(None, None, Event::ConfigWasWrittenToDisk)];
                    wasm_bridge
                        .update_plugins(updates, shutdown_send.clone())
                        .non_fatal();
                }
            },
            PluginInstruction::FailedToWriteConfigToDisk { file_path } => {
                let updates = vec![(
                    None,
                    None,
                    Event::FailedToWriteConfigToDisk(file_path.map(|f| f.display().to_string())),
                )];
                wasm_bridge
                    .update_plugins(updates, shutdown_send.clone())
                    .non_fatal();
            },
            PluginInstruction::WatchFilesystem => {
                wasm_bridge.start_fs_watcher_if_not_started();
            },
            PluginInstruction::ChangePluginHostDir(new_host_folder, plugin_id, client_id) => {
                if wasm_bridge
                    .change_plugin_host_dir(new_host_folder.clone(), plugin_id, client_id)
                    .is_ok()
                {
                    drop(
                        bus.senders.send_to_pty(PtyInstruction::ReportPluginCwd(
                            plugin_id,
                            new_host_folder,
                        )),
                    );
                }
            },
            PluginInstruction::WebServerStarted(base_url) => {
                let updates = vec![(
                    None,
                    None,
                    Event::WebServerStatus(WebServerStatus::Online(base_url)),
                )];
                wasm_bridge
                    .update_plugins(updates, shutdown_send.clone())
                    .non_fatal();
            },
            PluginInstruction::FailedToStartWebServer(error) => {
                let updates = vec![(None, None, Event::FailedToStartWebServer(error))];
                wasm_bridge
                    .update_plugins(updates, shutdown_send.clone())
                    .non_fatal();
            },
            PluginInstruction::PaneRenderReport(pane_render_report) => {
                wasm_bridge
                    .handle_pane_render_report(pane_render_report, shutdown_send.clone())
                    .non_fatal();
            },
            PluginInstruction::UserInput {
                client_id,
                action,
                terminal_id,
                cli_client_id,
            } => {
                // Fire Event::UserAction to all subscribed plugins with InterceptInput permission
                let updates = vec![(
                    None,
                    None,
                    Event::UserAction(action, client_id, terminal_id, cli_client_id),
                )];
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
            },
            PluginInstruction::LayoutListUpdate(layouts, errors) => {
                wasm_bridge.update_available_layouts(layouts, errors);
            },
            PluginInstruction::RequestStateUpdateForPlugin(plugin_id) => {
                wasm_bridge.state_update_for_plugin(plugin_id);
            },
            PluginInstruction::UpdateSessionSaveTime(timestamp_millis) => {
                // Store timestamp in WasmBridge (as Unix epoch for internal use)
                *wasm_bridge.last_session_save_time.lock().unwrap() = Some(timestamp_millis);
            },
            PluginInstruction::GetLastSessionSaveTime { response_channel } => {
                let timestamp = *wasm_bridge.last_session_save_time.lock().unwrap();
                let _ = response_channel.send(timestamp);
            },
            PluginInstruction::DetectPluginConfigChanges(new_plugins) => {
                wasm_bridge
                    .detect_and_notify_plugin_config_changes(&new_plugins, shutdown_send.clone())?;
            },
            PluginInstruction::HighlightClicked {
                plugin_id,
                client_id,
                pane_id,
                pattern,
                matched_string,
                context,
            } => {
                let event = Event::HighlightClicked {
                    pane_id: pane_id.into(),
                    pattern,
                    matched_string,
                    context,
                };
                let updates = vec![(Some(plugin_id), Some(client_id), event)];
                wasm_bridge.update_plugins(updates, shutdown_send.clone())?;
            },
            PluginInstruction::Exit => {
                break;
            },
        }
    }
    info!("wasm main thread exits");

    // first drop our sender, then call recv.
    // once all senders are dropped or the timeout is reached, recv will return an error, that we ignore

    drop(shutdown_send);
    let runtime = crate::global_async_runtime::get_tokio_runtime();
    runtime.block_on(async {
        let result = tokio::time::timeout(EXIT_TIMEOUT, shutdown_receive.recv()).await;
        if let Err(err) = result {
            log::error!("timeout waiting for plugin tasks to finish: {}", err);
        }
    });

    wasm_bridge.cleanup();

    fs::remove_dir_all(&plugin_global_data_dir)
        .or_else(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                // I don't care...
                Ok(())
            } else {
                Err(err)
            }
        })
        .context("failed to cleanup plugin data directory")
}

fn populate_session_layout_metadata(
    session_layout_metadata: &mut SessionLayoutMetadata,
    wasm_bridge: &WasmBridge,
    plugin_aliases: &PluginAliases,
    exclude_plugin_id: Option<u32>,
) {
    // Remove the requesting plugin from the layout to prevent deadlock
    if let Some(plugin_id) = exclude_plugin_id {
        session_layout_metadata.remove_plugin_from_layout(plugin_id);
    }

    let plugin_ids = session_layout_metadata.all_plugin_ids();
    let mut plugin_ids_to_cmds: HashMap<u32, RunPlugin> = HashMap::new();
    for plugin_id in plugin_ids {
        let plugin_cmd = wasm_bridge.run_plugin_of_plugin_id(plugin_id);
        match plugin_cmd {
            Some(plugin_cmd) => {
                plugin_ids_to_cmds.insert(plugin_id, plugin_cmd.clone());
            },
            None => log::error!("Plugin with id: {plugin_id} not found"),
        }
    }
    session_layout_metadata.update_plugin_cmds(plugin_ids_to_cmds);
    session_layout_metadata.update_plugin_aliases_in_default_layout(plugin_aliases);
}

fn pipe_to_all_plugins(
    pipe_source: PipeSource,
    name: &str,
    payload: &Option<String>,
    args: &Option<BTreeMap<String, String>>,
    wasm_bridge: &mut WasmBridge,
    pipe_messages: &mut Vec<(Option<PluginId>, Option<ClientId>, PipeMessage)>,
) {
    let is_private = false;
    let all_plugin_ids = wasm_bridge.all_plugin_ids();
    for (plugin_id, client_id) in all_plugin_ids {
        pipe_messages.push((
            Some(plugin_id),
            Some(client_id),
            PipeMessage::new(pipe_source.clone(), name, payload, args, is_private),
        ));
    }
}

#[allow(clippy::too_many_arguments)] // inherited pre-fork surface; de-arg refactor is its own cut
fn pipe_to_specific_plugins(
    pipe_source: PipeSource,
    plugin_url: &str,
    configuration: &Option<BTreeMap<String, String>>,
    cwd: &Option<PathBuf>,
    skip_cache: bool,
    should_float: bool,
    pane_id_to_replace: &Option<PaneId>,
    pane_title: &Option<String>,
    cli_client_id: Option<ClientId>,
    pipe_messages: &mut Vec<(Option<PluginId>, Option<ClientId>, PipeMessage)>,
    name: &str,
    payload: &Option<String>,
    args: &Option<BTreeMap<String, String>>,
    bus: &Bus<PluginInstruction>,
    wasm_bridge: &mut WasmBridge,
    plugin_aliases: &PluginAliases,
    floating_pane_coordinates: Option<FloatingPaneCoordinates>,
    should_focus: Option<bool>,
) {
    let is_private = true;
    let size = Size::default();
    match RunPluginOrAlias::from_url(plugin_url, configuration, Some(plugin_aliases), cwd.clone()) {
        Ok(run_plugin_or_alias) => {
            let initial_cwd = run_plugin_or_alias.get_initial_cwd();
            let all_plugin_ids = wasm_bridge.get_or_load_plugins(
                run_plugin_or_alias,
                size,
                initial_cwd.or_else(|| cwd.clone()),
                skip_cache,
                should_float,
                pane_id_to_replace.is_some(),
                pane_title.clone(),
                *pane_id_to_replace,
                cli_client_id,
                floating_pane_coordinates,
                should_focus.unwrap_or(false),
            );
            for (plugin_id, client_id) in all_plugin_ids {
                pipe_messages.push((
                    Some(plugin_id),
                    client_id,
                    PipeMessage::new(pipe_source.clone(), name, payload, args, is_private),
                ));
            }
        },
        Err(e) => match cli_client_id {
            Some(cli_client_id) => {
                let _ = bus.senders.send_to_server(ServerInstruction::LogError(
                    vec![format!("Failed to parse plugin url: {}", e)],
                    cli_client_id,
                    None,
                ));
            },
            None => {
                log::error!("Failed to parse plugin url: {}", e);
            },
        },
    }
}

fn mark_layout_completion_failed(completion: Option<&mut NotificationEnd>, message: &str) {
    if let Some(completion) = completion {
        completion.mark_failure(message);
    }
}

fn register_layout_plugin_projectors(
    wasm_bridge: &WasmBridge,
    bus: &Bus<PluginInstruction>,
    transaction_id: LayoutTransactionId,
) -> std::result::Result<(), String> {
    let bindings = wasm_bridge.layout_plugin_projector_bindings(transaction_id);
    if bindings.is_empty() {
        return Ok(());
    }
    // This instruction is queued before the Plugin -> PTY layout handoff, and
    // both eventually reach Screen through the same ordered channel. Waiting
    // synchronously for Screen here creates a bootstrap cycle: Screen can be
    // waiting for Plugin activation while Plugin waits for this ACK. Preserve
    // FIFO ordering and let Screen validate the transaction when it consumes
    // the registration.
    let (ack_tx, _ack_rx) = channels::bounded(1);
    bus.senders
        .send_to_screen(ScreenInstruction::RegisterPluginProjectors {
            transaction_id,
            bindings,
            ack: ack_tx,
        })
        .map_err(|error| {
            format!(
                "failed to register plugin projectors for layout transaction {transaction_id}: {error:#}"
            )
        })
}

fn release_layout_plugin_reservation(
    wasm_bridge: &mut WasmBridge,
    transaction_id: LayoutTransactionId,
    plugin_ids: &HashMap<RunPluginOrAlias, Vec<PluginId>>,
    original_error: anyhow::Error,
) -> (anyhow::Error, LayoutPreparationCleanup) {
    release_layout_plugin_reservation_maps(
        wasm_bridge,
        transaction_id,
        &[plugin_ids],
        original_error,
    )
}

fn release_layout_plugin_reservation_maps(
    wasm_bridge: &mut WasmBridge,
    transaction_id: LayoutTransactionId,
    plugin_id_maps: &[&HashMap<RunPluginOrAlias, Vec<PluginId>>],
    original_error: anyhow::Error,
) -> (anyhow::Error, LayoutPreparationCleanup) {
    let mut allocated_plugin_ids = plugin_id_maps
        .iter()
        .flat_map(|plugin_ids| plugin_ids.values())
        .flatten()
        .copied()
        .collect::<Vec<_>>();
    allocated_plugin_ids.sort_unstable();
    allocated_plugin_ids.dedup();
    let unresolved_cleanup = LayoutPreparationCleanup::ReleasePluginReservation {
        plugin_ids: allocated_plugin_ids.clone(),
        pty_cleanup_succeeded: true,
    };

    match wasm_bridge.resolve_layout_plugins(
        transaction_id,
        LayoutPluginResolution::Release {
            reason: original_error.to_string(),
        },
        allocated_plugin_ids.clone(),
    ) {
        Ok(LayoutPluginReceipt::Released { mut plugin_ids }) => {
            plugin_ids.sort_unstable();
            if plugin_ids == allocated_plugin_ids {
                (original_error, LayoutPreparationCleanup::Resolved)
            } else {
                (
                    original_error.context(format!(
                        "layout plugin transaction {transaction_id} returned mismatched release ids {plugin_ids:?}, expected {allocated_plugin_ids:?}"
                    )),
                    unresolved_cleanup,
                )
            }
        },
        Ok(receipt) => (
            original_error.context(format!(
                "layout plugin transaction {transaction_id} returned unexpected release receipt: {receipt:?}"
            )),
            unresolved_cleanup,
        ),
        Err(error) => (
            original_error.context(format!(
                "failed to release suspended plugin allocation for transaction {transaction_id}: {error}"
            )),
            unresolved_cleanup,
        ),
    }
}

fn reject_layout_preparation(
    bus: &Bus<PluginInstruction>,
    transaction_id: LayoutTransactionId,
    tab_id: Option<usize>,
    mut completion_tx: Option<NotificationEnd>,
    layout_generation: Option<Box<DurableTabLayoutGeneration>>,
    message: String,
    cleanup: LayoutPreparationCleanup,
) {
    mark_layout_completion_failed(completion_tx.as_mut(), &message);
    let instruction = ScreenInstruction::LayoutPreparationFailed {
        transaction_id,
        tab_id,
        completion_tx,
        layout_generation,
        message: message.clone(),
        cleanup,
    };
    if let Err(send_failure) = bus.senders.send_to_screen_recover(instruction) {
        let (recovered_instruction, send_error) = send_failure.into_parts();
        // Dropping the recovered instruction now reports the already-marked
        // failure to the original action waiter. Screen is gone, so there is
        // no remaining owner capable of mutating pending-tab state.
        log::error!(
            "failed to report rejected layout transaction {} to Screen: {:#}; cause: {}",
            transaction_id,
            send_error,
            message
        );
        drop(recovered_instruction);
    }
}

fn load_background_plugin(
    mut run_plugin_or_alias: RunPluginOrAlias,
    wasm_bridge: &mut WasmBridge,
    bus: &Bus<PluginInstruction>,
    plugin_aliases: &PluginAliases,
    client_id: ClientId,
) {
    run_plugin_or_alias.populate_run_plugin_if_needed(plugin_aliases);
    let cwd = run_plugin_or_alias.get_initial_cwd();
    let run_plugin = run_plugin_or_alias.get_run_plugin();
    let size = Size::default();
    let skip_cache = false;
    match wasm_bridge.load_plugin(
        &run_plugin,
        None,
        size,
        cwd.clone(),
        skip_cache,
        Some(client_id),
    ) {
        Ok((plugin_id, client_id)) => {
            let should_float = None;
            let should_be_open_in_place = false;
            let pane_title = None;
            let pane_id_to_replace = None;
            let start_suppressed = true;
            drop(bus.senders.send_to_screen(ScreenInstruction::AddPlugin(
                should_float,
                should_be_open_in_place,
                false, // close_replaced_pane
                run_plugin_or_alias,
                pane_title,
                None,
                plugin_id,
                pane_id_to_replace,
                cwd,
                start_suppressed,
                None,
                None,
                Some(client_id),
                None,
            )));
        },
        Err(e) => {
            log::error!("Failed to load plugin: {e}");
        },
    }
}

const EXIT_TIMEOUT: Duration = Duration::from_secs(3);

#[path = "./unit/plugin_tests.rs"]
#[cfg(test)]
mod plugin_tests;
