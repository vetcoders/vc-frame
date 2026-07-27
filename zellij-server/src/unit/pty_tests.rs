use super::*;
use crate::os_input_output::{NullAsyncReader, ServerOsApi, resolve_reserved_terminal_spawn};
use crate::plugins::PluginInstruction;
use crate::thread_bus::{Bus, ThreadSenders};
use interprocess::local_socket::Stream as LocalSocketStream;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use zellij_utils::channels::{self, SenderWithContext};
use zellij_utils::data::{Event, OriginatingPlugin, Palette};
use zellij_utils::errors::ErrorContext;
use zellij_utils::input::command::RunCommand;
use zellij_utils::ipc::{ClientToServerMsg, IpcReceiverWithContext, ServerToClientMsg};

#[derive(Clone)]
struct MockOsApi {
    cwds: Arc<Mutex<HashMap<u32, PathBuf>>>,
    cmds: Arc<Mutex<HashMap<u32, Vec<String>>>>,
    cmds_by_ppid: Arc<Mutex<HashMap<String, Vec<String>>>>,
    fail_spawn_terminal: Arc<AtomicBool>,
    fail_on_spawn_call: Arc<AtomicUsize>,
    command_not_found_on_spawn_call: Arc<AtomicUsize>,
    command_not_found_payload_terminal_id: Arc<AtomicUsize>,
    spawn_terminal_calls: Arc<AtomicUsize>,
    spawned_child_pid: Arc<AtomicUsize>,
    next_terminal_id: Arc<AtomicUsize>,
    reserved_terminal_ids: Arc<Mutex<Vec<u32>>>,
    activated_terminal_ids: Arc<Mutex<Vec<u32>>>,
    cleared_terminal_ids: Arc<Mutex<Vec<u32>>>,
    fail_clear_terminal_ids: Arc<Mutex<Vec<u32>>>,
    killed_child_pids: Arc<Mutex<Vec<u32>>>,
    fail_kill_child_pids: Arc<Mutex<Vec<u32>>>,
    unconfirmed_exit_child_pids: Arc<Mutex<Vec<u32>>>,
    gone_child_pids: Arc<Mutex<Vec<u32>>>,
    quit_callbacks: Arc<Mutex<Vec<QuitCallback>>>,
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct OneByteAsyncReader;

#[async_trait::async_trait]
impl AsyncReader for OneByteAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, io::Error> {
        buf[0] = b'x';
        Ok(1)
    }
}

struct FailingAsyncReader;

#[async_trait::async_trait]
impl AsyncReader for FailingAsyncReader {
    async fn read(&mut self, _buf: &mut [u8]) -> std::result::Result<usize, io::Error> {
        Err(io::Error::other("injected terminal reader failure"))
    }
}

impl MockOsApi {
    fn new() -> Self {
        MockOsApi {
            cwds: Arc::new(Mutex::new(HashMap::new())),
            cmds: Arc::new(Mutex::new(HashMap::new())),
            cmds_by_ppid: Arc::new(Mutex::new(HashMap::new())),
            fail_spawn_terminal: Arc::new(AtomicBool::new(false)),
            fail_on_spawn_call: Arc::new(AtomicUsize::new(0)),
            command_not_found_on_spawn_call: Arc::new(AtomicUsize::new(0)),
            command_not_found_payload_terminal_id: Arc::new(AtomicUsize::new(0)),
            spawn_terminal_calls: Arc::new(AtomicUsize::new(0)),
            spawned_child_pid: Arc::new(AtomicUsize::new(0)),
            next_terminal_id: Arc::new(AtomicUsize::new(100)),
            reserved_terminal_ids: Arc::new(Mutex::new(vec![])),
            activated_terminal_ids: Arc::new(Mutex::new(vec![])),
            cleared_terminal_ids: Arc::new(Mutex::new(vec![])),
            fail_clear_terminal_ids: Arc::new(Mutex::new(vec![])),
            killed_child_pids: Arc::new(Mutex::new(vec![])),
            fail_kill_child_pids: Arc::new(Mutex::new(vec![])),
            unconfirmed_exit_child_pids: Arc::new(Mutex::new(vec![])),
            gone_child_pids: Arc::new(Mutex::new(vec![])),
            quit_callbacks: Arc::new(Mutex::new(vec![])),
        }
    }
    fn fail_spawn_terminal(&self) {
        self.fail_spawn_terminal.store(true, Ordering::Relaxed);
    }
    fn fail_on_spawn_call(&self, call: usize) {
        self.fail_on_spawn_call.store(call, Ordering::Relaxed);
    }
    fn command_not_found_on_spawn_call(&self, call: usize) {
        self.command_not_found_on_spawn_call
            .store(call, Ordering::Relaxed);
    }
    fn mismatched_command_not_found_on_spawn_call(&self, call: usize, foreign_terminal_id: u32) {
        self.command_not_found_on_spawn_call(call);
        self.command_not_found_payload_terminal_id
            .store(foreign_terminal_id as usize, Ordering::Relaxed);
    }
    fn fail_clear_terminal_id(&self, terminal_id: u32) {
        lock_recover(&self.fail_clear_terminal_ids).push(terminal_id);
    }
    fn allow_clear_terminal_id(&self, terminal_id: u32) {
        lock_recover(&self.fail_clear_terminal_ids).retain(|id| *id != terminal_id);
    }
    fn return_child_pid_on_spawn(&self, child_pid: u32) {
        self.spawned_child_pid
            .store(child_pid as usize, Ordering::Relaxed);
    }
    fn fail_kill(&self, child_pid: u32) {
        lock_recover(&self.fail_kill_child_pids).push(child_pid);
    }
    fn report_child_already_gone(&self, child_pid: u32) {
        lock_recover(&self.gone_child_pids).push(child_pid);
    }
    fn delay_exit_confirmation(&self, child_pid: u32) {
        lock_recover(&self.unconfirmed_exit_child_pids).push(child_pid);
    }
    fn confirm_exit(&self, child_pid: u32) {
        lock_recover(&self.unconfirmed_exit_child_pids).retain(|pid| *pid != child_pid);
        lock_recover(&self.gone_child_pids).push(child_pid);
    }
    fn allow_kill(&self, child_pid: u32) {
        lock_recover(&self.fail_kill_child_pids).retain(|pid| *pid != child_pid);
    }
    fn killed_child_pids(&self) -> Vec<u32> {
        lock_recover(&self.killed_child_pids).clone()
    }
    fn record_cleared_terminal_id(&self, terminal_id: u32) {
        lock_recover(&self.cleared_terminal_ids).push(terminal_id);
    }
    fn cleared_terminal_ids(&self) -> Vec<u32> {
        lock_recover(&self.cleared_terminal_ids).clone()
    }
    fn reserved_terminal_ids(&self) -> Vec<u32> {
        lock_recover(&self.reserved_terminal_ids).clone()
    }
    fn activated_terminal_ids(&self) -> Vec<u32> {
        lock_recover(&self.activated_terminal_ids).clone()
    }
    fn fire_next_quit_callback(
        &self,
        pane_id: PaneId,
        exit_status: Option<i32>,
        command: RunCommand,
    ) {
        let callback = lock_recover(&self.quit_callbacks)
            .pop()
            .expect("a successful spawn must retain its quit callback");
        callback(pane_id, exit_status, command);
    }
    fn set_cwd(&self, pid: u32, path: PathBuf) {
        self.cwds.lock().unwrap().insert(pid, path);
    }
    fn set_cmd(&self, pid: u32, cmd: Vec<String>) {
        self.cmds.lock().unwrap().insert(pid, cmd);
    }
    fn set_foreground_cmd(&self, ppid: u32, cmd: Vec<String>) {
        self.cmds_by_ppid
            .lock()
            .unwrap()
            .insert(ppid.to_string(), cmd);
    }
    fn clear_foreground_cmd(&self, ppid: u32) {
        self.cmds_by_ppid.lock().unwrap().remove(&ppid.to_string());
    }
}

