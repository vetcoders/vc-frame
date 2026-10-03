//! The `[cli_client]` is used to attach to a running server session
//! and dispatch actions, that are specified through the command line.
use std::collections::{BTreeMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::process;
use std::str::FromStr;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

use crate::os_input_output::ClientOsApi;
use uuid::Uuid;
use zellij_utils::{
    cli::{SubscribeCli, SubscribeFormat},
    data::PaneId,
    envs::{PANE_ID_ENV_KEY, VC_FRAME_PANE_ID_ENV_KEY},
    errors::{ErrorContext, prelude::*},
    input::actions::Action,
    ipc::{ClientToServerMsg, ExitReason, ServerToClientMsg},
};

/// Transport completion is distinct from an application's acknowledgment.
/// `pipe_output` contains only output addressed to this invocation's pipe ID.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CliClientOutput {
    pub exit_code: i32,
    pub pipe_output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliClientMode {
    /// Preserve ordinary CLI streaming and stdout behavior.
    Cli,
    /// Send exactly one payload, capture its reply, and return to the caller.
    Request,
}

pub fn start_cli_client(
    mut os_input: Box<dyn ClientOsApi>,
    session_name: &str,
    actions: Vec<Action>,
    mode: CliClientMode,
) -> CliClientOutput {
    let caller = declared_caller(&*os_input, "anonymous");
    let deadline = ActionDeadline::arm(&*os_input, caller.clone());
    let zellij_ipc_pipe: PathBuf = {
        let mut sock_dir = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
        zellij_utils::consts::ensure_socket_runtime_dirs(&sock_dir).unwrap();
        sock_dir.push(session_name);
        sock_dir
    };
    crate::check_ipc_pipe_length(&zellij_ipc_pipe);
    os_input.connect_to_server(&zellij_ipc_pipe);
    // The socket is up. The window starts here, then slides on later life.
    deadline.note_life();
    send_with_life(
        os_input.as_ref(),
        &deadline,
        ClientToServerMsg::DeclareCaller { caller },
    );
    let pane_id = os_input
        .env_variable(VC_FRAME_PANE_ID_ENV_KEY)
        .or_else(|| os_input.env_variable(PANE_ID_ENV_KEY))
        .and_then(|e| e.trim().parse().ok());

    let mut output = CliClientOutput::default();
    for action in actions {
        output.exit_code = match action {
            Action::CliPipe {
                pipe_id,
                name,
                payload,
                plugin,
                args,
                configuration,
                launch_new,
                skip_cache,
                floating,
                in_place,
                cwd,
                pane_title,
            } => pipe_client(
                &mut os_input,
                PipeClientParams {
                    pipe_id,
                    name,
                    payload,
                    plugin,
                    args,
                    configuration,
                    launch_new,
                    skip_cache,
                    floating,
                    in_place,
                    pane_id,
                    cwd,
                    pane_title,
                },
                mode,
                &mut output.pipe_output,
                &deadline,
            ),
            action => individual_messages_client(&mut os_input, action, pane_id, &deadline),
        };
        if output.exit_code != 0 {
            break;
        }
    }
    send_with_life(
        os_input.as_ref(),
        &deadline,
        ClientToServerMsg::ClientExited,
    );
    deadline.complete();
    output
}

fn declared_caller(os_input: &dyn ClientOsApi, fallback: &str) -> String {
    os_input
        .env_variable("VC_FRAME_CALLER")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn ttl_seconds(os_input: &dyn ClientOsApi) -> u64 {
    os_input
        .env_variable("VC_FRAME_ACTION_TTL_SECONDS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(1, 3600)
}

fn monotonic_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let start = START.get_or_init(Instant::now);
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// True when nothing has counted as life for a full TTL.
/// A connect-anchored clock is the bug: later progress must move `last_life_ms`.
fn idle_exceeded(now_ms: u64, last_life_ms: u64, ttl_seconds: u64) -> bool {
    now_ms.saturating_sub(last_life_ms) >= ttl_seconds.saturating_mul(1000)
}

fn expired_client_line(caller: &str, ttl_seconds: u64) -> String {
    format!(
        "warden.expired_client caller={caller} ttl_seconds={ttl_seconds} result=client_self_retired"
    )
}

struct ActionDeadline {
    completed: Arc<AtomicBool>,
    last_life_ms: Arc<AtomicU64>,
}

impl ActionDeadline {
    fn arm(os_input: &dyn ClientOsApi, caller: String) -> Self {
        let seconds = ttl_seconds(os_input);
        let completed = Arc::new(AtomicBool::new(false));
        let last_life_ms = Arc::new(AtomicU64::new(monotonic_ms()));
        let watchdog_completed = completed.clone();
        let watchdog_life = last_life_ms.clone();
        std::thread::spawn(move || {
            let slice = Duration::from_millis(100);
            loop {
                std::thread::sleep(slice);
                if watchdog_completed.load(Ordering::Acquire) {
                    return;
                }
                let last = watchdog_life.load(Ordering::Acquire);
                if idle_exceeded(monotonic_ms(), last, seconds)
                    && !watchdog_completed.load(Ordering::Acquire)
                {
                    eprintln!("{}", expired_client_line(&caller, seconds));
                    process::exit(124);
                }
            }
        });
        Self {
            completed,
            last_life_ms,
        }
    }

    fn note_life(&self) {
        self.last_life_ms.store(monotonic_ms(), Ordering::Release);
    }

    fn complete(&self) {
        self.completed.store(true, Ordering::Release);
    }

    /// No watchdog. Unit tests that drive the pipe loop must not be able to
    /// `process::exit` the test runner when the ambient TTL is short.
    #[cfg(test)]
    fn parked() -> Self {
        Self {
            completed: Arc::new(AtomicBool::new(true)),
            last_life_ms: Arc::new(AtomicU64::new(monotonic_ms())),
        }
    }
}

impl Drop for ActionDeadline {
    fn drop(&mut self) {
        self.complete();
    }
}

fn send_with_life(
    os_input: &dyn ClientOsApi,
    deadline: &ActionDeadline,
    message: ClientToServerMsg,
) {
    deadline.note_life();
    os_input.send_to_server(message);
}

fn recv_with_life(
    os_input: &dyn ClientOsApi,
    deadline: &ActionDeadline,
) -> Option<(ServerToClientMsg, ErrorContext)> {
    let message = os_input.recv_from_server();
    if message.is_some() {
        deadline.note_life();
    }
    message
}

pub fn doctor_routes_client(os_input: Box<dyn ClientOsApi>, session_name: &str, json: bool) -> i32 {
    let caller = declared_caller(&*os_input, "operator");
    let deadline = ActionDeadline::arm(&*os_input, caller.clone());
    let mut socket = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
    socket.push(session_name);
    os_input.connect_to_server(&socket);
    deadline.note_life();
    send_with_life(
        os_input.as_ref(),
        &deadline,
        ClientToServerMsg::DeclareCaller { caller },
    );
    send_with_life(
        os_input.as_ref(),
        &deadline,
        ClientToServerMsg::DoctorRoutes { json },
    );
    let exit_code = loop {
        match recv_with_life(os_input.as_ref(), &deadline).map(|(message, _)| message) {
            Some(ServerToClientMsg::Log { lines }) => {
                for line in lines {
                    println!("{line}");
                }
                break 0;
            },
            Some(ServerToClientMsg::LogError { lines }) => {
                for line in lines {
                    eprintln!("{line}");
                }
                break 1;
            },
            Some(ServerToClientMsg::Connected | ServerToClientMsg::UnblockInputThread) => {},
            None => {
                eprintln!("route telemetry request ended without a receipt");
                break 1;
            },
            Some(_) => {},
        }
    };
    deadline.complete();
    exit_code
}

struct PipeClientParams {
    pipe_id: String,
    name: Option<String>,
    payload: Option<String>,
    plugin: Option<String>,
    args: Option<BTreeMap<String, String>>,
    configuration: Option<BTreeMap<String, String>>,
    launch_new: bool,
    skip_cache: bool,
    floating: Option<bool>,
    in_place: Option<bool>,
    pane_id: Option<u32>,
    cwd: Option<PathBuf>,
    pane_title: Option<String>,
}

fn pipe_client(
    os_input: &mut Box<dyn ClientOsApi>,
    params: PipeClientParams,
    mode: CliClientMode,
    pipe_output: &mut String,
    deadline: &ActionDeadline,
) -> i32 {
    let PipeClientParams {
        pipe_id,
        mut name,
        mut payload,
        plugin,
        args,
        mut configuration,
        launch_new,
        skip_cache,
        floating,
        in_place,
        pane_id,
        cwd,
        pane_title,
    } = params;
    // Request mode must never lock or consume the caller's stdin.
    let mut stdin = (mode == CliClientMode::Cli).then(|| os_input.get_stdin_reader());
    let name = name
        // first we try to take the explicitly supplied message name
        .take()
        // then we use the plugin, to facilitate using aliases
        .or_else(|| plugin.clone())
        // then we use a uuid to at least have some sort of identifier for this message
        .or_else(|| Some(Uuid::new_v4().to_string()));
    if launch_new {
        // we do this to make sure the plugin is unique (has a unique configuration parameter) so
        // that a new one would be launched, but we'll still send it to the same instance rather
        // than launching a new one in every iteration of the loop
        configuration
            .get_or_insert_with(BTreeMap::new)
            .insert("_zellij_id".to_owned(), Uuid::new_v4().to_string());
    }
    let create_msg = |payload: Option<String>| -> ClientToServerMsg {
        ClientToServerMsg::Action {
            action: Action::CliPipe {
                pipe_id: pipe_id.clone(),
                name: name.clone(),
                payload,
                args: args.clone(),
                plugin: plugin.clone(),
                configuration: configuration.clone(),
                floating,
                in_place,
                launch_new,
                skip_cache,
                cwd: cwd.clone(),
                pane_title: pane_title.clone(),
            },
            terminal_id: pane_id,
            client_id: None,
            is_cli_client: true,
        }
    };
    let is_piped = mode == CliClientMode::Cli && !os_input.stdin_is_terminal();
    loop {
        if let Some(payload) = payload.take() {
            let msg = create_msg(Some(payload));
            send_with_life(os_input.as_ref(), deadline, msg);
        } else if !is_piped {
            // here we send an empty message to trigger the plugin, because we don't have any more
            // data
            let msg = create_msg(None);
            send_with_life(os_input.as_ref(), deadline, msg);
        } else {
            // we didn't get payload from the command line, meaning we listen on STDIN because this
            // signifies the user is about to pipe more (eg. cat my-large-file | zellij pipe ...)
            let mut buffer = String::new();
            if stdin.as_mut().unwrap().read_line(&mut buffer).is_err() {
                return 2;
            }
            if buffer.is_empty() {
                let msg = create_msg(None);
                send_with_life(os_input.as_ref(), deadline, msg);
                break;
            } else {
                // we've got data! send it down the pipe (most common)
                let msg = create_msg(Some(buffer));
                send_with_life(os_input.as_ref(), deadline, msg);
            }
        }
        loop {
            // wait for a response and act accordingly
            match recv_with_life(os_input.as_ref(), deadline) {
                Some((ServerToClientMsg::UnblockCliPipeInput { pipe_name }, _))
                    if pipe_name == pipe_id =>
                {
                    // unblock this pipe, meaning we need to stop waiting for a response and read
                    // once more from STDIN
                    if !is_piped {
                        // This releases transport input; the caller still has to validate
                        // any application-level acknowledgment in pipe_output.
                        return 0;
                    } else {
                        break;
                    }
                },
                Some((ServerToClientMsg::CliPipeOutput { pipe_name, output }, _)) => {
                    // send data to STDOUT, this *does not* mean we need to unblock the input
                    let err_context = "Failed to write to stdout";
                    if pipe_name == pipe_id {
                        if mode == CliClientMode::Request {
                            pipe_output.push_str(&output);
                            continue;
                        }
                        let mut stdout = os_input.get_stdout_writer();
                        stdout
                            .write_all(output.as_bytes())
                            .context(err_context)
                            .non_fatal();
                        stdout.flush().context(err_context).non_fatal();
                    }
                },
                Some((ServerToClientMsg::Log { lines: log_lines }, _)) => {
                    log_lines.iter().for_each(|line| println!("{line}"));
                    return 0;
                },
                Some((ServerToClientMsg::LogError { lines: log_lines }, _)) => {
                    log_lines.iter().for_each(|line| eprintln!("{line}"));
                    return 2;
                },
                Some((ServerToClientMsg::Exit { exit_reason }, _)) => match exit_reason {
                    ExitReason::Error(e) => {
                        eprintln!("{}", e);
                        return 2;
                    },
                    _ => {
                        return 0;
                    },
                },
                None => {
                    eprintln!("server disconnected before completing the CLI pipe");
                    return 2;
                },
                _ => {},
            }
        }
    }
    0
}

fn individual_messages_client(
    os_input: &mut Box<dyn ClientOsApi>,
    action: Action,
    pane_id: Option<u32>,
    deadline: &ActionDeadline,
) -> i32 {
    let msg = ClientToServerMsg::Action {
        action,
        terminal_id: pane_id,
        client_id: None,
        is_cli_client: true,
    };
    send_with_life(os_input.as_ref(), deadline, msg);
    loop {
        let message = recv_with_life(os_input.as_ref(), deadline).map(|(message, _)| message);
        match classify_cli_action_response(message) {
            CliActionResponse::Wait => {},
            CliActionResponse::Success(log_lines) => {
                log_lines.iter().for_each(|line| println!("{line}"));
                break;
            },
            CliActionResponse::Error(log_lines) => {
                log_lines.iter().for_each(|line| eprintln!("{line}"));
                return 2;
            },
            CliActionResponse::Exit(exit_reason) => match exit_reason {
                ExitReason::Error(e) => {
                    eprintln!("{}", e);
                    return 2;
                },
                ExitReason::CustomExitStatus(exit_status) => {
                    return exit_status;
                },
                _ => {
                    break;
                },
            },
            CliActionResponse::Disconnected => {
                eprintln!("server disconnected before acknowledging the CLI action");
                return 2;
            },
        }
    }
    0
}

#[derive(Debug)]
enum CliActionResponse {
    Wait,
    Success(Vec<String>),
    Error(Vec<String>),
    Exit(ExitReason),
    Disconnected,
}

fn classify_cli_action_response(message: Option<ServerToClientMsg>) -> CliActionResponse {
    match message {
        // This signal is intentionally session-wide and can belong to an
        // unrelated interactive client. CLI actions finish only on their
        // targeted Log/LogError/Exit acknowledgement.
        Some(ServerToClientMsg::UnblockInputThread) => CliActionResponse::Wait,
        Some(ServerToClientMsg::Log { lines }) => CliActionResponse::Success(lines),
        Some(ServerToClientMsg::LogError { lines }) => CliActionResponse::Error(lines),
        Some(ServerToClientMsg::Exit { exit_reason }) => CliActionResponse::Exit(exit_reason),
        Some(_) => CliActionResponse::Wait,
        None => CliActionResponse::Disconnected,
    }
}

pub fn start_subscribe_client(
    os_input: Box<dyn ClientOsApi>,
    session_name: &str,
    subscribe_cli: SubscribeCli,
) {
    let zellij_ipc_pipe: PathBuf = {
        let mut sock_dir = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
        zellij_utils::consts::ensure_socket_runtime_dirs(&sock_dir).unwrap();
        sock_dir.push(session_name);
        sock_dir
    };
    crate::check_ipc_pipe_length(&zellij_ipc_pipe);
    os_input.connect_to_server(&zellij_ipc_pipe);

    // Parse pane IDs
    let pane_ids: Vec<PaneId> = subscribe_cli
        .pane_id
        .iter()
        .map(|s| {
            PaneId::from_str(s).unwrap_or_else(|e| {
                eprintln!("Invalid pane ID '{}': {}", s, e);
                process::exit(2);
            })
        })
        .collect();

    // Send subscribe message
    os_input.send_to_server(ClientToServerMsg::SubscribeToPaneRenders {
        pane_ids: pane_ids.clone(),
        scrollback: subscribe_cli.scrollback_lines(),
        ansi: subscribe_cli.ansi,
    });

    // Track remaining panes for exit-on-all-closed
    let mut remaining_panes: HashSet<PaneId> = pane_ids.into_iter().collect();

    // Streaming receive loop
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    loop {
        match os_input.recv_from_server() {
            Some((
                ServerToClientMsg::PaneRenderUpdate {
                    pane_id,
                    viewport,
                    scrollback,
                    is_initial,
                },
                _,
            )) => match subscribe_cli.format {
                SubscribeFormat::Raw => {
                    if let Some(ref scrollback_lines) = scrollback {
                        for line in scrollback_lines {
                            let _ = writeln!(stdout, "{}", line);
                        }
                    }
                    for line in &viewport {
                        let _ = writeln!(stdout, "{}", line);
                    }
                    let _ = stdout.flush();
                },
                SubscribeFormat::Json => {
                    let json = serde_json::json!({
                        "event": "pane_update",
                        "pane_id": pane_id.to_string(),
                        "viewport": viewport,
                        "scrollback": scrollback,
                        "is_initial": is_initial,
                    });
                    let _ = writeln!(stdout, "{}", json);
                    let _ = stdout.flush();
                },
            },
            Some((ServerToClientMsg::SubscribedPaneClosed { pane_id }, _)) => {
                remaining_panes.remove(&pane_id);
                match subscribe_cli.format {
                    SubscribeFormat::Raw => {},
                    SubscribeFormat::Json => {
                        let json = serde_json::json!({
                            "event": "pane_closed",
                            "pane_id": pane_id.to_string(),
                        });
                        let _ = writeln!(stdout, "{}", json);
                        let _ = stdout.flush();
                    },
                }
                if remaining_panes.is_empty() {
                    break;
                }
            },
            Some((ServerToClientMsg::Exit { .. }, _)) => break,
            Some((ServerToClientMsg::LogError { lines }, _)) => {
                for line in lines {
                    eprintln!("{}", line);
                }
                process::exit(2);
            },
            None => break,
            _ => {},
        }
    }

    os_input.send_to_server(ClientToServerMsg::ClientExited);
}

#[cfg(test)]
mod tests {
    use super::*;
    use zellij_utils::errors::ErrorContext;

    #[test]
    fn generic_unblock_does_not_complete_a_cli_action() {
        assert!(matches!(
            classify_cli_action_response(Some(ServerToClientMsg::UnblockInputThread)),
            CliActionResponse::Wait
        ));
    }

    #[test]
    fn targeted_empty_log_is_an_explicit_success_ack() {
        assert!(matches!(
            classify_cli_action_response(Some(ServerToClientMsg::Log { lines: vec![] })),
            CliActionResponse::Success(lines) if lines.is_empty()
        ));
    }

    #[test]
    fn disconnect_before_acknowledgement_fails_closed() {
        assert!(matches!(
            classify_cli_action_response(None),
            CliActionResponse::Disconnected
        ));
    }
    #[derive(Clone, Debug, Default)]
    struct PipeTestOs {
        received: Arc<std::sync::Mutex<std::collections::VecDeque<ServerToClientMsg>>>,
        sent: Arc<std::sync::Mutex<Vec<ClientToServerMsg>>>,
        env: Arc<std::sync::Mutex<BTreeMap<String, String>>>,
    }

    impl ClientOsApi for PipeTestOs {
        fn get_terminal_size(&self) -> zellij_utils::pane_size::Size {
            Default::default()
        }
        fn set_raw_mode(&mut self) {}
        fn unset_raw_mode(&self) -> io::Result<()> {
            Ok(())
        }
        fn get_stdout_writer(&self) -> Box<dyn Write> {
            Box::new(io::sink())
        }
        fn get_stdin_reader(&self) -> Box<dyn BufRead> {
            panic!("a request must not acquire stdin")
        }
        fn stdin_is_terminal(&self) -> bool {
            false
        }
        fn update_session_name(&mut self, _: String) {}
        fn read_from_stdin(&mut self) -> Result<Vec<u8>, &'static str> {
            panic!("unexpected stdin")
        }
        fn box_clone(&self) -> Box<dyn ClientOsApi> {
            Box::new(self.clone())
        }
        fn send_to_server(&self, message: ClientToServerMsg) {
            self.sent.lock().unwrap().push(message);
        }
        fn recv_from_server(&self) -> Option<(ServerToClientMsg, ErrorContext)> {
            self.received
                .lock()
                .unwrap()
                .pop_front()
                .map(|message| (message, ErrorContext::default()))
        }
        fn handle_signals(
            &self,
            _: Box<dyn Fn()>,
            _: Box<dyn Fn()>,
            _: Box<dyn Fn()>,
            _: Option<std::sync::mpsc::Receiver<()>>,
        ) {
        }
        fn connect_to_server(&self, _: &std::path::Path) {}
        fn env_variable(&self, name: &str) -> Option<String> {
            self.env.lock().unwrap().get(name).cloned()
        }
        fn load_palette(&self) -> zellij_utils::data::Palette {
            Default::default()
        }
        fn enable_mouse(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn disable_mouse(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn request_pipe(messages: Vec<ServerToClientMsg>) -> (i32, String, PipeTestOs) {
        let os = PipeTestOs::default();
        os.received.lock().unwrap().extend(messages);
        let mut input: Box<dyn ClientOsApi> = Box::new(os.clone());
        let mut output = String::new();
        let deadline = ActionDeadline::parked();
        let status = pipe_client(
            &mut input,
            PipeClientParams {
                pipe_id: "this-request".into(),
                name: Some("test".into()),
                payload: Some("payload".into()),
                plugin: None,
                args: None,
                configuration: None,
                launch_new: false,
                skip_cache: false,
                floating: None,
                in_place: None,
                pane_id: None,
                cwd: None,
                pane_title: None,
            },
            CliClientMode::Request,
            &mut output,
            &deadline,
        );
        (status, output, os)
    }

    #[test]
    fn request_pipe_returns_only_addressed_output_without_reading_headless_stdin() {
        let (status, output, os) = request_pipe(vec![
            ServerToClientMsg::CliPipeOutput {
                pipe_name: "old-request".into(),
                output: "stale".into(),
            },
            ServerToClientMsg::UnblockCliPipeInput {
                pipe_name: "old-request".into(),
            },
            ServerToClientMsg::UnblockInputThread,
            ServerToClientMsg::CliPipeOutput {
                pipe_name: "this-request".into(),
                output: "reply".into(),
            },
            ServerToClientMsg::UnblockCliPipeInput {
                pipe_name: "this-request".into(),
            },
        ]);
        assert_eq!(status, 0);
        assert_eq!(output, "reply");
        assert_eq!(os.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn request_pipe_returns_transport_errors_and_eof_instead_of_exiting_or_spinning() {
        for messages in [
            vec![],
            vec![ServerToClientMsg::LogError {
                lines: vec!["refused".into()],
            }],
            vec![ServerToClientMsg::Exit {
                exit_reason: ExitReason::Error("failed".into()),
            }],
        ] {
            let (status, _, _) = request_pipe(messages);
            assert_eq!(status, 2);
        }
        let (status, output, _) = request_pipe(vec![ServerToClientMsg::Log { lines: vec![] }]);
        assert_eq!(status, 0);
        assert!(
            output.is_empty(),
            "a generic log is not an application acknowledgment"
        );
    }

    #[test]
    fn silence_for_a_full_ttl_is_still_a_corpse() {
        assert!(!idle_exceeded(999, 0, 1));
        assert!(idle_exceeded(1_000, 0, 1));
        assert!(idle_exceeded(20_000, 0, 20));
    }

    #[test]
    fn life_after_connect_keeps_a_slow_action_inside_the_window() {
        // 30s after connect, TTL 20: the old connect-anchored clock is already dead.
        // A sign of life at 15s leaves only 15s of idle, so the action stays.
        assert!(!idle_exceeded(30_000, 15_000, 20));
        // A later gap of a full TTL, with no further life, is still a corpse.
        assert!(idle_exceeded(35_000, 15_000, 20));
        assert!(!idle_exceeded(34_999, 15_000, 20));
    }

    #[test]
    fn presented_caller_reaches_the_warden_line() {
        let fork = expired_client_line("vibecrafted-fork", 60);
        assert_eq!(
            fork,
            "warden.expired_client caller=vibecrafted-fork ttl_seconds=60 result=client_self_retired"
        );
        assert!(!fork.contains("anonymous"));
        let workspace = expired_client_line("workspace-project-cli", 20);
        assert!(
            workspace
                .starts_with("warden.expired_client caller=workspace-project-cli ttl_seconds=20 ")
        );
    }

    #[test]
    fn blank_caller_stays_on_the_fallback_and_a_name_does_not() {
        let os = PipeTestOs::default();
        assert_eq!(declared_caller(&os, "anonymous"), "anonymous");
        assert_eq!(declared_caller(&os, "operator"), "operator");
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_CALLER".into(), "   ".into());
        assert_eq!(declared_caller(&os, "anonymous"), "anonymous");
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_CALLER".into(), "vibecrafted-fork".into());
        assert_eq!(declared_caller(&os, "anonymous"), "vibecrafted-fork");
    }

    #[test]
    fn ttl_parses_from_the_client_env_and_clamps() {
        let os = PipeTestOs::default();
        assert_eq!(ttl_seconds(&os), 60);
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_ACTION_TTL_SECONDS".into(), "nope".into());
        assert_eq!(ttl_seconds(&os), 60);
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_ACTION_TTL_SECONDS".into(), "0".into());
        assert_eq!(ttl_seconds(&os), 1);
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_ACTION_TTL_SECONDS".into(), "99999".into());
        assert_eq!(ttl_seconds(&os), 3600);
        os.env
            .lock()
            .unwrap()
            .insert("VC_FRAME_ACTION_TTL_SECONDS".into(), "2".into());
        assert_eq!(ttl_seconds(&os), 2);
    }

    #[test]
    fn a_server_byte_is_life_and_a_wedged_recv_is_not() {
        let os = PipeTestOs::default();
        let deadline = ActionDeadline::parked();
        // The monotonic clock can still be 0 in the first millisecond, so the
        // sentinel has to be a value note_life will not write.
        let frozen = u64::MAX / 2;
        deadline.last_life_ms.store(frozen, Ordering::Release);
        assert!(recv_with_life(&os, &deadline).is_none());
        assert_eq!(deadline.last_life_ms.load(Ordering::Acquire), frozen);

        os.received
            .lock()
            .unwrap()
            .push_back(ServerToClientMsg::UnblockInputThread);
        assert!(recv_with_life(&os, &deadline).is_some());
        assert_ne!(deadline.last_life_ms.load(Ordering::Acquire), frozen);

        deadline.last_life_ms.store(frozen, Ordering::Release);
        send_with_life(&os, &deadline, ClientToServerMsg::ClientExited);
        assert_ne!(deadline.last_life_ms.load(Ordering::Acquire), frozen);
        assert_eq!(os.sent.lock().unwrap().len(), 1);
    }
}
