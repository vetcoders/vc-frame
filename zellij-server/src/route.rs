use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::sync::oneshot;

use crate::global_async_runtime::get_tokio_runtime;
use crate::thread_bus::ThreadSenders;
use crate::{
    ServerInstruction, SessionMetaData, SessionState,
    os_input_output::ServerOsApi,
    panes::PaneId,
    plugins::PluginInstruction,
    pty::{ClientTabIndexOrPaneId, PtyInstruction},
    screen::{DumpScreenTargetIdentity, ScreenInstruction},
    session_layout_metadata::SessionLayoutMetadata,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use zellij_utils::{
    channels::SenderWithContext,
    data::{
        BareKey, ConnectToSession, Direction, Event, InputMode, KeyModifier, ListPanesResponse,
        ListTabsResponse, NewPanePlacement, PaneListEntry, ResizeStrategy, TabInfo,
        UnblockCondition,
    },
    envs,
    errors::prelude::*,
    input::{
        actions::{Action, SearchDirection, SearchOption},
        command::TerminalAction,
    },
    ipc::{
        ClientReceiveOutcome, ClientToServerMsg, ExitReason, IpcReceiverWithContext,
        ServerToClientMsg,
    },
};

use crate::ClientId;

const ACTION_COMPLETION_TIMEOUT: Duration = Duration::from_secs(1);
// Shared PTY-enrich budget for CLI list-clients. This is a total deadline for
// enqueue + wait across every focused terminal, not 100ms multiplied by pane
// count. Silent unfocused panels must not extend it.
const LIST_CLIENTS_PTY_ENRICH_DEADLINE: Duration = Duration::from_millis(100);
// Most `CliTriageIo` child commands have a 10-second outer budget. NewTab is
// the exception: `NEW_TAB_COMMAND_TIMEOUT` is 30s because cold debug wasm
// plugin load on layout activation (tab-bar/status-bar/session-manager) can
// legitimately exceed 8s on hosted CI. Keep critical completion under that
// outer budget so the route still fails closed instead of hanging forever.
const CRITICAL_ACTION_COMPLETION_TIMEOUT: Duration = Duration::from_secs(25);
// Route -> PTY spawn -> Screen placement. A pane completion resolves only once
// Screen has actually installed the pane, so this budget has to cover a Screen
// FIFO that is already carrying live `PtyBytes` / `PluginBytes` from an
// attached client: after a real attach that drain reaches the second range,
// which the generic 1s route budget reports as a timeout while the pane
// exists. It stays well inside the client-side warden
// (`VC_FRAME_ACTION_TTL_SECONDS`, 20s in the workspace-host fixture) so the
// route is still the surface that fails closed with a real error instead of
// the client self-retiring, and it stays under the 25s critical budget it is
// not entitled to.
const PANE_PLACEMENT_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
// Route -> Screen -> PTY -> plugin load -> Screen placement. A plugin operation
// is the only completion that crosses the Screen FIFO *twice*, and between the
// two traversals it also queues behind the PTY and plugin actors. On top of that
// sits the cost `CRITICAL_ACTION_COMPLETION_TIMEOUT` already documents: a cold
// debug wasm load of exactly these plugins (tab-bar/status-bar/session-manager)
// can legitimately exceed 8s. `PanePlacement` is budgeted for a strictly shorter
// chain with no wasm in it, so plugins get their own deadline instead of
// silently re-using one whose reasoning does not cover them. It still stays
// inside the client-side warden (`VC_FRAME_ACTION_TTL_SECONDS`, 20s in the
// workspace-host fixture) so the route remains the surface that fails closed,
// and under the 25s critical budget it is not entitled to.
const PLUGIN_LOAD_COMPLETION_TIMEOUT: Duration = Duration::from_secs(15);
static QUICK_CMD_DIAGNOSTIC_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Which deadline a routed action's completion is judged by.
///
/// A wider budget never means "assume success": every variant resolves through
/// `wait_for_action_completion_with_timeout`, and an expired budget stays an
/// explicit failure. The variants only encode how far the acknowledgement has
/// to travel before the action is logically done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletionBudget {
    /// Screen answers on its own thread.
    Route,
    /// Route -> PTY spawn -> Screen placement before a pane exists.
    PanePlacement,
    /// Route -> Screen -> PTY -> plugin load -> Screen placement.
    PluginLoad,
    /// Blocking CLI actions that own an outer command timeout.
    Critical,
}