impl ServerOsApi for MockOsApi {
    fn set_terminal_size_using_terminal_id(
        &self,
        _: u32,
        _: u16,
        _: u16,
        _: Option<u16>,
        _: Option<u16>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
    fn spawn_terminal(
        &self,
        _: TerminalAction,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        _: Option<PathBuf>,
    ) -> anyhow::Result<(u32, Box<dyn AsyncReader>, Option<u32>)> {
        let call = self.spawn_terminal_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let terminal_id = self.next_terminal_id.fetch_add(1, Ordering::Relaxed) as u32;
        let spawn_result: anyhow::Result<(u32, Box<dyn AsyncReader>, Option<u32>)> =
            if self.fail_spawn_terminal.load(Ordering::Relaxed)
                || self.fail_on_spawn_call.load(Ordering::Relaxed) == call
            {
                Err(anyhow::Error::new(io::Error::other(
                    "injected EMFILE-like spawn failure",
                )))
            } else if self.command_not_found_on_spawn_call.load(Ordering::Relaxed) == call {
                let payload_terminal_id = self
                    .command_not_found_payload_terminal_id
                    .load(Ordering::Relaxed);
                let payload_terminal_id = if payload_terminal_id == 0 {
                    terminal_id
                } else {
                    payload_terminal_id as u32
                };
                Err(anyhow::Error::new(ZellijError::CommandNotFound {
                    terminal_id: payload_terminal_id,
                    command: "injected-missing-command".to_owned(),
                }))
            } else {
                lock_recover(&self.quit_callbacks).push(quit_cb);
                Ok((terminal_id, Box::new(NullAsyncReader), None))
            };
        resolve_reserved_terminal_spawn(terminal_id, spawn_result, |terminal_id| {
            self.record_cleared_terminal_id(terminal_id);
        })
    }
    fn spawn_terminal_with_reserved_id(
        &self,
        terminal_id: u32,
        _: TerminalAction,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        _: Option<PathBuf>,
    ) -> anyhow::Result<(Box<dyn AsyncReader>, Option<u32>)> {
        if !lock_recover(&self.reserved_terminal_ids).contains(&terminal_id) {
            return Err(anyhow!(
                "terminal {terminal_id} was not reserved before exact activation"
            ));
        }
        let call = self.spawn_terminal_calls.fetch_add(1, Ordering::Relaxed) + 1;
        let spawn_result: anyhow::Result<(Box<dyn AsyncReader>, Option<u32>)> =
            if self.fail_spawn_terminal.load(Ordering::Relaxed)
                || self.fail_on_spawn_call.load(Ordering::Relaxed) == call
            {
                Err(anyhow::Error::new(io::Error::other(
                    "injected EMFILE-like spawn failure",
                )))
            } else if self.command_not_found_on_spawn_call.load(Ordering::Relaxed) == call {
                let payload_terminal_id = self
                    .command_not_found_payload_terminal_id
                    .load(Ordering::Relaxed);
                let payload_terminal_id = if payload_terminal_id == 0 {
                    terminal_id
                } else {
                    payload_terminal_id as u32
                };
                Err(anyhow::Error::new(ZellijError::CommandNotFound {
                    terminal_id: payload_terminal_id,
                    command: "injected-missing-command".to_owned(),
                }))
            } else {
                lock_recover(&self.quit_callbacks).push(quit_cb);
                lock_recover(&self.activated_terminal_ids).push(terminal_id);
                let child_pid = self.spawned_child_pid.load(Ordering::Relaxed);
                Ok((
                    Box::new(NullAsyncReader),
                    (child_pid != 0).then_some(child_pid as u32),
                ))
            };
        resolve_reserved_terminal_spawn(terminal_id, spawn_result, |_| {})
    }
    fn reserve_terminal_id(&self) -> anyhow::Result<u32> {
        let terminal_id = self.next_terminal_id.fetch_add(1, Ordering::Relaxed) as u32;
        lock_recover(&self.reserved_terminal_ids).push(terminal_id);
        Ok(terminal_id)
    }
    fn write_to_tty_stdin(&self, _: u32, buf: &[u8]) -> anyhow::Result<usize> {
        Ok(buf.len())
    }
    fn tcdrain(&self, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn kill(&self, child_pid: u32) -> anyhow::Result<()> {
        lock_recover(&self.killed_child_pids).push(child_pid);
        if lock_recover(&self.gone_child_pids).contains(&child_pid) {
            Err(anyhow::Error::new(io::Error::new(
                io::ErrorKind::NotFound,
                format!("child {child_pid} already exited"),
            )))
        } else if lock_recover(&self.fail_kill_child_pids).contains(&child_pid) {
            Err(anyhow::Error::new(io::Error::other(format!(
                "injected kill failure for child {child_pid}"
            ))))
        } else if lock_recover(&self.unconfirmed_exit_child_pids).contains(&child_pid) {
            Err(anyhow::Error::new(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("child {child_pid} accepted termination but exit/reap remains unconfirmed"),
            )))
        } else {
            Ok(())
        }
    }
    fn force_kill(&self, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn send_sigint(&self, _: u32) -> anyhow::Result<()> {
        Ok(())
    }
    fn box_clone(&self) -> Box<dyn ServerOsApi> {
        Box::new((*self).clone())
    }
    fn send_to_client(&self, _: ClientId, _: ServerToClientMsg) -> anyhow::Result<()> {
        Ok(())
    }
    fn new_client(
        &mut self,
        _: ClientId,
        _: LocalSocketStream,
    ) -> anyhow::Result<IpcReceiverWithContext<ClientToServerMsg>> {
        unimplemented!()
    }
    fn new_client_with_reply(
        &mut self,
        _: ClientId,
        _: LocalSocketStream,
        _: LocalSocketStream,
    ) -> anyhow::Result<IpcReceiverWithContext<ClientToServerMsg>> {
        unimplemented!()
    }
    fn remove_client(&mut self, _: ClientId) -> anyhow::Result<()> {
        Ok(())
    }
    fn load_palette(&self) -> Palette {
        Palette::default()
    }
    fn get_cwd(&self, pid: u32) -> Option<PathBuf> {
        self.cwds.lock().unwrap().get(&pid).cloned()
    }
    fn get_cwds(&self, pids: Vec<u32>) -> (HashMap<u32, PathBuf>, HashMap<u32, Vec<String>>) {
        let cwds_lock = self.cwds.lock().unwrap();
        let cmds_lock = self.cmds.lock().unwrap();
        let cwds = pids
            .iter()
            .filter_map(|pid| cwds_lock.get(pid).map(|cwd| (*pid, cwd.clone())))
            .collect();
        let cmds = pids
            .iter()
            .filter_map(|pid| cmds_lock.get(pid).map(|cmd| (*pid, cmd.clone())))
            .collect();
        (cwds, cmds)
    }
    fn get_all_cmds_by_ppid(&self, _: &Option<String>) -> HashMap<String, Vec<String>> {
        self.cmds_by_ppid.lock().unwrap().clone()
    }
    fn write_to_file(&mut self, _: String, _: Option<String>) -> anyhow::Result<()> {
        Ok(())
    }
    fn re_run_command_in_terminal(
        &self,
        _: u32,
        _: RunCommand,
        _: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    ) -> anyhow::Result<(Box<dyn AsyncReader>, Option<u32>)> {
        unimplemented!()
    }
    fn clear_terminal_id(&self, terminal_id: u32) -> anyhow::Result<()> {
        self.record_cleared_terminal_id(terminal_id);
        if lock_recover(&self.fail_clear_terminal_ids).contains(&terminal_id) {
            Err(anyhow!("injected clear failure for terminal {terminal_id}"))
        } else {
            lock_recover(&self.reserved_terminal_ids).retain(|id| *id != terminal_id);
            lock_recover(&self.activated_terminal_ids).retain(|id| *id != terminal_id);
            Ok(())
        }
    }
}

fn make_pty_with_plugin_receiver(
    mock: MockOsApi,
) -> (Pty, channels::Receiver<(PluginInstruction, ErrorContext)>) {
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let plugin_sender = SenderWithContext::new(plugin_tx);
    let mut bus: Bus<PtyInstruction> = Bus::empty().should_silently_fail();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(plugin_sender);
    let pty = Pty::new(bus, false, None, None);
    (pty, plugin_rx)
}

fn set_active_terminal(pty: &mut Pty, terminal_id: u32, child_pid: u32) {
    let flag = Arc::new(AtomicBool::new(true));
    pty.id_to_child_pid.insert(terminal_id, child_pid);
    pty.pane_activity_flags.insert(terminal_id, flag);
}

fn collect_cwd_changed_events(
    rx: &channels::Receiver<(PluginInstruction, ErrorContext)>,
) -> Vec<(PaneId, PathBuf)> {
    let mut events = Vec::new();
    while let Ok((instruction, _)) = rx.try_recv() {
        if let PluginInstruction::Update(updates) = instruction {
            for (_, _, event) in updates {
                if let Event::CwdChanged(pane_id, cwd, _) = event {
                    events.push((pane_id.into(), cwd));
                }
            }
        }
    }
    events
}

fn collect_command_changed_events(
    rx: &channels::Receiver<(PluginInstruction, ErrorContext)>,
) -> Vec<(PaneId, Vec<String>, bool)> {
    let mut events = Vec::new();
    while let Ok((instruction, _)) = rx.try_recv() {
        if let PluginInstruction::Update(updates) = instruction {
            for (_, _, event) in updates {
                if let Event::CommandChanged(pane_id, cmd, is_foreground, _) = event {
                    events.push((pane_id.into(), cmd, is_foreground));
                }
            }
        }
    }
    events
}

#[test]
fn new_tab_spawn_failure_does_not_terminate_pty_thread() {
    let mock = MockOsApi::new();
    mock.fail_spawn_terminal();
    let probe = mock.clone();
    let (pty_tx, pty_rx) = channels::unbounded();
    let pty_sender = SenderWithContext::new(pty_tx);
    let (screen_tx, _screen_rx) = channels::unbounded();
    let screen_sender = SenderWithContext::new(screen_tx);
    let bus = Bus::new(
        vec![pty_rx],
        ThreadSenders {
            to_screen: Some(screen_sender),
            should_silently_fail: true,
            ..Default::default()
        },
        Some(Box::new(mock)),
    );
    let pty = Pty::new(bus, false, None, None);

    pty_sender
        .send(PtyInstruction::NewTab(
            None,
            None,
            Box::new(Some(TiledPaneLayout::default())),
            vec![],
            0,
            1,
            HashMap::new(),
            None,
            false,
            true,
            (0, false),
            None,
            None,
        ))
        .unwrap();
    let (ack_tx, ack_rx) = channels::bounded(1);
    pty_sender
        .send(PtyInstruction::LayoutCommitResolved {
            transaction_id: 1,
            outcome: LayoutCommitOutcome::Committed,
            ack: ack_tx,
        })
        .unwrap();
    pty_sender.send(PtyInstruction::Exit).unwrap();

    let result = pty_thread_main(pty, Box::<Layout>::default());

    assert!(
        result.is_ok(),
        "new-tab spawn failures such as EMFILE must be logged and keep the pty thread alive"
    );
    let ack = ack_rx
        .recv()
        .expect("PTY must acknowledge the failed activation");
    assert!(matches!(
        ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("injected EMFILE-like spawn failure")
    ));
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
}

#[test]
fn pty_channel_disconnect_rolls_back_and_rejects_every_pending_layout() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (pty_tx, pty_rx) = channels::unbounded();
    let pty_sender = SenderWithContext::new(pty_tx);
    let (screen_tx, screen_rx) = channels::unbounded();
    let bus = Bus::new(
        vec![pty_rx],
        ThreadSenders {
            to_screen: Some(SenderWithContext::new(screen_tx)),
            should_silently_fail: false,
            ..Default::default()
        },
        Some(Box::new(mock)),
    );
    let pty = Pty::new(bus, false, None, None);
    let plugin = RunPluginOrAlias::from_url("file:/pending-exit.wasm", &None, None, None).unwrap();
    let layout_generation = DurableTabLayoutGeneration {
        tab_id: 7,
        tab_name: "pending-exit".to_owned(),
        tab_instance_id: "0123456789abcdef0123456789abcdef".to_owned(),
        generation: 9,
        viewer_creation_fence: None,
    };

    pty_sender
        .send(PtyInstruction::NewTab(
            None,
            None,
            Box::new(Some(TiledPaneLayout::default())),
            vec![],
            7,
            61,
            HashMap::from([(plugin, vec![77])]),
            None,
            false,
            true,
            (0, false),
            None,
            Some(Box::new(layout_generation.clone())),
        ))
        .unwrap();
    drop(pty_sender);

    let result = pty_thread_main(pty, Box::<Layout>::default());
    assert!(
        result.is_ok(),
        "a disconnected producer must close PTY cleanly instead of panicking"
    );
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100],
        "the unresolved terminal ledger must be released exactly once"
    );
    let instructions = screen_rx
        .try_iter()
        .map(|(instruction, _)| instruction)
        .collect::<Vec<_>>();
    assert_eq!(instructions.len(), 2);
    assert!(matches!(
        instructions.first(),
        Some(ScreenInstruction::ApplyLayout(
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
            _,
            61
        ))
    ));
    assert!(matches!(
        instructions.get(1),
        Some(ScreenInstruction::LayoutPreparationFailed {
            transaction_id: 61,
            tab_id: Some(7),
            layout_generation: reported_generation,
            cleanup:
                LayoutPreparationCleanup::ReleasePluginReservation {
                    plugin_ids,
                    pty_cleanup_succeeded: true,
                },
            ..
        }) if plugin_ids == &vec![77]
            && reported_generation.as_deref() == Some(&layout_generation)
    ));
}

