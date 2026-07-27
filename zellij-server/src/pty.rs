use crate::background_jobs::{BackgroundJob, SessionLayoutSnapshot, write_session_state_to_disk};
use crate::global_async_runtime::get_tokio_runtime as async_runtime;
use crate::os_input_output::{AsyncReader, ServerOsApi};
use crate::route::NotificationEnd;
use crate::terminal_bytes::TerminalBytes;
use crate::{
    ClientId, ServerInstruction,
    panes::PaneId,
    plugins::{DumpSessionLayoutResponse, PluginId, PluginInstruction},
    screen::{DurableTabLayoutGeneration, ScreenInstruction, TabOverrideResult},
    session_layout_metadata::SessionLayoutMetadata,
    thread_bus::{Bus, ThreadSenders},
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;
use zellij_utils::{
    data::{
        CommandOrPlugin, Event, FloatingPaneCoordinates, GetPaneCwdResponse, GetPanePidResponse,
        GetPaneRunningCommandResponse, NewPanePlacement, OriginatingPlugin, SessionInfo,
    },
    errors::prelude::*,
    errors::{ContextType, PtyContext},
    input::{
        command::{OpenFilePayload, RunCommand, TerminalAction},
        layout::{
            FloatingPaneLayout, Layout, Run, RunPluginOrAlias, SwapFloatingLayout, SwapTiledLayout,
            TabLayoutInfo, TiledPaneLayout,
        },
    },
    pane_size::Size,
    session_serialization,
};

pub type VteBytes = Vec<u8>;
pub type TabIndex = u32;
/// Zero is reserved for legacy/test Screen instructions without a PTY owner.
pub type LayoutTransactionId = u64;
const MAX_LAYOUT_COMMIT_RECEIPTS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutCommitOutcome {
    Committed,
    Rejected(String),
}

type QuitCallback = Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>;
type QuitCallbackInvocation = (PaneId, Option<i32>, RunCommand);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QuitCallbackFenceStatus {
    Pending,
    Committed,
    Cancelled,
    Fired,
}

struct QuitCallbackFenceState {
    status: QuitCallbackFenceStatus,
    callback: Option<QuitCallback>,
    pending_invocation: Option<QuitCallbackInvocation>,
}

#[derive(Clone)]
struct QuitCallbackFence {
    state: Arc<Mutex<QuitCallbackFenceState>>,
}

impl QuitCallbackFence {
    fn wrap(callback: QuitCallback) -> (Self, QuitCallback) {
        let fence = Self {
            state: Arc::new(Mutex::new(QuitCallbackFenceState {
                status: QuitCallbackFenceStatus::Pending,
                callback: Some(callback),
                pending_invocation: None,
            })),
        };
        let callback_fence = fence.clone();
        let fenced_callback = Box::new(move |pane_id, exit_status, command| {
            callback_fence.handle_exit(pane_id, exit_status, command);
        });
        (fence, fenced_callback)
    }

    fn handle_exit(&self, pane_id: PaneId, exit_status: Option<i32>, command: RunCommand) {
        let invocation = (pane_id, exit_status, command);
        let callback_to_fire = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match state.status {
                QuitCallbackFenceStatus::Pending => {
                    if state.pending_invocation.is_none() {
                        state.pending_invocation = Some(invocation);
                    }
                    None
                },
                QuitCallbackFenceStatus::Committed => {
                    state.status = QuitCallbackFenceStatus::Fired;
                    state.callback.take().map(|callback| (callback, invocation))
                },
                QuitCallbackFenceStatus::Cancelled | QuitCallbackFenceStatus::Fired => None,
            }
        };
        if let Some((callback, invocation)) = callback_to_fire {
            callback(invocation.0, invocation.1, invocation.2);
        }
    }

    fn commit(&self) {
        let callback_to_fire = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if state.status != QuitCallbackFenceStatus::Pending {
                return;
            }
            if let Some(invocation) = state.pending_invocation.take() {
                state.status = QuitCallbackFenceStatus::Fired;
                state.callback.take().map(|callback| (callback, invocation))
            } else {
                state.status = QuitCallbackFenceStatus::Committed;
                None
            }
        };
        if let Some((callback, invocation)) = callback_to_fire {
            callback(invocation.0, invocation.1, invocation.2);
        }
    }

    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if matches!(
            state.status,
            QuitCallbackFenceStatus::Pending | QuitCallbackFenceStatus::Committed
        ) {
            state.status = QuitCallbackFenceStatus::Cancelled;
            state.callback = None;
            state.pending_invocation = None;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientTabIndexOrPaneId {
    ClientId(ClientId),
    TabIndex(usize),
    PaneId(PaneId),
}

/// Instructions related to PTYs (pseudoterminals).
#[derive(Clone, Debug)]
pub enum PtyInstruction {
    SpawnTerminal(
        Option<TerminalAction>,
        Option<String>,
        NewPanePlacement,
        bool, // start suppressed
        ClientTabIndexOrPaneId,
        Option<NotificationEnd>, // completion signal
        bool,                    // set_blocking
    ), // bool (if Some) is
    // should_float, String is an optional pane name
    OpenInPlaceEditor(
        PathBuf,
        Option<usize>,
        ClientTabIndexOrPaneId,
        Option<NotificationEnd>,
    ), // Option<usize> is the optional line number
    UpdateActivePane(Option<PaneId>, ClientId),
    GoToTab(TabIndex, ClientId),
    NewTab(
        Option<PathBuf>,
        Option<TerminalAction>,
        Box<Option<TiledPaneLayout>>,
        Vec<FloatingPaneLayout>,
        usize,                               // tab_index
        LayoutTransactionId,                 // allocated by Screen before any layout resource
        HashMap<RunPluginOrAlias, Vec<u32>>, // plugin_ids
        Option<Vec<CommandOrPlugin>>,        // initial_panes
        bool,                                // block_on_first_terminal
        bool,                                // should change focus to new tab
        (ClientId, bool),                    // bool -> is_web_client
        Option<NotificationEnd>,             // completion signal
        Option<Box<DurableTabLayoutGeneration>>,
    ), // the String is the tab name
    OverrideLayout(
        Option<PathBuf>,                                           // CWD
        Option<TerminalAction>,                                    // Default Shell
        Vec<(TabLayoutInfo, HashMap<RunPluginOrAlias, Vec<u32>>)>, // (layout, plugin_ids) per tab
        LayoutTransactionId, // allocated by Screen before any layout resource
        bool,                // retain_existing_terminal_panes
        bool,                // retain_existing_plugin_panes
        ClientId,
        Option<NotificationEnd>,
        Option<Box<DurableTabLayoutGeneration>>,
    ),
    ClosePane(PaneId, Option<NotificationEnd>),
    CloseTab(Vec<PaneId>),
    ReRunCommandInPane(PaneId, RunCommand, Option<NotificationEnd>),
    DropToShellInPane {
        pane_id: PaneId,
        shell: Option<PathBuf>,
        working_dir: Option<PathBuf>,
        completion_tx: Option<NotificationEnd>,
    },
    SpawnInPlaceTerminal(
        Option<TerminalAction>,
        Option<String>,
        bool, // close replaced pane
        ClientTabIndexOrPaneId,
        Option<NotificationEnd>, // completion signal
    ), // String is an optional pane name
    DumpLayout(SessionLayoutMetadata, ClientId, Option<NotificationEnd>),
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
    SaveSessionToDisk {
        session_name: String,
        session_info: SessionInfo,
        session_layout_metadata: SessionLayoutMetadata,
        generation: u64,
        completion_tx: Option<NotificationEnd>,
    },
    FillPluginCwd(
        Option<bool>,   // should float
        bool,           // should be opened in place
        bool,           // close_replaced_pane
        Option<String>, // pane title
        RunPluginOrAlias,
        usize,          // tab index
        Option<PaneId>, // pane id to replace if this is to be opened "in-place"
        ClientId,
        Size,
        bool,            // skip cache
        Option<PathBuf>, // if Some, will not fill cwd but just forward the message
        Option<bool>,    // should focus plugin
        Option<FloatingPaneCoordinates>,
        Option<NotificationEnd>,
    ),
    ListClientsMetadata(SessionLayoutMetadata, ClientId, Option<NotificationEnd>),
    Reconfigure {
        client_id: ClientId,
        default_editor: Option<PathBuf>,
        post_command_discovery_hook: Option<String>,
    },
    ListClientsToPlugin(SessionLayoutMetadata, PluginId, ClientId),
    ReportPluginCwd(PluginId, PathBuf),
    SendSigintToPaneId(PaneId),
    SendSigkillToPaneId(PaneId),
    GetPanePid {
        pane_id: PaneId,
        response_channel: crossbeam::channel::Sender<GetPanePidResponse>,
    },
    GetPaneRunningCommand {
        pane_id: PaneId,
        response_channel: crossbeam::channel::Sender<GetPaneRunningCommandResponse>,
    },
    GetPaneCwd {
        pane_id: PaneId,
        response_channel: crossbeam::channel::Sender<GetPaneCwdResponse>,
    },
    UpdateAndReportCwds,
    NotifyCwdFromOsc7(u32, PathBuf),
    LayoutCommitResolved {
        transaction_id: LayoutTransactionId,
        outcome: LayoutCommitOutcome,
        ack: zellij_utils::channels::Sender<std::result::Result<(), String>>,
    },
    Exit,
}

impl From<&PtyInstruction> for PtyContext {
    fn from(pty_instruction: &PtyInstruction) -> Self {
        match *pty_instruction {
            PtyInstruction::SpawnTerminal(..) => PtyContext::SpawnTerminal,
            PtyInstruction::OpenInPlaceEditor(..) => PtyContext::OpenInPlaceEditor,
            PtyInstruction::UpdateActivePane(..) => PtyContext::UpdateActivePane,
            PtyInstruction::GoToTab(..) => PtyContext::GoToTab,
            PtyInstruction::ClosePane(..) => PtyContext::ClosePane,
            PtyInstruction::CloseTab(_) => PtyContext::CloseTab,
            PtyInstruction::NewTab(..) => PtyContext::NewTab,
            PtyInstruction::OverrideLayout(..) => PtyContext::OverrideLayout,
            PtyInstruction::ReRunCommandInPane(..) => PtyContext::ReRunCommandInPane,
            PtyInstruction::DropToShellInPane { .. } => PtyContext::DropToShellInPane,
            PtyInstruction::SpawnInPlaceTerminal(..) => PtyContext::SpawnInPlaceTerminal,
            PtyInstruction::DumpLayout(..) => PtyContext::DumpLayout,
            PtyInstruction::DumpLayoutToPlugin { .. } => PtyContext::DumpLayoutToPlugin,
            PtyInstruction::LogLayoutToHd { .. } => PtyContext::LogLayoutToHd,
            PtyInstruction::SaveSessionToDisk { .. } => PtyContext::SaveSessionToDisk,
            PtyInstruction::FillPluginCwd(..) => PtyContext::FillPluginCwd,
            PtyInstruction::ListClientsMetadata(..) => PtyContext::ListClientsMetadata,
            PtyInstruction::Reconfigure { .. } => PtyContext::Reconfigure,
            PtyInstruction::ListClientsToPlugin(..) => PtyContext::ListClientsToPlugin,
            PtyInstruction::ReportPluginCwd(..) => PtyContext::ReportPluginCwd,
            PtyInstruction::SendSigintToPaneId(..) => PtyContext::SendSigintToPaneId,
            PtyInstruction::SendSigkillToPaneId(..) => PtyContext::SendSigkillToPaneId,
            PtyInstruction::GetPanePid { .. } => PtyContext::GetPanePid,
            PtyInstruction::GetPaneRunningCommand { .. } => PtyContext::GetPaneRunningCommand,
            PtyInstruction::GetPaneCwd { .. } => PtyContext::GetPaneCwd,
            PtyInstruction::UpdateAndReportCwds => PtyContext::UpdateAndReportCwds,
            PtyInstruction::NotifyCwdFromOsc7(..) => PtyContext::NotifyCwdFromOsc7,
            PtyInstruction::LayoutCommitResolved { .. } => PtyContext::LayoutCommitResolved,
            PtyInstruction::Exit => PtyContext::Exit,
        }
    }
}

#[derive(Default)]
struct LayoutAllocationLedger {
    allocated_ids: BTreeSet<PaneId>,
    quit_callback_fences: Vec<QuitCallbackFence>,
    drop_cleanup: Option<LayoutAllocationCleanup>,
}

struct LayoutAllocationCleanup {
    senders: ThreadSenders,
    os_input: Option<Box<dyn ServerOsApi>>,
}

impl LayoutAllocationLedger {
    fn armed_for_bus(bus: &Bus<PtyInstruction>) -> Self {
        Self {
            allocated_ids: BTreeSet::new(),
            quit_callback_fences: vec![],
            drop_cleanup: Some(LayoutAllocationCleanup {
                senders: bus.senders.clone(),
                os_input: bus.os_input.as_ref().map(|os_input| os_input.box_clone()),
            }),
        }
    }

    fn track_plugin_ids(&mut self, plugin_ids: &HashMap<RunPluginOrAlias, Vec<u32>>) {
        self.allocated_ids
            .extend(plugin_ids.values().flatten().copied().map(PaneId::Plugin));
    }

    fn track_terminal(&mut self, terminal_id: u32) {
        self.allocated_ids.insert(PaneId::Terminal(terminal_id));
    }

    fn track_quit_callback_fence(&mut self, fence: QuitCallbackFence) {
        self.quit_callback_fences.push(fence);
    }

    fn disarm(mut self) {
        self.drop_cleanup.take();
        self.allocated_ids.clear();
        self.quit_callback_fences.clear();
    }
}

impl Drop for LayoutAllocationLedger {
    fn drop(&mut self) {
        let Some(cleanup) = self.drop_cleanup.take() else {
            return;
        };
        for quit_callback_fence in self.quit_callback_fences.drain(..) {
            quit_callback_fence.cancel();
        }
        while let Some(pane_id) = self.allocated_ids.pop_first() {
            let result = match pane_id {
                PaneId::Terminal(terminal_id) => cleanup
                    .os_input
                    .as_ref()
                    .context("layout allocation guard has no OS interface")
                    .and_then(|os_input| os_input.clear_terminal_id(terminal_id)),
                PaneId::Plugin(plugin_id) => cleanup
                    .senders
                    .send_to_plugin_recover(PluginInstruction::Unload(plugin_id))
                    .map_err(|send_failure| send_failure.into_parts().1),
            };
            if let Err(error) = result {
                log::error!("layout allocation guard failed to release {pane_id:?}: {error:#}");
            }
        }
    }
}

enum PreparedTerminal {
    Runnable {
        terminal_id: u32,
        reader: Box<dyn AsyncReader>,
        quit_callback_fence: QuitCallbackFence,
    },
    HeldTerminal {
        terminal_id: u32,
        run_command: RunCommand,
        command_not_found: bool,
    },
}

impl PreparedTerminal {
    fn terminal_id(&self) -> u32 {
        match self {
            PreparedTerminal::Runnable { terminal_id, .. }
            | PreparedTerminal::HeldTerminal { terminal_id, .. } => *terminal_id,
        }
    }

    fn layout_entry(&self) -> (u32, Option<RunCommand>) {
        match self {
            PreparedTerminal::Runnable { terminal_id, .. } => (*terminal_id, None),
            PreparedTerminal::HeldTerminal {
                terminal_id,
                run_command,
                ..
            } => (*terminal_id, Some(run_command.clone())),
        }
    }
}

struct PreparedTabOverride {
    tab_result: TabOverrideResult,
    terminals: Vec<PreparedTerminal>,
    originating_plugins_to_inform: Vec<(u32, OriginatingPlugin)>,
}

struct PendingLayoutCommit {
    allocation_ledger: LayoutAllocationLedger,
    terminals: Vec<PreparedTerminal>,
    originating_plugins_to_inform: Vec<(u32, OriginatingPlugin)>,
    tab_id: Option<usize>,
}

struct LayoutCommitStartError {
    error: anyhow::Error,
    pending_commit: Box<PendingLayoutCommit>,
}

impl LayoutCommitStartError {
    fn new(error: anyhow::Error, pending_commit: PendingLayoutCommit) -> Self {
        Self {
            error,
            pending_commit: Box::new(pending_commit),
        }
    }

    fn into_parts(self) -> (anyhow::Error, PendingLayoutCommit) {
        (self.error, *self.pending_commit)
    }
}

#[derive(Clone)]
struct LayoutCommitReceipt {
    outcome: LayoutCommitOutcome,
    local_error: Option<String>,
    ack_result: std::result::Result<(), String>,
}

impl LayoutCommitReceipt {
    fn replay(&self) -> (Result<()>, std::result::Result<(), String>) {
        let local_result = self
            .local_error
            .as_ref()
            .map_or_else(|| Ok(()), |message| Err(anyhow!(message.clone())));
        (local_result, self.ack_result.clone())
    }
}

pub(crate) struct Pty {
    pub active_panes: HashMap<ClientId, PaneId>,
    pub bus: Bus<PtyInstruction>,
    pub id_to_child_pid: HashMap<u32, u32>, // terminal_id => child pid
    originating_plugins: HashMap<u32, OriginatingPlugin>,
    debug_to_file: bool,
    task_handles: HashMap<u32, JoinHandle<()>>, // terminal_id to join-handle
    default_editor: Option<PathBuf>,
    post_command_discovery_hook: Option<String>,
    plugin_cwds: HashMap<u32, PathBuf>,   // plugin_id -> cwd
    terminal_cwds: HashMap<u32, PathBuf>, // terminal_id -> cwd
    pane_activity_flags: HashMap<u32, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    terminal_cmds: HashMap<u32, Vec<String>>,
    terminal_foreground_cmds: HashMap<u32, Vec<String>>,
    pending_layout_commits: HashMap<LayoutTransactionId, PendingLayoutCommit>,
    resolved_layout_commits: BTreeMap<LayoutTransactionId, LayoutCommitReceipt>,
}

pub(crate) fn pty_thread_main(mut pty: Pty, layout: Box<Layout>) -> Result<()> {
    let result = pty_thread_main_loop(&mut pty, layout);
    // This is intentionally unconditional: any `?` in the instruction loop is
    // another thread-exit path. Draining here makes those failures obey the
    // same transaction rollback contract as explicit Exit and channel
    // disconnect. Explicit paths may already have drained the map; a second
    // drain is a no-op.
    pty.rollback_pending_layout_commits_on_exit();
    result
}

fn pty_thread_main_loop(pty: &mut Pty, layout: Box<Layout>) -> Result<()> {
    loop {
        let (event, mut err_ctx) = match pty.bus.recv() {
            Ok(event) => event,
            Err(error) => {
                log::error!("PTY instruction channel disconnected: {error}");
                pty.rollback_pending_layout_commits_on_exit();
                break;
            },
        };
        err_ctx.add_call(ContextType::Pty((&event).into()));
        match event {
            PtyInstruction::SpawnTerminal(
                terminal_action,
                name,
                new_pane_placement,
                start_suppressed,
                client_or_tab_index,
                completion_tx,
                set_blocking,
            ) => {
                let err_context =
                    || format!("failed to spawn terminal for {:?}", client_or_tab_index);

                let (hold_on_close, run_command, pane_title, open_file_payload) =
                    match &terminal_action {
                        Some(TerminalAction::RunCommand(run_command)) => (
                            run_command.hold_on_close,
                            Some(run_command.clone()),
                            if name.is_some() {
                                // User explicitly provided a name — use it regardless
                                // of use_terminal_title
                                name
                            } else if run_command.use_terminal_title {
                                None
                            } else {
                                Some(run_command.to_string())
                            },
                            None,
                        ),
                        Some(TerminalAction::OpenFile(open_file_payload)) => {
                            (false, None, name, Some(open_file_payload.clone()))
                        },
                        _ => (false, None, name, None),
                    };
                let invoked_with = match &terminal_action {
                    Some(TerminalAction::RunCommand(run_command)) => {
                        Some(Run::Command(run_command.clone()))
                    },
                    Some(TerminalAction::OpenFile(payload)) => Some(Run::EditFile(
                        payload.path.clone(),
                        payload.line_number,
                        payload.cwd.clone(),
                    )),
                    _ => None,
                };
                match pty
                    .spawn_terminal(terminal_action, client_or_tab_index)
                    .with_context(err_context)
                {
                    Ok((pid, starts_held)) => {
                        let hold_for_command = if starts_held {
                            run_command.clone()
                        } else {
                            None
                        };

                        // if this command originated in a plugin, we send the plugin back an event
                        // to let it know the command started and which pane_id it has
                        if let Some(originating_plugin) =
                            run_command.and_then(|r| r.originating_plugin)
                        {
                            pty.originating_plugins
                                .insert(pid, originating_plugin.clone());
                            let update_event =
                                Event::CommandPaneOpened(pid, originating_plugin.context.clone());
                            pty.bus
                                .senders
                                .send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(originating_plugin.plugin_id),
                                    Some(originating_plugin.client_id),
                                    update_event,
                                )]))
                                .with_context(err_context)?;
                        }
                        if let Some(originating_plugin) =
                            open_file_payload.and_then(|o| o.originating_plugin)
                        {
                            let update_event =
                                Event::EditPaneOpened(pid, originating_plugin.context.clone());
                            pty.bus
                                .senders
                                .send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(originating_plugin.plugin_id),
                                    Some(originating_plugin.client_id),
                                    update_event,
                                )]))
                                .with_context(err_context)?;
                        }

                        pty.bus
                            .senders
                            .send_to_screen(ScreenInstruction::NewPane(
                                PaneId::Terminal(pid),
                                pane_title,
                                hold_for_command,
                                invoked_with,
                                new_pane_placement,
                                start_suppressed,
                                client_or_tab_index,
                                completion_tx,
                                set_blocking,
                            ))
                            .with_context(err_context)?;
                    },
                    Err(err) => match err.downcast_ref::<ZellijError>() {
                        Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                            if hold_on_close {
                                let hold_for_command = None; // we do not hold an "error" pane
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::NewPane(
                                        PaneId::Terminal(*terminal_id),
                                        pane_title,
                                        hold_for_command,
                                        invoked_with,
                                        new_pane_placement,
                                        start_suppressed,
                                        client_or_tab_index,
                                        completion_tx,
                                        set_blocking,
                                    ))
                                    .with_context(err_context)?;
                                if let Some(run_command) = run_command {
                                    send_command_not_found_to_screen(
                                        pty.bus.senders.clone(),
                                        *terminal_id,
                                        run_command.clone(),
                                    )
                                    .with_context(err_context)?;
                                }
                            } else {
                                log::error!("Failed to spawn terminal: {:?}", err);
                                pty.close_pane(PaneId::Terminal(*terminal_id))
                                    .with_context(err_context)?;
                            }
                        },
                        _ => Err::<(), _>(err).non_fatal(),
                    },
                }
            },
            PtyInstruction::SpawnInPlaceTerminal(
                terminal_action,
                name,
                close_replaced_pane,
                client_id_tab_index_or_pane_id,
                completion_tx,
            ) => {
                let err_context = || {
                    format!(
                        "failed to spawn terminal for {:?}",
                        client_id_tab_index_or_pane_id
                    )
                };
                let (hold_on_close, run_command, pane_title) = match &terminal_action {
                    Some(TerminalAction::RunCommand(run_command)) => (
                        run_command.hold_on_close,
                        Some(run_command.clone()),
                        if run_command.use_terminal_title {
                            None
                        } else {
                            Some(name.unwrap_or_else(|| run_command.to_string()))
                        },
                    ),
                    _ => (false, None, name),
                };
                let invoked_with = match &terminal_action {
                    Some(TerminalAction::RunCommand(run_command)) => {
                        Some(Run::Command(run_command.clone()))
                    },
                    Some(TerminalAction::OpenFile(payload)) => Some(Run::EditFile(
                        payload.path.clone(),
                        payload.line_number,
                        payload.cwd.clone(),
                    )),
                    _ => None,
                };
                match pty
                    .spawn_terminal(terminal_action, client_id_tab_index_or_pane_id)
                    .with_context(err_context)
                {
                    Ok((pid, starts_held)) => {
                        let hold_for_command = if starts_held { run_command } else { None };
                        pty.bus
                            .senders
                            .send_to_screen(ScreenInstruction::ReplacePane(
                                PaneId::Terminal(pid),
                                hold_for_command,
                                pane_title,
                                invoked_with,
                                close_replaced_pane,
                                client_id_tab_index_or_pane_id,
                                completion_tx,
                            ))
                            .with_context(err_context)?;
                    },
                    Err(err) => match err.downcast_ref::<ZellijError>() {
                        Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                            if hold_on_close {
                                let hold_for_command = None; // we do not hold an "error" pane
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::ReplacePane(
                                        PaneId::Terminal(*terminal_id),
                                        hold_for_command,
                                        pane_title,
                                        invoked_with,
                                        close_replaced_pane,
                                        client_id_tab_index_or_pane_id,
                                        completion_tx,
                                    ))
                                    .with_context(err_context)?;
                                if let Some(run_command) = run_command {
                                    send_command_not_found_to_screen(
                                        pty.bus.senders.clone(),
                                        *terminal_id,
                                        run_command.clone(),
                                    )
                                    .with_context(err_context)?;
                                }
                            } else {
                                log::error!("Failed to spawn terminal: {:?}", err);
                                pty.close_pane(PaneId::Terminal(*terminal_id))
                                    .with_context(err_context)?;
                            }
                        },
                        _ => Err::<(), _>(err).non_fatal(),
                    },
                }
            },
            PtyInstruction::OpenInPlaceEditor(
                temp_file,
                line_number,
                client_tab_index_or_pane_id,
                _completion_tx,
            ) => {
                let err_context = || "failed to open in-place editor for client".to_string();

                match pty.spawn_terminal(
                    Some(TerminalAction::OpenFile(OpenFilePayload::new(
                        temp_file,
                        line_number,
                        None,
                    ))),
                    client_tab_index_or_pane_id,
                ) {
                    Ok((pid, _starts_held)) => {
                        pty.bus
                            .senders
                            .send_to_screen(ScreenInstruction::OpenInPlaceEditor(
                                PaneId::Terminal(pid),
                                client_tab_index_or_pane_id,
                            ))
                            .with_context(err_context)?;
                    },
                    Err(e) => {
                        Err::<(), _>(e).with_context(err_context).non_fatal();
                    },
                }
            },
            PtyInstruction::UpdateActivePane(pane_id, client_id) => {
                pty.set_active_pane(pane_id, client_id);
            },
            PtyInstruction::GoToTab(tab_index, client_id) => {
                pty.bus
                    .senders
                    .send_to_screen(ScreenInstruction::GoToTab(tab_index, Some(client_id), None))
                    .with_context(|| {
                        format!("failed to move client {} to tab {}", client_id, tab_index)
                    })?;
            },
            PtyInstruction::NewTab(
                cwd,
                terminal_action,
                tab_layout,
                floating_panes_layout,
                tab_index,
                transaction_id,
                plugin_ids,
                initial_panes,
                block_on_first_terminal,
                should_change_focus_to_new_tab,
                client_id_and_is_web_client,
                completion_tx,
                layout_generation,
            ) => {
                let err_context = || "failed to open new tab";
                log::info!(
                    "PtyInstruction::NewTab: spawning terminals for tab {}",
                    tab_index
                );

                let floating_panes_layout = if floating_panes_layout.is_empty() {
                    layout.new_tab().1
                } else {
                    floating_panes_layout
                };
                if let Err(e) = pty.spawn_terminals_for_layout(
                    cwd,
                    (*tab_layout).unwrap_or_else(|| layout.new_tab().0),
                    floating_panes_layout,
                    terminal_action.clone(),
                    plugin_ids,
                    initial_panes,
                    tab_index,
                    transaction_id,
                    block_on_first_terminal,
                    should_change_focus_to_new_tab,
                    client_id_and_is_web_client,
                    completion_tx,
                    layout_generation,
                ) {
                    Err::<(), _>(e).with_context(err_context).non_fatal();
                }
            },
            PtyInstruction::OverrideLayout(
                cwd,
                default_shell,
                tab_layouts_with_plugin_ids,
                transaction_id,
                retain_existing_terminal_panes,
                retain_existing_plugin_panes,
                client_id,
                completion_tx,
                layout_generation,
            ) => {
                let err_context = || "failed to override layout";
                if let Err(error) = pty.override_layout_transaction(
                    cwd,
                    default_shell,
                    tab_layouts_with_plugin_ids,
                    transaction_id,
                    retain_existing_terminal_panes,
                    retain_existing_plugin_panes,
                    client_id,
                    completion_tx,
                    layout_generation,
                ) {
                    Err::<(), _>(error).with_context(err_context).non_fatal();
                }
            },
            PtyInstruction::ClosePane(id, _completion_tx) => {
                pty.close_pane(id)
                    .and_then(|_| {
                        pty.bus
                            .senders
                            .send_to_server(ServerInstruction::UnblockInputThread)
                    })
                    .with_context(|| format!("failed to close pane {:?}", id))?;
            },
            PtyInstruction::CloseTab(ids) => {
                pty.close_tab(ids)
                    .and_then(|_| {
                        pty.bus
                            .senders
                            .send_to_server(ServerInstruction::UnblockInputThread)
                    })
                    .context("failed to close tabs")?;
            },
            PtyInstruction::ReRunCommandInPane(pane_id, run_command, _completion_tx) => {
                let err_context = || format!("failed to rerun command in pane {:?}", pane_id);

                match pty
                    .rerun_command_in_pane(pane_id, run_command.clone())
                    .with_context(err_context)
                {
                    Ok(..) => {},
                    Err(err) => match err.downcast_ref::<ZellijError>() {
                        Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                            if run_command.hold_on_close {
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::PtyBytes(
                                        *terminal_id,
                                        format!(
                                            "Command not found: {}",
                                            run_command.command.display()
                                        )
                                        .as_bytes()
                                        .to_vec(),
                                    ))
                                    .with_context(err_context)?;
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::HoldPane(
                                        PaneId::Terminal(*terminal_id),
                                        Some(2), // exit status
                                        run_command,
                                    ))
                                    .with_context(err_context)?;
                            }
                        },
                        _ => Err::<(), _>(err).non_fatal(),
                    },
                }
            },
            PtyInstruction::DropToShellInPane {
                pane_id,
                shell,
                working_dir,
                completion_tx: _completion_tx,
            } => {
                let err_context = || format!("failed to rerun command in pane {:?}", pane_id);

                // TODO: get configured default_shell from screen/tab as an option and default to
                // this otherwise (also look for a place that turns get_default_shell into a
                // RunCommand, we might have done this before)
                let run_command = RunCommand {
                    command: shell.unwrap_or_else(get_default_shell),
                    hold_on_close: false,
                    hold_on_start: false,
                    cwd: working_dir,
                    ..Default::default()
                };
                match pty
                    .rerun_command_in_pane(pane_id, run_command.clone())
                    .with_context(err_context)
                {
                    Ok(..) => {},
                    Err(err) => match err.downcast_ref::<ZellijError>() {
                        Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                            if run_command.hold_on_close {
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::PtyBytes(
                                        *terminal_id,
                                        format!(
                                            "Command not found: {}",
                                            run_command.command.display()
                                        )
                                        .as_bytes()
                                        .to_vec(),
                                    ))
                                    .with_context(err_context)?;
                                pty.bus
                                    .senders
                                    .send_to_screen(ScreenInstruction::HoldPane(
                                        PaneId::Terminal(*terminal_id),
                                        Some(2), // exit status
                                        run_command,
                                    ))
                                    .with_context(err_context)?;
                            }
                        },
                        _ => Err::<(), _>(err).non_fatal(),
                    },
                }
            },
            PtyInstruction::DumpLayout(mut session_layout_metadata, client_id, completion_tx) => {
                let err_context = || "Failed to dump layout".to_string();
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                match session_serialization::serialize_session_layout(
                    session_layout_metadata.into(),
                ) {
                    Ok((kdl_layout, _pane_contents)) => {
                        pty.bus
                            .senders
                            .send_to_server(ServerInstruction::Log(
                                vec![kdl_layout],
                                client_id,
                                completion_tx,
                            ))
                            .with_context(err_context)
                            .non_fatal();
                    },
                    Err(e) => {
                        pty.bus
                            .senders
                            .send_to_server(ServerInstruction::Log(
                                vec![e.to_owned()],
                                client_id,
                                completion_tx,
                            ))
                            .with_context(err_context)
                            .non_fatal();
                    },
                }
            },
            PtyInstruction::ListClientsMetadata(
                mut session_layout_metadata,
                client_id,
                completion_tx,
            ) => {
                let err_context = || "Failed to dump layout".to_string();
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                pty.bus
                    .senders
                    .send_to_server(ServerInstruction::Log(
                        vec![format!(
                            "{}",
                            session_layout_metadata.list_clients_metadata(),
                        )],
                        client_id,
                        completion_tx,
                    ))
                    .with_context(err_context)
                    .non_fatal();
            },
            PtyInstruction::DumpLayoutToPlugin {
                mut session_layout_metadata,
                plugin_id,
                response_channel,
            } => {
                let err_context = || "Failed to dump layout".to_string();
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                pty.bus
                    .senders
                    .send_to_plugin(PluginInstruction::DumpLayoutToPlugin {
                        session_layout_metadata,
                        plugin_id,
                        response_channel,
                    })
                    .with_context(err_context)
                    .non_fatal();
            },
            PtyInstruction::ListClientsToPlugin(
                mut session_layout_metadata,
                plugin_id,
                client_id,
            ) => {
                let err_context = || "Failed to dump layout".to_string();
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                pty.bus
                    .senders
                    .send_to_plugin(PluginInstruction::ListClientsToPlugin(
                        session_layout_metadata,
                        plugin_id,
                        client_id,
                    ))
                    .with_context(err_context)
                    .non_fatal();
            },
            PtyInstruction::ReportPluginCwd(plugin_id, cwd) => {
                pty.plugin_cwds.insert(plugin_id, cwd);
            },
            PtyInstruction::LogLayoutToHd {
                session_name,
                generation,
                mut session_layout_metadata,
            } => {
                let err_context = || "Failed to dump layout".to_string();
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                if session_layout_metadata.is_dirty() {
                    match session_serialization::serialize_session_layout(
                        session_layout_metadata.into(),
                    ) {
                        Ok(kdl_layout_and_pane_contents) => {
                            pty.bus
                                .senders
                                .send_to_background_jobs(BackgroundJob::ReportLayoutInfo(
                                    SessionLayoutSnapshot {
                                        session_name,
                                        generation,
                                        layout: kdl_layout_and_pane_contents,
                                    },
                                ))
                                .with_context(err_context)?;
                        },
                        Err(e) => {
                            log::error!("Failed to log layout to HD: {}", e);
                        },
                    }
                }
            },
            PtyInstruction::SaveSessionToDisk {
                session_name,
                session_info,
                mut session_layout_metadata,
                generation,
                mut completion_tx,
            } => {
                pty.populate_session_layout_metadata(&mut session_layout_metadata);
                match session_serialization::serialize_session_layout(
                    session_layout_metadata.into(),
                ) {
                    Ok(kdl_and_files) => {
                        match write_session_state_to_disk(
                            generation,
                            session_name.clone(),
                            session_info,
                            kdl_and_files.clone(),
                        ) {
                            Err(error) => {
                                log::error!("Failed to save session to durable storage: {}", error);
                                if let Some(completion_tx) = completion_tx.as_mut() {
                                    completion_tx.set_exit_status(1);
                                    completion_tx.set_error_message(error);
                                }
                            },
                            Ok(false) => {
                                let error = format!(
                                    "session save generation {} for '{}' was superseded before commit; retry the save",
                                    generation, session_name
                                );
                                log::error!("{}", error);
                                if let Some(completion_tx) = completion_tx.as_mut() {
                                    completion_tx.set_exit_status(1);
                                    completion_tx.set_error_message(error);
                                }
                            },
                            Ok(true) => {
                                // Update session save time for plugin query only after
                                // all resurrection files reached durable storage.
                                let timestamp_millis = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                                    as u64;
                                let _ = pty.bus.senders.send_to_plugin(
                                    PluginInstruction::UpdateSessionSaveTime(timestamp_millis),
                                );

                                let _ = pty.bus.senders.send_to_background_jobs(
                                    BackgroundJob::ReportLayoutInfo(SessionLayoutSnapshot {
                                        session_name,
                                        generation,
                                        layout: kdl_and_files,
                                    }),
                                );
                            },
                        }
                    },
                    Err(e) => {
                        log::error!("Failed to serialize layout: {}", e);
                        if let Some(completion_tx) = completion_tx.as_mut() {
                            completion_tx.set_exit_status(1);
                            completion_tx
                                .set_error_message(format!("Failed to serialize layout: {}", e));
                        }
                    },
                };
            },
            PtyInstruction::FillPluginCwd(
                should_float,
                should_be_open_in_place,
                close_replaced_pane,
                pane_title,
                run,
                tab_index,
                pane_id_to_replace,
                client_id,
                size,
                skip_cache,
                cwd,
                should_focus_plugin,
                floating_pane_coordinates,
                completion_tx,
            ) => {
                pty.fill_plugin_cwd(
                    should_float,
                    should_be_open_in_place,
                    close_replaced_pane,
                    pane_title,
                    run,
                    tab_index,
                    pane_id_to_replace,
                    client_id,
                    size,
                    skip_cache,
                    cwd,
                    should_focus_plugin,
                    floating_pane_coordinates,
                    completion_tx,
                )?;
            },
            PtyInstruction::Reconfigure {
                default_editor,
                post_command_discovery_hook,
                client_id: _,
            } => {
                pty.reconfigure(default_editor, post_command_discovery_hook);
            },
            PtyInstruction::SendSigintToPaneId(pane_id) => {
                pty.send_sigint_to_pane(pane_id);
            },
            PtyInstruction::SendSigkillToPaneId(pane_id) => {
                pty.send_sigkill_to_pane(pane_id);
            },
            PtyInstruction::GetPanePid {
                pane_id,
                response_channel,
            } => {
                let response = pty.get_pane_pid(pane_id);
                let _ = response_channel.send(response);
            },
            PtyInstruction::GetPaneRunningCommand {
                pane_id,
                response_channel,
            } => {
                let response = pty.get_pane_running_command(pane_id);
                let _ = response_channel.send(response);
            },
            PtyInstruction::GetPaneCwd {
                pane_id,
                response_channel,
            } => {
                let response = pty.get_pane_cwd(pane_id);
                let _ = response_channel.send(response);
            },
            PtyInstruction::UpdateAndReportCwds => {
                pty.update_and_report_cwds();
            },
            PtyInstruction::NotifyCwdFromOsc7(terminal_id, path) => {
                pty.notify_cwd_from_osc7(terminal_id, path);
            },
            PtyInstruction::LayoutCommitResolved {
                transaction_id,
                outcome,
                ack,
            } => {
                let (resolution, ack_result) =
                    pty.resolve_layout_commit_with_ack(transaction_id, outcome);
                let _ = ack.send(ack_result);
                resolution.non_fatal();
            },
            PtyInstruction::Exit => {
                pty.rollback_pending_layout_commits_on_exit();
                break;
            },
        }
    }
    Ok(())
}