impl CompletionBudget {
    fn timeout(self) -> Duration {
        match self {
            CompletionBudget::Route => ACTION_COMPLETION_TIMEOUT,
            CompletionBudget::PanePlacement => PANE_PLACEMENT_COMPLETION_TIMEOUT,
            CompletionBudget::PluginLoad => PLUGIN_LOAD_COMPLETION_TIMEOUT,
            CompletionBudget::Critical => CRITICAL_ACTION_COMPLETION_TIMEOUT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionCompletionResult {
    pub exit_status: Option<i32>,
    pub affected_pane_id: Option<PaneId>,
    pub affected_tab_id: Option<usize>,
    pub error_message: Option<String>,
    pub stdout_message: Option<String>,
}

pub fn wait_for_action_completion(
    receiver: oneshot::Receiver<ActionCompletionResult>,
    action_name: &str,
    critical_completion: bool,
) -> ActionCompletionResult {
    let completion_timeout = if critical_completion {
        CRITICAL_ACTION_COMPLETION_TIMEOUT
    } else {
        ACTION_COMPLETION_TIMEOUT
    };
    wait_for_action_completion_with_timeout(receiver, action_name, completion_timeout)
}

fn wait_for_action_completion_with_timeout(
    receiver: oneshot::Receiver<ActionCompletionResult>,
    action_name: &str,
    completion_timeout: Duration,
) -> ActionCompletionResult {
    let runtime = get_tokio_runtime();
    match runtime.block_on(async { tokio::time::timeout(completion_timeout, receiver).await }) {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            log::error!("Failed to wait for action {}: {}", action_name, error);
            ActionCompletionResult {
                exit_status: Some(1),
                affected_pane_id: None,
                affected_tab_id: None,
                error_message: Some(format!(
                    "action '{}' completion channel closed before acknowledgement: {}; outcome unresolved, execution is not cancelled",
                    action_name, error
                )),
                stdout_message: None,
            }
        },
        Err(_) => {
            log::error!(
                "Action {} did not complete within {:?} timeout",
                action_name,
                completion_timeout
            );
            ActionCompletionResult {
                exit_status: Some(1),
                affected_pane_id: None,
                affected_tab_id: None,
                error_message: Some(format!(
                    "action '{}' did not acknowledge completion within {:?}; outcome unresolved, execution is not cancelled; template adoption must reconcile the same identity and expected generation",
                    action_name, completion_timeout
                )),
                stdout_message: None,
            }
        },
    }
}

// This is used to wait for actions that span multiple threads until they logically end
// dropping this struct sends a notification through the oneshot channel to the receiver, letting
// it know the action is ended and thus releasing it
//
// Note: Cloning this struct DOES NOT clone that internal sender, it only implements Clone so
// that it can be included in various other larger structs - DO NOT RELY ON CLONING IT!
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationResolution {
    Pending,
    Success,
    Failure,
}

const PENDING_NOTIFICATION_DROPPED_ERROR: &str =
    "action completion dropped before explicit success or failure resolution";

#[derive(Debug)]
pub struct NotificationEnd {
    channel: Option<oneshot::Sender<ActionCompletionResult>>,
    // `None` preserves the legacy drop-as-success contract for callers that
    // have not migrated to explicit completion yet. Once a caller opts in via
    // `require_explicit_resolution`, dropping while Pending is always failure.
    resolution: Option<NotificationResolution>,
    exit_status: Option<i32>,
    unblock_condition: Option<UnblockCondition>,
    affected_pane_id: Option<PaneId>, // optional payload of the pane id affected by this action
    affected_tab_id: Option<usize>,   // optional payload of the tab id affected by this action
    error_message: Option<String>,
    stdout_message: Option<String>,
}

impl Clone for NotificationEnd {
    fn clone(&self) -> Self {
        // Always clone as None - only the original holder should signal completion
        NotificationEnd {
            channel: None,
            resolution: self.resolution,
            exit_status: self.exit_status,
            unblock_condition: self.unblock_condition,
            affected_pane_id: self.affected_pane_id,
            affected_tab_id: self.affected_tab_id,
            error_message: self.error_message.clone(),
            stdout_message: self.stdout_message.clone(),
        }
    }
}

impl NotificationEnd {
    pub fn new(sender: oneshot::Sender<ActionCompletionResult>) -> Self {
        NotificationEnd {
            channel: Some(sender),
            resolution: None,
            exit_status: None,
            unblock_condition: None,
            affected_pane_id: None,
            affected_tab_id: None,
            error_message: None,
            stdout_message: None,
        }
    }

    pub fn new_with_condition(
        sender: oneshot::Sender<ActionCompletionResult>,
        unblock_condition: UnblockCondition,
    ) -> Self {
        NotificationEnd {
            channel: Some(sender),
            resolution: None,
            exit_status: None,
            unblock_condition: Some(unblock_condition),
            affected_pane_id: None,
            affected_tab_id: None,
            error_message: None,
            stdout_message: None,
        }
    }

    pub fn set_exit_status(&mut self, exit_status: i32) {
        self.exit_status = Some(exit_status);
        if self
            .unblock_condition
            .is_some_and(|condition| !condition.is_met(exit_status))
        {
            // A blocking terminal can be rerun until its requested condition
            // is met. An intermediate exit updates the eventual payload but
            // must not poison the still-pending completion.
            return;
        }
        if exit_status == 0 {
            self.mark_success();
        } else {
            self.resolution = Some(NotificationResolution::Failure);
        }
    }

    pub fn set_affected_pane_id(&mut self, pane_id: PaneId) {
        self.affected_pane_id = Some(pane_id);
    }

    pub fn set_affected_tab_id(&mut self, tab_id: usize) {
        self.affected_tab_id = Some(tab_id);
    }

    pub fn set_error_message(&mut self, message: String) {
        self.error_message = Some(message);
        if self.exit_status.is_none_or(|exit_status| exit_status == 0) {
            self.exit_status = Some(1);
        }
        self.resolution = Some(NotificationResolution::Failure);
    }

    pub fn set_stdout_message(&mut self, message: String) {
        self.stdout_message = Some(message);
    }

    pub fn unblock_condition(&self) -> Option<UnblockCondition> {
        self.unblock_condition
    }

    pub fn require_explicit_resolution(&mut self) {
        if self.resolution.is_none() {
            self.resolution = Some(NotificationResolution::Pending);
        }
    }

    pub fn mark_success(&mut self) {
        if self.resolution != Some(NotificationResolution::Failure) {
            self.resolution = Some(NotificationResolution::Success);
        }
    }

    pub fn mark_failure(&mut self, message: impl Into<String>) {
        self.set_error_message(message.into());
    }
}

impl Drop for NotificationEnd {
    fn drop(&mut self) {
        if let Some(tx) = self.channel.take() {
            if self.resolution == Some(NotificationResolution::Pending) {
                self.exit_status = Some(1);
                self.error_message = Some(PENDING_NOTIFICATION_DROPPED_ERROR.to_string());
                self.resolution = Some(NotificationResolution::Failure);
            } else if self.resolution == Some(NotificationResolution::Failure)
                && self.exit_status.is_none_or(|exit_status| exit_status == 0)
            {
                self.exit_status = Some(1);
            }
            let result = ActionCompletionResult {
                exit_status: self.exit_status,
                affected_pane_id: self.affected_pane_id,
                affected_tab_id: self.affected_tab_id,
                error_message: self.error_message.take(),
                stdout_message: self.stdout_message.take(),
            };
            let _ = tx.send(result);
        }
    }
}

fn complete_action_immediately(sender: oneshot::Sender<ActionCompletionResult>) {
    let mut completion = NotificationEnd::new(sender);
    completion.require_explicit_resolution();
    completion.mark_success();
}

/// The completion token for an action on the plugin chain.
///
/// A plugin operation travels Route -> Screen -> PTY -> plugin load -> Screen
/// placement, and every hop on that chain owns a `log::error!` dead end: no
/// active tab, no connected client, a load that failed, a tab index that does
/// not exist. Under the legacy drop-as-success contract each of those reports
/// exit 0 to a client whose plugin was never placed. None of those hops has a
/// legitimate reason to drop the token, so on this chain silence is a failure
/// and success has to be said out loud.
fn plugin_completion(sender: oneshot::Sender<ActionCompletionResult>) -> Option<NotificationEnd> {
    let mut completion = NotificationEnd::new(sender);
    completion.require_explicit_resolution();
    Some(completion)
}

/// Refuse a plugin operation at a dead end on that chain, by name.
///
/// Screen and the plugin thread both own branches that can only log and give
/// up - no active tab, no connected client, a load that failed, a plugin alias
/// that resolves to nothing. Each of them still holds the completion token, and
/// simply dropping it would hand the client the legacy drop-as-success. The
/// client asked for a plugin pane and did not get one, so it hears why.
pub(crate) fn refuse_plugin_completion(completion_tx: Option<NotificationEnd>, reason: &str) {
    if let Some(mut completion) = completion_tx {
        completion.mark_failure(reason);
    }
}

// `route_action` must not borrow from the `session_data` read guard.
// otherwise blocking-CLI actions
// (`CompletionBudget::Critical`) park this function while still holding the guard,
// deadlocking concurrent `session_data.write()`s.
pub(crate) struct RouteActionParams<'a> {
    pub action: Action,
    pub caller: &'a str,
    pub client_id: ClientId,
    pub cli_client_id: Option<ClientId>,
    pub pane_id: Option<PaneId>,
    pub senders: ThreadSenders,
    pub default_shell: Option<TerminalAction>,
    pub seen_cli_pipes: Option<&'a mut HashSet<String>>,
    pub default_mode: InputMode,
}

pub(crate) fn route_action(
    params: RouteActionParams<'_>,
) -> Result<(bool, Option<ActionCompletionResult>)> {
    let RouteActionParams {
        action,
        caller,
        client_id,
        cli_client_id,
        pane_id,
        senders,
        default_shell,
        mut seen_cli_pipes,
        default_mode,
    } = params;
    let route_started = Instant::now();
    let mut should_break = false;
    let err_context = || format!("failed to route action for client {client_id}");
    let action_name = action.to_string();

    if !action.is_mouse_action() {
        // mouse actions should only send InputReceived to plugins
        // if they do not result in text being marked, this is handled in Tab
        senders
            .send_to_plugin(PluginInstruction::Update(vec![(
                None,
                Some(client_id),
                Event::InputReceived,
            )]))
            .with_context(err_context)?;
    }

    // we use this oneshot channel to wait for an action to be "logically"
    // done, meaning that it traveled through all the threads it needed to travel through and the
    // app has confirmed that it is complete. Once this happens, we get a signal through the
    // wait_for_action_completion call below (or its bounded deadline) and release this thread,
    // allowing the client to produce another action without risking races
    let (completion_tx, completion_rx) = oneshot::channel();

    let mut completion_budget = CompletionBudget::Route;

    match action {
        Action::ToggleTab => {
            senders
                .send_to_screen(ScreenInstruction::ToggleTab(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Write {
            key_with_modifier,
            bytes: raw_bytes,
            is_kitty_keyboard_protocol,
        } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScroll(client_id))
                .with_context(err_context)?;
            senders
                .send_to_screen(ScreenInstruction::WriteCharacter(
                    key_with_modifier,
                    raw_bytes,
                    is_kitty_keyboard_protocol,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::WriteChars { chars } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScroll(client_id))
                .with_context(err_context)?;
            let chars = chars.into_bytes();
            senders
                .send_to_screen(ScreenInstruction::WriteCharacter(
                    None,
                    chars,
                    false,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::WriteToPaneId { bytes, pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScroll(client_id))
                .with_context(err_context)?;
            senders
                .send_to_screen(ScreenInstruction::WriteToPaneId(
                    bytes,
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::WriteCharsToPaneId { chars, pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScroll(client_id))
                .with_context(err_context)?;
            let bytes = chars.into_bytes();
            senders
                .send_to_screen(ScreenInstruction::WriteToPaneId(
                    bytes,
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Paste { chars, pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScroll(client_id))
                .with_context(err_context)?;
            let bytes = chars.into_bytes();
            senders
                .send_to_screen(ScreenInstruction::Paste(
                    bytes,
                    pane_id.map(|p| p.into()),
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::SetPaneColor { pane_id, fg, bg } => {
            senders
                .send_to_screen(ScreenInstruction::SetPaneColor(
                    pane_id.into(),
                    fg,
                    bg,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::SwitchToMode { input_mode } => {
            senders
                .send_to_server(ServerInstruction::ChangeMode(client_id, input_mode))
                .with_context(err_context)?;
            senders
                .send_to_screen(ScreenInstruction::ChangeMode(
                    input_mode,
                    Some(default_mode),
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
            senders
                .send_to_screen(ScreenInstruction::Render)
                .with_context(err_context)?;
        },
        Action::Resize { resize, direction } => {
            let screen_instr = ScreenInstruction::Resize(
                client_id,
                ResizeStrategy::new(resize, direction),
                Some(NotificationEnd::new(completion_tx)),
            );
            senders
                .send_to_screen(screen_instr)
                .with_context(err_context)?;
        },
        Action::SwitchFocus => {
            senders
                .send_to_screen(ScreenInstruction::SwitchFocus(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::FocusNextPane => {
            senders
                .send_to_screen(ScreenInstruction::FocusNextPane(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::FocusPreviousPane => {
            senders
                .send_to_screen(ScreenInstruction::FocusPreviousPane(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::FocusPaneByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::FocusPaneWithId(
                    pane_id.into(),
                    true,  // should_float_if_hidden
                    false, // should_be_in_place_if_hidden
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::MoveFocus { direction } => {
            let notification_end = Some(NotificationEnd::new(completion_tx));

            let screen_instr = match direction {
                Direction::Left => ScreenInstruction::MoveFocusLeft(client_id, notification_end),
                Direction::Right => ScreenInstruction::MoveFocusRight(client_id, notification_end),
                Direction::Up => ScreenInstruction::MoveFocusUp(client_id, notification_end),
                Direction::Down => ScreenInstruction::MoveFocusDown(client_id, notification_end),
            };
            senders
                .send_to_screen(screen_instr)
                .with_context(err_context)?;
        },
        Action::MoveFocusOrTab { direction } => {
            let notification_end = Some(NotificationEnd::new(completion_tx));

            let screen_instr = match direction {
                Direction::Left => {
                    ScreenInstruction::MoveFocusLeftOrPreviousTab(client_id, notification_end)
                },
                Direction::Right => {
                    ScreenInstruction::MoveFocusRightOrNextTab(client_id, notification_end)
                },
                Direction::Up => ScreenInstruction::SwitchTabNext(client_id, notification_end),
                Direction::Down => ScreenInstruction::SwitchTabPrev(client_id, notification_end),
            };
            senders
                .send_to_screen(screen_instr)
                .with_context(err_context)?;
        },
        Action::MovePane { direction } => {
            let notification_end = Some(NotificationEnd::new(completion_tx));

            let screen_instr = match direction {
                Some(Direction::Left) => {
                    ScreenInstruction::MovePaneLeft(client_id, notification_end)
                },
                Some(Direction::Right) => {
                    ScreenInstruction::MovePaneRight(client_id, notification_end)
                },
                Some(Direction::Up) => ScreenInstruction::MovePaneUp(client_id, notification_end),
                Some(Direction::Down) => {
                    ScreenInstruction::MovePaneDown(client_id, notification_end)
                },
                None => ScreenInstruction::MovePane(client_id, notification_end),
            };
            senders
                .send_to_screen(screen_instr)
                .with_context(err_context)?;
        },
        Action::MovePaneBackwards => {
            senders
                .send_to_screen(ScreenInstruction::MovePaneBackwards(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ClearScreen => {
            senders
                .send_to_screen(ScreenInstruction::ClearScreen(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::DumpScreen {
            file_path,
            include_scrollback,
            pane_id,
            ansi,
            expected_tab_id,
            expected_tab_name,
            expected_session_incarnation,
            expected_tab_instance_id,
        } => {
            let target_identity = match (
                expected_tab_id,
                expected_tab_name,
                expected_session_incarnation,
                expected_tab_instance_id,
            ) {
                (None, None, None, None) => None,
                (
                    Some(tab_id),
                    Some(tab_name),
                    Some(session_incarnation),
                    Some(tab_instance_id),
                ) => Some(DumpScreenTargetIdentity {
                    tab_id: tab_id as usize,
                    tab_name,
                    session_incarnation,
                    tab_instance_id,
                }),
                _ => {
                    return Err(anyhow!(
                        "typed dump requires a complete tab identity selector"
                    ));
                },
            };
            completion_budget = CompletionBudget::Critical;
            senders
                .send_to_screen(ScreenInstruction::DumpScreen(
                    file_path,
                    client_id,
                    include_scrollback,
                    pane_id.map(|p| p.into()),
                    Some(NotificationEnd::new(completion_tx)),
                    cli_client_id,
                    ansi,
                    target_identity,
                ))
                .with_context(err_context)?;
        },
        Action::CopyPaneScrollback => {
            senders
                .send_to_screen(ScreenInstruction::CopyPaneScrollback(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::DumpLayout => {
            let default_shell = match default_shell {
                Some(TerminalAction::RunCommand(run_command)) => Some(run_command.command),
                _ => None,
            };
            senders
                .send_to_screen(ScreenInstruction::DumpLayout(
                    default_shell,
                    cli_client_id.unwrap_or(client_id), // we prefer the cli client here because
                    // this is a cli query and we want to print
                    // it there
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::SaveSession => {
            senders
                .send_to_screen(ScreenInstruction::SaveSession(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::EditScrollback { ansi } => {
            senders
                .send_to_screen(ScreenInstruction::EditScrollback(
                    client_id,
                    ansi,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },

        Action::ScrollUp => {
            senders
                .send_to_screen(ScreenInstruction::ScrollUp(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollUpAt { position } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollUpAt(
                    position,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollDown => {
            senders
                .send_to_screen(ScreenInstruction::ScrollDown(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollDownAt { position } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollDownAt(
                    position,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollToBottom => {
            senders
                .send_to_screen(ScreenInstruction::ScrollToBottom(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollToTop => {
            senders
                .send_to_screen(ScreenInstruction::ScrollToTop(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PageScrollUp => {
            senders
                .send_to_screen(ScreenInstruction::PageScrollUp(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PageScrollDown => {
            senders
                .send_to_screen(ScreenInstruction::PageScrollDown(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::HalfPageScrollUp => {
            senders
                .send_to_screen(ScreenInstruction::HalfPageScrollUp(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::HalfPageScrollDown => {
            senders
                .send_to_screen(ScreenInstruction::HalfPageScrollDown(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleFocusFullscreen => {
            senders
                .send_to_screen(ScreenInstruction::ToggleActiveTerminalFullscreen(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TogglePaneFrames => {
            senders
                .send_to_screen(ScreenInstruction::TogglePaneFrames(Some(
                    NotificationEnd::new(completion_tx),
                )))
                .with_context(err_context)?;
        },
        Action::NewPane {
            direction,
            pane_name,
            start_suppressed,
        } => {
            let shell = default_shell.clone();
            let new_pane_placement = match direction {
                Some(direction) => NewPanePlacement::Tiled {
                    direction: Some(direction),
                    borderless: None,
                },
                None => NewPanePlacement::NoPreference { borderless: None },
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    shell,
                    pane_name,
                    new_pane_placement,
                    start_suppressed,
                    ClientTabIndexOrPaneId::ClientId(client_id),
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                ))
                .with_context(err_context)?;
        },
        Action::NewBlockingPane {
            placement,
            pane_name,
            command,
            unblock_condition,
            near_current_pane,
            tab_id,
        } => {
            let command = TerminalAction::for_new_pane(command, default_shell.clone());
            let set_pane_blocking = true;

            let notification_end = if let Some(condition) = unblock_condition {
                Some(NotificationEnd::new_with_condition(
                    completion_tx,
                    condition,
                ))
            } else {
                Some(NotificationEnd::new(completion_tx))
            };

            // we prefer the pane id provided by the action explicitly over the one that originated
            // it (this might be a bit misleading with "near_current_pane", but it's still the
            // right behavior - in the latter case, if the originator does not wish for this
            // behavior, they should not provide pane
            // inside the placement, but rather have the current pane id be picked up instead)
            let pane_id = match placement {
                NewPanePlacement::Stacked {
                    pane_id_to_stack_under,
                    ..
                } => pane_id_to_stack_under.map(|p| p.into()).or(pane_id),
                NewPanePlacement::InPlace {
                    pane_id_to_replace, ..
                } => pane_id_to_replace.map(|p| p.into()).or(pane_id),
                _ => pane_id,
            };

            let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                ClientTabIndexOrPaneId::TabIndex(tab_id)
            } else if near_current_pane {
                match pane_id {
                    Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                    None => ClientTabIndexOrPaneId::ClientId(client_id),
                }
            } else {
                ClientTabIndexOrPaneId::ClientId(client_id)
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    command,
                    pane_name,
                    placement,
                    false,
                    client_tab_index_or_paneid,
                    notification_end,
                    set_pane_blocking,
                ))
                .with_context(err_context)?;
            completion_budget = CompletionBudget::Critical;
        },
        Action::EditFile {
            payload: open_file_payload,
            direction: split_direction,
            floating: should_float,
            in_place: should_open_in_place,
            close_replaced_pane,
            start_suppressed,
            coordinates: floating_pane_coordinates,
            near_current_pane,
            tab_id,
        } => {
            let title = format!("Editing: {}", open_file_payload.path.display());
            let open_file = TerminalAction::OpenFile(open_file_payload);
            let pty_instr = if should_open_in_place {
                let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                    ClientTabIndexOrPaneId::TabIndex(tab_id)
                } else if near_current_pane {
                    match pane_id {
                        Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                        None => ClientTabIndexOrPaneId::ClientId(client_id),
                    }
                } else {
                    ClientTabIndexOrPaneId::ClientId(client_id)
                };
                PtyInstruction::SpawnInPlaceTerminal(
                    Some(open_file),
                    Some(title),
                    close_replaced_pane,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                )
            } else {
                let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                    ClientTabIndexOrPaneId::TabIndex(tab_id)
                } else {
                    ClientTabIndexOrPaneId::ClientId(client_id)
                };
                PtyInstruction::SpawnTerminal(
                    Some(open_file),
                    Some(title),
                    if should_float {
                        NewPanePlacement::Floating(floating_pane_coordinates)
                    } else {
                        NewPanePlacement::Tiled {
                            direction: split_direction,
                            borderless: None,
                        }
                    },
                    start_suppressed,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                )
            };
            senders.send_to_pty(pty_instr).with_context(err_context)?;
        },
        Action::SwitchModeForAllClients { input_mode } => {
            // ModeUpdate broadcast is handled by the screen thread via
            // change_mode_for_all_clients() -> change_mode() -> update_input_modes()
            senders
                .send_to_server(ServerInstruction::ChangeModeForAllClients(input_mode))
                .with_context(err_context)?;

            senders
                .send_to_screen(ScreenInstruction::ChangeModeForAllClients(
                    input_mode,
                    Some(default_mode),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NewFloatingPane {
            command: run_command,
            pane_name: name,
            coordinates: floating_pane_coordinates,
            near_current_pane,
            tab_id,
        } => {
            // Completion travels Route -> PTY -> Screen and resolves on
            // placement, not on enqueue.
            completion_budget = CompletionBudget::PanePlacement;
            let run_cmd = TerminalAction::for_new_pane(run_command, default_shell.clone());
            let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                ClientTabIndexOrPaneId::TabIndex(tab_id)
            } else if near_current_pane {
                match pane_id {
                    Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                    None => ClientTabIndexOrPaneId::ClientId(client_id),
                }
            } else {
                ClientTabIndexOrPaneId::ClientId(client_id)
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    run_cmd,
                    name,
                    NewPanePlacement::Floating(floating_pane_coordinates),
                    false,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                ))
                .with_context(err_context)?;
        },
        Action::NewInPlacePane {
            command: run_command,
            pane_name: name,
            near_current_pane,
            pane_id_to_replace,
            close_replaced_pane,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PanePlacement;
            let run_cmd = TerminalAction::for_new_pane(run_command, default_shell.clone());
            let explicit_replace = pane_id_to_replace.map(|p| p.into());
            let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                ClientTabIndexOrPaneId::TabIndex(tab_id)
            } else if let Some(pid) = explicit_replace {
                ClientTabIndexOrPaneId::PaneId(pid)
            } else if near_current_pane {
                match pane_id {
                    Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                    None => ClientTabIndexOrPaneId::ClientId(client_id),
                }
            } else {
                ClientTabIndexOrPaneId::ClientId(client_id)
            };
            senders
                .send_to_pty(PtyInstruction::SpawnInPlaceTerminal(
                    run_cmd,
                    name,
                    close_replaced_pane,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NewStackedPane {
            command: run_command,
            pane_name: name,
            near_current_pane,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PanePlacement;
            let run_cmd = TerminalAction::for_new_pane(run_command, default_shell.clone());

            let (pane_placement, client_tab_index_or_paneid) = if let Some(tab_id) = tab_id {
                (
                    NewPanePlacement::Stacked {
                        pane_id_to_stack_under: None,
                        borderless: None,
                    },
                    ClientTabIndexOrPaneId::TabIndex(tab_id),
                )
            } else if near_current_pane {
                match pane_id {
                    Some(pid) => (
                        NewPanePlacement::Stacked {
                            pane_id_to_stack_under: Some(pid.into()),
                            borderless: None,
                        },
                        ClientTabIndexOrPaneId::PaneId(pid),
                    ),
                    None => (
                        NewPanePlacement::Stacked {
                            pane_id_to_stack_under: None,
                            borderless: None,
                        },
                        ClientTabIndexOrPaneId::ClientId(client_id),
                    ),
                }
            } else {
                (
                    NewPanePlacement::Stacked {
                        pane_id_to_stack_under: None,
                        borderless: None,
                    },
                    ClientTabIndexOrPaneId::ClientId(client_id),
                )
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    run_cmd,
                    name,
                    pane_placement,
                    false,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                ))
                .with_context(err_context)?;
        },
        Action::NewTiledPane {
            direction,
            command: run_command,
            pane_name: name,
            near_current_pane,
            borderless,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PanePlacement;
            let run_cmd = TerminalAction::for_new_pane(run_command, default_shell.clone());
            let client_tab_index_or_paneid = if let Some(tab_id) = tab_id {
                ClientTabIndexOrPaneId::TabIndex(tab_id)
            } else if near_current_pane {
                match pane_id {
                    Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                    None => ClientTabIndexOrPaneId::ClientId(client_id),
                }
            } else {
                ClientTabIndexOrPaneId::ClientId(client_id)
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    run_cmd,
                    name,
                    NewPanePlacement::Tiled {
                        direction,
                        borderless,
                    },
                    false,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                ))
                .with_context(err_context)?;
        },
        Action::TogglePaneEmbedOrFloating => {
            senders
                .send_to_screen(ScreenInstruction::TogglePaneEmbedOrFloating(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleFloatingPanes => {
            senders
                .send_to_screen(ScreenInstruction::ToggleFloatingPanes(
                    client_id,
                    default_shell.clone(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PaneNameInput { input } => {
            senders
                .send_to_screen(ScreenInstruction::UpdatePaneName(
                    input,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::UndoRenamePane => {
            senders
                .send_to_screen(ScreenInstruction::UndoRenamePane(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Run {
            command,
            near_current_pane,
        } => {
            completion_budget = CompletionBudget::PanePlacement;
            let run_cmd = Some(TerminalAction::RunCommand(command.clone().into()));
            let client_tab_index_or_paneid = if near_current_pane {
                match pane_id {
                    Some(pid) => ClientTabIndexOrPaneId::PaneId(pid),
                    None => ClientTabIndexOrPaneId::ClientId(client_id),
                }
            } else {
                ClientTabIndexOrPaneId::ClientId(client_id)
            };
            senders
                .send_to_pty(PtyInstruction::SpawnTerminal(
                    run_cmd,
                    None,
                    NewPanePlacement::Tiled {
                        direction: command.direction,
                        borderless: None,
                    },
                    false,
                    client_tab_index_or_paneid,
                    Some(NotificationEnd::new(completion_tx)),
                    false, // set_blocking
                ))
                .with_context(err_context)?;
        },
        Action::CloseFocus => {
            senders
                .send_to_screen(ScreenInstruction::CloseFocusedPane(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NewTab {
            tiled_layout: tab_layout,
            floating_layouts: floating_panes_layout,
            swap_tiled_layouts,
            swap_floating_layouts,
            tab_name,
            should_change_focus_to_new_tab,
            cwd,
            initial_panes,
            first_pane_unblock_condition,
            placement,
        } => {
            // New-tab completion is the commit acknowledgement. Returning after
            // the generic one-second timeout lets a late server writer create a
            // duplicate tab after the caller has already retried.
            completion_budget = CompletionBudget::Critical;
            let shell = default_shell.clone();
            let is_web_client = false; // actions cannot be initiated directly from the web

            // Construct completion_tx conditionally
            let (mut completion_tx, block_on_first_terminal) = if let Some(condition) =
                first_pane_unblock_condition
            {
                let notification = NotificationEnd::new_with_condition(completion_tx, condition);
                completion_budget = CompletionBudget::Critical;
                (notification, true)
            } else {
                (NotificationEnd::new(completion_tx), false)
            };
            // NewTab spans Route -> Screen -> Plugin -> PTY -> Screen -> PTY.
            // Opt in before the first handoff so losing ownership anywhere in
            // that chain can never look like a successful commit.
            completion_tx.require_explicit_resolution();

            senders
                .send_to_screen(ScreenInstruction::NewTab(
                    cwd,
                    shell,
                    tab_layout,
                    floating_panes_layout,
                    tab_name,
                    (swap_tiled_layouts, swap_floating_layouts),
                    initial_panes,
                    block_on_first_terminal,
                    should_change_focus_to_new_tab,
                    placement,
                    (client_id, is_web_client),
                    Some(completion_tx),
                ))
                .with_context(err_context)?;
        },
        Action::GoToNextTab => {
            senders
                .send_to_screen(ScreenInstruction::SwitchTabNext(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::GoToPreviousTab => {
            senders
                .send_to_screen(ScreenInstruction::SwitchTabPrev(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleActiveSyncTab => {
            senders
                .send_to_screen(ScreenInstruction::ToggleActiveSyncTab(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CloseTab => {
            senders
                .send_to_screen(ScreenInstruction::CloseTab(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::GoToTab { index } => {
            senders
                .send_to_screen(ScreenInstruction::GoToTab(
                    index,
                    Some(client_id),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::GoToTabName { name, create } => {
            let shell = default_shell.clone();
            senders
                .send_to_screen(ScreenInstruction::GoToTabName(
                    name,
                    shell,
                    create,
                    Some(client_id),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TabNameInput { input } => {
            senders
                .send_to_screen(ScreenInstruction::UpdateTabName(
                    input,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::UndoRenameTab => {
            senders
                .send_to_screen(ScreenInstruction::UndoRenameTab(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::GoToTabById { id } => {
            senders
                .send_to_screen(ScreenInstruction::GoToTabWithId(
                    id as usize,
                    Some(client_id),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CloseTabById { id } => {
            senders
                .send_to_screen(ScreenInstruction::CloseTabWithId(
                    id as usize,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CloseTabByIdIfName {
            id,
            expected_name,
            expected_session_incarnation,
            expected_tab_instance_id,
        } => {
            completion_budget = CompletionBudget::Critical;
            senders
                .send_to_screen(ScreenInstruction::CloseTabWithIdIfName(
                    id as usize,
                    expected_name,
                    expected_session_incarnation,
                    expected_tab_instance_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CloseTabByIdIfNameIfQuiescent {
            id,
            expected_name,
            expected_session_incarnation,
            expected_tab_instance_id,
        } => {
            completion_budget = CompletionBudget::Critical;
            senders
                .send_to_screen(ScreenInstruction::CloseTabWithIdIfNameIfQuiescent(
                    id as usize,
                    expected_name,
                    expected_session_incarnation,
                    expected_tab_instance_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenameTabById { id, name } => {
            senders
                .send_to_screen(ScreenInstruction::RenameTabWithId(
                    id as usize,
                    name.into_bytes(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::MoveTab { direction } => {
            let screen_instr = match direction {
                Direction::Left | Direction::Up => ScreenInstruction::MoveTabLeft(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ),
                Direction::Right | Direction::Down => ScreenInstruction::MoveTabRight(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ),
            };
            senders
                .send_to_screen(screen_instr)
                .with_context(err_context)?;
        },
        Action::Quit => {
            senders
                .send_to_server(ServerInstruction::ClientExit(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
            should_break = true;
        },
        Action::Detach => {
            senders
                .send_to_server(ServerInstruction::DetachSession(
                    vec![client_id],
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
            should_break = true;
        },
        Action::SetDarkTheme => {
            senders
                .send_to_screen(ScreenInstruction::SetDarkTheme(Some(NotificationEnd::new(
                    completion_tx,
                ))))
                .with_context(err_context)?;
        },
        Action::SetLightTheme => {
            senders
                .send_to_screen(ScreenInstruction::SetLightTheme(Some(
                    NotificationEnd::new(completion_tx),
                )))
                .with_context(err_context)?;
        },
        Action::ToggleTheme => {
            senders
                .send_to_screen(ScreenInstruction::ToggleTheme(Some(NotificationEnd::new(
                    completion_tx,
                ))))
                .with_context(err_context)?;
        },
        Action::SwitchSession {
            name,
            tab_position,
            pane_id,
            layout,
            cwd,
        } => {
            let current_session_name = envs::get_session_name().unwrap_or_else(|_| String::new());
            if name != current_session_name {
                let connect_to_session = ConnectToSession {
                    name: Some(name.clone()),
                    tab_position,
                    pane_id,
                    layout: layout.clone(),
                    cwd: cwd.clone(),
                };
                senders
                    .send_to_server(ServerInstruction::SwitchSession(
                        connect_to_session,
                        client_id,
                        Some(NotificationEnd::new(completion_tx)),
                    ))
                    .with_context(err_context)?;
                should_break = true;
            } else {
                complete_action_immediately(completion_tx);
            }
        },
        Action::MouseEvent { event } => {
            senders
                .send_to_screen(ScreenInstruction::MouseEvent(
                    event,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Copy => {
            senders
                .send_to_screen(ScreenInstruction::Copy(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Confirm | Action::Deny => {
            // no-op, these are deprecated and should be removed when we upgrade the server/client
            // contract
            complete_action_immediately(completion_tx);
        },
        Action::SkipConfirm { action } => match *action {
            Action::Quit => {
                complete_action_immediately(completion_tx);
                senders
                    .send_to_server(ServerInstruction::ClientExit(client_id, None))
                    .with_context(err_context)?;
                should_break = true;
            },
            _ => complete_action_immediately(completion_tx),
        },
        Action::NoOp => {
            complete_action_immediately(completion_tx);
        },
        Action::SearchInput { input } => {
            senders
                .send_to_screen(ScreenInstruction::UpdateSearch(
                    input,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::Search { direction } => {
            let notification_end = Some(NotificationEnd::new(completion_tx));

            let instruction = match direction {
                SearchDirection::Down => ScreenInstruction::SearchDown(client_id, notification_end),
                SearchDirection::Up => ScreenInstruction::SearchUp(client_id, notification_end),
            };
            senders
                .send_to_screen(instruction)
                .with_context(err_context)?;
        },
        Action::SearchToggleOption { option } => {
            let notification_end = Some(NotificationEnd::new(completion_tx));

            let instruction = match option {
                SearchOption::CaseSensitivity => {
                    ScreenInstruction::SearchToggleCaseSensitivity(client_id, notification_end)
                },
                SearchOption::WholeWord => {
                    ScreenInstruction::SearchToggleWholeWord(client_id, notification_end)
                },
                SearchOption::Wrap => {
                    ScreenInstruction::SearchToggleWrap(client_id, notification_end)
                },
            };
            senders
                .send_to_screen(instruction)
                .with_context(err_context)?;
        },
        Action::ToggleMouseMode => complete_action_immediately(completion_tx), // Handled client side
        Action::PreviousSwapLayout => {
            senders
                .send_to_screen(ScreenInstruction::PreviousSwapLayout(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NextSwapLayout => {
            senders
                .send_to_screen(ScreenInstruction::NextSwapLayout(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::OverrideLayout {
            template_adoption,
            tabs,
            retain_existing_terminal_panes,
            retain_existing_plugin_panes,
            apply_only_to_active_tab,
        } => {
            critical_completion = true;
            let cwd = None;
            let shell = default_shell.clone();

            senders
                .send_to_screen(ScreenInstruction::OverrideLayout(
                    cwd,
                    shell,
                    tabs,
                    template_adoption,
                    retain_existing_terminal_panes,
                    retain_existing_plugin_panes,
                    apply_only_to_active_tab,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::QueryTabNames => {
            senders
                .send_to_screen(ScreenInstruction::QueryTabNames(
                    cli_client_id.unwrap_or(client_id),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NewTiledPluginPane {
            plugin: run_plugin,
            pane_name: name,
            skip_cache,
            cwd,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PluginLoad;
            senders
                .send_to_screen(ScreenInstruction::NewTiledPluginPane(
                    run_plugin,
                    name,
                    skip_cache,
                    cwd,
                    client_id,
                    plugin_completion(completion_tx),
                    tab_id,
                ))
                .with_context(err_context)?;
        },
        Action::NewFloatingPluginPane {
            plugin: run_plugin,
            pane_name: name,
            skip_cache,
            cwd,
            coordinates: floating_pane_coordinates,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PluginLoad;
            senders
                .send_to_screen(ScreenInstruction::NewFloatingPluginPane(
                    run_plugin,
                    name,
                    skip_cache,
                    cwd,
                    floating_pane_coordinates,
                    client_id,
                    plugin_completion(completion_tx),
                    tab_id,
                ))
                .with_context(err_context)?;
        },
        Action::NewInPlacePluginPane {
            plugin: run_plugin,
            pane_name: name,
            skip_cache,
            close_replaced_pane,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PluginLoad;
            if let Some(pane_id) = pane_id {
                senders
                    .send_to_screen(ScreenInstruction::NewInPlacePluginPane(
                        run_plugin,
                        name,
                        pane_id,
                        skip_cache,
                        close_replaced_pane,
                        client_id,
                        plugin_completion(completion_tx),
                        tab_id,
                    ))
                    .with_context(err_context)?;
            } else {
                log::error!("Must have pane_id in order to open in place pane");
            }
        },
        Action::StartOrReloadPlugin { plugin: run_plugin } => {
            completion_budget = CompletionBudget::PluginLoad;
            senders
                .send_to_screen(ScreenInstruction::StartOrReloadPluginPane(
                    run_plugin,
                    None,
                    plugin_completion(completion_tx),
                ))
                .with_context(err_context)?;
        },
        Action::LaunchOrFocusPlugin {
            plugin: run_plugin,
            should_float,
            move_to_focused_tab,
            should_open_in_place,
            close_replaced_pane,
            skip_cache,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PluginLoad;
            senders
                .send_to_screen(ScreenInstruction::LaunchOrFocusPlugin(
                    run_plugin,
                    should_float,
                    move_to_focused_tab,
                    should_open_in_place,
                    close_replaced_pane,
                    pane_id,
                    skip_cache,
                    client_id,
                    plugin_completion(completion_tx),
                    tab_id,
                ))
                .with_context(err_context)?;
        },
        Action::LaunchPlugin {
            plugin: run_plugin,
            should_float,
            should_open_in_place,
            close_replaced_pane,
            skip_cache,
            cwd,
            tab_id,
        } => {
            completion_budget = CompletionBudget::PluginLoad;
            senders
                .send_to_screen(ScreenInstruction::LaunchPlugin(
                    run_plugin,
                    should_float,
                    should_open_in_place,
                    close_replaced_pane,
                    pane_id,
                    skip_cache,
                    cwd,
                    client_id,
                    plugin_completion(completion_tx),
                    tab_id,
                ))
                .with_context(err_context)?;
        },
        Action::CloseTerminalPane {
            pane_id: terminal_pane_id,
        } => {
            senders
                .send_to_screen(ScreenInstruction::ClosePane(
                    PaneId::Terminal(terminal_pane_id),
                    None, // we send None here so that the terminal pane would be closed anywhere
                    // in the app, not just in the client's tab
                    Some(NotificationEnd::new(completion_tx)),
                    None,
                ))
                .with_context(err_context)?;
        },
        Action::ClosePluginPane {
            pane_id: plugin_pane_id,
        } => {
            senders
                .send_to_screen(ScreenInstruction::ClosePane(
                    PaneId::Plugin(plugin_pane_id),
                    None, // we send None here so that the terminal pane would be closed anywhere
                    // in the app, not just in the client's tab
                    Some(NotificationEnd::new(completion_tx)),
                    None,
                ))
                .with_context(err_context)?;
        },
        Action::FocusTerminalPaneWithId {
            pane_id,
            should_float_if_hidden,
            should_be_in_place_if_hidden,
        } => {
            senders
                .send_to_screen(ScreenInstruction::FocusPaneWithId(
                    PaneId::Terminal(pane_id),
                    should_float_if_hidden,
                    should_be_in_place_if_hidden,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::FocusPluginPaneWithId {
            pane_id,
            should_float_if_hidden,
            should_be_in_place_if_hidden,
        } => {
            senders
                .send_to_screen(ScreenInstruction::FocusPaneWithId(
                    PaneId::Plugin(pane_id),
                    should_float_if_hidden,
                    should_be_in_place_if_hidden,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenameTerminalPane {
            pane_id,
            name: name_bytes,
        } => {
            senders
                .send_to_screen(ScreenInstruction::RenamePane(
                    PaneId::Terminal(pane_id),
                    name_bytes,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenamePluginPane {
            pane_id,
            name: name_bytes,
        } => {
            senders
                .send_to_screen(ScreenInstruction::RenamePane(
                    PaneId::Plugin(pane_id),
                    name_bytes,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenameTab {
            tab_index: tab_position,
            name: name_bytes,
        } => {
            senders
                .send_to_screen(ScreenInstruction::RenameTab(
                    tab_position as usize,
                    name_bytes,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::BreakPane => {
            senders
                .send_to_screen(ScreenInstruction::BreakPane(
                    default_shell.clone(),
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::BreakPaneRight => {
            senders
                .send_to_screen(ScreenInstruction::BreakPaneRight(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::BreakPaneLeft => {
            senders
                .send_to_screen(ScreenInstruction::BreakPaneLeft(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenameSession { name } => {
            senders
                .send_to_screen(ScreenInstruction::RenameSession(
                    name,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CliPipe {
            pipe_id,
            mut name,
            payload,
            plugin,
            args,
            configuration,
            floating,
            in_place,
            skip_cache,
            cwd,
            pane_title,
            ..
        } => {
            // Route-level dispatch is complete immediately. The CLI client
            // remains blocked independently until a destination plugin sends
            // UnblockCliPipeInput for this pipe id.
            complete_action_immediately(completion_tx);
            if let Some(seen_cli_pipes) = seen_cli_pipes.as_mut()
                && !seen_cli_pipes.contains(&pipe_id)
            {
                seen_cli_pipes.insert(pipe_id.clone());
                senders
                    .send_to_server(ServerInstruction::AssociatePipeWithClient {
                        pipe_id: pipe_id.clone(),
                        client_id: cli_client_id.unwrap_or(client_id),
                    })
                    .with_context(err_context)?;
            }
            if let Some(name) = name.take() {
                let should_open_in_place = in_place.unwrap_or(false);
                if should_open_in_place && pane_id.is_none() {
                    log::error!(
                        "Was asked to open a new plugin in-place, but cannot identify the pane id... is the VC_FRAME_PANE_ID or ZELLIJ_PANE_ID variable set?"
                    );
                }
                let pane_id_to_replace = if should_open_in_place { pane_id } else { None };
                senders
                    .send_to_plugin(PluginInstruction::CliPipe {
                        pipe_id,
                        name,
                        payload,
                        plugin,
                        args,
                        configuration,
                        floating,
                        pane_id_to_replace,
                        cwd,
                        pane_title,
                        skip_cache,
                        cli_client_id: cli_client_id.unwrap_or(client_id),
                    })
                    .with_context(err_context)?;
            } else {
                log::error!("Message must have a name");
            }
        },
        Action::KeybindPipe {
            mut name,
            payload,
            plugin,
            args,
            mut configuration,
            floating,
            in_place,
            skip_cache,
            cwd,
            pane_title,
            launch_new,
            plugin_id,
            ..
        } => {
            if let Some(name) = name.take() {
                // Quick cmd synchronously opens a terminal, changes the origin's
                // mode and names the pane before its guest pipe acknowledges.
                // The outer action must cover the complete command, not expire
                // at the ordinary one-second key deadline between host calls.
                completion_budget = if name == "vc_quick_cmd" {
                    CompletionBudget::Critical
                } else {
                    CompletionBudget::Route
                };
                let should_open_in_place = in_place.unwrap_or(false);
                let pane_id_to_replace = if should_open_in_place { pane_id } else { None };
                if launch_new && plugin_id.is_none() {
                    // we do this to make sure the plugin is unique (has a unique configuration parameter)
                    configuration
                        .get_or_insert_with(BTreeMap::new)
                        .insert("_zellij_id".to_owned(), Uuid::new_v4().to_string());
                }
                let diagnostic_request = (name == "vc_quick_cmd").then(|| {
                    (
                        QUICK_CMD_DIAGNOSTIC_REQUEST_ID.fetch_add(1, Ordering::Relaxed),
                        Instant::now(),
                    )
                });
                if let Some((request_id, _)) = diagnostic_request
                    && std::env::var_os("VC_FRAME_ROUTE_DIAGNOSTICS").is_some()
                {
                    log::info!(
                        "quick_cmd_route_enqueue request={} origin={}",
                        request_id,
                        client_id
                    );
                }
                senders
                    .send_to_plugin(PluginInstruction::KeybindPipe {
                        name,
                        payload,
                        plugin,
                        args,
                        configuration,
                        floating,
                        pane_id_to_replace,
                        cwd,
                        pane_title,
                        skip_cache,
                        cli_client_id: client_id,
                        plugin_and_client_id: plugin_id.map(|plugin_id| (plugin_id, client_id)),
                        notification_end: Some(NotificationEnd::new(completion_tx)),
                        diagnostic_request,
                    })
                    .with_context(err_context)?;
            } else {
                log::error!("Message must have a name");
            }
        },
        Action::ListClients => {
            let mut completion = NotificationEnd::new(completion_tx);
            let default_shell = match default_shell {
                Some(TerminalAction::RunCommand(run_command)) => Some(run_command.command),
                _ => None,
            };
            let maybe_metadata = request_list_clients_from_screen(&senders, default_shell)
                .with_context(err_context)?;

            if let Some(mut metadata) = maybe_metadata {
                enrich_list_clients_with_pty_data(&mut metadata, &senders)
                    .with_context(err_context)?;
                completion.set_stdout_message(metadata.list_clients_metadata());
            } else {
                completion.set_exit_status(1);
                completion.set_error_message("Timeout listing clients".to_string());
            }
            drop(completion);
        },
        Action::ListPanes {
            show_tab,
            show_command,
            show_state,
            show_geometry,
            show_all,
            output_json,
        } => {
            let mut completion = NotificationEnd::new(completion_tx);
            let maybe_panes =
                request_panes_from_screen(&senders, show_all).with_context(err_context)?;

            if let Some(mut pane_entries) = maybe_panes {
                if show_command || show_all || output_json {
                    enrich_panes_with_pty_data(&mut pane_entries, &senders)
                        .with_context(err_context)?;
                }

                let output_lines = if output_json {
                    format_panes_as_json(&pane_entries)
                } else {
                    format_panes_table(
                        &pane_entries,
                        show_tab || show_all,
                        show_command || show_all,
                        show_state || show_all,
                        show_geometry || show_all,
                    )
                };

                completion.set_stdout_message(output_lines.join("\n"));
            } else {
                completion.set_exit_status(1);
                completion.set_error_message("Timeout listing panes".to_string());
            }
            drop(completion);
        },
        Action::ListTabs {
            show_state,
            show_dimensions,
            show_panes,
            show_layout,
            show_all,
            output_json,
        } => {
            let mut completion = NotificationEnd::new(completion_tx);
            let maybe_tabs =
                request_tabs_from_screen(&senders, client_id).with_context(err_context)?;

            if let Some(tab_infos) = maybe_tabs {
                let output_lines = if output_json {
                    format_tabs_as_json(&tab_infos)
                } else {
                    format_tabs_table(
                        &tab_infos.tabs,
                        show_state || show_all,
                        show_dimensions || show_all,
                        show_panes || show_all,
                        show_layout || show_all,
                    )
                };

                completion.set_stdout_message(output_lines.join("\n"));
            } else {
                completion.set_exit_status(1);
                completion.set_error_message("Timeout listing tabs".to_string());
            }
            drop(completion);
        },
        Action::CurrentTabInfo { output_json } => {
            let mut completion = NotificationEnd::new(completion_tx);
            let maybe_tab_info = request_current_tab_info_from_screen(&senders, client_id)
                .with_context(err_context)?;

            match maybe_tab_info {
                Some(tab_info) => {
                    let output_lines = if output_json {
                        format_current_tab_info_as_json(&tab_info)
                    } else {
                        format_current_tab_info_plain(&tab_info)
                    };
                    completion.set_stdout_message(output_lines.join("\n"));
                },
                None => {
                    completion.set_exit_status(1);
                    completion
                        .set_error_message("No active tab found for current client".to_string());
                },
            }
            drop(completion);
        },
        Action::TogglePanePinned => {
            senders
                .send_to_screen(ScreenInstruction::TogglePanePinned(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::StackPanes {
            pane_ids: pane_ids_to_stack,
        } => {
            senders
                .send_to_screen(ScreenInstruction::StackPanes(
                    pane_ids_to_stack.iter().map(|p| PaneId::from(*p)).collect(),
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ChangeFloatingPaneCoordinates {
            pane_id,
            coordinates,
        } => {
            senders
                .send_to_screen(ScreenInstruction::ChangeFloatingPanesCoordinates(
                    vec![(pane_id.into(), coordinates)],
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TogglePaneBorderless { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::TogglePaneBorderless(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::SetPaneBorderless {
            pane_id,
            borderless,
        } => {
            senders
                .send_to_screen(ScreenInstruction::SetPaneBorderless(
                    pane_id.into(),
                    borderless,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TogglePaneInGroup => {
            senders
                .send_to_screen(ScreenInstruction::TogglePaneInGroup(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleGroupMarking => {
            senders
                .send_to_screen(ScreenInstruction::ToggleGroupMarking(
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ShowFloatingPanes { tab_id } => {
            senders
                .send_to_screen(ScreenInstruction::ShowFloatingPanes {
                    client_id,
                    tab_id,
                    completion: Some(NotificationEnd::new(completion_tx)),
                })
                .with_context(err_context)?;
        },
        Action::HideFloatingPanes { tab_id } => {
            senders
                .send_to_screen(ScreenInstruction::HideFloatingPanes {
                    client_id,
                    tab_id,
                    completion: Some(NotificationEnd::new(completion_tx)),
                })
                .with_context(err_context)?;
        },
        Action::AreFloatingPanesVisible { tab_id } => {
            senders
                .send_to_screen(ScreenInstruction::AreFloatingPanesVisible {
                    client_id,
                    tab_id,
                    completion: Some(NotificationEnd::new(completion_tx)),
                })
                .with_context(err_context)?;
        },
        // Pane-targeting CLI-only variants
        Action::ScrollUpByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollUpWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollDownByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollDownWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollToTopByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollToTopWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ScrollToBottomByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ScrollToBottomWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PageScrollUpByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::PageScrollUpWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PageScrollDownByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::PageScrollDownWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::HalfPageScrollUpByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::HalfPageScrollUpWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::HalfPageScrollDownByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::HalfPageScrollDownWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ResizeByPaneId {
            pane_id,
            resize,
            direction,
        } => {
            let resize_strategy = ResizeStrategy::new(resize, direction);
            senders
                .send_to_screen(ScreenInstruction::ResizeWithPaneId(
                    pane_id.into(),
                    resize_strategy,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::MovePaneByPaneId { pane_id, direction } => {
            senders
                .send_to_screen(ScreenInstruction::MovePaneWithPaneIdCli(
                    pane_id.into(),
                    direction,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::MovePaneBackwardsByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::MovePaneBackwardsWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ClearScreenByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ClearScreenWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::EditScrollbackByPaneId { pane_id, ansi } => {
            senders
                .send_to_screen(ScreenInstruction::EditScrollbackWithPaneId(
                    pane_id.into(),
                    ansi,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleFocusFullscreenByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::ToggleFullscreenWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TogglePaneEmbedOrFloatingByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::TogglePaneEmbedOrFloatingWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::CloseFocusByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::CloseFocusWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::RenamePaneByPaneId { pane_id, name } => {
            let instruction = match pane_id {
                Some(pane_id) => ScreenInstruction::RenamePaneWithPaneId(
                    pane_id.into(),
                    name,
                    Some(NotificationEnd::new(completion_tx)),
                ),
                None => ScreenInstruction::RenameActivePane(
                    name,
                    client_id,
                    Some(NotificationEnd::new(completion_tx)),
                ),
            };
            senders
                .send_to_screen(instruction)
                .with_context(err_context)?;
        },
        Action::UndoRenamePaneByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::UndoRenamePaneWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::TogglePanePinnedByPaneId { pane_id } => {
            senders
                .send_to_screen(ScreenInstruction::TogglePanePinnedWithPaneId(
                    pane_id.into(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        // Tab-targeting CLI-only variants
        Action::UndoRenameTabByTabId { id } => {
            senders
                .send_to_screen(ScreenInstruction::UndoRenameTabWithTabId(
                    id as usize,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleActiveSyncTabByTabId { id } => {
            senders
                .send_to_screen(ScreenInstruction::ToggleActiveSyncTabWithTabId(
                    id as usize,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::ToggleFloatingPanesByTabId { id } => {
            senders
                .send_to_screen(ScreenInstruction::ToggleFloatingPanesWithTabId(
                    id as usize,
                    default_shell.clone(),
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::PreviousSwapLayoutByTabId { id } => {
            senders
                .send_to_screen(ScreenInstruction::PreviousSwapLayoutWithTabId(
                    id as usize,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::NextSwapLayoutByTabId { id } => {
            senders
                .send_to_screen(ScreenInstruction::NextSwapLayoutWithTabId(
                    id as usize,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
        Action::MoveTabByTabId { id, direction } => {
            senders
                .send_to_screen(ScreenInstruction::MoveTabWithTabId(
                    id as usize,
                    direction,
                    Some(NotificationEnd::new(completion_tx)),
                ))
                .with_context(err_context)?;
        },
    }
    let result = wait_for_action_completion_with_timeout(
        completion_rx,
        &action_name,
        completion_budget.timeout(),
    );
    let timed_out = result
        .error_message
        .as_deref()
        .is_some_and(|message| message.contains("did not acknowledge completion within"));
    crate::route_telemetry::record(
        caller,
        &action_name,
        route_started.elapsed(),
        timed_out,
        result.error_message.is_none() && result.exit_status.is_none_or(|status| status == 0),
    );
    Ok((should_break, Some(result)))
}

// this should only be used for one-off startup instructions
macro_rules! send_to_screen_or_retry_queue {
    ($senders:expr, $message:expr, $instruction: expr, $retry_queue:expr) => {{
        match $senders.as_ref() {
            Some(senders) => senders.send_to_screen($message),
            None => {
                log::warn!("Server not ready, trying to place instruction in retry queue...");
                if let Some(retry_queue) = $retry_queue.as_mut() {
                    retry_queue.push_back($instruction);
                }
                Ok(())
            },
        }
    }};
}

fn normalize_route_caller(value: &str) -> String {
    let normalized = value
        .trim()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '/')
        })
        .take(64)
        .collect::<String>();
    if normalized.is_empty() {
        "anonymous".to_string()
    } else {
        normalized
    }
}

pub(crate) fn route_thread_main(
    session_data: Arc<RwLock<Option<SessionMetaData>>>,
    session_state: Arc<RwLock<SessionState>>,
    os_input: Box<dyn ServerOsApi>,
    to_server: SenderWithContext<ServerInstruction>,
    mut receiver: IpcReceiverWithContext<ClientToServerMsg>,
    client_id: ClientId,
) -> Result<()> {
    let mut retry_queue = VecDeque::new();
    let mut caller = "anonymous".to_string();
    let connection_started = Instant::now();
    let err_context = || format!("failed to handle instruction for client {client_id}");
    let mut seen_cli_pipes = HashSet::new();
    'route_loop: loop {
        match receiver.recv_client_msg_outcome() {
            ClientReceiveOutcome::Message(instruction, err_ctx) => {
                let instruction = *instruction;
                err_ctx.update_thread_ctx();
                let mut handle_instruction = |instruction: ClientToServerMsg,
                                              mut retry_queue: Option<
                    &mut VecDeque<ClientToServerMsg>,
                >|
                 -> Result<bool> {
                    let mut should_break = false;
                    let senders = session_data
                        .read()
                        .to_anyhow()
                        .ok()
                        .and_then(|r| r.as_ref().map(|r| r.senders.clone()));

                    // Check if this is a watcher client and ignore input messages
                    let is_watcher = session_state.read().unwrap().is_watcher(&client_id);
                    if is_watcher {
                        match &instruction {
                            ClientToServerMsg::Key { key, .. }
                                if ((key.bare_key == BareKey::Char('q')
                                    && key.key_modifiers.contains(&KeyModifier::Ctrl))
                                    || key.bare_key == BareKey::Esc
                                    || (key.bare_key == BareKey::Char('c')
                                        && key.key_modifiers.contains(&KeyModifier::Ctrl))) =>
                            {
                                let _ = os_input.send_to_client(
                                    client_id,
                                    ServerToClientMsg::Exit {
                                        exit_reason: ExitReason::Normal,
                                    },
                                );
                                let _ = senders.as_ref().map(|s| {
                                    s.send_to_screen(ScreenInstruction::RemoveWatcherClient(
                                        client_id,
                                    ))
                                });
                                should_break = true;
                            },
                            ClientToServerMsg::TerminalResize { new_size } => {
                                // For watchers: send size to Screen for rendering adjustments, but
                                // this does not affect the screen size
                                send_to_screen_or_retry_queue!(
                                    senders,
                                    ScreenInstruction::WatcherTerminalResize(client_id, *new_size),
                                    instruction.clone(),
                                    retry_queue
                                )
                                .with_context(err_context)?;
                            },
                            _ => {
                                // Ignore all input from watcher clients
                            },
                        }
                        // don't do anything else for watchers
                        return Ok(should_break);
                    }

                    match instruction {
                        ClientToServerMsg::DeclareCaller { caller: declared } => {
                            caller = normalize_route_caller(&declared);
                        },
                        ClientToServerMsg::DoctorRoutes { json: _ } => {
                            os_input
                                .send_to_client(
                                    client_id,
                                    ServerToClientMsg::Log {
                                        lines: vec![crate::route_telemetry::snapshot_json()],
                                    },
                                )
                                .with_context(err_context)?;
                            should_break = true;
                        },
                        ClientToServerMsg::Key {
                            key,
                            raw_bytes,
                            is_kitty_keyboard_protocol,
                        } => {
                            // Track this as the last active client
                            session_state
                                .write()
                                .unwrap()
                                .set_last_active_client(client_id);

                            // The read guard ends as a temporary in this expression so
                            // `route_action` runs without holding `session_data.read()` —
                            // see the doc comment on `route_action` for why this matters.
                            let dispatch_inputs =
                                session_data.read().unwrap().as_ref().and_then(|s| {
                                    let (kb, im, dim) =
                                        s.get_client_keybinds_and_mode(&client_id)?;
                                    let actions: Vec<Action> = kb
                                        .get_actions_for_key_in_mode_or_default_action(
                                            im,
                                            &key,
                                            raw_bytes,
                                            dim,
                                            is_kitty_keyboard_protocol,
                                        );
                                    Some((
                                        s.senders.clone(),
                                        s.default_shell.clone(),
                                        s.session_configuration
                                            .get_client_default_input_mode(&client_id),
                                        actions,
                                    ))
                                });
                            if let Some((senders, default_shell, client_input_mode, actions)) =
                                dispatch_inputs
                            {
                                for action in actions {
                                    // Send user input to plugin thread for logging
                                    let _ = senders.send_to_plugin(PluginInstruction::UserInput {
                                        client_id,
                                        action: action.clone(),
                                        terminal_id: None,
                                        cli_client_id: None,
                                    });

                                    match route_action(RouteActionParams {
                                        action,
                                        caller: "interactive",
                                        client_id,
                                        cli_client_id: None,
                                        pane_id: None,
                                        senders: senders.clone(),
                                        default_shell: default_shell.clone(),
                                        seen_cli_pipes: Some(&mut seen_cli_pipes),
                                        default_mode: client_input_mode,
                                    }) {
                                        Ok(route_action_should_break) => {
                                            if route_action_should_break.0 {
                                                should_break = true;
                                            }
                                        },
                                        Err(e) => {
                                            log::error!("{}", e);
                                        },
                                    }
                                }
                            }
                        },
                        ClientToServerMsg::Action {
                            action,
                            terminal_id: maybe_pane_id,
                            client_id: maybe_client_id,
                            is_cli_client,
                        } => {
                            let cli_client_id = client_id;
                            let client_id = if is_cli_client {
                                // for cli clients, we want to default to the last active client
                                // (i.e. the last client to have issued a keystroke) this is to
                                // interpret actions that require a client_id (such as move focus,
                                // detach, etc.) for which using the cli client id will not be
                                // doing the right thing - using the last_active_client is almost
                                // certainly correct in almost all cases
                                session_state
                                    .read()
                                    .unwrap()
                                    .get_last_active_client()
                                    .or(maybe_client_id)
                                    .unwrap_or(client_id)
                            } else {
                                maybe_client_id.unwrap_or(client_id)
                            };

                            // Send user input to plugin thread for logging
                            if let Some(ref senders) = senders {
                                let _ = senders.send_to_plugin(PluginInstruction::UserInput {
                                    client_id,
                                    action: action.clone(),
                                    terminal_id: maybe_pane_id,
                                    cli_client_id: if is_cli_client {
                                        Some(cli_client_id)
                                    } else {
                                        None
                                    },
                                });
                            }

                            // The read guard ends as a temporary in this expression so
                            // `route_action` runs without holding `session_data.read()` —
                            // see the doc comment on `route_action` for why this matters.
                            let session_data_assets =
                                session_data.read().unwrap().as_ref().map(|s| {
                                    (
                                        s.senders.clone(),
                                        s.default_shell.clone(),
                                        s.session_configuration
                                            .get_client_default_input_mode(&client_id),
                                    )
                                });
                            if let Some((senders, default_shell, client_input_mode)) =
                                session_data_assets
                            {
                                let dedicated_response = cli_action_has_dedicated_response(&action);
                                match route_action(RouteActionParams {
                                    action,
                                    caller: &caller,
                                    client_id,
                                    cli_client_id: Some(cli_client_id),
                                    pane_id: maybe_pane_id.map(PaneId::Terminal),
                                    senders,
                                    default_shell,
                                    seen_cli_pipes: Some(&mut seen_cli_pipes),
                                    default_mode: client_input_mode,
                                }) {
                                    Ok((route_action_should_break, completion)) => {
                                        if route_action_should_break {
                                            should_break = true;
                                        }
                                        if is_cli_client
                                            && cli_should_send_route_completion(
                                                dedicated_response,
                                                completion.as_ref(),
                                            )
                                        {
                                            let message =
                                                cli_action_completion_message(completion.as_ref());
                                            if let Err(error) =
                                                os_input.send_to_client(cli_client_id, message)
                                            {
                                                log::error!(
                                                    "failed to send CLI action completion to client {}: {}",
                                                    cli_client_id,
                                                    error
                                                );
                                            }
                                        }
                                    },
                                    Err(e) => {
                                        log::error!("{}", e);
                                        if is_cli_client {
                                            let _ = os_input.send_to_client(
                                                cli_client_id,
                                                ServerToClientMsg::LogError {
                                                    lines: vec![format!(
                                                        "failed to route CLI action: {e}"
                                                    )],
                                                },
                                            );
                                        }
                                    },
                                }
                            } else if is_cli_client {
                                let _ = os_input.send_to_client(
                                    cli_client_id,
                                    ServerToClientMsg::LogError {
                                        lines: vec![
                                            "session runtime is not ready for CLI actions"
                                                .to_string(),
                                        ],
                                    },
                                );
                            }
                        },
                        ClientToServerMsg::TerminalResize { new_size } => {
                            // Check if this is a watcher or regular client
                            if is_watcher {
                                // For watchers: send size to Screen for tracking, don't affect screen size
                                send_to_screen_or_retry_queue!(
                                    senders.clone(),
                                    ScreenInstruction::WatcherTerminalResize(client_id, new_size),
                                    instruction,
                                    retry_queue
                                )
                                .with_context(err_context)?;
                            } else {
                                session_state
                                    .write()
                                    .to_anyhow()
                                    .with_context(err_context)?
                                    .set_client_size(client_id, new_size);
                                // Per-tab sizing: Screen's RecomputeTabSize
                                // handler records the viewport even for
                                // clients without an active tab yet (resizes
                                // arriving before AddClient is processed).
                                // While the session is still booting the
                                // instruction is queued rather than dropped —
                                // a lost resize leaves the tab sized for a
                                // terminal that no longer exists.
                                send_to_screen_or_retry_queue!(
                                    senders.clone(),
                                    ScreenInstruction::RecomputeTabSize(client_id, new_size),
                                    instruction,
                                    retry_queue
                                )
                                .with_context(err_context)?;
                            }
                        },
                        ClientToServerMsg::TerminalPixelDimensions { pixel_dimensions } => {
                            send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::TerminalPixelDimensions(pixel_dimensions),
                                instruction,
                                retry_queue
                            )
                            .with_context(err_context)?;
                        },
                        ClientToServerMsg::BackgroundColor {
                            color: ref background_color_instruction,
                        } => {
                            send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::TerminalBackgroundColor(
                                    background_color_instruction.clone()
                                ),
                                instruction,
                                retry_queue
                            )
                            .with_context(err_context)?;
                        },
                        ClientToServerMsg::ForegroundColor {
                            color: ref foreground_color_instruction,
                        } => {
                            send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::TerminalForegroundColor(
                                    foreground_color_instruction.clone()
                                ),
                                instruction,
                                retry_queue
                            )
                            .with_context(err_context)?;
                        },
                        ClientToServerMsg::ColorRegisters {
                            ref color_registers,
                        } => {
                            send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::TerminalColorRegisters(
                                    color_registers
                                        .iter()
                                        .map(|c| (c.index, c.color.clone()))
                                        .collect()
                                ),
                                instruction,
                                retry_queue
                            )
                            .with_context(err_context)?;
                        },
                        ClientToServerMsg::FirstClientConnected {
                            cli_assets,
                            is_web_client,
                        } => {
                            let new_client_instruction = ServerInstruction::FirstClientConnected(
                                cli_assets,
                                is_web_client,
                                client_id,
                            );
                            to_server
                                .send(new_client_instruction)
                                .with_context(err_context)?;
                        },
                        ClientToServerMsg::AttachClient {
                            cli_assets,
                            tab_position_to_focus,
                            pane_to_focus: pane_id_to_focus,
                            is_web_client,
                        } => {
                            let allow_web_connections = session_data
                                .read()
                                .ok()
                                .and_then(|s| {
                                    s.as_ref().map(|s| s.web_sharing.web_clients_allowed())
                                })
                                .unwrap_or(false);
                            let should_allow_connection = !is_web_client || allow_web_connections;
                            if should_allow_connection {
                                let attach_client_instruction = ServerInstruction::AttachClient(
                                    cli_assets,
                                    tab_position_to_focus,
                                    pane_id_to_focus.map(|p| (p.pane_id, p.is_plugin)),
                                    is_web_client,
                                    client_id,
                                );
                                to_server
                                    .send(attach_client_instruction)
                                    .with_context(err_context)?;
                            } else {
                                let error = "This session does not allow web connections.";
                                let _ = to_server.send(ServerInstruction::LogError(
                                    vec![error.to_owned()],
                                    client_id,
                                    None,
                                ));
                                let _ = to_server
                                    .send(ServerInstruction::SendWebClientsForbidden(client_id));
                            }
                        },
                        ClientToServerMsg::AttachWatcherClient {
                            terminal_size,
                            is_web_client,
                        } => {
                            let allow_web_connections = session_data
                                .read()
                                .ok()
                                .and_then(|s| {
                                    s.as_ref().map(|s| s.web_sharing.web_clients_allowed())
                                })
                                .unwrap_or(false);
                            let should_allow_connection = !is_web_client || allow_web_connections;

                            if should_allow_connection {
                                let attach_watcher_instruction =
                                    ServerInstruction::AttachWatcherClient(
                                        client_id,
                                        terminal_size,
                                        is_web_client,
                                    );
                                to_server
                                    .send(attach_watcher_instruction)
                                    .with_context(err_context)?;
                            } else {
                                let error = "This session does not allow web connections.";
                                let _ = to_server.send(ServerInstruction::LogError(
                                    vec![error.to_owned()],
                                    client_id,
                                    None,
                                ));
                                let _ = to_server
                                    .send(ServerInstruction::SendWebClientsForbidden(client_id));
                            }
                        },
                        ClientToServerMsg::ClientExited => {
                            let _ = to_server.send(ServerInstruction::RemoveClient(client_id));
                            return Ok(true);
                        },
                        ClientToServerMsg::KillSession => {
                            to_server
                                .send(ServerInstruction::KillSession)
                                .with_context(err_context)?;
                        },
                        ClientToServerMsg::ConnStatus => {
                            let _ = to_server.send(ServerInstruction::ConnStatus(client_id));
                            should_break = true;
                        },
                        ClientToServerMsg::DetachSession { client_ids } => {
                            let _ =
                                to_server.send(ServerInstruction::DetachSession(client_ids, None));
                            should_break = true;
                        },
                        ClientToServerMsg::WebServerStarted { base_url } => {
                            let _ = to_server.send(ServerInstruction::WebServerStarted(base_url));
                        },
                        ClientToServerMsg::FailedToStartWebServer { error } => {
                            let _ =
                                to_server.send(ServerInstruction::FailedToStartWebServer(error));
                        },
                        ClientToServerMsg::DesktopNotificationResponse { ref raw_bytes } => {
                            let _ = send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::DesktopNotificationResponse(
                                    raw_bytes.clone(),
                                    client_id,
                                ),
                                instruction,
                                retry_queue
                            );
                        },
                        ClientToServerMsg::ForwardedReplyFromHost {
                            token,
                            ref reply_bytes,
                        } => {
                            // The client that owns this forward
                            // answered — drop the in-flight entry
                            // first so a later disconnect of that
                            // client can't synthesize a spurious
                            // empty reply for the same token.
                            session_state
                                .write()
                                .unwrap()
                                .clear_forward_in_flight(token);
                            let _ = send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::ForwardedReplyFromHost {
                                    token,
                                    reply_bytes: reply_bytes.clone(),
                                },
                                instruction,
                                retry_queue
                            );
                        },
                        ClientToServerMsg::HostTerminalThemeChanged { mode } => {
                            let _ = send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::HostTerminalThemeChanged(mode),
                                instruction,
                                retry_queue
                            );
                        },
                        ClientToServerMsg::SubscribeToPaneRenders {
                            ref pane_ids,
                            ref scrollback,
                            ansi,
                        } => {
                            let _ = send_to_screen_or_retry_queue!(
                                senders,
                                ScreenInstruction::SubscribeToPaneRenders {
                                    client_id,
                                    pane_ids: pane_ids.clone(),
                                    scrollback: *scrollback,
                                    ansi,
                                },
                                instruction,
                                retry_queue
                            );
                        },
                    }
                    Ok(should_break)
                };
                let mut repeat_retries = VecDeque::new();
                while let Some(instruction_to_retry) = retry_queue.pop_front() {
                    log::warn!("Server ready, retrying sending instruction.");
                    thread::sleep(Duration::from_millis(5));
                    let should_break =
                        handle_instruction(instruction_to_retry, Some(&mut repeat_retries))?;
                    if should_break {
                        break 'route_loop;
                    }
                }
                // retry on loop around
                retry_queue.append(&mut repeat_retries);
                let should_break = handle_instruction(instruction, Some(&mut retry_queue))?;
                if should_break {
                    break 'route_loop;
                }
                // signal to the client that the action has finished processing and it can either
                // exit (if it's a cli client) or allow the user to perform another action (if it's
                // an actively connected user)
                let _ = os_input.send_to_client(client_id, ServerToClientMsg::UnblockInputThread);
            },
            ClientReceiveOutcome::Disconnected => {
                // Clean EOF / broken pipe — end the route without the historical
                // "unknown message" retry loop that eventually forced logout.
                let age = connection_started.elapsed();
                if age >= Duration::from_secs(60) {
                    log::warn!(
                        "warden.expired_client caller={} client_id={} age_seconds={} result=disconnected",
                        caller,
                        client_id,
                        age.as_secs()
                    );
                }
                log::info!("Client {client_id} disconnected");
                break 'route_loop;
            },
            ClientReceiveOutcome::ProtocolError(reason) => {
                log::error!("Client {client_id} protocol error: {reason}");
                let _ = os_input.send_to_client(
                    client_id,
                    ServerToClientMsg::Exit {
                        exit_reason: ExitReason::Error(format!("Protocol error: {reason}")),
                    },
                );
                let _ = to_server.send(ServerInstruction::RemoveClient(client_id));
                break 'route_loop;
            },
        }
    }
    // route thread exited, make sure we clean up
    let _ = to_server.send(ServerInstruction::RemoveClient(client_id));
    Ok(())
}

fn request_panes_from_screen(
    senders: &ThreadSenders,
    show_all: bool,
) -> Result<Option<ListPanesResponse>> {
    use crossbeam::channel::{RecvTimeoutError, unbounded};
    use std::time::Duration;

    let (response_sender, response_receiver) = unbounded();
    senders.send_to_screen(ScreenInstruction::ListPanes {
        show_all,
        response_channel: response_sender,
    })?;

    match response_receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(entries) => Ok(Some(entries)),
        Err(RecvTimeoutError::Timeout) => {
            log::error!("ListPanes timed out waiting for Screen response");
            Ok(None)
        },
        Err(RecvTimeoutError::Disconnected) => {
            log::error!("ListPanes channel disconnected");
            Ok(None)
        },
    }
}

fn request_tabs_from_screen(
    senders: &ThreadSenders,
    client_id: ClientId,
) -> Result<Option<ListTabsResponse>> {
    use crossbeam::channel::{RecvTimeoutError, unbounded};
    use std::time::Duration;

    let (response_sender, response_receiver) = unbounded();
    senders.send_to_screen(ScreenInstruction::ListTabs {
        client_id,
        response_channel: response_sender,
    })?;

    match response_receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(entries) => Ok(Some(entries)),
        Err(RecvTimeoutError::Timeout) => {
            log::error!("ListTabs timed out waiting for Screen response");
            Ok(None)
        },
        Err(RecvTimeoutError::Disconnected) => {
            log::error!("ListTabs channel disconnected");
            Ok(None)
        },
    }
}

fn request_list_clients_from_screen(
    senders: &ThreadSenders,
    default_shell: Option<PathBuf>,
) -> Result<Option<SessionLayoutMetadata>> {
    use crossbeam::channel::{RecvTimeoutError, unbounded};
    use std::time::Duration;

    let (response_sender, response_receiver) = unbounded();
    senders.send_to_screen(ScreenInstruction::ListClients {
        default_shell,
        response_channel: response_sender,
    })?;

    match response_receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(RecvTimeoutError::Timeout) => {
            log::error!("ListClients timed out waiting for Screen response");
            Ok(None)
        },
        Err(RecvTimeoutError::Disconnected) => {
            log::error!("ListClients channel disconnected");
            Ok(None)
        },
    }
}

fn enrich_list_clients_with_pty_data(
    metadata: &mut SessionLayoutMetadata,
    senders: &ThreadSenders,
) -> Result<()> {
    use crossbeam::channel::{RecvTimeoutError, unbounded};
    use std::collections::HashMap;
    use zellij_utils::data::GetPaneRunningCommandResponse;

    metadata.clear_list_client_unconfirmed_terminals();
    let deadline = Instant::now() + LIST_CLIENTS_PTY_ENRICH_DEADLINE;
    let focused_terminal_ids = metadata.focused_list_client_terminal_ids();

    let mut pending = Vec::new();
    for terminal_id in focused_terminal_ids {
        if Instant::now() >= deadline {
            metadata.mark_list_client_terminal_unconfirmed(terminal_id);
            continue;
        }
        let (cmd_sender, cmd_receiver) = unbounded();
        senders.send_to_pty(PtyInstruction::GetPaneRunningCommand {
            pane_id: PaneId::Terminal(terminal_id),
            response_channel: cmd_sender,
        })?;
        pending.push((terminal_id, cmd_receiver));
    }

    let mut terminal_ids_to_commands = HashMap::new();
    for (terminal_id, cmd_receiver) in pending {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            metadata.mark_list_client_terminal_unconfirmed(terminal_id);
            continue;
        }
        match cmd_receiver.recv_timeout(remaining) {
            Ok(GetPaneRunningCommandResponse::Ok(command_vec)) if !command_vec.is_empty() => {
                terminal_ids_to_commands.insert(terminal_id, command_vec);
            },
            Ok(_) | Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                metadata.mark_list_client_terminal_unconfirmed(terminal_id);
            },
        }
    }

    metadata.update_terminal_commands(terminal_ids_to_commands);
    let editor = metadata.default_editor.clone();
    metadata.update_default_editor(&editor);
    metadata.detect_editor_panes();
    Ok(())
}

fn request_current_tab_info_from_screen(
    senders: &ThreadSenders,
    client_id: ClientId,
) -> Result<Option<TabInfo>> {
    use crossbeam::channel::{RecvTimeoutError, unbounded};
    use std::time::Duration;

    let (response_sender, response_receiver) = unbounded();
    senders.send_to_screen(ScreenInstruction::GetCurrentTabInfo {
        client_id,
        response_channel: response_sender,
    })?;

    match response_receiver.recv_timeout(Duration::from_secs(1)) {
        Ok(tab_info_opt) => Ok(tab_info_opt),
        Err(RecvTimeoutError::Timeout) => {
            log::error!("GetCurrentTabInfo timed out waiting for Screen response");
            Ok(None)
        },
        Err(RecvTimeoutError::Disconnected) => {
            log::error!("GetCurrentTabInfo channel disconnected");
            Ok(None)
        },
    }
}

fn enrich_panes_with_pty_data(
    pane_entries: &mut [PaneListEntry],
    senders: &ThreadSenders,
) -> Result<()> {
    for entry in pane_entries.iter_mut() {
        if !entry.pane_info.is_plugin {
            let pane_id = PaneId::Terminal(entry.pane_info.id);
            enrich_pane_with_running_command(entry, pane_id, senders)?;
            enrich_pane_with_cwd(entry, pane_id, senders)?;
        }
    }
    Ok(())
}

fn enrich_pane_with_running_command(
    entry: &mut PaneListEntry,
    pane_id: PaneId,
    senders: &ThreadSenders,
) -> Result<()> {
    use crossbeam::channel::unbounded;
    use std::time::Duration;
    use zellij_utils::data::GetPaneRunningCommandResponse;

    let (cmd_sender, cmd_receiver) = unbounded();
    senders.send_to_pty(PtyInstruction::GetPaneRunningCommand {
        pane_id,
        response_channel: cmd_sender,
    })?;

    if let Ok(GetPaneRunningCommandResponse::Ok(command_vec)) =
        cmd_receiver.recv_timeout(Duration::from_millis(100))
    {
        entry.pane_command = Some(command_vec.join(" "));
    }

    Ok(())
}

fn enrich_pane_with_cwd(
    entry: &mut PaneListEntry,
    pane_id: PaneId,
    senders: &ThreadSenders,
) -> Result<()> {
    use crossbeam::channel::unbounded;
    use std::time::Duration;
    use zellij_utils::data::GetPaneCwdResponse;

    let (cwd_sender, cwd_receiver) = unbounded();
    senders.send_to_pty(PtyInstruction::GetPaneCwd {
        pane_id,
        response_channel: cwd_sender,
    })?;

    if let Ok(GetPaneCwdResponse::Ok(cwd)) = cwd_receiver.recv_timeout(Duration::from_millis(100)) {
        entry.pane_cwd = Some(cwd.to_string_lossy().to_string());
    }

    Ok(())
}

fn format_panes_as_json(pane_entries: &[PaneListEntry]) -> Vec<String> {
    vec![serde_json::to_string_pretty(pane_entries).unwrap_or_else(|_| "[]".to_string())]
}

fn format_panes_table(
    entries: &[PaneListEntry],
    show_tab: bool,
    show_command: bool,
    show_state: bool,
    show_geometry: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(build_table_header(
        show_tab,
        show_command,
        show_state,
        show_geometry,
    ));

    for entry in entries {
        lines.push(build_table_row(
            entry,
            show_tab,
            show_command,
            show_state,
            show_geometry,
        ));
    }

    lines
}

fn build_table_header(
    show_tab: bool,
    show_command: bool,
    show_state: bool,
    show_geometry: bool,
) -> String {
    let mut header = Vec::new();

    if show_tab {
        header.push("TAB_ID");
        header.push("TAB_POS");
        header.push("TAB_NAME");
    }

    header.push("PANE_ID");
    header.push("TYPE");
    header.push("TITLE");

    if show_command {
        header.push("COMMAND");
        header.push("CWD");
    }

    if show_state {
        header.push("FOCUSED");
        header.push("FLOATING");
        header.push("EXITED");
    }

    if show_geometry {
        header.push("X");
        header.push("Y");
        header.push("ROWS");
        header.push("COLS");
    }

    header.join("  ")
}

fn build_table_row(
    entry: &PaneListEntry,
    show_tab: bool,
    show_command: bool,
    show_state: bool,
    show_geometry: bool,
) -> String {
    let mut row = Vec::new();

    if show_tab {
        row.push(entry.tab_id.to_string());
        row.push(entry.tab_position.to_string());
        row.push(entry.tab_name.clone());
    }

    row.push(format_pane_id(&entry.pane_info));
    row.push(format_pane_type(&entry.pane_info));
    row.push(entry.pane_info.title.clone());

    if show_command {
        row.push(extract_command(entry));
        row.push(extract_cwd(entry));
    }

    if show_state {
        row.push(entry.pane_info.is_focused.to_string());
        row.push(entry.pane_info.is_floating.to_string());
        row.push(entry.pane_info.exited.to_string());
    }

    if show_geometry {
        row.push(entry.pane_info.pane_x.to_string());
        row.push(entry.pane_info.pane_y.to_string());
        row.push(entry.pane_info.pane_rows.to_string());
        row.push(entry.pane_info.pane_columns.to_string());
    }

    row.join("  ")
}

fn format_pane_id(pane_info: &zellij_utils::data::PaneInfo) -> String {
    if pane_info.is_plugin {
        format!("plugin_{}", pane_info.id)
    } else {
        format!("terminal_{}", pane_info.id)
    }
}

fn format_pane_type(pane_info: &zellij_utils::data::PaneInfo) -> String {
    if pane_info.is_plugin {
        "plugin".to_string()
    } else {
        "terminal".to_string()
    }
}

fn extract_command(entry: &PaneListEntry) -> String {
    entry
        .pane_command
        .as_ref()
        .or(entry.pane_info.terminal_command.as_ref())
        .or(entry.pane_info.plugin_url.as_ref())
        .map(|s| s.as_str())
        .unwrap_or("-")
        .to_string()
}

fn extract_cwd(entry: &PaneListEntry) -> String {
    entry.pane_cwd.as_deref().unwrap_or("-").to_string()
}

fn format_tabs_as_json(response: &ListTabsResponse) -> Vec<String> {
    let tab_infos = response
        .tabs
        .iter()
        .filter_map(|tab| serde_json::to_value(tab).ok())
        .map(|mut tab| {
            if let Some(tab) = tab.as_object_mut() {
                let tab_id = tab.get("tab_id").and_then(serde_json::Value::as_u64);
                tab.insert(
                    "session_incarnation".to_owned(),
                    serde_json::Value::String(response.session_incarnation.clone()),
                );
                if let Some(tab_instance_id) =
                    tab_id.and_then(|tab_id| response.tab_instance_ids.get(&(tab_id as usize)))
                {
                    tab.insert(
                        "tab_instance_id".to_owned(),
                        serde_json::Value::String(tab_instance_id.clone()),
                    );
                }
            }
            tab
        })
        .collect::<Vec<_>>();
    vec![serde_json::to_string_pretty(&tab_infos).unwrap_or_else(|_| "[]".to_string())]
}

fn format_tabs_table(
    tabs: &[TabInfo],
    show_state: bool,
    show_dimensions: bool,
    show_panes: bool,
    show_layout: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(build_tabs_table_header(
        show_state,
        show_dimensions,
        show_panes,
        show_layout,
    ));

    for tab_info in tabs {
        lines.push(build_tabs_table_row(
            tab_info,
            show_state,
            show_dimensions,
            show_panes,
            show_layout,
        ));
    }

    lines
}

fn build_tabs_table_header(
    show_state: bool,
    show_dimensions: bool,
    show_panes: bool,
    show_layout: bool,
) -> String {
    let mut header = Vec::new();

    // Core fields (always shown)
    header.push("TAB_ID");
    header.push("POSITION");
    header.push("NAME");

    if show_state {
        header.push("ACTIVE");
        header.push("FULLSCREEN");
        header.push("SYNC_PANES");
        header.push("FLOATING_VIS");
    }

    if show_dimensions {
        header.push("VP_ROWS");
        header.push("VP_COLS");
        header.push("DA_ROWS");
        header.push("DA_COLS");
    }

    if show_panes {
        header.push("TILED_PANES");
        header.push("FLOAT_PANES");
        header.push("HIDDEN_PANES");
    }

    if show_layout {
        header.push("SWAP_LAYOUT");
        header.push("LAYOUT_DIRTY");
    }

    header.join("  ")
}

fn build_tabs_table_row(
    tab_info: &TabInfo,
    show_state: bool,
    show_dimensions: bool,
    show_panes: bool,
    show_layout: bool,
) -> String {
    let mut row = Vec::new();

    // Core fields
    row.push(tab_info.tab_id.to_string());
    row.push(tab_info.position.to_string());
    row.push(tab_info.name.clone());

    if show_state {
        row.push(tab_info.active.to_string());
        row.push(tab_info.is_fullscreen_active.to_string());
        row.push(tab_info.is_sync_panes_active.to_string());
        row.push(tab_info.are_floating_panes_visible.to_string());
    }

    if show_dimensions {
        row.push(tab_info.viewport_rows.to_string());
        row.push(tab_info.viewport_columns.to_string());
        row.push(tab_info.display_area_rows.to_string());
        row.push(tab_info.display_area_columns.to_string());
    }

    if show_panes {
        row.push(tab_info.selectable_tiled_panes_count.to_string());
        row.push(tab_info.selectable_floating_panes_count.to_string());
        row.push(tab_info.panes_to_hide.to_string());
    }

    if show_layout {
        row.push(
            tab_info
                .active_swap_layout_name
                .as_deref()
                .unwrap_or("-")
                .to_string(),
        );
        row.push(tab_info.is_swap_layout_dirty.to_string());
    }

    row.join("  ")
}

fn format_current_tab_info_as_json(tab_info: &TabInfo) -> Vec<String> {
    vec![serde_json::to_string_pretty(tab_info).unwrap_or_else(|_| "{}".to_string())]
}

fn format_current_tab_info_plain(tab_info: &TabInfo) -> Vec<String> {
    vec![
        format!("name: {}", tab_info.name),
        format!("id: {}", tab_info.tab_id),
        format!("position: {}", tab_info.position),
    ]
}

fn cli_action_completion_message(result: Option<&ActionCompletionResult>) -> ServerToClientMsg {
    let Some(result) = result else {
        return ServerToClientMsg::LogError {
            lines: vec!["CLI action ended without a completion result".to_string()],
        };
    };

    if let Some(error_message) = &result.error_message {
        ServerToClientMsg::LogError {
            lines: vec![error_message.clone()],
        }
    } else if let Some(stdout_message) = &result.stdout_message {
        ServerToClientMsg::Log {
            lines: vec![stdout_message.clone()],
        }
    } else if let Some(exit_status) = result.exit_status {
        ServerToClientMsg::Exit {
            exit_reason: ExitReason::CustomExitStatus(exit_status),
        }
    } else if let Some(tab_id) = result.affected_tab_id {
        ServerToClientMsg::Log {
            lines: vec![tab_id.to_string()],
        }
    } else if let Some(pane_id) = result.affected_pane_id {
        ServerToClientMsg::Log {
            lines: vec![pane_id.to_string()],
        }
    } else {
        // Generic input unblocks are session-wide and can race an unrelated CLI
        // request. A targeted empty Log is the explicit success acknowledgement
        // for an action that has no stdout payload.
        ServerToClientMsg::Log { lines: vec![] }
    }
}

fn cli_action_has_dedicated_response(action: &Action) -> bool {
    match action {
        Action::CliPipe { .. } | Action::DumpLayout | Action::QueryTabNames => true,
        Action::DumpScreen { file_path, .. } => file_path.is_none(),
        _ => false,
    }
}

fn cli_should_send_route_completion(
    dedicated_response: bool,
    result: Option<&ActionCompletionResult>,
) -> bool {
    if !dedicated_response {
        return true;
    }
    result.is_some_and(|completion| {
        completion.error_message.is_some()
            || completion.exit_status.is_some_and(|status| status != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_cmd_pipe_waits_for_guest_completion_past_the_key_deadline() {
        use zellij_utils::data::KeyWithModifier;
        use zellij_utils::input::config::Config;
        let config = Config::from_kdl(
            r#"
            keybinds { shared { bind "Super Shift ." {
                MessagePlugin "compact-bar" { name "vc_quick_cmd"; }
            }; }; }
        "#,
            None,
        )
        .unwrap();
        let key = KeyWithModifier::new(BareKey::Char('.'))
            .with_super_modifier()
            .with_shift_modifier();
        let action = config
            .keybinds
            .get_actions_for_key_in_mode(&InputMode::Tab, &key)
            .unwrap()[0]
            .clone();
        let (tx, rx) = zellij_utils::channels::unbounded();
        let senders = ThreadSenders {
            to_plugin: Some(SenderWithContext::new(tx)),
            should_silently_fail: true,
            ..Default::default()
        };
        let guest = thread::spawn(move || {
            while let Ok((instruction, _)) = rx.recv() {
                if let PluginInstruction::KeybindPipe {
                    cli_client_id,
                    notification_end,
                    ..
                } = instruction
                {
                    assert_eq!(cli_client_id, 8);
                    thread::sleep(Duration::from_millis(1100));
                    drop(notification_end);
                    return;
                }
            }
        });
        let result = route_action(RouteActionParams {
            action,
            caller: "interactive",
            client_id: 8,
            cli_client_id: None,
            pane_id: None,
            senders,
            default_shell: None,
            seen_cli_pipes: None,
            default_mode: InputMode::Normal,
        })
        .unwrap()
        .1
        .unwrap();
        guest.join().unwrap();
        assert_eq!(result.error_message, None);
        assert_eq!(result.exit_status, None);
    }

    #[test]
    fn route_caller_is_bounded_and_safe_for_receipts() {
        assert_eq!(
            normalize_route_caller("  settlement worker  "),
            "settlementworker"
        );
        assert_eq!(normalize_route_caller("💥"), "anonymous");
        assert_eq!(normalize_route_caller(&"a".repeat(100)).len(), 64);
    }

    #[test]
    fn test_notification_end_sets_affected_tab_id() {
        let (tx, rx) = oneshot::channel();
        let mut notification_end = NotificationEnd::new(tx);

        notification_end.set_affected_tab_id(42);

        drop(notification_end);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.affected_tab_id, Some(42));
    }

    #[test]
    fn test_notification_end_default_affected_tab_id_none() {
        let (tx, rx) = oneshot::channel();
        let notification_end = NotificationEnd::new(tx);

        drop(notification_end);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.affected_tab_id, None);
        assert_eq!(result.exit_status, None);
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn test_action_completion_result_includes_tab_id() {
        let result = ActionCompletionResult {
            exit_status: None,
            affected_pane_id: None,
            affected_tab_id: Some(123),
            error_message: None,
            stdout_message: None,
        };

        assert_eq!(result.affected_tab_id, Some(123));
    }

    #[test]
    fn cli_completion_without_payload_gets_targeted_success_ack() {
        let result = ActionCompletionResult {
            exit_status: None,
            affected_pane_id: None,
            affected_tab_id: None,
            error_message: None,
            stdout_message: None,
        };

        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines } if lines.is_empty()
        ));
    }

    #[test]
    fn cli_completion_carries_stdout_instead_of_generic_unblock() {
        let result = ActionCompletionResult {
            exit_status: None,
            affected_pane_id: None,
            affected_tab_id: None,
            error_message: None,
            stdout_message: Some("[{\"tab_id\":1}]".to_string()),
        };

        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines }
                if lines == vec!["[{\"tab_id\":1}]".to_string()]
        ));
    }

    #[test]
    fn missing_cli_completion_is_an_explicit_error() {
        assert!(matches!(
            cli_action_completion_message(None),
            ServerToClientMsg::LogError { lines }
                if lines == vec!["CLI action ended without a completion result".to_string()]
        ));
    }

    fn route_list_clients(senders: ThreadSenders) -> ActionCompletionResult {
        route_action(RouteActionParams {
            action: Action::ListClients,
            caller: "anonymous",
            client_id: 2,
            cli_client_id: Some(9),
            pane_id: None,
            senders,
            default_shell: None,
            seen_cli_pipes: None,
            default_mode: InputMode::Normal,
        })
        .unwrap()
        .1
        .unwrap()
    }

    fn list_clients_test_senders(
        screen_tx: zellij_utils::channels::Sender<(
            ScreenInstruction,
            zellij_utils::errors::ErrorContext,
        )>,
        plugin_tx: zellij_utils::channels::Sender<(
            PluginInstruction,
            zellij_utils::errors::ErrorContext,
        )>,
        pty_tx: zellij_utils::channels::Sender<(
            PtyInstruction,
            zellij_utils::errors::ErrorContext,
        )>,
    ) -> ThreadSenders {
        ThreadSenders {
            to_screen: Some(SenderWithContext::new(screen_tx)),
            to_plugin: Some(SenderWithContext::new(plugin_tx)),
            to_pty: Some(SenderWithContext::new(pty_tx)),
            should_silently_fail: true,
            ..Default::default()
        }
    }

    fn list_clients_command_pane(
        terminal_id: u32,
        command: &str,
        args: &[&str],
        focused_clients: Vec<ClientId>,
    ) -> crate::session_layout_metadata::PaneLayoutMetadata {
        use crate::session_layout_metadata::PaneLayoutMetadata;
        use std::path::PathBuf;
        use zellij_utils::input::command::RunCommand;
        use zellij_utils::input::layout::Run;
        use zellij_utils::pane_size::PaneGeom;

        let mut run_command = RunCommand::new(PathBuf::from(command));
        run_command.args = args.iter().map(|arg| (*arg).to_string()).collect();
        PaneLayoutMetadata {
            id: PaneId::Terminal(terminal_id),
            geom: PaneGeom::default(),
            run: Some(Run::Command(run_command)),
            cwd: None,
            is_borderless: false,
            title: None,
            is_focused: !focused_clients.is_empty(),
            pane_contents: None,
            focused_clients,
            default_fg: None,
            default_bg: None,
        }
    }

    fn spawn_list_clients_screen(
        screen_rx: zellij_utils::channels::Receiver<(
            ScreenInstruction,
            zellij_utils::errors::ErrorContext,
        )>,
        metadata: crate::session_layout_metadata::SessionLayoutMetadata,
    ) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            while let Ok((instruction, _)) = screen_rx.recv() {
                if let ScreenInstruction::ListClients {
                    response_channel, ..
                } = instruction
                {
                    let _ = response_channel.send(metadata);
                    return;
                }
            }
        })
    }

    fn list_clients_command_cell<'a>(stdout: &'a str, pane_token: &str) -> &'a str {
        let row = stdout
            .lines()
            .find(|line| line.contains(pane_token))
            .unwrap_or_else(|| panic!("missing {pane_token} in {stdout}"));
        row.split_once(pane_token)
            .map(|(_, rest)| rest.trim())
            .expect("command cell")
    }

    #[test]
    fn list_clients_completes_from_screen_without_plugin_metadata_hop() {
        use crate::session_layout_metadata::SessionLayoutMetadata;
        use zellij_utils::data::GetPaneRunningCommandResponse;

        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let mut metadata = SessionLayoutMetadata::default();
        metadata.add_tab(
            "A".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            true,
            true,
            vec![list_clients_command_pane(
                7,
                "stale-invoked",
                &["--old"],
                vec![2],
            )],
            vec![],
        );
        let screen = spawn_list_clients_screen(screen_rx, metadata);

        let pty = thread::spawn(move || {
            while let Ok((instruction, _)) = pty_rx.recv() {
                if let PtyInstruction::GetPaneRunningCommand {
                    pane_id,
                    response_channel,
                } = instruction
                {
                    assert_eq!(pane_id, PaneId::Terminal(7));
                    let _ = response_channel.send(GetPaneRunningCommandResponse::Ok(vec![
                        "workload".into(),
                        "--pid".into(),
                    ]));
                    return;
                }
            }
        });

        let result = route_list_clients(senders);
        screen.join().unwrap();
        pty.join().unwrap();

        let stdout = result
            .stdout_message
            .as_deref()
            .expect("list-clients must complete with stdout");
        assert!(stdout.contains("CLIENT_ID"));
        assert!(stdout.contains("ZELLIJ_PANE_ID"));
        assert!(stdout.contains("RUNNING_COMMAND"));
        assert!(stdout.contains("terminal_7"));
        let command = list_clients_command_cell(stdout, "terminal_7");
        assert!(command.starts_with("workload"));
        assert!(!command.starts_with("UNAVAILABLE"));
        assert!(!command.contains("stale-invoked"));
        assert_eq!(result.error_message, None);

        let plugin_ops: Vec<_> = plugin_rx
            .try_iter()
            .map(|(instruction, _)| instruction)
            .collect();
        assert!(
            plugin_ops.iter().all(|instruction| {
                !matches!(instruction, PluginInstruction::ListClientsMetadata(..))
            }),
            "CLI ListClients must not wait on the plugin metadata hop: {plugin_ops:?}"
        );
        assert!(!cli_action_has_dedicated_response(&Action::ListClients));
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines }
                if lines.len() == 1 && lines[0].contains("terminal_7")
        ));
    }

    #[test]
    fn list_clients_many_unresponsive_panes_stay_inside_fixed_pty_deadline() {
        use crate::session_layout_metadata::SessionLayoutMetadata;
        use std::sync::{Arc, Mutex};

        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, _plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let mut tiled = Vec::new();
        for terminal_id in 1..=100 {
            tiled.push(list_clients_command_pane(
                terminal_id,
                "silent",
                &["sleep"],
                vec![],
            ));
        }
        tiled.push(list_clients_command_pane(
            101,
            "stale-invoked",
            &["--old"],
            vec![2],
        ));
        let mut metadata = SessionLayoutMetadata::default();
        metadata.add_tab(
            "A".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            true,
            true,
            tiled,
            vec![],
        );
        let screen = spawn_list_clients_screen(screen_rx, metadata);

        let requested = Arc::new(Mutex::new(Vec::new()));
        let requested_for_pty = requested.clone();
        let pty = thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((instruction, _)) = pty_rx.recv() {
                if let PtyInstruction::GetPaneRunningCommand {
                    pane_id,
                    response_channel,
                } = instruction
                {
                    requested_for_pty.lock().unwrap().push(pane_id);
                    held.push(response_channel);
                }
            }
        });

        let started = Instant::now();
        let result = route_list_clients(senders);
        let elapsed = started.elapsed();
        screen.join().unwrap();
        pty.join().unwrap();

        assert!(
            elapsed < Duration::from_secs(1),
            "100 silent panes must not multiply the 100ms PTY budget: {elapsed:?}"
        );
        assert_eq!(&*requested.lock().unwrap(), &[PaneId::Terminal(101)]);
        let stdout = result.stdout_message.expect("stdout");
        let command = list_clients_command_cell(&stdout, "terminal_101");
        assert!(command.starts_with("UNAVAILABLE"));
        assert!(command.contains("last: stale-invoked --old"));
        assert!(stdout.contains("CLIENT_ID"));
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn list_clients_plugin_focused_row_does_not_query_pty() {
        use crate::session_layout_metadata::{PaneLayoutMetadata, SessionLayoutMetadata};
        use std::sync::{Arc, Mutex};
        use zellij_utils::input::layout::{Run, RunPlugin, RunPluginOrAlias};
        use zellij_utils::pane_size::PaneGeom;

        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, _plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let tiled = vec![
            list_clients_command_pane(1, "silent", &["sleep"], vec![]),
            PaneLayoutMetadata {
                id: PaneId::Plugin(3),
                geom: PaneGeom::default(),
                run: Some(Run::Plugin(RunPluginOrAlias::RunPlugin(
                    RunPlugin::from_url("vc-frame:compact-bar").unwrap(),
                ))),
                cwd: None,
                is_borderless: false,
                title: None,
                is_focused: true,
                pane_contents: None,
                focused_clients: vec![2],
                default_fg: None,
                default_bg: None,
            },
        ];
        let mut metadata = SessionLayoutMetadata::default();
        metadata.add_tab(
            "A".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            true,
            true,
            tiled,
            vec![],
        );
        let screen = spawn_list_clients_screen(screen_rx, metadata);

        let requested = Arc::new(Mutex::new(Vec::new()));
        let requested_for_pty = requested.clone();
        let pty = thread::spawn(move || {
            while let Ok((instruction, _)) = pty_rx.recv() {
                if let PtyInstruction::GetPaneRunningCommand { pane_id, .. } = instruction {
                    requested_for_pty.lock().unwrap().push(pane_id);
                }
            }
        });

        let result = route_list_clients(senders);
        screen.join().unwrap();
        pty.join().unwrap();

        assert!(requested.lock().unwrap().is_empty());
        let stdout = result.stdout_message.expect("stdout");
        let command = list_clients_command_cell(&stdout, "plugin_3");
        assert!(command.contains("vc-frame:compact-bar"));
        assert!(!command.starts_with("UNAVAILABLE"));
    }

    #[test]
    fn list_clients_editor_row_uses_pty_confirmed_command() {
        use crate::session_layout_metadata::{PaneLayoutMetadata, SessionLayoutMetadata};
        use std::path::PathBuf;
        use zellij_utils::data::GetPaneRunningCommandResponse;
        use zellij_utils::input::layout::Run;
        use zellij_utils::pane_size::PaneGeom;

        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, _plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let mut metadata = SessionLayoutMetadata::default();
        metadata.default_editor = Some(PathBuf::from("nvim"));
        metadata.add_tab(
            "A".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            true,
            true,
            vec![PaneLayoutMetadata {
                id: PaneId::Terminal(9),
                geom: PaneGeom::default(),
                run: Some(Run::EditFile(PathBuf::from("stale.md"), Some(3), None)),
                cwd: None,
                is_borderless: false,
                title: None,
                is_focused: true,
                pane_contents: None,
                focused_clients: vec![2],
                default_fg: None,
                default_bg: None,
            }],
            vec![],
        );
        let screen = spawn_list_clients_screen(screen_rx, metadata);

        let pty = thread::spawn(move || {
            while let Ok((instruction, _)) = pty_rx.recv() {
                if let PtyInstruction::GetPaneRunningCommand {
                    pane_id,
                    response_channel,
                } = instruction
                {
                    assert_eq!(pane_id, PaneId::Terminal(9));
                    let _ = response_channel.send(GetPaneRunningCommandResponse::Ok(vec![
                        "nvim".into(),
                        "+12".into(),
                        "notes.md".into(),
                    ]));
                    return;
                }
            }
        });

        let result = route_list_clients(senders);
        screen.join().unwrap();
        pty.join().unwrap();

        let stdout = result.stdout_message.expect("stdout");
        let command = list_clients_command_cell(&stdout, "terminal_9");
        assert!(command.contains("nvim"));
        assert!(command.contains("notes.md"));
        assert!(!command.contains("stale.md"));
        assert!(!command.starts_with("UNAVAILABLE"));
    }

    #[test]
    fn list_clients_editor_row_is_unavailable_when_pty_does_not_confirm() {
        use crate::session_layout_metadata::{PaneLayoutMetadata, SessionLayoutMetadata};
        use std::path::PathBuf;
        use zellij_utils::input::layout::Run;
        use zellij_utils::pane_size::PaneGeom;

        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, _plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let mut metadata = SessionLayoutMetadata::default();
        metadata.default_editor = Some(PathBuf::from("nvim"));
        metadata.add_tab(
            "A".into(),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            true,
            true,
            vec![PaneLayoutMetadata {
                id: PaneId::Terminal(9),
                geom: PaneGeom::default(),
                run: Some(Run::EditFile(PathBuf::from("notes.md"), Some(12), None)),
                cwd: None,
                is_borderless: false,
                title: None,
                is_focused: true,
                pane_contents: None,
                focused_clients: vec![2],
                default_fg: None,
                default_bg: None,
            }],
            vec![],
        );
        let screen = spawn_list_clients_screen(screen_rx, metadata);
        let pty = thread::spawn(move || {
            let mut held = Vec::new();
            while let Ok((instruction, _)) = pty_rx.recv() {
                if let PtyInstruction::GetPaneRunningCommand {
                    response_channel, ..
                } = instruction
                {
                    held.push(response_channel);
                }
            }
        });

        let result = route_list_clients(senders);
        screen.join().unwrap();
        pty.join().unwrap();

        let stdout = result.stdout_message.expect("stdout");
        let command = list_clients_command_cell(&stdout, "terminal_9");
        assert!(
            command.starts_with("UNAVAILABLE"),
            "EditFile invoked_with must not be confirmed current: {command}"
        );
        assert!(command.contains("last: nvim notes.md"));
    }

    #[test]
    fn list_clients_missing_screen_sender_is_an_explicit_cli_error() {
        // No Screen sender: send_to_screen drops the oneshot and recv
        // disconnects immediately. This is not a held-open timeout.
        let senders = ThreadSenders {
            should_silently_fail: true,
            ..Default::default()
        };
        let started = Instant::now();
        let result = route_list_clients(senders);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "missing Screen sender must fail closed immediately, not wait the 1s timeout"
        );
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some("Timeout listing clients")
        );
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::LogError { lines }
                if lines == vec!["Timeout listing clients".to_string()]
        ));
    }

    #[test]
    fn list_clients_held_open_screen_timeout_is_an_explicit_cli_error() {
        let (screen_tx, screen_rx) = zellij_utils::channels::unbounded();
        let senders = ThreadSenders {
            to_screen: Some(SenderWithContext::new(screen_tx)),
            should_silently_fail: true,
            ..Default::default()
        };
        let started = Instant::now();
        let result = route_list_clients(senders);
        let elapsed = started.elapsed();
        drop(screen_rx);
        assert!(
            elapsed >= Duration::from_millis(900),
            "held-open Screen must wait the 1s recv_timeout, got {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(3));
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some("Timeout listing clients")
        );
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::LogError { lines }
                if lines == vec!["Timeout listing clients".to_string()]
        ));
    }

    #[test]
    fn existing_cli_response_protocols_do_not_get_a_second_ack() {
        let cli_pipe = Action::CliPipe {
            pipe_id: "pipe".to_string(),
            name: Some("test".to_string()),
            payload: None,
            args: None,
            plugin: None,
            configuration: None,
            launch_new: false,
            skip_cache: false,
            floating: None,
            in_place: None,
            cwd: None,
            pane_title: None,
        };

        assert!(cli_action_has_dedicated_response(&cli_pipe));
        assert!(cli_action_has_dedicated_response(&Action::DumpLayout));
        assert!(!cli_action_has_dedicated_response(&Action::ListClients));
        assert!(cli_action_has_dedicated_response(&Action::QueryTabNames));
        let dump_to_stdout = Action::DumpScreen {
            file_path: None,
            include_scrollback: false,
            pane_id: None,
            ansi: false,
            expected_tab_id: None,
            expected_tab_name: None,
            expected_session_incarnation: None,
            expected_tab_instance_id: None,
        };
        let dump_to_file = Action::DumpScreen {
            file_path: Some("dump".into()),
            include_scrollback: false,
            pane_id: None,
            ansi: false,
            expected_tab_id: None,
            expected_tab_name: None,
            expected_session_incarnation: None,
            expected_tab_instance_id: None,
        };
        assert!(cli_action_has_dedicated_response(&dump_to_stdout));
        assert!(!cli_action_has_dedicated_response(&dump_to_file));
        assert!(!cli_action_has_dedicated_response(&Action::NoOp));
        let dump_failure = ActionCompletionResult {
            exit_status: Some(1),
            affected_pane_id: None,
            affected_tab_id: None,
            error_message: Some("No dumpable pane after clients detached".into()),
            stdout_message: None,
        };
        assert!(
            !cli_should_send_route_completion(
                true,
                Some(&ActionCompletionResult {
                    exit_status: None,
                    affected_pane_id: None,
                    affected_tab_id: None,
                    error_message: None,
                    stdout_message: Some("visible".into()),
                })
            ),
            "successful dedicated dump-screen must not get a second route ack"
        );
        assert!(
            cli_should_send_route_completion(true, Some(&dump_failure)),
            "a dedicated dump-screen that never produced Log must still unblock the CLI"
        );
    }

    #[test]
    fn cli_completion_precedence_is_error_stdout_exit_tab_then_pane() {
        let mut result = ActionCompletionResult {
            exit_status: Some(7),
            affected_pane_id: Some(PaneId::Terminal(9)),
            affected_tab_id: Some(8),
            error_message: Some("error".to_string()),
            stdout_message: Some("stdout".to_string()),
        };

        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::LogError { lines } if lines == vec!["error".to_string()]
        ));
        result.error_message = None;
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines } if lines == vec!["stdout".to_string()]
        ));
        result.stdout_message = None;
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Exit {
                exit_reason: ExitReason::CustomExitStatus(7)
            }
        ));
        result.exit_status = None;
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines } if lines == vec!["8".to_string()]
        ));
        result.affected_tab_id = None;
        assert!(matches!(
            cli_action_completion_message(Some(&result)),
            ServerToClientMsg::Log { lines } if lines == vec!["terminal_9".to_string()]
        ));
    }

    #[test]
    fn closed_action_completion_channel_is_an_explicit_failure() {
        let (tx, rx) = oneshot::channel();
        drop(tx);

        let result = wait_for_action_completion(rx, "dump-screen", true);

        assert_eq!(result.exit_status, Some(1));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("closed before acknowledgement"))
        );
    }

    #[test]
    fn immediate_action_completion_is_an_acknowledged_success() {
        let (tx, rx) = oneshot::channel();
        complete_action_immediately(tx);

        let result = wait_for_action_completion(rx, "CliPipe", false);

        assert_eq!(result.exit_status, None);
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn pending_notification_drop_is_an_explicit_failure() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some(PENDING_NOTIFICATION_DROPPED_ERROR)
        );
    }

    #[test]
    fn explicitly_successful_notification_is_success() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.mark_success();

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, None);
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn explicit_zero_exit_status_resolves_success() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.set_exit_status(0);

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(0));
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn unmet_blocking_exit_does_not_poison_a_later_success() {
        let (tx, rx) = oneshot::channel();
        let mut completion =
            NotificationEnd::new_with_condition(tx, UnblockCondition::OnExitSuccess);
        completion.require_explicit_resolution();
        completion.set_exit_status(7);
        assert_eq!(completion.resolution, Some(NotificationResolution::Pending));
        completion.set_exit_status(0);

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(0));
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn unmet_blocking_success_waits_for_a_later_failure() {
        let (tx, rx) = oneshot::channel();
        let mut completion =
            NotificationEnd::new_with_condition(tx, UnblockCondition::OnExitFailure);
        completion.require_explicit_resolution();
        completion.set_exit_status(0);
        assert_eq!(completion.resolution, Some(NotificationResolution::Pending));
        completion.set_exit_status(9);

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(9));
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn explicitly_failed_notification_preserves_the_exact_failure() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.mark_failure("layout commit rejected");

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some("layout commit rejected")
        );
    }

    #[test]
    fn explicit_nonzero_exit_status_preserves_the_exact_status() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.set_exit_status(7);

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(7));
        assert_eq!(result.error_message, None);
    }

    #[test]
    fn explicit_failure_cannot_be_overwritten_by_late_success() {
        let (tx, rx) = oneshot::channel();
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.mark_failure("commit activation failed");
        completion.mark_success();

        drop(completion);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some("commit activation failed")
        );
    }

    #[test]
    fn notification_clone_cannot_resolve_the_owning_channel() {
        let (tx, mut rx) = oneshot::channel();
        let mut owner = NotificationEnd::new(tx);
        owner.require_explicit_resolution();
        let mut cloned = owner.clone();

        cloned.mark_success();
        drop(cloned);
        assert!(matches!(
            rx.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        drop(owner);
        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some(PENDING_NOTIFICATION_DROPPED_ERROR)
        );
    }

    #[test]
    fn notification_drop_tolerates_a_disconnected_receiver() {
        let (tx, rx) = oneshot::channel();
        drop(rx);
        let mut completion = NotificationEnd::new(tx);
        completion.require_explicit_resolution();
        completion.mark_success();

        drop(completion);
    }

    #[test]
    fn action_completion_timeout_is_an_explicit_failure() {
        let (_tx, rx) = oneshot::channel();

        let result =
            wait_for_action_completion_with_timeout(rx, "legacy-action", Duration::from_millis(20));

        assert_eq!(result.exit_status, Some(1));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("did not acknowledge completion"))
        );
    }

    #[test]
    fn pane_placement_budget_outlives_the_screen_queue_and_dies_before_the_client_warden() {
        // A pane completion resolves on Screen placement. After a real attach
        // that placement sits behind live `PtyBytes` / `PluginBytes`, and the
        // drain reaches the second range - which the generic 1s route budget
        // reports as a timeout for a pane that already exists.
        assert!(PANE_PLACEMENT_COMPLETION_TIMEOUT > Duration::from_secs(4));
        // `VC_FRAME_ACTION_TTL_SECONDS` is the client-side warden (20s in the
        // workspace-host fixture). The route has to be the surface that fails
        // closed, and placement is not entitled to the critical budget.
        assert!(PANE_PLACEMENT_COMPLETION_TIMEOUT < Duration::from_secs(20));
        assert!(PANE_PLACEMENT_COMPLETION_TIMEOUT < CRITICAL_ACTION_COMPLETION_TIMEOUT);
    }

    #[test]
    fn completion_budgets_only_widen_the_deadline_never_the_verdict() {
        assert_eq!(CompletionBudget::Route.timeout(), ACTION_COMPLETION_TIMEOUT);
        assert_eq!(
            CompletionBudget::PanePlacement.timeout(),
            PANE_PLACEMENT_COMPLETION_TIMEOUT
        );
        assert_eq!(
            CompletionBudget::PluginLoad.timeout(),
            PLUGIN_LOAD_COMPLETION_TIMEOUT
        );
        assert_eq!(
            CompletionBudget::Critical.timeout(),
            CRITICAL_ACTION_COMPLETION_TIMEOUT
        );

        // Whatever the budget, an unanswered action is a failure - a wider
        // deadline must never turn into "assume it worked".
        let (_tx, rx) = oneshot::channel();
        let result =
            wait_for_action_completion_with_timeout(rx, "new-pane", Duration::from_millis(20));
        assert_eq!(result.exit_status, Some(1));
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|message| message.contains("did not acknowledge completion"))
        );
    }

    #[test]
    fn plugin_load_budget_covers_the_wasm_chain_and_dies_before_the_client_warden() {
        // A plugin operation crosses the Screen FIFO twice - Route -> Screen ->
        // PTY -> plugin load -> Screen placement - and between the traversals it
        // also queues behind the PTY and plugin actors. `PanePlacement` is
        // budgeted for a strictly shorter chain that contains no wasm at all, so
        // the plugin chain cannot be judged by it.
        assert!(PLUGIN_LOAD_COMPLETION_TIMEOUT > PANE_PLACEMENT_COMPLETION_TIMEOUT);
        // `CRITICAL_ACTION_COMPLETION_TIMEOUT` already documents that a cold
        // debug wasm load of exactly these plugins can exceed 8s.
        assert!(PLUGIN_LOAD_COMPLETION_TIMEOUT > Duration::from_secs(8));
        // `VC_FRAME_ACTION_TTL_SECONDS` is the client-side warden (20s in the
        // workspace-host fixture): the route stays the surface that fails
        // closed, and it is not entitled to the critical budget.
        assert!(PLUGIN_LOAD_COMPLETION_TIMEOUT < Duration::from_secs(20));
        assert!(PLUGIN_LOAD_COMPLETION_TIMEOUT < CRITICAL_ACTION_COMPLETION_TIMEOUT);
    }

    #[test]
    fn a_dropped_plugin_completion_is_a_failure_not_a_silent_success() {
        // The whole point of the wider deadline: it buys the chain time, it does
        // not buy it forgiveness. Every `log::error!` dead end on the plugin
        // chain used to drop the token under the legacy drop-as-success
        // contract and hand the client exit 0 for a plugin it never got.
        let (tx, rx) = oneshot::channel();
        drop(plugin_completion(tx));

        let result = rx.blocking_recv().expect("a dropped token still reports");
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some(PENDING_NOTIFICATION_DROPPED_ERROR)
        );
    }

    #[test]
    fn a_refused_plugin_operation_reaches_the_client_by_name() {
        let (tx, rx) = oneshot::channel();
        refuse_plugin_completion(
            plugin_completion(tx),
            "no active tab to place the plugin pane in",
        );

        let result = rx.blocking_recv().expect("a refusal still reports");
        assert_eq!(result.exit_status, Some(1));
        assert_eq!(
            result.error_message.as_deref(),
            Some("no active tab to place the plugin pane in"),
            "a plugin refusal must name the dead end, not the generic drop"
        );
    }

    #[test]
    fn critical_action_deadline_finishes_before_the_outer_new_tab_cli_timeout() {
        // `CliTriageIo::NEW_TAB_COMMAND_TIMEOUT` is 30s; critical completion must
        // stay strictly inside that outer budget so the route fails first.
        assert!(CRITICAL_ACTION_COMPLETION_TIMEOUT < Duration::from_secs(30));
        assert!(CRITICAL_ACTION_COMPLETION_TIMEOUT > Duration::from_secs(8));
    }

    #[test]
    fn test_notification_end_with_pane_and_tab_ids() {
        let (tx, rx) = oneshot::channel();
        let mut notification_end = NotificationEnd::new(tx);

        notification_end.set_affected_pane_id(PaneId::Terminal(10));
        notification_end.set_affected_tab_id(5);

        drop(notification_end);

        let result = rx.blocking_recv().unwrap();
        assert_eq!(result.affected_pane_id, Some(PaneId::Terminal(10)));
        assert_eq!(result.affected_tab_id, Some(5));
    }

    #[test]
    fn test_notification_end_clone_does_not_copy_channel() {
        let (tx, _rx) = oneshot::channel();
        let mut notification_end = NotificationEnd::new(tx);
        notification_end.set_affected_tab_id(99);

        let cloned = notification_end.clone();

        // Verify the clone has the same data
        assert_eq!(cloned.affected_tab_id, Some(99));
        // But channel should be None (as per the Clone implementation comment)
        assert!(cloned.channel.is_none());
    }
    fn configured_default_shell() -> TerminalAction {
        // A shell the session configured, arguments included: a pane that named
        // only a directory has to start this, not whatever the ambient
        // environment happens to call a shell.
        TerminalAction::RunCommand(zellij_utils::input::command::RunCommand {
            command: PathBuf::from("/bin/zsh"),
            args: vec!["-l".to_string()],
            use_terminal_title: true,
            ..Default::default()
        })
    }

    /// Every production route that spawns a pane, carrying the same request.
    ///
    /// They are five separate arms of `route_action`, so a resolution that only
    /// one of them performs is a bug the other four keep.
    fn new_pane_variants(
        command: Option<zellij_utils::input::command::RunCommandAction>,
    ) -> Vec<(&'static str, Action)> {
        vec![
            (
                "new-pane --blocking",
                Action::NewBlockingPane {
                    placement: NewPanePlacement::NoPreference { borderless: None },
                    pane_name: None,
                    command: command.clone(),
                    unblock_condition: None,
                    near_current_pane: false,
                    tab_id: None,
                },
            ),
            (
                "new-pane --floating",
                Action::NewFloatingPane {
                    command: command.clone(),
                    pane_name: None,
                    coordinates: None,
                    near_current_pane: false,
                    tab_id: None,
                },
            ),
            (
                "new-pane --in-place",
                Action::NewInPlacePane {
                    command: command.clone(),
                    pane_name: None,
                    near_current_pane: false,
                    pane_id_to_replace: None,
                    close_replaced_pane: false,
                    tab_id: None,
                },
            ),
            (
                "new-pane --stacked",
                Action::NewStackedPane {
                    command: command.clone(),
                    pane_name: None,
                    near_current_pane: false,
                    tab_id: None,
                },
            ),
            (
                "new-pane (tiled)",
                Action::NewTiledPane {
                    direction: None,
                    command,
                    pane_name: None,
                    near_current_pane: false,
                    borderless: None,
                    tab_id: None,
                },
            ),
        ]
    }

    /// What the route actually handed the PTY for one new-pane action.
    ///
    /// This drives the real `route_action`, so the answer is the spawn request a
    /// PTY thread receives - not what a resolution helper returns when called
    /// directly. The stand-in PTY releases the completion the route is parked
    /// on, the way the live one does once the pane is on its way.
    fn spawned_terminal_action(
        action: Action,
        default_shell: Option<TerminalAction>,
    ) -> Option<TerminalAction> {
        let (screen_tx, _screen_rx) = zellij_utils::channels::unbounded();
        let (plugin_tx, _plugin_rx) = zellij_utils::channels::unbounded();
        let (pty_tx, pty_rx) = zellij_utils::channels::unbounded();
        let senders = list_clients_test_senders(screen_tx, plugin_tx, pty_tx);

        let pty = thread::spawn(move || {
            while let Ok((instruction, _)) = pty_rx.recv() {
                match instruction {
                    PtyInstruction::SpawnTerminal(
                        terminal_action,
                        _,
                        _,
                        _,
                        _,
                        notification_end,
                        _,
                    )
                    | PtyInstruction::SpawnInPlaceTerminal(
                        terminal_action,
                        _,
                        _,
                        _,
                        notification_end,
                    ) => {
                        drop(notification_end);
                        return terminal_action;
                    },
                    _ => {},
                }
            }
            None
        });

        let completion = route_action(RouteActionParams {
            action,
            caller: "cli",
            client_id: 3,
            cli_client_id: Some(11),
            pane_id: None,
            senders,
            default_shell,
            seen_cli_pipes: None,
            default_mode: InputMode::Normal,
        })
        .unwrap()
        .1
        .unwrap();
        assert_eq!(
            completion.error_message, None,
            "a routed pane still has to acknowledge completion"
        );
        pty.join().unwrap()
    }

    fn spawned_run_command(
        action: Action,
        default_shell: Option<TerminalAction>,
    ) -> zellij_utils::input::command::RunCommand {
        match spawned_terminal_action(action, default_shell) {
            Some(TerminalAction::RunCommand(run_command)) => run_command,
            other => panic!("expected a command to run, got {other:?}"),
        }
    }

    #[test]
    fn every_new_pane_variant_starts_the_configured_shell_in_the_requested_cwd() {
        use zellij_utils::input::command::RunCommandAction;

        for (variant, action) in new_pane_variants(Some(RunCommandAction::cwd_only(PathBuf::from(
            "/tmp/pane-beta",
        )))) {
            let spawned = spawned_run_command(action, Some(configured_default_shell()));
            assert_eq!(
                spawned.command,
                PathBuf::from("/bin/zsh"),
                "{variant} must start the shell the session configured, not the ambient one"
            );
            assert_eq!(
                spawned.args,
                vec!["-l".to_string()],
                "{variant} must keep the configured shell's own arguments"
            );
            assert_eq!(
                spawned.cwd,
                Some(PathBuf::from("/tmp/pane-beta")),
                "{variant} must start in the directory the caller named"
            );
        }
    }

    #[test]
    fn an_explicit_command_reaches_every_new_pane_variant_unchanged() {
        use zellij_utils::input::command::RunCommandAction;

        for (variant, action) in new_pane_variants(Some(RunCommandAction {
            command: PathBuf::from("htop"),
            args: vec!["--tree".to_string()],
            cwd: Some(PathBuf::from("/tmp/pane-beta")),
            ..Default::default()
        })) {
            let spawned = spawned_run_command(action, Some(configured_default_shell()));
            assert_eq!(
                spawned.command,
                PathBuf::from("htop"),
                "{variant} must run the command the caller named, not the default shell"
            );
            assert_eq!(
                spawned.args,
                vec!["--tree".to_string()],
                "{variant} must pass the command's own arguments through"
            );
            assert_eq!(
                spawned.cwd,
                Some(PathBuf::from("/tmp/pane-beta")),
                "{variant} must run that command in the directory the caller named"
            );
        }
    }

    #[test]
    fn a_new_pane_that_named_no_cwd_still_gets_the_bare_configured_shell() {
        for (variant, action) in new_pane_variants(None) {
            let spawned = spawned_run_command(action, Some(configured_default_shell()));
            assert_eq!(
                spawned.command,
                PathBuf::from("/bin/zsh"),
                "{variant} must still start the configured shell"
            );
            assert_eq!(
                spawned.args,
                vec!["-l".to_string()],
                "{variant} must still carry the configured shell's arguments"
            );
            assert_eq!(
                spawned.cwd, None,
                "{variant} named no directory, so the PTY still fills it from the pane the caller was looking at"
            );
        }
    }
}