#[test]
fn preparation_clear_failure_retains_debt_and_reports_exact_followup_receipt() {
    let mock = MockOsApi::new();
    mock.fail_clear_terminal_id(100);
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let generation = DurableTabLayoutGeneration {
        tab_id: 7,
        tab_name: "preparation-debt".to_owned(),
        tab_instance_id: "fedcba9876543210fedcba9876543210".to_owned(),
        generation: 3,
        viewer_creation_fence: None,
    };

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout::default(),
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        66,
        false,
        true,
        (1, false),
        None,
        Some(Box::new(generation.clone())),
    )
    .expect("the prepared layout must reach Screen");
    let rollback = pty.reject_pending_layout_send(66, anyhow!("injected Screen handoff failure"));
    pty.reject_layout_preparation(
        66,
        Some(7),
        None,
        Some(Box::new(generation.clone())),
        vec![801, 802],
        rollback,
    );

    assert!(pty.pending_layout_cleanups.contains_key(&66));
    assert_eq!(probe.reserved_terminal_ids(), vec![100]);
    probe.allow_clear_terminal_id(100);
    pty.retry_preparation_cleanup_debts();
    assert!(!pty.pending_layout_cleanups.contains_key(&66));
    assert!(probe.reserved_terminal_ids().is_empty());

    let instructions = screen_rx
        .try_iter()
        .map(|(instruction, _)| instruction)
        .collect::<Vec<_>>();
    let cleanup_receipts = instructions
        .iter()
        .filter_map(|instruction| match instruction {
            ScreenInstruction::LayoutPreparationFailed {
                transaction_id: 66,
                layout_generation,
                cleanup:
                    LayoutPreparationCleanup::ReleasePluginReservation {
                        plugin_ids,
                        pty_cleanup_succeeded,
                    },
                ..
            } => Some((
                layout_generation.as_deref(),
                plugin_ids.as_slice(),
                *pty_cleanup_succeeded,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(cleanup_receipts.len(), 2);
    assert_eq!(cleanup_receipts[0].0, Some(&generation));
    assert_eq!(cleanup_receipts[0].1, &[801, 802]);
    assert!(!cleanup_receipts[0].2);
    assert_eq!(cleanup_receipts[1].0, Some(&generation));
    assert_eq!(cleanup_receipts[1].1, &[801, 802]);
    assert!(cleanup_receipts[1].2);
}

#[test]
fn partial_new_tab_activation_failure_releases_every_reserved_terminal() {
    let mock = MockOsApi::new();
    mock.fail_on_spawn_call(2);
    let probe = mock.clone();
    let (mut pty, plugin_rx) = make_pty_with_plugin_receiver(mock);
    let (screen_tx, screen_rx) = channels::unbounded();
    pty.bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    let plugin =
        RunPluginOrAlias::from_url("file:/partial-new-tab.wasm", &None, None, None).unwrap();
    let plugin_ids = HashMap::from([(plugin, vec![77])]);
    let layout = TiledPaneLayout {
        children: vec![TiledPaneLayout::default(), TiledPaneLayout::default()],
        ..Default::default()
    };
    let default_shell = TerminalAction::RunCommand(RunCommand {
        command: PathBuf::from("sh"),
        ..Default::default()
    });

    pty.spawn_terminals_for_layout(
        None,
        layout,
        vec![],
        Some(default_shell),
        plugin_ids,
        None,
        7,
        1,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("prepare must reserve IDs without spawning");
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert_eq!(probe.reserved_terminal_ids(), vec![100, 101]);
    assert!(probe.activated_terminal_ids().is_empty());
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("the second exact activation must fail");
    assert!(format!("{error:#}").contains("injected EMFILE-like spawn failure"));
    assert!(matches!(
        ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("injected EMFILE-like spawn failure")
    ));
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100, 101],
        "the active first terminal and failed second reservation must be cleared in ID order"
    );
    assert!(probe.reserved_terminal_ids().is_empty());
    assert!(probe.activated_terminal_ids().is_empty());
    assert!(
        unloaded_plugin_ids(&plugin_rx).is_empty(),
        "PTY must not compensate Plugin-owned reservations"
    );
}

#[test]
fn floating_nth_activation_failure_releases_every_prior_terminal_exactly_once() {
    let mock = MockOsApi::new();
    mock.fail_on_spawn_call(3);
    let probe = mock.clone();
    let (mut pty, plugin_rx) = make_pty_with_plugin_receiver(mock);
    let plugin =
        RunPluginOrAlias::from_url("file:/floating-failure.wasm", &None, None, None).unwrap();

    let (screen_tx, screen_rx) = channels::unbounded();
    pty.bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout::default(),
        vec![FloatingPaneLayout::default(), FloatingPaneLayout::default()],
        Some(TerminalAction::RunCommand(RunCommand {
            command: PathBuf::from("sh"),
            ..Default::default()
        })),
        HashMap::from([(plugin, vec![77])]),
        None,
        7,
        2,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("all three terminal IDs must be reserved without spawning");
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("the third exact activation must fail");
    assert!(format!("{error:#}").contains("injected EMFILE-like spawn failure"));
    assert!(matches!(ack, Ok(LayoutCommitAck::ActivationRolledBack(_))));
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100, 101, 102],
        "the ledger rolls every reserved ID back exactly once in stable order"
    );
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

#[test]
fn new_tab_apply_layout_failure_releases_terminals_and_plugins() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let plugin_sender = SenderWithContext::new(plugin_tx);
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(plugin_sender);
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let plugin =
        RunPluginOrAlias::from_url("file:/rejected-new-tab.wasm", &None, None, None).unwrap();
    let plugin_ids = HashMap::from([(plugin, vec![77])]);
    let default_shell = TerminalAction::RunCommand(RunCommand {
        command: PathBuf::from("sh"),
        ..Default::default()
    });

    let error = pty
        .spawn_terminals_for_layout(
            None,
            TiledPaneLayout::default(),
            vec![],
            Some(default_shell),
            plugin_ids,
            None,
            7,
            3,
            false,
            true,
            (1, false),
            None,
            None,
        )
        .expect_err("a missing screen receiver must reject ApplyLayout");

    assert!(
        format!("{:#}", error).contains("failed to get screen sender"),
        "the ApplyLayout delivery failure must remain in the error chain"
    );
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100],
        "the terminal reserved before ApplyLayout must not remain reserved"
    );
    assert_eq!(
        probe.spawn_terminal_calls.load(Ordering::Relaxed),
        0,
        "a failed Screen handoff must never activate a process"
    );
    assert!(
        unloaded_plugin_ids(&plugin_rx).is_empty(),
        "Plugin owns release of its own transaction reservation"
    );
}