impl Pty {
    pub fn new(
        bus: Bus<PtyInstruction>,
        debug_to_file: bool,
        default_editor: Option<PathBuf>,
        post_command_discovery_hook: Option<String>,
    ) -> Self {
        Pty {
            active_panes: HashMap::new(),
            bus,
            id_to_child_pid: HashMap::new(),
            debug_to_file,
            task_handles: HashMap::new(),
            default_editor,
            originating_plugins: HashMap::new(),
            post_command_discovery_hook,
            plugin_cwds: HashMap::new(),
            terminal_cwds: HashMap::new(),
            pane_activity_flags: HashMap::new(),
            terminal_cmds: HashMap::new(),
            terminal_foreground_cmds: HashMap::new(),
            pending_layout_commits: HashMap::new(),
            resolved_layout_commits: BTreeMap::new(),
        }
    }

    fn begin_layout_commit(
        &mut self,
        transaction_id: LayoutTransactionId,
        pending_commit: PendingLayoutCommit,
    ) -> std::result::Result<(), LayoutCommitStartError> {
        if transaction_id == 0 {
            return Err(LayoutCommitStartError::new(
                anyhow!("layout transaction id 0 is reserved"),
                pending_commit,
            ));
        }
        if self.resolved_layout_commits.contains_key(&transaction_id) {
            return Err(LayoutCommitStartError::new(
                anyhow!("layout transaction id {transaction_id} is already resolved"),
                pending_commit,
            ));
        }
        match self.pending_layout_commits.entry(transaction_id) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(pending_commit);
                Ok(())
            },
            std::collections::hash_map::Entry::Occupied(_) => Err(LayoutCommitStartError::new(
                anyhow!("duplicate pending layout transaction id {transaction_id}"),
                pending_commit,
            )),
        }
    }

    fn resolve_layout_commit_with_ack(
        &mut self,
        transaction_id: LayoutTransactionId,
        outcome: LayoutCommitOutcome,
    ) -> (Result<()>, std::result::Result<(), String>) {
        if let Some(receipt) = self.resolved_layout_commits.get(&transaction_id) {
            if receipt.outcome == outcome {
                return receipt.replay();
            }
            let failure = format!(
                "conflicting resolution for layout transaction {transaction_id}: recorded {:?}, received {:?}",
                receipt.outcome, outcome
            );
            return (Err(anyhow!(failure.clone())), Err(failure));
        }
        let Some(pending_commit) = self.pending_layout_commits.remove(&transaction_id) else {
            let failure = format!("cannot resolve unknown layout transaction {transaction_id}");
            log::error!("{failure}");
            return (Err(anyhow!(failure.clone())), Err(failure));
        };

        let recorded_outcome = outcome.clone();
        let resolution = match outcome {
            LayoutCommitOutcome::Committed => {
                let allocation_ledger = pending_commit.allocation_ledger;
                for prepared_terminal in pending_commit.terminals {
                    if let Err(error) = self.activate_prepared_terminal(prepared_terminal) {
                        let resolution = self.layout_activation_failure(
                            transaction_id,
                            error,
                            allocation_ledger,
                        );
                        self.record_layout_commit_receipt(
                            transaction_id,
                            recorded_outcome,
                            &resolution,
                        );
                        return resolution;
                    }
                }
                for (terminal_id, originating_plugin) in
                    pending_commit.originating_plugins_to_inform
                {
                    if let Err(error) =
                        self.inform_originating_plugin_of_open(terminal_id, originating_plugin)
                    {
                        let resolution = self.layout_activation_failure(
                            transaction_id,
                            error,
                            allocation_ledger,
                        );
                        self.record_layout_commit_receipt(
                            transaction_id,
                            recorded_outcome,
                            &resolution,
                        );
                        return resolution;
                    }
                }
                // Keep the allocation ledger armed until every prepared
                // runtime surface has crossed its activation fence. The
                // suspended-allocation checkpoint will make activation
                // fallible; this ordering is the invariant it relies on.
                allocation_ledger.disarm();
                (Ok(()), Ok(()))
            },
            LayoutCommitOutcome::Rejected(message) => {
                let business_failure =
                    format!("screen rejected layout transaction {transaction_id}: {message}");
                let cleanup_errors =
                    self.cleanup_layout_allocations(pending_commit.allocation_ledger);
                if cleanup_errors.is_empty() {
                    // Rejection is the expected business outcome. The ACK only
                    // certifies that PTY resolved its ledger, so a successful
                    // cleanup must ACK Ok even though the local diagnostic
                    // remains an Err for logs and direct tests.
                    (Err(anyhow!(business_failure)), Ok(()))
                } else {
                    let failure = format!(
                        "{business_failure}: failed to release one or more partial layout allocations: {}",
                        cleanup_errors.join("; ")
                    );
                    (Err(anyhow!(failure.clone())), Err(failure))
                }
            },
        };
        self.record_layout_commit_receipt(transaction_id, recorded_outcome, &resolution);
        resolution
    }

    fn record_layout_commit_receipt(
        &mut self,
        transaction_id: LayoutTransactionId,
        outcome: LayoutCommitOutcome,
        resolution: &(Result<()>, std::result::Result<(), String>),
    ) {
        let receipt = LayoutCommitReceipt {
            outcome,
            local_error: resolution
                .0
                .as_ref()
                .err()
                .map(|error| format!("{error:#}")),
            ack_result: resolution.1.clone(),
        };
        self.resolved_layout_commits.insert(transaction_id, receipt);
        while self.resolved_layout_commits.len() > MAX_LAYOUT_COMMIT_RECEIPTS {
            let Some(oldest_transaction_id) = self.resolved_layout_commits.keys().next().copied()
            else {
                break;
            };
            self.resolved_layout_commits.remove(&oldest_transaction_id);
        }
    }

    fn layout_activation_failure(
        &mut self,
        transaction_id: LayoutTransactionId,
        error: anyhow::Error,
        allocation_ledger: LayoutAllocationLedger,
    ) -> (Result<()>, std::result::Result<(), String>) {
        let error = self.rollback_partial_layout_allocations(
            error.context(format!(
                "failed to activate committed layout transaction {transaction_id}"
            )),
            allocation_ledger,
        );
        let ack_error = format!("{error:#}");
        (Err(error), Err(ack_error))
    }

    fn rollback_pending_layout_commits_on_exit(&mut self) {
        let pending_layout_commits = std::mem::take(&mut self.pending_layout_commits);
        for (transaction_id, pending_commit) in pending_layout_commits {
            let tab_id = pending_commit.tab_id;
            let error = self.rollback_partial_layout_allocations(
                anyhow!("PTY exited with layout transaction {transaction_id} unresolved"),
                pending_commit.allocation_ledger,
            );
            let rejection =
                self.reject_layout_preparation(transaction_id, tab_id, None, None, error);
            Err::<(), _>(rejection).non_fatal();
        }
    }

    fn reject_pending_layout_send(
        &mut self,
        transaction_id: LayoutTransactionId,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let Some(pending_commit) = self.pending_layout_commits.remove(&transaction_id) else {
            return error.context(format!(
                "layout transaction {transaction_id} disappeared before send rollback"
            ));
        };
        self.rollback_partial_layout_allocations(error, pending_commit.allocation_ledger)
    }

    fn reject_layout_preparation(
        &mut self,
        transaction_id: LayoutTransactionId,
        tab_id: Option<usize>,
        mut completion_tx: Option<NotificationEnd>,
        layout_generation: Option<Box<DurableTabLayoutGeneration>>,
        error: anyhow::Error,
    ) -> anyhow::Error {
        let message = format!("{error:#}");
        if let Some(completion) = completion_tx.as_mut() {
            completion.mark_failure(message.clone());
        }
        let instruction = ScreenInstruction::LayoutPreparationFailed {
            transaction_id,
            tab_id,
            completion_tx,
            layout_generation,
            message: message.clone(),
        };
        if let Err(send_failure) = self.bus.senders.send_to_screen_recover(instruction) {
            let (recovered_instruction, send_error) = send_failure.into_parts();
            // The recovered instruction still owns the sole NotificationEnd
            // channel. It is already marked failed, so dropping it cannot
            // report a false success even though Screen is unavailable.
            drop(recovered_instruction);
            return error.context(format!(
                "failed to report rejected layout transaction {transaction_id} to Screen: {send_error:#}"
            ));
        }
        error
    }

    pub fn get_default_terminal(
        &self,
        cwd: Option<PathBuf>,
        default_shell: Option<TerminalAction>,
    ) -> TerminalAction {
        match default_shell {
            Some(mut default_shell) => {
                if let Some(cwd) = cwd {
                    match default_shell {
                        TerminalAction::RunCommand(ref mut command) => {
                            command.cwd = Some(cwd);
                        },
                        TerminalAction::OpenFile(ref mut payload) => {
                            match payload.cwd.as_mut() {
                                Some(edit_cwd) => {
                                    *edit_cwd = cwd.join(&edit_cwd);
                                },
                                None => {
                                    let _ = payload.cwd.insert(cwd.clone());
                                },
                            };
                        },
                    }
                }
                default_shell
            },
            None => {
                let shell = get_default_shell();
                TerminalAction::RunCommand(RunCommand {
                    args: vec![],
                    command: shell,
                    cwd, // note: this might also be filled by the calling function, eg. spawn_terminal
                    hold_on_close: false,
                    hold_on_start: false,
                    ..Default::default()
                })
            },
        }
    }
    fn fill_cwd(&self, terminal_action: &mut TerminalAction, client_id: ClientId) {
        let cwd = match terminal_action {
            TerminalAction::RunCommand(run_command) => &mut run_command.cwd,
            TerminalAction::OpenFile(payload) => &mut payload.cwd,
        };
        if cwd.is_none() {
            *cwd = self
                .active_panes
                .get(&client_id)
                .and_then(|pane| match pane {
                    PaneId::Plugin(plugin_id) => self.plugin_cwds.get(plugin_id).cloned(),
                    PaneId::Terminal(id) => {
                        // Try to get CWD from OS, fall back to cached value
                        self.id_to_child_pid
                            .get(id)
                            .and_then(|&pid| {
                                self.bus
                                    .os_input
                                    .as_ref()
                                    .and_then(|input| input.get_cwd(pid))
                            })
                            .or_else(|| self.terminal_cwds.get(id).cloned())
                    },
                })
        };
    }
    fn fill_cwd_from_pane_id(&self, terminal_action: &mut TerminalAction, pane_id: &PaneId) {
        let cwd = match terminal_action {
            TerminalAction::RunCommand(run_command) => &mut run_command.cwd,
            TerminalAction::OpenFile(payload) => &mut payload.cwd,
        };
        if cwd.is_none() {
            *cwd = match pane_id {
                PaneId::Terminal(terminal_pane_id) => {
                    // Try to get CWD from OS, fall back to cached value
                    self.id_to_child_pid
                        .get(terminal_pane_id)
                        .and_then(|&pid| {
                            self.bus
                                .os_input
                                .as_ref()
                                .and_then(|input| input.get_cwd(pid))
                        })
                        .or_else(|| self.terminal_cwds.get(terminal_pane_id).cloned())
                },
                PaneId::Plugin(plugin_id) => self.plugin_cwds.get(plugin_id).cloned(),
            };
        };
    }
    pub fn spawn_terminal(
        &mut self,
        terminal_action: Option<TerminalAction>,
        client_or_tab_index: ClientTabIndexOrPaneId,
    ) -> Result<(u32, bool)> {
        // bool is starts_held
        let err_context = || format!("failed to spawn terminal for {:?}", client_or_tab_index);

        // returns the terminal id
        let terminal_action = match client_or_tab_index {
            ClientTabIndexOrPaneId::ClientId(client_id) => {
                let mut terminal_action =
                    terminal_action.unwrap_or_else(|| self.get_default_terminal(None, None));
                self.fill_cwd(&mut terminal_action, client_id);
                terminal_action
            },
            ClientTabIndexOrPaneId::TabIndex(_) => {
                terminal_action.unwrap_or_else(|| self.get_default_terminal(None, None))
            },
            ClientTabIndexOrPaneId::PaneId(pane_id) => {
                let mut terminal_action =
                    terminal_action.unwrap_or_else(|| self.get_default_terminal(None, None));
                self.fill_cwd_from_pane_id(&mut terminal_action, &pane_id);
                terminal_action
            },
        };
        let (hold_on_start, hold_on_close, originating_command_plugin, originating_edit_plugin) =
            match &terminal_action {
                TerminalAction::RunCommand(run_command) => (
                    run_command.hold_on_start,
                    run_command.hold_on_close,
                    run_command.originating_plugin.clone(),
                    None,
                ),
                TerminalAction::OpenFile(open_file_payload) => (
                    false,
                    false,
                    None,
                    open_file_payload.originating_plugin.clone(),
                ),
            };

        if hold_on_start {
            // we don't actually open a terminal in this case, just wait for the user to run it
            let starts_held = hold_on_start;
            let terminal_id = self
                .bus
                .os_input
                .as_mut()
                .context("couldn't get mutable reference to OS interface")
                .and_then(|os_input| os_input.reserve_terminal_id())
                .with_context(err_context)?;
            return Ok((terminal_id, starts_held));
        }

        let originating_command_plugin = Arc::new(originating_command_plugin.clone());
        let originating_edit_plugin = Arc::new(originating_edit_plugin.clone());
        let quit_cb = Box::new({
            let senders = self.bus.senders.clone();
            move |pane_id, exit_status, command| {
                // if this command originated in a plugin, we send the plugin an event letting it
                // know the command exited and some other useful information
                if let PaneId::Terminal(pane_id) = pane_id {
                    if let Some(originating_command_plugin) = originating_command_plugin.as_ref() {
                        let update_event = Event::CommandPaneExited(
                            pane_id,
                            exit_status,
                            originating_command_plugin.context.clone(),
                        );
                        let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                            Some(originating_command_plugin.plugin_id),
                            Some(originating_command_plugin.client_id),
                            update_event,
                        )]));
                    }
                    if let Some(originating_edit_plugin) = originating_edit_plugin.as_ref() {
                        let update_event = Event::EditPaneExited(
                            pane_id,
                            exit_status,
                            originating_edit_plugin.context.clone(),
                        );
                        let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                            Some(originating_edit_plugin.plugin_id),
                            Some(originating_edit_plugin.client_id),
                            update_event,
                        )]));
                    }
                }

                if hold_on_close {
                    let _ = senders.send_to_screen(ScreenInstruction::HoldPane(
                        pane_id,
                        exit_status,
                        command,
                    ));
                } else {
                    let _ = senders.send_to_screen(ScreenInstruction::ClosePane(
                        pane_id,
                        None,
                        None,
                        exit_status,
                    ));
                }
            }
        });
        let (terminal_id, reader, child_pid): (u32, Box<dyn AsyncReader>, Option<u32>) = self
            .bus
            .os_input
            .as_mut()
            .context("no OS I/O interface found")
            .and_then(|os_input| {
                os_input.spawn_terminal(terminal_action, quit_cb, self.default_editor.clone())
            })
            .with_context(err_context)?;
        let activity_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let terminal_bytes = async_runtime().spawn({
            let err_context =
                |terminal_id: u32| format!("failed to run async task for terminal {terminal_id}");
            let senders = self.bus.senders.clone();
            let debug_to_file = self.debug_to_file;
            let activity_flag = activity_flag.clone();
            async move {
                TerminalBytes::new(terminal_id, reader, senders, debug_to_file, activity_flag)
                    .listen()
                    .await
                    .with_context(|| err_context(terminal_id))
                    .fatal();
            }
        });

        self.task_handles.insert(terminal_id, terminal_bytes);
        self.pane_activity_flags.insert(terminal_id, activity_flag);
        if let Some(child_pid) = child_pid {
            self.id_to_child_pid.insert(terminal_id, child_pid);
            self.capture_initial_cwd(terminal_id, child_pid);
        }

        let starts_held = false;
        Ok((terminal_id, starts_held))
    }
    pub fn spawn_terminals_for_layout(
        &mut self,
        cwd: Option<PathBuf>,
        layout: TiledPaneLayout,
        floating_panes_layout: Vec<FloatingPaneLayout>,
        default_shell: Option<TerminalAction>,
        plugin_ids: HashMap<RunPluginOrAlias, Vec<u32>>,
        initial_panes: Option<Vec<CommandOrPlugin>>,
        tab_index: usize,
        transaction_id: LayoutTransactionId,
        block_on_first_terminal: bool,
        should_change_focus_to_new_tab: bool,
        client_id_and_is_web_client: (ClientId, bool),
        mut completion_tx: Option<NotificationEnd>,
        mut layout_generation: Option<Box<DurableTabLayoutGeneration>>,
    ) -> Result<()> {
        let err_context = || "failed to spawn terminals for layout for".to_string();
        if transaction_id == 0
            || self.pending_layout_commits.contains_key(&transaction_id)
            || self.resolved_layout_commits.contains_key(&transaction_id)
        {
            let error =
                anyhow!("layout transaction id {transaction_id} is reserved or already pending");
            let mut allocation_ledger = LayoutAllocationLedger::armed_for_bus(&self.bus);
            allocation_ledger.track_plugin_ids(&plugin_ids);
            let error = self.rollback_partial_layout_allocations(error, allocation_ledger);
            return Err(self.reject_layout_preparation(
                transaction_id,
                Some(tab_index),
                completion_tx,
                layout_generation,
                error,
            ));
        }

        let mut default_shell =
            default_shell.unwrap_or_else(|| self.get_default_terminal(cwd, None));
        let (client_id, is_web_client) = client_id_and_is_web_client;
        self.fill_cwd(&mut default_shell, client_id);

        // Match initial_panes commands to empty slots in the layout
        let mut layout = layout;
        if let Some(ref initial_panes_vec) = initial_panes {
            for initial_pane in initial_panes_vec.iter() {
                if let CommandOrPlugin::Command(run_command_action) = initial_pane {
                    let run_command: RunCommand = run_command_action.clone().into();
                    if !layout.replace_next_empty_slot_with_run(Run::Command(run_command)) {
                        log::warn!("More initial_panes provided than empty slots available");
                        break;
                    }
                } else if let CommandOrPlugin::File(file_to_open) = initial_pane
                    && !layout.replace_next_empty_slot_with_run(Run::EditFile(
                        file_to_open.path.clone(),
                        file_to_open.line_number,
                        file_to_open.cwd.clone(),
                    ))
                {
                    log::warn!("More initial_panes provided than empty slots available");
                    break;
                }
                // Skip CommandOrPlugin::Plugin entries (already handled by plugin thread)
            }
        }

        let extracted_run_instructions = layout.extract_run_instructions();
        let extracted_floating_run_instructions = floating_panes_layout
            .iter()
            .filter(|f| !f.already_running)
            .map(|f| f.run.clone());
        let mut new_pane_pids = Vec::new();
        let mut new_floating_panes_pids = Vec::new();

        let mut originating_plugins_to_inform = vec![];
        let mut allocation_ledger = LayoutAllocationLedger::armed_for_bus(&self.bus);
        allocation_ledger.track_plugin_ids(&plugin_ids);

        for run_instruction in extracted_run_instructions {
            let originating_plugin = run_instruction.as_ref().and_then(|r| {
                if let Run::Command(run_command) = r {
                    run_command.originating_plugin.clone()
                } else {
                    None
                }
            });
            let mut terminal_id = None;
            match self.apply_run_instruction(
                run_instruction,
                default_shell.clone(),
                &mut allocation_ledger,
            ) {
                Ok(Some(prepared_terminal)) => {
                    terminal_id = Some(prepared_terminal.terminal_id());
                    new_pane_pids.push(prepared_terminal);
                },
                Ok(None) => {},
                Err(error) => {
                    let error = self.rollback_partial_layout_allocations(error, allocation_ledger);
                    return Err(self.reject_layout_preparation(
                        transaction_id,
                        Some(tab_index),
                        completion_tx,
                        layout_generation,
                        error,
                    ));
                },
            }
            if let (Some(originating_plugin), Some(terminal_id)) = (originating_plugin, terminal_id)
            {
                originating_plugins_to_inform.push((terminal_id, originating_plugin));
            }
        }
        for run_instruction in extracted_floating_run_instructions {
            let originating_plugin = run_instruction.as_ref().and_then(|r| {
                if let Run::Command(run_command) = r {
                    run_command.originating_plugin.clone()
                } else {
                    None
                }
            });
            let mut terminal_id = None;
            match self.apply_run_instruction(
                run_instruction,
                default_shell.clone(),
                &mut allocation_ledger,
            ) {
                Ok(Some(prepared_terminal)) => {
                    terminal_id = Some(prepared_terminal.terminal_id());
                    new_floating_panes_pids.push(prepared_terminal);
                },
                Ok(None) => {},
                Err(error) => {
                    let error = self.rollback_partial_layout_allocations(error, allocation_ledger);
                    return Err(self.reject_layout_preparation(
                        transaction_id,
                        Some(tab_index),
                        completion_tx,
                        layout_generation,
                        error,
                    ));
                },
            }
            if let (Some(originating_plugin), Some(terminal_id)) = (originating_plugin, terminal_id)
            {
                originating_plugins_to_inform.push((terminal_id, originating_plugin));
            }
        }

        let new_tab_pane_ids: Vec<(u32, Option<RunCommand>)> = new_pane_pids
            .iter()
            .map(PreparedTerminal::layout_entry)
            .collect();
        let new_tab_floating_pane_ids: Vec<(u32, Option<RunCommand>)> = new_floating_panes_pids
            .iter()
            .map(PreparedTerminal::layout_entry)
            .collect();

        // Track the first terminal_id if blocking is requested
        let first_initial_pane_terminal_id = if block_on_first_terminal && !new_pane_pids.is_empty()
        {
            Some(new_pane_pids[0].terminal_id())
        } else {
            None
        };

        let mut terminals = new_pane_pids;
        terminals.extend(new_floating_panes_pids);
        let pending_commit = PendingLayoutCommit {
            allocation_ledger,
            terminals,
            originating_plugins_to_inform,
            tab_id: Some(tab_index),
        };
        if let Err(start_failure) = self.begin_layout_commit(transaction_id, pending_commit) {
            let (error, pending_commit) = start_failure.into_parts();
            let error =
                self.rollback_partial_layout_allocations(error, pending_commit.allocation_ledger);
            return Err(self.reject_layout_preparation(
                transaction_id,
                Some(tab_index),
                completion_tx,
                layout_generation,
                error,
            ));
        }

        // Prepare blocking_terminal for ApplyLayout only after the PTY ledger
        // owns every allocation.  Until then the original completion remains
        // recoverable by the preparation-failure path.
        let (direct_completion_tx, blocking_terminal) =
            if let Some(terminal_id) = first_initial_pane_terminal_id {
                (None, completion_tx.take().map(|tx| (terminal_id, tx)))
            } else {
                (completion_tx.take(), None)
            };

        log::info!(
            "spawn_terminals_for_layout: {} tiled + {} floating panes created, sending ApplyLayout",
            new_tab_pane_ids.len(),
            new_tab_floating_pane_ids.len()
        );
        let instruction = ScreenInstruction::ApplyLayout(
            layout,
            floating_panes_layout,
            new_tab_pane_ids.clone(),
            new_tab_floating_pane_ids.clone(),
            plugin_ids,
            tab_index,
            should_change_focus_to_new_tab,
            (client_id, is_web_client),
            direct_completion_tx,
            blocking_terminal,
            layout_generation.take(),
            transaction_id,
        );
        if let Err(send_failure) = self.bus.senders.send_to_screen_recover(instruction) {
            let (instruction, send_error) = send_failure.into_parts();
            let (
                direct_completion_tx,
                blocking_terminal,
                recovered_layout_generation,
                recovered_expected_kind,
            ) = match instruction {
                ScreenInstruction::ApplyLayout(
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    _,
                    direct_completion_tx,
                    blocking_terminal,
                    recovered_layout_generation,
                    _,
                ) => (
                    direct_completion_tx,
                    blocking_terminal,
                    recovered_layout_generation,
                    true,
                ),
                _ => (None, None, None, false),
            };
            completion_tx = direct_completion_tx
                .or_else(|| blocking_terminal.map(|(_, completion)| completion));
            layout_generation = recovered_layout_generation;
            let handoff_error = if recovered_expected_kind {
                send_error.context(err_context())
            } else {
                send_error.context(format!(
                    "Screen handoff returned an unexpected instruction for Apply layout transaction {transaction_id}"
                ))
            };
            let error = self.reject_pending_layout_send(transaction_id, handoff_error);
            return Err(self.reject_layout_preparation(
                transaction_id,
                Some(tab_index),
                completion_tx,
                layout_generation,
                error,
            ));
        }
        Ok(())
    }
    fn override_layout_transaction(
        &mut self,
        cwd: Option<PathBuf>,
        default_shell: Option<TerminalAction>,
        tab_layouts_with_plugin_ids: Vec<(TabLayoutInfo, HashMap<RunPluginOrAlias, Vec<u32>>)>,
        transaction_id: LayoutTransactionId,
        retain_existing_terminal_panes: bool,
        retain_existing_plugin_panes: bool,
        client_id: ClientId,
        mut completion_tx: Option<NotificationEnd>,
        mut layout_generation: Option<Box<DurableTabLayoutGeneration>>,
    ) -> Result<()> {
        let mut allocation_ledger = LayoutAllocationLedger::armed_for_bus(&self.bus);
        for (_, plugin_ids) in &tab_layouts_with_plugin_ids {
            allocation_ledger.track_plugin_ids(plugin_ids);
        }
        if transaction_id == 0
            || self.pending_layout_commits.contains_key(&transaction_id)
            || self.resolved_layout_commits.contains_key(&transaction_id)
        {
            let error =
                anyhow!("layout transaction id {transaction_id} is reserved or already pending");
            let error = self.rollback_partial_layout_allocations(error, allocation_ledger);
            return Err(self.reject_layout_preparation(
                transaction_id,
                layout_generation
                    .as_ref()
                    .map(|generation| generation.tab_id),
                completion_tx,
                layout_generation,
                error,
            ));
        }
        // Keep the preflight ledger armed while each tab is prepared.
        // `track_plugin_ids` writes into a BTreeSet, so revisiting the same IDs
        // is idempotent and must not create a short-lived second guard whose
        // Drop would unload valid plugins before the Screen handoff.
        let mut all_tab_results = Vec::new();
        let mut all_prepared_terminals = Vec::new();
        let mut all_originating_plugins_to_inform = Vec::new();

        for (tab_layout_info, plugin_ids) in tab_layouts_with_plugin_ids {
            let tab_index = tab_layout_info.tab_index;
            match self.prepare_terminals_for_layout_override(
                cwd.clone(),
                tab_layout_info.tiled_layout,
                tab_layout_info.floating_layouts,
                default_shell.clone(),
                plugin_ids,
                tab_index,
                tab_layout_info.tab_name,
                client_id,
                tab_layout_info.swap_tiled_layouts,
                tab_layout_info.swap_floating_layouts,
                &mut allocation_ledger,
            ) {
                Ok(mut prepared_tab) => {
                    all_tab_results.push(prepared_tab.tab_result);
                    all_prepared_terminals.append(&mut prepared_tab.terminals);
                    all_originating_plugins_to_inform
                        .append(&mut prepared_tab.originating_plugins_to_inform);
                },
                Err(error) => {
                    let error = error.context(format!(
                        "failed to prepare recovered layout tab {tab_index}"
                    ));
                    let error = self.rollback_partial_layout_allocations(error, allocation_ledger);
                    return Err(self.reject_layout_preparation(
                        transaction_id,
                        layout_generation
                            .as_ref()
                            .map(|generation| generation.tab_id),
                        completion_tx,
                        layout_generation,
                        error,
                    ));
                },
            }
        }

        let pending_commit = PendingLayoutCommit {
            allocation_ledger,
            terminals: all_prepared_terminals,
            originating_plugins_to_inform: all_originating_plugins_to_inform,
            tab_id: layout_generation
                .as_ref()
                .map(|generation| generation.tab_id),
        };
        if let Err(start_failure) = self.begin_layout_commit(transaction_id, pending_commit) {
            let (error, pending_commit) = start_failure.into_parts();
            let error =
                self.rollback_partial_layout_allocations(error, pending_commit.allocation_ledger);
            return Err(self.reject_layout_preparation(
                transaction_id,
                layout_generation
                    .as_ref()
                    .map(|generation| generation.tab_id),
                completion_tx,
                layout_generation,
                error,
            ));
        }
        let instruction = ScreenInstruction::OverrideLayoutComplete(
            all_tab_results,
            retain_existing_terminal_panes,
            retain_existing_plugin_panes,
            client_id,
            completion_tx.take(),
            layout_generation.take(),
            transaction_id,
        );
        if let Err(send_failure) = self.bus.senders.send_to_screen_recover(instruction) {
            let (instruction, send_error) = send_failure.into_parts();
            let (recovered_completion_tx, recovered_layout_generation, recovered_expected_kind) =
                match instruction {
                    ScreenInstruction::OverrideLayoutComplete(
                        _,
                        _,
                        _,
                        _,
                        recovered_completion_tx,
                        recovered_layout_generation,
                        _,
                    ) => (recovered_completion_tx, recovered_layout_generation, true),
                    _ => (None, None, false),
                };
            completion_tx = recovered_completion_tx;
            layout_generation = recovered_layout_generation;
            let handoff_error = if recovered_expected_kind {
                send_error.context("failed to commit recovered layout")
            } else {
                send_error.context(format!(
                    "Screen handoff returned an unexpected instruction for Override layout transaction {transaction_id}"
                ))
            };
            let error = self.reject_pending_layout_send(transaction_id, handoff_error);
            return Err(self.reject_layout_preparation(
                transaction_id,
                layout_generation
                    .as_ref()
                    .map(|generation| generation.tab_id),
                completion_tx,
                layout_generation,
                error,
            ));
        }
        Ok(())
    }

    fn prepare_terminals_for_layout_override(
        &mut self,
        cwd: Option<PathBuf>,
        layout: TiledPaneLayout,
        floating_panes_layout: Vec<FloatingPaneLayout>,
        default_shell: Option<TerminalAction>,
        plugin_ids: HashMap<RunPluginOrAlias, Vec<u32>>,
        tab_index: usize,
        tab_name: Option<String>,
        client_id: ClientId,
        swap_tiled_layouts: Option<Vec<SwapTiledLayout>>,
        swap_floating_layouts: Option<Vec<SwapFloatingLayout>>,
        allocation_ledger: &mut LayoutAllocationLedger,
    ) -> Result<PreparedTabOverride> {
        allocation_ledger.track_plugin_ids(&plugin_ids);

        let mut default_shell =
            default_shell.unwrap_or_else(|| self.get_default_terminal(cwd, None));
        self.fill_cwd(&mut default_shell, client_id);

        let extracted_run_instructions = layout.extract_run_instructions();
        let extracted_floating_run_instructions = floating_panes_layout
            .iter()
            .filter(|f| !f.already_running)
            .map(|f| f.run.clone());
        let mut new_pane_pids = Vec::new();
        let mut new_floating_panes_pids = Vec::new();
        let mut originating_plugins_to_inform = vec![];

        for run_instruction in extracted_run_instructions {
            let originating_plugin = run_instruction.as_ref().and_then(|r| {
                if let Run::Command(run_command) = r {
                    run_command.originating_plugin.clone()
                } else {
                    None
                }
            });
            let mut terminal_id = None;
            match self.apply_run_instruction(
                run_instruction,
                default_shell.clone(),
                allocation_ledger,
            ) {
                Ok(Some(prepared_terminal)) => {
                    terminal_id = Some(prepared_terminal.terminal_id());
                    new_pane_pids.push(prepared_terminal);
                },
                Ok(None) => {},
                Err(error) => return Err(error),
            }
            if let (Some(originating_plugin), Some(terminal_id)) = (originating_plugin, terminal_id)
            {
                originating_plugins_to_inform.push((terminal_id, originating_plugin));
            }
        }
        for run_instruction in extracted_floating_run_instructions {
            let originating_plugin = run_instruction.as_ref().and_then(|r| {
                if let Run::Command(run_command) = r {
                    run_command.originating_plugin.clone()
                } else {
                    None
                }
            });
            let mut terminal_id = None;
            match self.apply_run_instruction(
                run_instruction,
                default_shell.clone(),
                allocation_ledger,
            ) {
                Ok(Some(prepared_terminal)) => {
                    terminal_id = Some(prepared_terminal.terminal_id());
                    new_floating_panes_pids.push(prepared_terminal);
                },
                Ok(None) => {},
                Err(error) => return Err(error),
            }
            if let (Some(originating_plugin), Some(terminal_id)) = (originating_plugin, terminal_id)
            {
                originating_plugins_to_inform.push((terminal_id, originating_plugin));
            }
        }

        let new_tab_pane_ids: Vec<(u32, Option<RunCommand>)> = new_pane_pids
            .iter()
            .map(PreparedTerminal::layout_entry)
            .collect();
        let new_tab_floating_pane_ids: Vec<(u32, Option<RunCommand>)> = new_floating_panes_pids
            .iter()
            .map(PreparedTerminal::layout_entry)
            .collect();

        let tab_result = TabOverrideResult {
            tab_index,
            tab_name,
            tiled_layout: layout,
            floating_layouts: floating_panes_layout,
            swap_tiled_layouts,
            swap_floating_layouts,
            new_terminal_pids: new_tab_pane_ids,
            new_floating_pane_pids: new_tab_floating_pane_ids,
            plugin_ids,
        };

        new_pane_pids.append(&mut new_floating_panes_pids);
        Ok(PreparedTabOverride {
            tab_result,
            terminals: new_pane_pids,
            originating_plugins_to_inform,
        })
    }

    fn rollback_partial_layout_allocations(
        &mut self,
        original_error: anyhow::Error,
        allocation_ledger: LayoutAllocationLedger,
    ) -> anyhow::Error {
        let cleanup_errors = self.cleanup_layout_allocations(allocation_ledger);
        if cleanup_errors.is_empty() {
            original_error
        } else {
            original_error.context(format!(
                "failed to release one or more partial layout allocations: {}",
                cleanup_errors.join("; ")
            ))
        }
    }

    fn cleanup_layout_allocations(
        &mut self,
        mut allocation_ledger: LayoutAllocationLedger,
    ) -> Vec<String> {
        let mut cleanup_errors = Vec::new();
        for quit_callback_fence in allocation_ledger.quit_callback_fences.drain(..) {
            quit_callback_fence.cancel();
        }
        while let Some(pane_id) = allocation_ledger.allocated_ids.pop_first() {
            if let Err(error) = self.close_pane(pane_id) {
                cleanup_errors.push(format!("{error:#}"));
            }
        }
        // Every tracked allocation was attempted exactly once above. Disable
        // the unwind guard even when an individual close failed so Drop does
        // not issue a second, ambiguous cleanup attempt.
        allocation_ledger.disarm();
        cleanup_errors
    }

    fn activate_prepared_terminal(&mut self, prepared_terminal: PreparedTerminal) -> Result<()> {
        match prepared_terminal {
            PreparedTerminal::Runnable {
                terminal_id,
                reader,
                quit_callback_fence,
            } => {
                let activity_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let terminal_bytes = async_runtime().spawn({
                    let senders = self.bus.senders.clone();
                    let debug_to_file = self.debug_to_file;
                    let activity_flag = activity_flag.clone();
                    async move {
                        TerminalBytes::new(
                            terminal_id,
                            reader,
                            senders,
                            debug_to_file,
                            activity_flag,
                        )
                        .listen()
                        .await
                        .context("failed to spawn terminals for layout")
                        .fatal();
                    }
                });
                self.task_handles.insert(terminal_id, terminal_bytes);
                self.pane_activity_flags.insert(terminal_id, activity_flag);
                quit_callback_fence.commit();
            },
            PreparedTerminal::HeldTerminal {
                terminal_id,
                run_command,
                command_not_found,
            } => {
                if command_not_found {
                    send_command_not_found_to_screen(
                        self.bus.senders.clone(),
                        terminal_id,
                        run_command,
                    )
                    .context("failed to notify screen about held command-not-found terminal")
                    .non_fatal();
                }
            },
        }
        Ok(())
    }
    fn inform_originating_plugin_of_open(
        &mut self,
        terminal_id: u32,
        originating_plugin: OriginatingPlugin,
    ) -> Result<()> {
        self.originating_plugins
            .insert(terminal_id, originating_plugin.clone());
        let update_event = Event::CommandPaneOpened(terminal_id, originating_plugin.context);
        if let Err(send_failure) =
            self.bus
                .senders
                .send_to_plugin_recover(PluginInstruction::Update(vec![(
                    Some(originating_plugin.plugin_id),
                    Some(originating_plugin.client_id),
                    update_event,
                )]))
        {
            let (_, error) = send_failure.into_parts();
            self.originating_plugins.remove(&terminal_id);
            return Err(error.context(format!(
                "failed to report terminal {terminal_id} activation to originating plugin {}",
                originating_plugin.plugin_id
            )));
        }
        Ok(())
    }

    fn apply_run_instruction(
        &mut self,
        run_instruction: Option<Run>,
        default_shell: TerminalAction,
        allocation_ledger: &mut LayoutAllocationLedger,
    ) -> Result<Option<PreparedTerminal>> {
        let err_context = || "failed to apply run instruction".to_string();
        let quit_cb = Box::new({
            let senders = self.bus.senders.clone();
            move |pane_id, exit_status, _command| {
                let _ = senders.send_to_screen(ScreenInstruction::ClosePane(
                    pane_id,
                    None,
                    None,
                    exit_status,
                ));
            }
        });

        let originating_plugin = run_instruction.as_ref().and_then(|r| {
            if let Run::Command(run_command) = r {
                run_command.originating_plugin.clone()
            } else {
                None
            }
        });

        match run_instruction {
            Some(Run::Command(mut command)) => {
                let starts_held = command.hold_on_start;
                let hold_on_close = command.hold_on_close;
                let quit_cb = Box::new({
                    let senders = self.bus.senders.clone();
                    move |pane_id, exit_status, command| {
                        if let PaneId::Terminal(terminal_pane_id) = pane_id
                            && let Some(originating_plugin) = originating_plugin.as_ref()
                        {
                            let update_event = Event::CommandPaneExited(
                                terminal_pane_id,
                                exit_status,
                                originating_plugin.context.clone(),
                            );
                            let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                Some(originating_plugin.plugin_id),
                                Some(originating_plugin.client_id),
                                update_event,
                            )]));
                        }

                        if hold_on_close {
                            let _ = senders.send_to_screen(ScreenInstruction::HoldPane(
                                pane_id,
                                exit_status,
                                command,
                            ));
                        } else {
                            let _ = senders.send_to_screen(ScreenInstruction::ClosePane(
                                pane_id,
                                None,
                                None,
                                exit_status,
                            ));
                        }
                    }
                });
                if command.cwd.is_none()
                    && let TerminalAction::RunCommand(cmd) = default_shell
                {
                    command.cwd = cmd.cwd;
                }
                let cmd = TerminalAction::RunCommand(command.clone());
                if starts_held {
                    // we don't actually open a terminal in this case, just wait for the user to run it
                    match self
                        .bus
                        .os_input
                        .as_mut()
                        .context("no OS I/O interface found")
                        .with_context(err_context)?
                        .reserve_terminal_id()
                    {
                        Ok(terminal_id) => {
                            allocation_ledger.track_terminal(terminal_id);
                            Ok(Some(PreparedTerminal::HeldTerminal {
                                terminal_id,
                                run_command: command,
                                command_not_found: false,
                            }))
                        },
                        Err(e) => Err(e),
                    }
                } else {
                    let (quit_callback_fence, quit_cb) = QuitCallbackFence::wrap(quit_cb);
                    match self
                        .bus
                        .os_input
                        .as_mut()
                        .context("no OS I/O interface found")
                        .with_context(err_context)?
                        .spawn_terminal(cmd, quit_cb, self.default_editor.clone())
                        .with_context(err_context)
                    {
                        Ok((terminal_id, reader, child_pid)) => {
                            allocation_ledger.track_terminal(terminal_id);
                            allocation_ledger
                                .track_quit_callback_fence(quit_callback_fence.clone());
                            if let Some(child_pid) = child_pid {
                                self.id_to_child_pid.insert(terminal_id, child_pid);
                                self.capture_initial_cwd(terminal_id, child_pid);
                            }
                            Ok(Some(PreparedTerminal::Runnable {
                                terminal_id,
                                reader,
                                quit_callback_fence,
                            }))
                        },
                        Err(error) => {
                            quit_callback_fence.cancel();
                            match error.downcast_ref::<ZellijError>() {
                                Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                                    let terminal_id = *terminal_id;
                                    allocation_ledger.track_terminal(terminal_id);
                                    if command.hold_on_close {
                                        Ok(Some(PreparedTerminal::HeldTerminal {
                                            terminal_id,
                                            run_command: command,
                                            command_not_found: true,
                                        }))
                                    } else {
                                        Err(error.context(
                                            "CommandNotFound terminal cannot enter a layout without \
                                             hold_on_close",
                                        ))
                                    }
                                },
                                _ => Err(error),
                            }
                        },
                    }
                }
            },
            Some(Run::Cwd(cwd)) => {
                let shell = self.get_default_terminal(Some(cwd), Some(default_shell.clone()));
                let (quit_callback_fence, quit_cb) = QuitCallbackFence::wrap(quit_cb);
                match self
                    .bus
                    .os_input
                    .as_mut()
                    .context("no OS I/O interface found")
                    .with_context(err_context)?
                    .spawn_terminal(shell, quit_cb, self.default_editor.clone())
                    .with_context(err_context)
                {
                    Ok((terminal_id, reader, child_pid)) => {
                        allocation_ledger.track_terminal(terminal_id);
                        allocation_ledger.track_quit_callback_fence(quit_callback_fence.clone());
                        if let Some(child_pid) = child_pid {
                            self.id_to_child_pid.insert(terminal_id, child_pid);
                            self.capture_initial_cwd(terminal_id, child_pid);
                        }
                        Ok(Some(PreparedTerminal::Runnable {
                            terminal_id,
                            reader,
                            quit_callback_fence,
                        }))
                    },
                    Err(error) => {
                        quit_callback_fence.cancel();
                        match error.downcast_ref::<ZellijError>() {
                            Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                                allocation_ledger.track_terminal(*terminal_id);
                                Err(error.context(
                                    "CommandNotFound Cwd terminal cannot enter a layout without an \
                                     explicit held command",
                                ))
                            },
                            _ => Err(error),
                        }
                    },
                }
            },
            Some(Run::EditFile(path_to_file, line_number, cwd)) => {
                let (quit_callback_fence, quit_cb) = QuitCallbackFence::wrap(quit_cb);
                match self
                    .bus
                    .os_input
                    .as_mut()
                    .context("no OS I/O interface found")
                    .with_context(err_context)?
                    .spawn_terminal(
                        TerminalAction::OpenFile(OpenFilePayload::new(
                            path_to_file,
                            line_number,
                            cwd,
                        )),
                        quit_cb,
                        self.default_editor.clone(),
                    )
                    .with_context(err_context)
                {
                    Ok((terminal_id, reader, child_pid)) => {
                        allocation_ledger.track_terminal(terminal_id);
                        allocation_ledger.track_quit_callback_fence(quit_callback_fence.clone());
                        if let Some(child_pid) = child_pid {
                            self.id_to_child_pid.insert(terminal_id, child_pid);
                            self.capture_initial_cwd(terminal_id, child_pid);
                        }
                        Ok(Some(PreparedTerminal::Runnable {
                            terminal_id,
                            reader,
                            quit_callback_fence,
                        }))
                    },
                    Err(error) => {
                        quit_callback_fence.cancel();
                        match error.downcast_ref::<ZellijError>() {
                            Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                                allocation_ledger.track_terminal(*terminal_id);
                                Err(error.context(
                                    "CommandNotFound editor terminal cannot enter a layout without an \
                                     explicit held command",
                                ))
                            },
                            _ => Err(error),
                        }
                    },
                }
            },
            None => {
                let (quit_callback_fence, quit_cb) = QuitCallbackFence::wrap(quit_cb);
                match self
                    .bus
                    .os_input
                    .as_mut()
                    .context("no OS I/O interface found")
                    .with_context(err_context)?
                    .spawn_terminal(default_shell.clone(), quit_cb, self.default_editor.clone())
                    .with_context(err_context)
                {
                    Ok((terminal_id, reader, child_pid)) => {
                        allocation_ledger.track_terminal(terminal_id);
                        allocation_ledger.track_quit_callback_fence(quit_callback_fence.clone());
                        if let Some(child_pid) = child_pid {
                            self.id_to_child_pid.insert(terminal_id, child_pid);
                            self.capture_initial_cwd(terminal_id, child_pid);
                        }
                        Ok(Some(PreparedTerminal::Runnable {
                            terminal_id,
                            reader,
                            quit_callback_fence,
                        }))
                    },
                    Err(error) => {
                        quit_callback_fence.cancel();
                        match error.downcast_ref::<ZellijError>() {
                            Some(ZellijError::CommandNotFound { terminal_id, .. }) => {
                                allocation_ledger.track_terminal(*terminal_id);
                                Err(error.context(
                                    "CommandNotFound default terminal cannot enter a layout without an \
                                     explicit held command",
                                ))
                            },
                            _ => Err(error),
                        }
                    },
                }
            },
            // Investigate moving plugin loading to here.
            Some(Run::Plugin(_)) => Ok(None),
        }
    }
    pub fn close_pane(&mut self, id: PaneId) -> Result<()> {
        let err_context = || format!("failed to close for pane {id:?}");
        match id {
            PaneId::Terminal(id) => {
                if let Some(handle) = self.task_handles.remove(&id) {
                    handle.abort();
                }
                if let Some(child_pid) = self.id_to_child_pid.remove(&id) {
                    let err_context = || format!("failed to kill child processes for pane {id}");
                    self.bus
                        .os_input
                        .as_mut()
                        .with_context(err_context)
                        .fatal()
                        .kill(child_pid)
                        .with_context(err_context)
                        .non_fatal();
                }
                self.pane_activity_flags.remove(&id);
                self.terminal_cwds.remove(&id);
                self.terminal_cmds.remove(&id);
                self.terminal_foreground_cmds.remove(&id);
                self.bus
                    .os_input
                    .as_ref()
                    .context("no OS I/O interface found")
                    .and_then(|os_input| os_input.clear_terminal_id(id))
                    .with_context(err_context)?;
            },
            PaneId::Plugin(pid) => self
                .bus
                .senders
                .send_to_plugin(PluginInstruction::Unload(pid))
                .with_context(err_context)?,
        }
        Ok(())
    }
    pub fn close_tab(&mut self, ids: Vec<PaneId>) -> Result<()> {
        for id in ids {
            self.close_pane(id)
                .with_context(|| format!("failed to close tab for pane {id:?}"))?;
        }
        Ok(())
    }
    pub fn set_active_pane(&mut self, pane_id: Option<PaneId>, client_id: ClientId) {
        if let Some(pane_id) = pane_id {
            self.active_panes.insert(client_id, pane_id);
        }
    }
    pub fn rerun_command_in_pane(
        &mut self,
        pane_id: PaneId,
        mut run_command: RunCommand,
    ) -> Result<()> {
        let err_context = || format!("failed to rerun command in pane {:?}", pane_id);

        match pane_id {
            PaneId::Terminal(id) => {
                if let Some(originating_plugins) = self.originating_plugins.get(&id) {
                    run_command.originating_plugin = Some(originating_plugins.clone());
                }
                let _ = self.task_handles.remove(&id); // if all is well, this shouldn't be here
                let _ = self.id_to_child_pid.remove(&id); // if all is wlel, this shouldn't be here

                let hold_on_close = run_command.hold_on_close;
                let originating_plugin = Arc::new(run_command.originating_plugin.clone());
                let quit_cb = Box::new({
                    let senders = self.bus.senders.clone();
                    move |pane_id, exit_status, command| {
                        if let PaneId::Terminal(pane_id) = pane_id
                            && let Some(originating_plugin) = originating_plugin.as_ref()
                        {
                            let update_event = Event::CommandPaneExited(
                                pane_id,
                                exit_status,
                                originating_plugin.context.clone(),
                            );
                            let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                Some(originating_plugin.plugin_id),
                                Some(originating_plugin.client_id),
                                update_event,
                            )]));
                        }
                        if hold_on_close {
                            let _ = senders.send_to_screen(ScreenInstruction::HoldPane(
                                pane_id,
                                exit_status,
                                command,
                            ));
                        } else {
                            let _ = senders.send_to_screen(ScreenInstruction::ClosePane(
                                pane_id,
                                None,
                                None,
                                exit_status,
                            ));
                        }
                    }
                });
                let (reader, child_pid): (Box<dyn AsyncReader>, Option<u32>) = self
                    .bus
                    .os_input
                    .as_mut()
                    .context("no OS I/O interface found")
                    .and_then(|os_input| {
                        os_input.re_run_command_in_terminal(id, run_command, quit_cb)
                    })
                    .with_context(err_context)?;
                let activity_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let terminal_bytes = async_runtime().spawn({
                    let err_context =
                        |pane_id| format!("failed to run async task for pane {pane_id:?}");
                    let senders = self.bus.senders.clone();
                    let debug_to_file = self.debug_to_file;
                    let activity_flag = activity_flag.clone();
                    async move {
                        TerminalBytes::new(id, reader, senders, debug_to_file, activity_flag)
                            .listen()
                            .await
                            .with_context(|| err_context(pane_id))
                            .fatal();
                    }
                });

                self.task_handles.insert(id, terminal_bytes);
                self.pane_activity_flags.insert(id, activity_flag);
                if let Some(child_pid) = child_pid {
                    self.id_to_child_pid.insert(id, child_pid);
                    self.capture_initial_cwd(id, child_pid);
                }
                if let Some(originating_plugin) = self.originating_plugins.get(&id) {
                    self.bus
                        .senders
                        .send_to_plugin(PluginInstruction::Update(vec![(
                            Some(originating_plugin.plugin_id),
                            Some(originating_plugin.client_id),
                            Event::CommandPaneReRun(id, originating_plugin.context.clone()),
                        )]))
                        .with_context(err_context)?;
                }
                Ok(())
            },
            _ => Err(anyhow!("cannot respawn plugin panes")).with_context(err_context),
        }
    }
    pub fn populate_session_layout_metadata(
        &mut self,
        session_layout_metadata: &mut SessionLayoutMetadata,
    ) {
        let terminal_ids = session_layout_metadata.all_terminal_ids();
        let mut terminal_ids_to_commands: HashMap<u32, Vec<String>> = HashMap::new();
        let mut terminal_ids_to_cwds: HashMap<u32, PathBuf> = HashMap::new();

        let pids: Vec<_> = terminal_ids
            .iter()
            .filter_map(|id| self.id_to_child_pid.get(id))
            .copied()
            .collect();
        let (pids_to_cwds, pids_to_cmds) = self
            .bus
            .os_input
            .as_ref()
            .map(|os_input| os_input.get_cwds(pids))
            .unwrap_or_default();
        let ppids_to_cmds = self
            .bus
            .os_input
            .as_ref()
            .map(|os_input| os_input.get_all_cmds_by_ppid(&self.post_command_discovery_hook))
            .unwrap_or_default();

        for terminal_id in terminal_ids {
            let process_id = self.id_to_child_pid.get(&terminal_id);
            let cwd = process_id.and_then(|pid| pids_to_cwds.get(pid));
            let cmd_sysinfo = process_id.and_then(|pid| pids_to_cmds.get(pid));
            let cmd_ps = process_id.and_then(|pid| ppids_to_cmds.get(&format!("{}", pid)));
            if let Some(cmd) = cmd_ps {
                terminal_ids_to_commands.insert(terminal_id, cmd.clone());
            } else if let Some(cmd) = cmd_sysinfo {
                terminal_ids_to_commands.insert(terminal_id, cmd.clone());
            }
            if let Some(cwd) = cwd {
                terminal_ids_to_cwds.insert(terminal_id, cwd.clone());
            }
        }
        session_layout_metadata.update_default_shell(get_default_shell());
        session_layout_metadata.update_terminal_commands(terminal_ids_to_commands);
        session_layout_metadata.update_terminal_cwds(terminal_ids_to_cwds);
        session_layout_metadata.update_default_editor(&self.default_editor);
        session_layout_metadata.detect_editor_panes();
    }
    pub fn fill_plugin_cwd(
        &self,
        should_float: Option<bool>,
        should_open_in_place: bool, // should be opened in place
        close_replaced_pane: bool,  // close_replaced_pane
        pane_title: Option<String>, // pane title
        mut run: RunPluginOrAlias,
        tab_index: usize,                   // tab index
        pane_id_to_replace: Option<PaneId>, // pane id to replace if this is to be opened "in-place"
        client_id: ClientId,
        size: Size,
        skip_cache: bool,
        cwd: Option<PathBuf>,
        should_focus_plugin: Option<bool>,
        floating_pane_coordinates: Option<FloatingPaneCoordinates>,
        completion_tx: Option<NotificationEnd>,
    ) -> Result<()> {
        let get_focused_cwd = || {
            self.active_panes
                .get(&client_id)
                .and_then(|pane| match pane {
                    PaneId::Plugin(plugin_id) => self.plugin_cwds.get(plugin_id).cloned(),
                    PaneId::Terminal(id) => {
                        // Try to get CWD from OS, fall back to cached value
                        self.id_to_child_pid
                            .get(id)
                            .and_then(|&pid| {
                                self.bus
                                    .os_input
                                    .as_ref()
                                    .and_then(|input| input.get_cwd(pid))
                            })
                            .or_else(|| self.terminal_cwds.get(id).cloned())
                    },
                })
        };

        let cwd = cwd.or_else(get_focused_cwd);
        let focused_plugin_id = self
            .active_panes
            .get(&client_id)
            .and_then(|pane| match pane {
                PaneId::Plugin(plugin_id) => Some(*plugin_id),
                _ => None,
            });

        if let RunPluginOrAlias::Alias(alias) = &mut run {
            let cwd = get_focused_cwd();
            alias.set_caller_cwd_if_not_set(cwd);
        }
        self.bus.senders.send_to_plugin(PluginInstruction::Load(
            should_float,
            should_open_in_place,
            close_replaced_pane,
            pane_title,
            run,
            Some(tab_index),
            pane_id_to_replace,
            client_id,
            size,
            cwd,
            focused_plugin_id,
            skip_cache,
            should_focus_plugin,
            floating_pane_coordinates,
            completion_tx,
        ))?;
        Ok(())
    }
    fn capture_initial_cwd(&mut self, terminal_id: u32, child_pid: u32) {
        if let Some(os_input) = self.bus.os_input.as_ref()
            && let Some(cwd) = os_input.get_cwd(child_pid)
        {
            self.terminal_cwds.insert(terminal_id, cwd);
        }
    }

    pub fn update_and_report_cwds(&mut self) {
        use std::sync::atomic::Ordering;

        let active_terminal_ids: Vec<u32> = self
            .id_to_child_pid
            .keys()
            .copied()
            .filter(|id| {
                self.pane_activity_flags
                    .get(id)
                    .map(|f| f.swap(false, Ordering::Relaxed))
                    .unwrap_or(false)
            })
            .collect();

        if active_terminal_ids.is_empty() {
            return;
        }

        let pids: Vec<_> = active_terminal_ids
            .iter()
            .filter_map(|id| self.id_to_child_pid.get(id))
            .copied()
            .collect();

        let (pids_to_cwds, pids_to_cmds) = self
            .bus
            .os_input
            .as_ref()
            .map(|os_input| os_input.get_cwds(pids))
            .unwrap_or_default();

        for terminal_id in &active_terminal_ids {
            let process_id = self.id_to_child_pid.get(terminal_id);
            let cwd = process_id.and_then(|pid| pids_to_cwds.get(pid));

            if let Some(cwd) = cwd {
                if self.terminal_cwds.get(terminal_id) != Some(cwd) {
                    let pane_id = PaneId::Terminal(*terminal_id);
                    let focused_client_ids: Vec<ClientId> = self
                        .active_panes
                        .iter()
                        .filter(|(_, active_pane)| *active_pane == &pane_id)
                        .map(|(client_id, _)| *client_id)
                        .collect();
                    let _ = self
                        .bus
                        .senders
                        .send_to_plugin(PluginInstruction::Update(vec![(
                            None,
                            None,
                            Event::CwdChanged(pane_id.into(), cwd.clone(), focused_client_ids),
                        )]));
                }
                self.terminal_cwds.insert(*terminal_id, cwd.clone());
            }

            let cmd = process_id.and_then(|pid| pids_to_cmds.get(pid));
            if let Some(cmd) = cmd {
                self.terminal_cmds.insert(*terminal_id, cmd.clone());
            }
        }

        let ppids_to_cmds = self
            .bus
            .os_input
            .as_ref()
            .map(|os_input| os_input.get_all_cmds_by_ppid(&self.post_command_discovery_hook))
            .unwrap_or_default();

        for terminal_id in &active_terminal_ids {
            let process_id = self.id_to_child_pid.get(terminal_id);
            let foreground_cmd: Vec<String> = process_id
                .and_then(|pid| ppids_to_cmds.get(&pid.to_string()))
                .cloned()
                .unwrap_or_default();

            if self.terminal_foreground_cmds.get(terminal_id) != Some(&foreground_cmd) {
                let pane_id = PaneId::Terminal(*terminal_id);
                let focused_client_ids: Vec<ClientId> = self
                    .active_panes
                    .iter()
                    .filter(|(_, active_pane)| *active_pane == &pane_id)
                    .map(|(client_id, _)| *client_id)
                    .collect();
                let (command, is_foreground) = if foreground_cmd.is_empty() {
                    let shell_cmd = self
                        .terminal_cmds
                        .get(terminal_id)
                        .cloned()
                        .unwrap_or_default();
                    (shell_cmd, false)
                } else {
                    (foreground_cmd.clone(), true)
                };
                let _ = self
                    .bus
                    .senders
                    .send_to_plugin(PluginInstruction::Update(vec![(
                        None,
                        None,
                        Event::CommandChanged(
                            pane_id.into(),
                            command,
                            is_foreground,
                            focused_client_ids,
                        ),
                    )]));
                self.terminal_foreground_cmds
                    .insert(*terminal_id, foreground_cmd);
            }
        }
    }

    pub fn reconfigure(
        &mut self,
        default_editor: Option<PathBuf>,
        post_command_discovery_hook: Option<String>,
    ) {
        self.default_editor = default_editor;
        self.post_command_discovery_hook = post_command_discovery_hook;
    }

    pub fn notify_cwd_from_osc7(&mut self, terminal_id: u32, path: PathBuf) {
        use std::sync::atomic::Ordering;

        if self.terminal_cwds.get(&terminal_id) != Some(&path) {
            let pane_id = PaneId::Terminal(terminal_id);
            let focused_client_ids: Vec<ClientId> = self
                .active_panes
                .iter()
                .filter(|(_, active_pane)| *active_pane == &pane_id)
                .map(|(client_id, _)| *client_id)
                .collect();
            let _ = self
                .bus
                .senders
                .send_to_plugin(PluginInstruction::Update(vec![(
                    None,
                    None,
                    Event::CwdChanged(pane_id.into(), path.clone(), focused_client_ids),
                )]));
            self.terminal_cwds.insert(terminal_id, path);
        }
        if let Some(flag) = self.pane_activity_flags.get(&terminal_id) {
            flag.store(false, Ordering::Relaxed);
        }
    }

    pub fn send_sigint_to_pane(&self, pane_id: PaneId) {
        let err_context = || format!("failed to send SIGINT to pane {:?}", pane_id);

        match pane_id {
            PaneId::Terminal(terminal_id) => {
                if let Some(&child_pid) = self.id_to_child_pid.get(&terminal_id) {
                    self.bus
                        .os_input
                        .as_ref()
                        .context("no OS I/O interface found")
                        .and_then(|os_input| os_input.send_sigint(child_pid))
                        .with_context(err_context)
                        .non_fatal();
                } else {
                    log::warn!("Terminal pane {} not found or not running", terminal_id);
                }
            },
            PaneId::Plugin(plugin_id) => {
                log::warn!("Cannot send SIGINT to plugin pane {}", plugin_id);
            },
        }
    }

    pub fn send_sigkill_to_pane(&self, pane_id: PaneId) {
        let err_context = || format!("failed to send SIGKILL to pane {:?}", pane_id);

        match pane_id {
            PaneId::Terminal(terminal_id) => {
                if let Some(&child_pid) = self.id_to_child_pid.get(&terminal_id) {
                    self.bus
                        .os_input
                        .as_ref()
                        .context("no OS I/O interface found")
                        .and_then(|os_input| os_input.force_kill(child_pid))
                        .with_context(err_context)
                        .non_fatal();
                } else {
                    log::warn!("Terminal pane {} not found or not running", terminal_id);
                }
            },
            PaneId::Plugin(plugin_id) => {
                log::warn!("Cannot send SIGKILL to plugin pane {}", plugin_id);
            },
        }
    }

    pub fn get_pane_pid(&self, pane_id: PaneId) -> GetPanePidResponse {
        match pane_id {
            PaneId::Terminal(terminal_id) => {
                if let Some(&child_pid) = self.id_to_child_pid.get(&terminal_id) {
                    GetPanePidResponse::Ok(child_pid as i32)
                } else {
                    GetPanePidResponse::Err(format!(
                        "Terminal pane {} not found or not running",
                        terminal_id
                    ))
                }
            },
            PaneId::Plugin(plugin_id) => {
                GetPanePidResponse::Err(format!("Cannot get PID for plugin pane {}", plugin_id))
            },
        }
    }
    pub fn get_pane_running_command(&self, pane_id: PaneId) -> GetPaneRunningCommandResponse {
        match pane_id {
            PaneId::Terminal(terminal_id) => {
                if let Some(&child_pid) = self.id_to_child_pid.get(&terminal_id) {
                    // Query OS for current running command
                    if let Some(os_input) = self.bus.os_input.as_ref() {
                        // First, try to get child process command (e.g., nvim running in bash)
                        let ppids_to_cmds =
                            os_input.get_all_cmds_by_ppid(&self.post_command_discovery_hook);
                        let cmd_ps = ppids_to_cmds.get(&format!("{}", child_pid));

                        // If no child process, fall back to parent process (e.g., the shell itself)
                        let (_cwds, cmds) = os_input.get_cwds(vec![child_pid]);
                        let cmd_sysinfo = cmds.get(&child_pid);

                        if let Some(command_args) = cmd_ps {
                            GetPaneRunningCommandResponse::Ok(command_args.clone())
                        } else if let Some(command_args) = cmd_sysinfo {
                            GetPaneRunningCommandResponse::Ok(command_args.clone())
                        } else {
                            GetPaneRunningCommandResponse::Err(format!(
                                "Could not retrieve running command for terminal pane {}",
                                terminal_id
                            ))
                        }
                    } else {
                        GetPaneRunningCommandResponse::Err("OS input not available".to_string())
                    }
                } else {
                    GetPaneRunningCommandResponse::Err(format!(
                        "Terminal pane {} not found or not running",
                        terminal_id
                    ))
                }
            },
            PaneId::Plugin(plugin_id) => GetPaneRunningCommandResponse::Err(format!(
                "Cannot get running command for plugin pane {}",
                plugin_id
            )),
        }
    }
    pub fn get_pane_cwd(&self, pane_id: PaneId) -> GetPaneCwdResponse {
        match pane_id {
            PaneId::Terminal(terminal_id) => {
                if let Some(&child_pid) = self.id_to_child_pid.get(&terminal_id) {
                    // Query OS for current working directory
                    if let Some(os_input) = self.bus.os_input.as_ref() {
                        let (cwds, _cmds) = os_input.get_cwds(vec![child_pid]);
                        if let Some(cwd) = cwds.get(&child_pid) {
                            GetPaneCwdResponse::Ok(cwd.clone())
                        } else {
                            GetPaneCwdResponse::Err(format!(
                                "Could not retrieve CWD for terminal pane {}",
                                terminal_id
                            ))
                        }
                    } else {
                        GetPaneCwdResponse::Err("OS input not available".to_string())
                    }
                } else {
                    GetPaneCwdResponse::Err(format!(
                        "Terminal pane {} not found or not running",
                        terminal_id
                    ))
                }
            },
            PaneId::Plugin(plugin_id) => {
                GetPaneCwdResponse::Err(format!("Cannot get CWD for plugin pane {}", plugin_id))
            },
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        let child_ids: Vec<u32> = self.id_to_child_pid.keys().copied().collect();
        for id in child_ids {
            self.close_pane(PaneId::Terminal(id))
                .with_context(|| format!("failed to close pane for pid {id}"))
                .fatal();
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static FAIL_COMMAND_NOT_FOUND_NOTIFICATION_AT_SEND: std::cell::Cell<u8> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn fail_next_command_not_found_notification() {
    FAIL_COMMAND_NOT_FOUND_NOTIFICATION_AT_SEND.with(|fail_at| fail_at.set(1));
}

#[cfg(test)]
fn fail_command_not_found_notification_between_messages() {
    FAIL_COMMAND_NOT_FOUND_NOTIFICATION_AT_SEND.with(|fail_at| fail_at.set(2));
}

#[cfg(test)]
fn command_not_found_notification_failure_is_due(send_index: u8) -> bool {
    FAIL_COMMAND_NOT_FOUND_NOTIFICATION_AT_SEND.with(|fail_at| {
        if fail_at.get() == send_index {
            fail_at.set(0);
            true
        } else {
            false
        }
    })
}

fn send_command_not_found_to_screen(
    senders: ThreadSenders,
    terminal_id: u32,
    run_command: RunCommand,
) -> Result<()> {
    #[cfg(test)]
    if command_not_found_notification_failure_is_due(1) {
        return Err(anyhow!(
            "injected post-transfer command-not-found notification failure"
        ));
    }
    let err_context = || format!("failed to send command_not_fount for terminal {terminal_id}");
    senders
        .send_to_screen(ScreenInstruction::PtyBytes(
            terminal_id,
            format!("Command not found: {}\n\rIf you were including arguments as part of the command, try including them as 'args' instead.", run_command.command.display())
                .as_bytes()
                .to_vec(),
        ))
        .with_context(err_context)?;
    #[cfg(test)]
    if command_not_found_notification_failure_is_due(2) {
        return Err(anyhow!(
            "injected between-message command-not-found notification failure"
        ));
    }
    senders
        .send_to_screen(ScreenInstruction::HoldPane(
            PaneId::Terminal(terminal_id),
            Some(2),
            run_command.clone(),
        ))
        .with_context(err_context)?;
    Ok(())
}

#[cfg(not(windows))]
pub fn get_default_shell() -> PathBuf {
    PathBuf::from(std::env::var("SHELL").unwrap_or_else(|_| {
        log::warn!("Cannot read SHELL env, falling back to use /bin/sh");
        "/bin/sh".to_string()
    }))
}

#[cfg(windows)]
pub fn get_default_shell() -> PathBuf {
    if let Ok(shell) = std::env::var("SHELL") {
        return PathBuf::from(shell);
    }
    PathBuf::from(std::env::var("COMSPEC").unwrap_or_else(|_| {
        log::warn!("Cannot read SHELL or COMSPEC env, falling back to use cmd.exe");
        "cmd.exe".to_string()
    }))
}

#[cfg(test)]
#[path = "./unit/pty_tests.rs"]
mod pty_tests;