#[test]
fn command_not_found_with_hold_transitions_only_after_commit() {
    let mock = MockOsApi::new();
    mock.command_not_found_on_spawn_call(1);
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let plugin =
        RunPluginOrAlias::from_url("file:/transferred-new-tab.wasm", &None, None, None).unwrap();
    let plugin_ids = HashMap::from([(plugin, vec![77])]);
    let layout = TiledPaneLayout {
        run: Some(Run::Command(RunCommand {
            command: PathBuf::from("missing-command"),
            hold_on_close: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    pty.spawn_terminals_for_layout(
        None,
        layout,
        vec![],
        None,
        plugin_ids,
        None,
        7,
        4,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("optional notification failure must not revoke a transferred ApplyLayout");

    let (screen_instruction, _) = screen_rx
        .try_recv()
        .expect("ApplyLayout must transfer ownership before the optional notification");
    let transaction_id = match screen_instruction {
        ScreenInstruction::ApplyLayout(
            _,
            _,
            terminal_ids,
            _,
            plugin_ids,
            _,
            _,
            _,
            _,
            _,
            _,
            transaction_id,
        ) => {
            assert_eq!(
                terminal_ids.len(),
                1,
                "one reserved terminal enters the prepared layout"
            );
            assert_eq!(terminal_ids[0].0, 100);
            assert!(
                terminal_ids[0].1.is_none(),
                "command existence is not probed by spawning during preparation"
            );
            assert_eq!(
                plugin_ids.values().flatten().copied().collect::<Vec<_>>(),
                vec![77]
            );
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    assert!(
        screen_rx.try_recv().is_err(),
        "no held-terminal notification may run before Screen commits ownership"
    );
    assert!(
        probe.cleared_terminal_ids().is_empty(),
        "pending terminal must not be cleared before the Screen resolution"
    );
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert!(
        !plugin_rx
            .try_iter()
            .any(|(instruction, _)| matches!(instruction, PluginInstruction::Unload(77))),
        "pending plugin must not be rolled back before the Screen resolution"
    );
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    resolution.expect("the Screen commit must transition the missing command to held");
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));
    assert!(matches!(
        screen_rx.try_recv(),
        Ok((ScreenInstruction::PtyBytes(100, _), _))
    ));
    assert!(matches!(
        screen_rx.try_recv(),
        Ok((
            ScreenInstruction::HoldPane(PaneId::Terminal(100), Some(2), _),
            _
        ))
    ));
    assert!(probe.activated_terminal_ids().is_empty());
    let (replayed_resolution, replayed_ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    replayed_resolution.expect("a lost ACK retry must replay the committed receipt");
    assert_eq!(replayed_ack, Ok(LayoutCommitAck::Resolved));
}

#[test]
fn fast_exit_callback_waits_for_screen_commit_at_the_common_pty_fence() {
    let mock = MockOsApi::new();
    let callback_probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let command = RunCommand {
        command: PathBuf::from("instant-exit"),
        ..Default::default()
    };

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(command.clone())),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        5,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the fast process must reach the Screen ownership fence");

    let (instruction, _) = screen_rx.try_recv().expect("ApplyLayout");
    let transaction_id = match instruction {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    assert!(
        lock_recover(&callback_probe.quit_callbacks).is_empty(),
        "no child and therefore no exit callback may exist before commit"
    );

    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    resolution.expect("Screen commit must activate the process and release its callback fence");
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));
    callback_probe.fire_next_quit_callback(PaneId::Terminal(100), Some(0), command);
    assert!(matches!(
        screen_rx.try_recv(),
        Ok((ScreenInstruction::ClosePane(PaneId::Terminal(100), ..), _))
    ));
    let (replayed_resolution, replayed_ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    replayed_resolution.expect("a lost ACK retry must replay without reactivation");
    assert_eq!(replayed_ack, Ok(LayoutCommitAck::Resolved));
    assert_eq!(
        callback_probe.spawn_terminal_calls.load(Ordering::Relaxed),
        1,
        "receipt replay must not spawn a second process"
    );
    assert!(
        screen_rx.try_recv().is_err(),
        "a fast exit must fire exactly once"
    );
}

#[test]
fn rejected_screen_commit_spawns_nothing_and_rolls_back_terminal_reservations() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let plugin =
        RunPluginOrAlias::from_url("file:/screen-rejected.wasm", &None, None, None).unwrap();
    let command = RunCommand {
        command: PathBuf::from("instant-exit-before-rejection"),
        ..Default::default()
    };

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(command.clone())),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::from([(plugin, vec![77])]),
        None,
        7,
        6,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the prepared layout must reach Screen");
    let (instruction, _) = screen_rx.try_recv().expect("ApplyLayout");
    let transaction_id = match instruction {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    assert!(
        lock_recover(&probe.quit_callbacks).is_empty(),
        "a rejected transaction must not have an exit callback to cancel"
    );

    let rejected_outcome =
        LayoutCommitOutcome::Rejected("injected Screen apply failure".to_owned());
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, rejected_outcome.clone());
    resolution.expect_err("a rejected commit reports the Screen failure after cleanup");
    assert_eq!(
        ack,
        Ok(LayoutCommitAck::Resolved),
        "a business rejection ACK certifies that PTY cleanup completed"
    );
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
    assert!(
        screen_rx.try_recv().is_err(),
        "rollback must cancel the queued quit callback instead of creating a ghost ClosePane"
    );

    let (replayed_resolution, replayed_ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, rejected_outcome);
    replayed_resolution.expect_err("the cached business rejection remains a local diagnostic");
    assert_eq!(
        replayed_ack,
        Ok(LayoutCommitAck::Resolved),
        "retry must replay the successful cleanup receipt"
    );
    pty.resolve_layout_commit_with_ack(
        transaction_id,
        LayoutCommitOutcome::Rejected("conflicting reason".to_owned()),
    )
    .0
    .expect_err("a conflicting rejection must not reuse another outcome's receipt");
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

#[test]
fn rejected_layout_ack_reports_only_real_cleanup_failure() {
    let mock = MockOsApi::new();
    mock.fail_clear_terminal_id(100);
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout::default(),
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        60,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the layout must reach the Screen ownership fence");
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };

    let (resolution, ack) = pty.resolve_layout_commit_with_ack(
        transaction_id,
        LayoutCommitOutcome::Rejected("injected Screen failure".to_owned()),
    );
    let resolution_error = resolution.expect_err("the rejection remains a local diagnostic");
    let ack_error = ack.expect_err("failed terminal cleanup must fail the PTY ACK");
    assert!(
        ack_error.contains("injected clear failure for terminal 100"),
        "unexpected cleanup ACK: {ack_error}"
    );
    assert!(format!("{resolution_error:#}").contains("injected clear failure for terminal 100"));
    assert!(
        pty.pending_layout_cleanups.contains_key(&transaction_id),
        "failed cleanup must retain an exact transaction-owned retry ledger"
    );
    assert!(
        !pty.resolved_layout_commits.contains_key(&transaction_id),
        "an unresolved cleanup must never be cached as a final receipt"
    );
    assert_eq!(probe.reserved_terminal_ids(), vec![100]);

    probe.allow_clear_terminal_id(100);
    let (retried_resolution, retried_ack) = pty.resolve_layout_commit_with_ack(
        transaction_id,
        LayoutCommitOutcome::Rejected("injected Screen failure".to_owned()),
    );
    retried_resolution.expect_err("the business rejection remains the local diagnostic");
    assert_eq!(
        retried_ack,
        Ok(LayoutCommitAck::Resolved),
        "same-outcome retry must resolve only after the retained ledger is empty"
    );
    assert!(!pty.pending_layout_cleanups.contains_key(&transaction_id));
    assert!(pty.resolved_layout_commits.contains_key(&transaction_id));
    assert!(probe.reserved_terminal_ids().is_empty());
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100, 100],
        "retry must target only the exact retained terminal"
    );
}

#[test]
fn partial_activation_failure_keeps_guard_armed_and_rolls_back_every_allocation() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(RunCommand {
                command: PathBuf::from("originating-plugin-command"),
                originating_plugin: Some(OriginatingPlugin::new(77, 1, Default::default())),
                ..Default::default()
            })),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        63,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the prepared layout must reach Screen");
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    pty.id_to_child_pid.insert(100, 4242);
    pty.terminal_cwds
        .insert(100, PathBuf::from("/tmp/partial-activation"));
    pty.terminal_cmds
        .insert(100, vec!["originating-plugin-command".to_owned()]);
    pty.terminal_foreground_cmds
        .insert(100, vec!["originating-plugin-command".to_owned()]);

    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let resolution_error =
        resolution.expect_err("the missing Plugin receiver must fail full activation");
    let rollback_message = match ack {
        Ok(LayoutCommitAck::ActivationRolledBack(message)) => message,
        other => panic!("full cleanup must return explicit rolled-back ACK, got {other:?}"),
    };
    assert!(rollback_message.contains("failed to get plugin sender"));
    assert!(format!("{resolution_error:#}").contains("failed to get plugin sender"));
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100],
        "the still-armed ledger must release the already-activated terminal"
    );
    assert!(!pty.task_handles.contains_key(&100));
    assert!(!pty.pane_activity_flags.contains_key(&100));
    assert!(!pty.originating_plugins.contains_key(&100));
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.terminal_cwds.contains_key(&100));
    assert!(!pty.terminal_cmds.contains_key(&100));
    assert!(!pty.terminal_foreground_cmds.contains_key(&100));
}

#[test]
fn activation_cleanup_kill_failure_retains_exact_debt_until_same_outcome_retry() {
    let mock = MockOsApi::new();
    mock.return_child_pid_on_spawn(4242);
    mock.fail_kill(4242);
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(RunCommand {
                command: PathBuf::from("cleanup-debt-command"),
                originating_plugin: Some(OriginatingPlugin::new(77, 1, Default::default())),
                ..Default::default()
            })),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        65,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the prepared layout must reach Screen");
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    pty.terminal_cwds
        .insert(100, PathBuf::from("/tmp/cleanup-debt"));
    pty.terminal_cmds
        .insert(100, vec!["cleanup-debt-command".to_owned()]);

    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    assert!(
        format!(
            "{:#}",
            resolution.expect_err("Plugin notification must fail")
        )
        .contains("injected kill failure for child 4242")
    );
    assert!(
        ack.expect_err("failed kill must not certify activation rollback")
            .contains("injected kill failure for child 4242")
    );
    assert!(pty.pending_layout_cleanups.contains_key(&transaction_id));
    assert_eq!(pty.id_to_child_pid.get(&100), Some(&4242));
    assert!(pty.task_handles.contains_key(&100));
    assert!(pty.pane_activity_flags.contains_key(&100));
    assert!(pty.terminal_cwds.contains_key(&100));
    assert!(pty.terminal_cmds.contains_key(&100));
    assert_eq!(probe.cleared_terminal_ids(), Vec::<u32>::new());

    probe.allow_kill(4242);
    let (retried_resolution, retried_ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    retried_resolution.expect_err("activation failure remains the local diagnostic");
    assert!(matches!(
        retried_ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("failed to get plugin sender")
    ));
    assert_eq!(probe.killed_child_pids(), vec![4242, 4242]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(!pty.pending_layout_cleanups.contains_key(&transaction_id));
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.task_handles.contains_key(&100));
    assert!(!pty.pane_activity_flags.contains_key(&100));
    assert!(!pty.terminal_cwds.contains_key(&100));
    assert!(!pty.terminal_cmds.contains_key(&100));
}

#[test]
fn runtime_panic_after_live_layout_spawn_is_caught_and_strictly_rolled_back() {
    let mock = MockOsApi::new();
    mock.return_child_pid_on_spawn(4242);
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout::default(),
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        67,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("the prepared layout must reach Screen");
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    panic_next_terminal_runtime_spawn();

    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("the injected runtime panic must fail activation");
    assert!(format!("{error:#}").contains("injected terminal runtime spawn panic"));
    assert!(matches!(
        ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("injected terminal runtime spawn panic")
    ));
    assert_eq!(probe.killed_child_pids(), vec![4242]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(!pty.pending_layout_cleanups.contains_key(&transaction_id));
    assert!(!pty.pending_terminal_cleanups.contains_key(&100));
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.task_handles.contains_key(&100));
    assert!(!pty.pane_activity_flags.contains_key(&100));
}

#[test]
fn terminal_reader_and_screen_send_failures_return_exact_cleanup_to_the_pty_owner() {
    let mock = MockOsApi::new();
    let (pty_tx, pty_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_pty = Some(SenderWithContext::new(pty_tx));
    bus.senders.should_silently_fail = false;
    let pty = Pty::new(bus, false, None, None);
    for (terminal_id, reader) in [
        (100, Box::new(FailingAsyncReader) as Box<dyn AsyncReader>),
        (101, Box::new(OneByteAsyncReader) as Box<dyn AsyncReader>),
    ] {
        let terminal_bytes = pty
            .spawn_terminal_bytes_task(terminal_id, reader, Arc::new(AtomicBool::new(false)))
            .expect("the global runtime must accept the reader task");
        async_runtime()
            .block_on(terminal_bytes)
            .expect("the failed reader task must return after handing cleanup back");
    }

    let mut cleanup_ids = pty_rx
        .try_iter()
        .filter_map(|(instruction, _)| match instruction {
            PtyInstruction::ClosePane(PaneId::Terminal(terminal_id), None) => Some(terminal_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    cleanup_ids.sort_unstable();
    assert_eq!(cleanup_ids, vec![100, 101]);
}

#[test]
fn close_terminal_treats_an_already_exited_child_as_certified_cleanup() {
    let mock = MockOsApi::new();
    mock.report_child_already_gone(4242);
    let probe = mock.clone();
    let (mut pty, _plugin_rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 100, 4242);
    pty.terminal_cwds
        .insert(100, PathBuf::from("/tmp/already-gone"));

    pty.close_pane(PaneId::Terminal(100))
        .expect("an already-exited child is a successful idempotent close");

    assert_eq!(probe.killed_child_pids(), vec![4242]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.pane_activity_flags.contains_key(&100));
    assert!(!pty.terminal_cwds.contains_key(&100));
}

#[test]
fn clear_retry_does_not_rekill_an_already_confirmed_dead_child() {
    let mock = MockOsApi::new();
    mock.fail_clear_terminal_id(100);
    let probe = mock.clone();
    let (mut pty, _plugin_rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 100, 4242);

    let error = pty
        .close_pane(PaneId::Terminal(100))
        .expect_err("the injected terminal-id clear failure must remain cleanup debt");
    assert!(format!("{error:#}").contains("injected clear failure"));
    assert_eq!(probe.killed_child_pids(), vec![4242]);
    assert!(pty.terminal_kill_confirmations.contains(&100));
    assert_eq!(pty.id_to_child_pid.get(&100), Some(&4242));
    assert!(pty.pane_activity_flags.contains_key(&100));

    probe.allow_clear_terminal_id(100);
    pty.close_pane(PaneId::Terminal(100))
        .expect("retry must resume at the exact remaining clear step");

    assert_eq!(
        probe.killed_child_pids(),
        vec![4242],
        "a confirmed-dead PID must not be targeted again while clear debt is retried"
    );
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 100]);
    assert!(!pty.terminal_kill_confirmations.contains(&100));
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.pane_activity_flags.contains_key(&100));
}

#[test]
fn close_kill_failure_stays_as_debt_without_terminating_the_pty_loop() {
    let mock = MockOsApi::new();
    mock.fail_kill(4242);
    let probe = mock.clone();
    let (pty_tx, pty_rx) = channels::unbounded();
    let pty_sender = SenderWithContext::new(pty_tx);
    let bus = Bus::new(
        vec![pty_rx],
        ThreadSenders {
            should_silently_fail: false,
            ..Default::default()
        },
        Some(Box::new(mock)),
    );
    let mut pty = Pty::new(bus, false, None, None);
    set_active_terminal(&mut pty, 100, 4242);
    pty_sender
        .send(PtyInstruction::ClosePane(PaneId::Terminal(100), None))
        .unwrap();
    pty_sender.send(PtyInstruction::Exit).unwrap();

    pty_thread_main_loop(&mut pty, Box::<Layout>::default())
        .expect("strict close failure must not terminate the PTY owner");

    assert!(pty.pending_terminal_cleanups.contains_key(&100));
    assert_eq!(pty.id_to_child_pid.get(&100), Some(&4242));
    assert!(pty.pane_activity_flags.contains_key(&100));
    assert_eq!(probe.killed_child_pids(), vec![4242, 4242]);
    assert!(probe.cleared_terminal_ids().is_empty());
}

#[test]
fn terminal_cleanup_debt_resolves_all_completion_waiters_only_after_certified_close() {
    let mock = MockOsApi::new();
    mock.fail_kill(4242);
    let probe = mock.clone();
    let (mut pty, _plugin_rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 100, 4242);
    let (first_tx, mut first_rx) = tokio::sync::oneshot::channel();
    let (second_tx, mut second_rx) = tokio::sync::oneshot::channel();

    pty.defer_terminal_cleanup(100, Some(NotificationEnd::new(first_tx)), true);
    pty.retry_terminal_cleanup_debts();
    assert!(matches!(
        first_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    pty.defer_terminal_cleanup(100, Some(NotificationEnd::new(second_tx)), true);
    probe.allow_kill(4242);
    pty.retry_terminal_cleanup_debts();

    for completion in [
        first_rx.try_recv().expect("first waiter must resolve"),
        second_rx
            .try_recv()
            .expect("deduplicated waiter must resolve"),
    ] {
        assert_eq!(completion.exit_status, None);
        assert_eq!(completion.error_message, None);
    }
    assert!(!pty.pending_terminal_cleanups.contains_key(&100));
    assert_eq!(probe.killed_child_pids(), vec![4242, 4242]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
}

#[test]
fn layout_terminal_cleanup_ack_retries_only_debt_and_replays_full_exact_receipt() {
    let mock = MockOsApi::new();
    mock.fail_kill(4343);
    let probe = mock.clone();
    let (mut pty, _plugin_rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 100, 4242);
    set_active_terminal(&mut pty, 101, 4343);

    let first_ack = pty.cleanup_layout_terminals_with_ack(901, vec![101, 100, 101]);
    let first_error = first_ack.expect_err("one failed kill must keep the exact cleanup pending");
    assert!(first_error.contains("Terminal(101)"));
    let pending = pty
        .pending_layout_terminal_cleanups
        .get(&901)
        .expect("the transaction must retain only unresolved cleanup debt");
    assert_eq!(pending.exact_terminal_ids, vec![100, 101]);
    assert_eq!(
        pending
            .remaining_terminal_ids
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![101]
    );
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert_eq!(pty.id_to_child_pid.get(&101), Some(&4343));
    assert_eq!(probe.killed_child_pids(), vec![4242, 4343]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);

    probe.allow_kill(4343);
    assert_eq!(
        pty.cleanup_layout_terminals_with_ack(901, vec![100, 101]),
        Ok(vec![100, 101])
    );
    assert_eq!(probe.killed_child_pids(), vec![4242, 4343, 4343]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert!(!pty.pending_layout_terminal_cleanups.contains_key(&901));
    assert_eq!(
        pty.resolved_layout_terminal_cleanups
            .get(&901)
            .map(Vec::as_slice),
        Some([100, 101].as_slice())
    );

    assert_eq!(
        pty.cleanup_layout_terminals_with_ack(901, vec![101, 100, 100]),
        Ok(vec![100, 101]),
        "a lost success ACK must replay without touching OS state again"
    );
    assert_eq!(probe.killed_child_pids(), vec![4242, 4343, 4343]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert!(
        pty.cleanup_layout_terminals_with_ack(901, vec![100])
            .expect_err("a reused transaction id with different exact IDs must be rejected")
            .contains("already resolved")
    );
}

#[test]
fn layout_terminal_cleanup_refuses_ack_until_process_exit_is_confirmed() {
    let mock = MockOsApi::new();
    mock.delay_exit_confirmation(4242);
    let probe = mock.clone();
    let (mut pty, _plugin_rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 100, 4242);

    let error = pty
        .cleanup_layout_terminals_with_ack(903, vec![100])
        .expect_err("unconfirmed exit must remain exact transaction debt");
    assert!(error.contains("exit/reap remains unconfirmed"));
    assert_eq!(probe.cleared_terminal_ids(), Vec::<u32>::new());
    assert_eq!(pty.id_to_child_pid.get(&100), Some(&4242));
    assert_eq!(
        pty.pending_layout_terminal_cleanups
            .get(&903)
            .map(|cleanup| cleanup
                .remaining_terminal_ids
                .iter()
                .copied()
                .collect::<Vec<_>>()),
        Some(vec![100])
    );
    assert!(
        !pty.resolved_layout_terminal_cleanups.contains_key(&903),
        "a process that may still be alive must not mint a replayable success receipt"
    );

    probe.confirm_exit(4242);
    assert_eq!(
        pty.cleanup_layout_terminals_with_ack(903, vec![100]),
        Ok(vec![100])
    );
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(!pty.id_to_child_pid.contains_key(&100));
    assert!(!pty.pending_layout_terminal_cleanups.contains_key(&903));
    assert_eq!(
        pty.resolved_layout_terminal_cleanups.get(&903),
        Some(&vec![100])
    );
}

#[test]
fn layout_terminal_cleanup_instruction_acks_without_terminating_the_pty_loop() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (pty_tx, pty_rx) = channels::unbounded();
    let pty_sender = SenderWithContext::new(pty_tx);
    let bus = Bus::new(
        vec![pty_rx],
        ThreadSenders {
            should_silently_fail: false,
            ..Default::default()
        },
        Some(Box::new(mock)),
    );
    let mut pty = Pty::new(bus, false, None, None);
    set_active_terminal(&mut pty, 100, 4242);
    let (ack_tx, ack_rx) = channels::bounded(1);
    pty_sender
        .send(PtyInstruction::CleanupLayoutTerminals {
            transaction_id: 902,
            terminal_ids: vec![100, 100],
            ack: ack_tx,
        })
        .unwrap();
    pty_sender.send(PtyInstruction::Exit).unwrap();

    pty_thread_main_loop(&mut pty, Box::<Layout>::default())
        .expect("a certified cleanup ACK must keep the PTY loop healthy");

    assert_eq!(ack_rx.recv().unwrap(), Ok(vec![100]));
    assert_eq!(probe.killed_child_pids(), vec![4242]);
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
}

#[test]
fn originating_plugin_open_notifications_cross_the_commit_fence_as_one_batch() {
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let first_origin = OriginatingPlugin::new(77, 1, Default::default());
    let second_origin = OriginatingPlugin::new(88, 2, Default::default());

    assert!(
        pty.begin_layout_commit(
            64,
            PendingLayoutCommit {
                allocation_ledger: LayoutAllocationLedger::default(),
                terminals: vec![],
                originating_plugins_to_inform: vec![
                    (100, first_origin.clone()),
                    (101, second_origin.clone()),
                ],
                layout_plugin_ids: vec![],
                tab_id: Some(7),
                layout_generation: None,
            },
        )
        .is_ok(),
        "the transaction must enter the PTY commit ledger"
    );

    let (resolution, ack) = pty.resolve_layout_commit_with_ack(64, LayoutCommitOutcome::Committed);
    resolution.expect("the atomic notification batch must be accepted");
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));
    let (instruction, _) = plugin_rx
        .try_recv()
        .expect("one Plugin update must carry the whole notification batch");
    let PluginInstruction::Update(updates) = instruction else {
        panic!("expected a batched Plugin update, got {instruction:?}");
    };
    assert_eq!(updates.len(), 2);
    assert!(matches!(
        &updates[0],
        (Some(77), Some(1), Event::CommandPaneOpened(100, _))
    ));
    assert!(matches!(
        &updates[1],
        (Some(88), Some(2), Event::CommandPaneOpened(101, _))
    ));
    assert!(
        plugin_rx.try_recv().is_err(),
        "a partial second delivery must be impossible"
    );
    assert_eq!(pty.originating_plugins.get(&100), Some(&first_origin));
    assert_eq!(pty.originating_plugins.get(&101), Some(&second_origin));
}

#[test]
fn duplicate_and_late_layout_resolutions_never_replace_the_live_ledger() {
    let mut pty = Pty::new(Bus::empty(), false, None, None);
    let mut first_ledger = LayoutAllocationLedger::default();
    first_ledger.track_terminal(100);
    assert!(
        pty.begin_layout_commit(
            62,
            PendingLayoutCommit {
                allocation_ledger: first_ledger,
                terminals: vec![],
                originating_plugins_to_inform: vec![],
                layout_plugin_ids: vec![],
                tab_id: Some(7),
                layout_generation: None,
            },
        )
        .is_ok(),
        "the first transaction must reserve the identity"
    );

    let mut duplicate_ledger = LayoutAllocationLedger::default();
    duplicate_ledger.track_terminal(101);
    let duplicate_failure = pty
        .begin_layout_commit(
            62,
            PendingLayoutCommit {
                allocation_ledger: duplicate_ledger,
                terminals: vec![],
                originating_plugins_to_inform: vec![],
                layout_plugin_ids: vec![],
                tab_id: Some(8),
                layout_generation: None,
            },
        )
        .expect_err("a duplicate identity must not replace the live transaction");
    let (duplicate_error, duplicate_commit) = duplicate_failure.into_parts();
    assert!(format!("{duplicate_error:#}").contains("duplicate pending layout transaction id 62"));
    assert!(
        duplicate_commit
            .allocation_ledger
            .terminal_ids
            .contains(&101)
    );
    let live_commit = pty.pending_layout_commits.get(&62).unwrap();
    assert!(live_commit.allocation_ledger.terminal_ids.contains(&100));
    assert!(!live_commit.allocation_ledger.terminal_ids.contains(&101));

    let (resolution, ack) = pty.resolve_layout_commit_with_ack(62, LayoutCommitOutcome::Committed);
    assert!(resolution.is_ok());
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));
    let (replayed_resolution, replayed_ack) =
        pty.resolve_layout_commit_with_ack(62, LayoutCommitOutcome::Committed);
    assert!(
        replayed_resolution.is_ok(),
        "same-outcome retry must replay the receipt"
    );
    assert_eq!(replayed_ack, Ok(LayoutCommitAck::Resolved));
    let mut reused_ledger = LayoutAllocationLedger::default();
    reused_ledger.track_terminal(102);
    let reuse_failure = pty
        .begin_layout_commit(
            62,
            PendingLayoutCommit {
                allocation_ledger: reused_ledger,
                terminals: vec![],
                originating_plugins_to_inform: vec![],
                layout_plugin_ids: vec![],
                tab_id: Some(9),
                layout_generation: None,
            },
        )
        .expect_err("a resolved transaction identity must not be reused");
    let (reuse_error, reused_commit) = reuse_failure.into_parts();
    assert!(format!("{reuse_error:#}").contains("layout transaction id 62 is already resolved"));
    assert!(reused_commit.allocation_ledger.terminal_ids.contains(&102));
    let (late_resolution, late_ack) = pty.resolve_layout_commit_with_ack(
        62,
        LayoutCommitOutcome::Rejected("late duplicate".to_owned()),
    );
    assert!(
        late_resolution.is_err(),
        "a late contradictory resolution must not certify a missing ledger"
    );
    assert!(
        late_ack
            .expect_err("late transaction ACK must fail")
            .contains("conflicting resolution for layout transaction 62")
    );
    let (unknown_resolution, unknown_ack) =
        pty.resolve_layout_commit_with_ack(999, LayoutCommitOutcome::Committed);
    assert!(unknown_resolution.is_err());
    assert!(
        unknown_ack
            .expect_err("unknown transaction ACK must fail")
            .contains("cannot resolve unknown layout transaction 999")
    );
}

#[test]
fn layout_resolution_receipts_are_strictly_bounded() {
    let mut pty = Pty::new(Bus::empty(), false, None, None);
    for transaction_id in 1..=(MAX_LAYOUT_COMMIT_RECEIPTS as u64 + 1) {
        let resolution = (Ok(()), Ok(LayoutCommitAck::Resolved));
        pty.record_layout_commit_receipt(
            transaction_id,
            LayoutCommitOutcome::Committed,
            &resolution,
        );
    }

    assert_eq!(
        pty.resolved_layout_commits.len(),
        MAX_LAYOUT_COMMIT_RECEIPTS
    );
    assert!(!pty.resolved_layout_commits.contains_key(&1));
    assert!(
        pty.resolved_layout_commits
            .contains_key(&(MAX_LAYOUT_COMMIT_RECEIPTS as u64 + 1))
    );
}

#[test]
fn command_not_found_without_explicit_hold_never_enters_a_layout() {
    let cases = vec![
        (
            "command without hold_on_close",
            Some(Run::Command(RunCommand {
                command: PathBuf::from("missing-command"),
                hold_on_close: false,
                ..Default::default()
            })),
        ),
        ("cwd", Some(Run::Cwd(PathBuf::from("/tmp")))),
        (
            "edit file",
            Some(Run::EditFile(
                PathBuf::from("/tmp/file.txt"),
                Some(1),
                Some(PathBuf::from("/tmp")),
            )),
        ),
        ("default shell", None),
    ];

    for (label, run) in cases {
        let mock = MockOsApi::new();
        mock.command_not_found_on_spawn_call(1);
        let probe = mock.clone();
        let (screen_tx, screen_rx) = channels::unbounded();
        let mut bus: Bus<PtyInstruction> = Bus::empty();
        bus.os_input = Some(Box::new(mock));
        bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
        bus.senders.should_silently_fail = false;
        let mut pty = Pty::new(bus, false, None, None);

        pty.spawn_terminals_for_layout(
            None,
            TiledPaneLayout {
                run,
                ..Default::default()
            },
            vec![],
            Some(TerminalAction::RunCommand(RunCommand {
                command: PathBuf::from("missing-default"),
                ..Default::default()
            })),
            HashMap::new(),
            None,
            7,
            7,
            false,
            true,
            (1, false),
            None,
            None,
        )
        .expect("prepare must not probe command existence by spawning");
        assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
        let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
            ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
                transaction_id
            },
            other => panic!("expected ApplyLayout, got {other:?}"),
        };
        let (resolution, ack) =
            pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
        let error = resolution.expect_err("activation must expose CommandNotFound");

        assert!(
            error.downcast_ref::<ZellijError>().is_some(),
            "{label}: the exact CommandNotFound remains the source"
        );
        assert_eq!(
            probe.cleared_terminal_ids(),
            vec![100],
            "{label}: the retained reservation must roll back before transfer"
        );
        assert!(matches!(
            ack,
            Ok(LayoutCommitAck::ActivationRolledBack(message))
                if message.contains("CommandNotFound")
                    || message.contains("failed to spawn terminal")
                    || message.contains("injected-missing-command")
        ));
        assert!(
            screen_rx.try_recv().is_err(),
            "{label}: PTY reports activation rollback by ACK; Screen owns topology rollback"
        );
    }
}

#[test]
fn hold_on_start_is_an_explicit_held_terminal_without_notification() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(RunCommand {
                command: PathBuf::from("held-before-start"),
                hold_on_start: true,
                ..Default::default()
            })),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::new(),
        None,
        7,
        8,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("hold_on_start must commit as an explicit held terminal");

    let (instruction, _) = screen_rx.try_recv().expect("ApplyLayout");
    let transaction_id = match instruction {
        ScreenInstruction::ApplyLayout(_, _, terminal_ids, ..) => {
            assert_eq!(terminal_ids.len(), 1);
            assert_eq!(terminal_ids[0].0, 100);
            assert!(terminal_ids[0].1.is_some());
            8
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    assert!(
        screen_rx.try_recv().is_err(),
        "hold_on_start is not a command-not-found notification"
    );
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    resolution.expect("held terminal requires no process activation");
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert!(probe.cleared_terminal_ids().is_empty());
}

#[test]
fn mismatched_command_not_found_never_transfers_or_clears_the_foreign_payload_id() {
    let mock = MockOsApi::new();
    mock.mismatched_command_not_found_on_spawn_call(1, 999);
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let plugin = RunPluginOrAlias::from_url("file:/foreign-id.wasm", &None, None, None).unwrap();

    pty.spawn_terminals_for_layout(
        None,
        TiledPaneLayout {
            run: Some(Run::Command(RunCommand {
                command: PathBuf::from("missing-command"),
                hold_on_close: true,
                ..Default::default()
            })),
            ..Default::default()
        },
        vec![],
        None,
        HashMap::from([(plugin, vec![77])]),
        None,
        7,
        9,
        false,
        true,
        (1, false),
        None,
        None,
    )
    .expect("prepare must only reserve the real terminal ID");
    let transaction_id = match screen_rx.try_recv().expect("ApplyLayout").0 {
        ScreenInstruction::ApplyLayout(_, _, _, _, _, _, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected ApplyLayout, got {other:?}"),
    };
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("a foreign CommandNotFound id is a protocol error");

    assert!(
        error.downcast_ref::<ZellijError>().is_none(),
        "the protocol error must not remain downcastable to CommandNotFound"
    );
    assert!(
        format!("{error:#}").contains("reserved terminal 100"),
        "the source chain must identify the real reservation: {error:#}"
    );
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100],
        "only the real reservation is cleared"
    );
    assert!(!probe.cleared_terminal_ids().contains(&999));
    assert!(matches!(
        ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("reserved terminal 100")
    ));
    assert!(screen_rx.try_recv().is_err());
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

fn override_tab(
    tab_index: usize,
    tiled_layout: TiledPaneLayout,
    floating_layouts: Vec<FloatingPaneLayout>,
) -> TabLayoutInfo {
    TabLayoutInfo {
        tab_index,
        tab_name: Some(format!("tab-{tab_index}")),
        tiled_layout,
        floating_layouts,
        swap_tiled_layouts: None,
        swap_floating_layouts: None,
    }
}

fn override_plugin(url: &str, plugin_id: u32) -> HashMap<RunPluginOrAlias, Vec<u32>> {
    HashMap::from([(
        RunPluginOrAlias::from_url(url, &None, None, None).unwrap(),
        vec![plugin_id],
    )])
}

fn unloaded_plugin_ids(
    plugin_rx: &channels::Receiver<(PluginInstruction, ErrorContext)>,
) -> Vec<u32> {
    plugin_rx
        .try_iter()
        .filter_map(|(instruction, _)| match instruction {
            PluginInstruction::Unload(plugin_id) => Some(plugin_id),
            _ => None,
        })
        .collect()
}

#[test]
fn override_notification_failure_rolls_back_before_screen_commit() {
    let mock = MockOsApi::new();
    mock.command_not_found_on_spawn_call(1);
    let probe = mock.clone();
    let (mut pty, plugin_rx) = make_pty_with_plugin_receiver(mock);
    let plugin =
        RunPluginOrAlias::from_url("file:/prepared-override.wasm", &None, None, None).unwrap();
    let plugin_ids = HashMap::from([(plugin, vec![77])]);
    let (screen_tx, screen_rx) = channels::unbounded();
    pty.bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    let layout = TiledPaneLayout {
        run: Some(Run::Command(RunCommand {
            command: PathBuf::from("missing-command"),
            hold_on_close: true,
            ..Default::default()
        })),
        ..Default::default()
    };
    fail_command_not_found_notification_between_messages();

    pty.override_layout_transaction(
        None,
        None,
        vec![(
            TabLayoutInfo {
                tab_index: 7,
                tab_name: Some("Recovered tab".to_owned()),
                tiled_layout: layout,
                floating_layouts: vec![],
                swap_tiled_layouts: None,
                swap_floating_layouts: None,
            },
            plugin_ids,
        )],
        10,
        true,
        true,
        1,
        None,
        None,
    )
    .expect("prepare must reserve the override without spawning");

    let (screen_instruction, _) = screen_rx
        .try_recv()
        .expect("the prepared override must transfer to screen");
    let transaction_id = match screen_instruction {
        ScreenInstruction::OverrideLayoutComplete(tab_results, _, _, _, _, _, transaction_id) => {
            assert_eq!(tab_results.len(), 1);
            assert_eq!(
                tab_results[0].new_terminal_pids,
                vec![(100, None)],
                "prepare must not probe command existence by spawning"
            );
            assert_eq!(
                tab_results[0]
                    .plugin_ids
                    .values()
                    .flatten()
                    .copied()
                    .collect::<Vec<_>>(),
                vec![77]
            );
            transaction_id
        },
        other => panic!("expected OverrideLayoutComplete, got {other:?}"),
    };
    assert!(
        screen_rx.try_recv().is_err(),
        "a prepared override must remain quiescent until Screen commits it"
    );
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("a partial held-pane notification must roll activation back");
    assert!(format!("{error:#}").contains("injected between-message"));
    assert!(matches!(
        ack,
        Ok(LayoutCommitAck::ActivationRolledBack(message))
            if message.contains("injected between-message")
    ));
    assert!(matches!(
        screen_rx.try_recv(),
        Ok((ScreenInstruction::PtyBytes(100, _), _))
    ));
    assert!(
        screen_rx.try_recv().is_err(),
        "the injected between-message failure suppresses HoldPane only"
    );
    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
    assert!(
        !plugin_rx
            .try_iter()
            .any(|(instruction, _)| matches!(instruction, PluginInstruction::Unload(77))),
        "Plugin compensation is not owned by PTY"
    );
}

#[test]
fn partial_override_activation_failure_releases_all_terminal_reservations() {
    let mock = MockOsApi::new();
    mock.fail_on_spawn_call(2);
    let probe = mock.clone();
    let (mut pty, plugin_rx) = make_pty_with_plugin_receiver(mock);
    let plugin =
        RunPluginOrAlias::from_url("file:/partial-allocation.wasm", &None, None, None).unwrap();
    let plugin_ids = HashMap::from([(plugin, vec![77])]);
    let (screen_tx, screen_rx) = channels::unbounded();
    pty.bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    let layout = TiledPaneLayout {
        children: vec![TiledPaneLayout::default(), TiledPaneLayout::default()],
        ..Default::default()
    };
    let default_shell = TerminalAction::RunCommand(RunCommand {
        command: PathBuf::from("sh"),
        ..Default::default()
    });

    pty.override_layout_transaction(
        None,
        Some(default_shell),
        vec![(
            TabLayoutInfo {
                tab_index: 7,
                tab_name: Some("Finalized runs".to_owned()),
                tiled_layout: layout,
                floating_layouts: vec![],
                swap_tiled_layouts: None,
                swap_floating_layouts: None,
            },
            plugin_ids,
        )],
        11,
        true,
        true,
        1,
        None,
        None,
    )
    .expect("prepare must reserve both override terminals");
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    let transaction_id = match screen_rx.try_recv().expect("OverrideLayoutComplete").0 {
        ScreenInstruction::OverrideLayoutComplete(_, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected OverrideLayoutComplete, got {other:?}"),
    };
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("the second exact activation must fail");

    assert!(
        format!("{:#}", error).contains("injected EMFILE-like spawn failure"),
        "the original spawn failure must remain in the error chain"
    );
    assert!(matches!(ack, Ok(LayoutCommitAck::ActivationRolledBack(_))));
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

#[test]
fn override_per_tab_activation_failure_rolls_back_current_and_all_prior_tabs() {
    let mock = MockOsApi::new();
    mock.fail_on_spawn_call(3);
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let second_tab_layout = TiledPaneLayout {
        children: vec![TiledPaneLayout::default(), TiledPaneLayout::default()],
        ..Default::default()
    };

    pty.override_layout_transaction(
        None,
        None,
        vec![
            (
                override_tab(0, TiledPaneLayout::default(), vec![]),
                override_plugin("file:/first-tab.wasm", 71),
            ),
            (
                override_tab(1, second_tab_layout, vec![]),
                override_plugin("file:/second-tab.wasm", 72),
            ),
        ],
        12,
        true,
        true,
        1,
        None,
        None,
    )
    .expect("all tabs must reserve without spawning");
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    let transaction_id = match screen_rx.try_recv().expect("OverrideLayoutComplete").0 {
        ScreenInstruction::OverrideLayoutComplete(_, _, _, _, _, _, transaction_id) => {
            transaction_id
        },
        other => panic!("expected OverrideLayoutComplete, got {other:?}"),
    };
    let (resolution, ack) =
        pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed);
    let error = resolution.expect_err("the third exact activation must abort the whole override");

    assert!(format!("{error:#}").contains("injected EMFILE-like spawn failure"));
    assert!(matches!(ack, Ok(LayoutCommitAck::ActivationRolledBack(_))));
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![100, 101, 102],
        "prior and current terminal reservations are cleared exactly once"
    );
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
    // A reader can enqueue its idempotent render wake-up immediately before
    // cleanup aborts the task. No pane mutation or exit callback may escape.
    while let Ok((instruction, _)) = screen_rx.try_recv() {
        assert!(
            matches!(instruction, ScreenInstruction::Render),
            "rollback emitted an unexpected mutating Screen instruction: {instruction:?}"
        );
    }
}

#[test]
fn override_final_send_failure_rolls_back_the_union_exactly_once() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    let error = pty
        .override_layout_transaction(
            None,
            None,
            vec![
                (
                    override_tab(0, TiledPaneLayout::default(), vec![]),
                    override_plugin("file:/first-final-send.wasm", 71),
                ),
                (
                    override_tab(1, TiledPaneLayout::default(), vec![]),
                    override_plugin("file:/second-final-send.wasm", 72),
                ),
            ],
            13,
            true,
            true,
            1,
            None,
            None,
        )
        .expect_err("a missing screen sender must reject the final transaction");

    assert!(format!("{error:#}").contains("failed to get screen sender"));
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

#[test]
fn rejected_multi_tab_override_ack_rolls_back_the_union_exactly_once() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let (plugin_tx, plugin_rx) = channels::unbounded();
    let (screen_tx, screen_rx) = channels::unbounded();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.to_plugin = Some(SenderWithContext::new(plugin_tx));
    bus.senders.to_screen = Some(SenderWithContext::new(screen_tx));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);

    pty.override_layout_transaction(
        None,
        None,
        vec![
            (
                override_tab(0, TiledPaneLayout::default(), vec![]),
                override_plugin("file:/first-screen-rejected.wasm", 71),
            ),
            (
                override_tab(1, TiledPaneLayout::default(), vec![]),
                override_plugin("file:/second-screen-rejected.wasm", 72),
            ),
        ],
        14,
        true,
        true,
        1,
        None,
        None,
    )
    .expect("the complete multi-tab payload must reach Screen");
    let (instruction, _) = screen_rx.try_recv().expect("OverrideLayoutComplete");
    let transaction_id = match instruction {
        ScreenInstruction::OverrideLayoutComplete(tab_results, _, _, _, _, _, transaction_id) => {
            assert_eq!(tab_results.len(), 2);
            transaction_id
        },
        other => panic!("expected OverrideLayoutComplete, got {other:?}"),
    };

    assert!(
        lock_recover(&probe.quit_callbacks).is_empty(),
        "rejected prepared tabs must not own child exit callbacks"
    );
    let (resolution, ack) = pty.resolve_layout_commit_with_ack(
        transaction_id,
        LayoutCommitOutcome::Rejected("second tab failed in Screen".to_owned()),
    );
    resolution.expect_err("Screen rejection remains visible after exact cleanup");
    assert_eq!(ack, Ok(LayoutCommitAck::Resolved));

    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert_eq!(probe.spawn_terminal_calls.load(Ordering::Relaxed), 0);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
    assert!(
        screen_rx.try_recv().is_err(),
        "rollback must cancel every tab's queued quit callback"
    );

    pty.resolve_layout_commit_with_ack(transaction_id, LayoutCommitOutcome::Committed)
        .0
        .expect_err("a stale contradictory ACK must expose the missing ledger");
    assert_eq!(probe.cleared_terminal_ids(), vec![100, 101]);
    assert!(unloaded_plugin_ids(&plugin_rx).is_empty());
}

#[test]
fn rollback_aggregates_cleanup_errors_in_stable_order_and_preserves_source() {
    let mock = MockOsApi::new();
    mock.fail_clear_terminal_id(2);
    let probe = mock.clone();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.should_silently_fail = false;
    let mut pty = Pty::new(bus, false, None, None);
    let mut allocation_ledger = LayoutAllocationLedger::default();
    allocation_ledger.track_terminal(2);
    allocation_ledger.track_terminal(1);

    let rollback = pty.rollback_partial_layout_allocations(
        anyhow::Error::new(io::Error::other("primary layout failure")),
        allocation_ledger,
    );
    assert!(
        !rollback.cleanup_succeeded,
        "an injected terminal cleanup failure must remain uncertified"
    );
    let error = rollback.error;
    assert!(
        error.downcast_ref::<io::Error>().is_some(),
        "cleanup context must preserve the original source"
    );
    let message = error.to_string();
    let terminal_2 = message
        .find("Terminal(2):")
        .expect("terminal cleanup error");
    assert!(terminal_2 < message.len());
    assert_eq!(message.matches("Terminal(2):").count(), 1);
    assert_eq!(
        probe.cleared_terminal_ids(),
        vec![1, 2],
        "every terminal cleanup is attempted once despite failures"
    );
}

#[test]
fn armed_layout_ledger_drop_is_a_last_resort_exact_cleanup_guard() {
    let mock = MockOsApi::new();
    let probe = mock.clone();
    let mut bus: Bus<PtyInstruction> = Bus::empty();
    bus.os_input = Some(Box::new(mock));
    bus.senders.should_silently_fail = false;
    let mut allocation_ledger = LayoutAllocationLedger::armed_for_bus(&bus);
    allocation_ledger.track_terminal(100);

    drop(allocation_ledger);

    assert_eq!(probe.cleared_terminal_ids(), vec![100]);
}

#[test]
fn foreground_command_emitted_with_is_foreground_true() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_foreground_cmd(child_pid, vec!["vim".into(), "file.rs".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();

    let events = collect_command_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, PaneId::Terminal(1));
    assert_eq!(events[0].1, vec!["vim", "file.rs"]);
    assert!(events[0].2, "expected is_foreground=true");
}

#[test]
fn empty_foreground_falls_back_to_shell_command() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_cmd(child_pid, vec!["/bin/bash".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();

    let events = collect_command_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, PaneId::Terminal(1));
    assert_eq!(events[0].1, vec!["/bin/bash"]);
    assert!(!events[0].2, "expected is_foreground=false");
}

#[test]
fn foreground_clearing_emits_shell_fallback() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_cmd(child_pid, vec!["/bin/zsh".into()]);
    mock.set_foreground_cmd(child_pid, vec!["cargo".into(), "build".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock.clone());
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert!(events[0].2, "first event should be foreground");
    assert_eq!(events[0].1, vec!["cargo", "build"]);

    mock.clear_foreground_cmd(child_pid);
    pty.pane_activity_flags
        .get(&1)
        .unwrap()
        .store(true, Ordering::Relaxed);

    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, vec!["/bin/zsh"]);
    assert!(
        !events[0].2,
        "after clearing foreground, should fall back to shell"
    );
}

#[test]
fn no_event_when_foreground_unchanged() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_foreground_cmd(child_pid, vec!["htop".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();
    let _ = collect_command_changed_events(&rx);

    pty.pane_activity_flags
        .get(&1)
        .unwrap()
        .store(true, Ordering::Relaxed);
    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert!(
        events.is_empty(),
        "no event expected when command unchanged"
    );
}

#[test]
fn no_event_for_inactive_terminal() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_foreground_cmd(child_pid, vec!["vim".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);
    pty.pane_activity_flags
        .get(&1)
        .unwrap()
        .store(false, Ordering::Relaxed);

    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert!(
        events.is_empty(),
        "inactive terminal should produce no events"
    );
}

#[test]
fn foreground_change_between_two_commands() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_foreground_cmd(child_pid, vec!["vim".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock.clone());
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert_eq!(events[0].1, vec!["vim"]);
    assert!(events[0].2);

    mock.set_foreground_cmd(child_pid, vec!["cargo".into(), "test".into()]);
    pty.pane_activity_flags
        .get(&1)
        .unwrap()
        .store(true, Ordering::Relaxed);

    pty.update_and_report_cwds();
    let events = collect_command_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].1, vec!["cargo", "test"]);
    assert!(events[0].2);
}

// --- Activity flag gating ---

#[test]
fn activity_flag_reset_after_poll() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    let (mut pty, _rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);
    assert!(
        pty.pane_activity_flags
            .get(&1)
            .unwrap()
            .load(Ordering::Relaxed)
    );

    pty.update_and_report_cwds();

    assert!(
        !pty.pane_activity_flags
            .get(&1)
            .unwrap()
            .load(Ordering::Relaxed),
        "activity flag should be reset to false after poll"
    );
}

#[test]
fn multiple_terminals_only_active_ones_polled() {
    let mock = MockOsApi::new();
    let pid_active = 100;
    let pid_inactive = 200;
    mock.set_cwd(pid_active, PathBuf::from("/active"));
    mock.set_cwd(pid_inactive, PathBuf::from("/inactive"));
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, pid_active);
    set_active_terminal(&mut pty, 2, pid_inactive);
    pty.pane_activity_flags
        .get(&2)
        .unwrap()
        .store(false, Ordering::Relaxed);

    pty.update_and_report_cwds();

    let events = collect_cwd_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, PaneId::Terminal(1));
    assert_eq!(events[0].1, PathBuf::from("/active"));
}

// --- CWD change events ---

#[test]
fn cwd_changed_event_emitted_on_change() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_cwd(child_pid, PathBuf::from("/home/user"));
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);

    pty.update_and_report_cwds();

    let events = collect_cwd_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, PaneId::Terminal(1));
    assert_eq!(events[0].1, PathBuf::from("/home/user"));
}

#[test]
fn no_cwd_event_when_unchanged() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_cwd(child_pid, PathBuf::from("/home/user"));
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);
    pty.terminal_cwds.insert(1, PathBuf::from("/home/user"));

    pty.update_and_report_cwds();

    let events = collect_cwd_changed_events(&rx);
    assert!(events.is_empty(), "no event expected when cwd unchanged");
}

// --- OSC7 CWD notification ---

#[test]
fn osc7_emits_cwd_changed() {
    let mock = MockOsApi::new();
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    pty.id_to_child_pid.insert(1, 100);

    pty.notify_cwd_from_osc7(1, PathBuf::from("/tmp/new"));

    let events = collect_cwd_changed_events(&rx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, PaneId::Terminal(1));
    assert_eq!(events[0].1, PathBuf::from("/tmp/new"));
    assert_eq!(
        pty.terminal_cwds.get(&1),
        Some(&PathBuf::from("/tmp/new")),
        "cache should be updated"
    );
}

#[test]
fn osc7_no_event_when_unchanged() {
    let mock = MockOsApi::new();
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    pty.id_to_child_pid.insert(1, 100);
    pty.terminal_cwds.insert(1, PathBuf::from("/same"));

    pty.notify_cwd_from_osc7(1, PathBuf::from("/same"));

    let events = collect_cwd_changed_events(&rx);
    assert!(events.is_empty(), "no event when osc7 path matches cache");
}

#[test]
fn osc7_clears_activity_flag() {
    let mock = MockOsApi::new();
    let (mut pty, _rx) = make_pty_with_plugin_receiver(mock);
    let flag = Arc::new(AtomicBool::new(true));
    pty.id_to_child_pid.insert(1, 100);
    pty.pane_activity_flags.insert(1, flag.clone());

    pty.notify_cwd_from_osc7(1, PathBuf::from("/new"));

    assert!(
        !flag.load(Ordering::Relaxed),
        "osc7 should clear the activity flag"
    );
}

#[test]
fn osc7_then_poll_skips_terminal() {
    let mock = MockOsApi::new();
    let child_pid = 100;
    mock.set_cwd(child_pid, PathBuf::from("/from-proc"));
    mock.set_foreground_cmd(child_pid, vec!["vim".into()]);
    let (mut pty, rx) = make_pty_with_plugin_receiver(mock);
    set_active_terminal(&mut pty, 1, child_pid);

    pty.notify_cwd_from_osc7(1, PathBuf::from("/from-osc7"));
    let osc7_events = collect_cwd_changed_events(&rx);
    assert_eq!(osc7_events.len(), 1);

    pty.update_and_report_cwds();
    let cwd_events = collect_cwd_changed_events(&rx);
    let cmd_events = collect_command_changed_events(&rx);
    assert!(
        cwd_events.is_empty() && cmd_events.is_empty(),
        "poll after osc7 should skip terminal since flag was cleared"
    );
}
