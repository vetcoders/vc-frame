use super::*;
use zellij_utils::input::command::RunCommand;
use zellij_utils::ipc::ServerToClientMsg;

fn make_server() -> ServerOsInputOutput {
    get_server_os_input().expect("failed to create server os input")
}

// --- Cross-platform command helpers ---

#[allow(dead_code)]
#[cfg(not(windows))]
fn long_running_cmd() -> Command {
    let mut cmd = Command::new("sleep");
    cmd.arg("60");
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn long_running_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("timeout");
    cmd.args(&["/T", "60"]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

#[allow(dead_code)]
#[cfg(not(windows))]
fn echo_cmd(msg: &str) -> Command {
    let mut cmd = Command::new("echo");
    cmd.arg(msg);
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn echo_cmd(msg: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("cmd");
    cmd.args(&["/C", "echo", msg]);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

#[allow(dead_code)]
#[cfg(not(windows))]
fn stdin_reader_cmd() -> Command {
    let mut cmd = Command::new("cat");
    cmd.stdin(std::process::Stdio::piped());
    cmd
}

#[allow(dead_code)]
#[cfg(windows)]
fn stdin_reader_cmd() -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new("findstr");
    cmd.arg("/R").arg(".*");
    cmd.stdin(std::process::Stdio::piped());
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd
}

// Unix socket fixtures must not inherit the process TMPDIR. On macOS that is a
// long /var/folders/.../T path; production ZELLIJ_TMP_DIR, e2e sockets, and the
// Python triage harness already parent under /tmp so sockaddr_un.sun_path fits.
#[cfg(unix)]
struct UnixTestSocket {
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
}

#[cfg(unix)]
fn unix_test_socket(label: &str) -> UnixTestSocket {
    use std::os::unix::ffi::OsStrExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use zellij_utils::consts::ZELLIJ_SOCK_MAX_LENGTH;

    static SOCKET_SERIAL: AtomicU32 = AtomicU32::new(0);

    assert!(
        !label.is_empty()
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "socket label must stay short and filesystem-safe: {label}"
    );

    let dir = tempfile::Builder::new()
        .prefix("vcf-")
        .rand_bytes(4)
        .tempdir_in("/tmp")
        .expect("short unix socket temp dir");
    let serial = SOCKET_SERIAL.fetch_add(1, Ordering::Relaxed);
    let path = dir
        .path()
        .join(format!("{label}-{}-{serial}.sock", std::process::id()));
    let bytes = path.as_os_str().as_bytes().len();
    assert!(
        bytes < ZELLIJ_SOCK_MAX_LENGTH,
        "fixture socket {} is {bytes} bytes; sun_path budget is {ZELLIJ_SOCK_MAX_LENGTH}",
        path.display()
    );
    UnixTestSocket { _dir: dir, path }
}

#[cfg(unix)]
#[test]
fn unix_test_socket_paths_stay_within_sun_path_and_are_distinct() {
    use std::os::unix::ffi::OsStrExt;
    use zellij_utils::consts::ZELLIJ_SOCK_MAX_LENGTH;

    let first = unix_test_socket("reg");
    let second = unix_test_socket("reg");
    let first_len = first.path.as_os_str().as_bytes().len();
    let second_len = second.path.as_os_str().as_bytes().len();
    assert!(
        first_len < ZELLIJ_SOCK_MAX_LENGTH,
        "first fixture {} is {first_len} bytes; sun_path budget is {ZELLIJ_SOCK_MAX_LENGTH}",
        first.path.display()
    );
    assert!(
        second_len < ZELLIJ_SOCK_MAX_LENGTH,
        "second fixture {} is {second_len} bytes; sun_path budget is {ZELLIJ_SOCK_MAX_LENGTH}",
        second.path.display()
    );
    assert_ne!(
        first.path, second.path,
        "parallel fixtures must not share a socket path"
    );
    assert_ne!(
        first.path.parent(),
        second.path.parent(),
        "parallel fixtures must own distinct temp directories"
    );

    let process_temp = std::env::temp_dir();
    let process_temp_is_short_root = process_temp == std::path::Path::new("/tmp")
        || process_temp == std::path::Path::new("/private/tmp");
    assert!(
        !first.path.starts_with(&process_temp) || process_temp_is_short_root,
        "fixture {} must not inherit a long process temp_dir {}",
        first.path.display(),
        process_temp.display()
    );

    let legacy = process_temp.join(format!(
        "client-backpressure-{}-{}.sock",
        std::process::id(),
        u128::MAX
    ));
    assert!(
        first_len < legacy.as_os_str().as_bytes().len(),
        "short fixture ({first_len}) must beat inherited TMPDIR+legacy name ({})",
        legacy.as_os_str().as_bytes().len()
    );
}

#[test]
fn get_cwd() {
    let server = make_server();

    let pid = std::process::id();
    assert!(
        server.get_cwd(pid).is_some(),
        "Get current working directory from PID {}",
        pid
    );
}

#[test]
fn failed_spawn_releases_reservation_except_command_not_found() {
    let mut cleared_terminal_ids = Vec::new();

    let ordinary_failure = resolve_reserved_terminal_spawn(
        41,
        Err::<(), _>(anyhow::Error::new(std::io::Error::other(
            "injected backend spawn failure",
        ))),
        |terminal_id| cleared_terminal_ids.push(terminal_id),
    )
    .expect_err("ordinary spawn failures must remain errors");
    assert!(
        ordinary_failure
            .to_string()
            .contains("injected backend spawn failure")
    );
    assert_eq!(cleared_terminal_ids, vec![41]);

    let command_not_found = resolve_reserved_terminal_spawn(
        42,
        Err::<(), _>(anyhow::Error::new(ZellijError::CommandNotFound {
            terminal_id: 42,
            command: "missing-command".to_owned(),
        })),
        |terminal_id| cleared_terminal_ids.push(terminal_id),
    )
    .expect_err("CommandNotFound remains an error for the pane hold path");
    assert!(matches!(
        command_not_found.downcast_ref::<ZellijError>(),
        Some(ZellijError::CommandNotFound {
            terminal_id: 42,
            ..
        })
    ));
    assert_eq!(
        cleared_terminal_ids,
        vec![41],
        "CommandNotFound deliberately transfers its reserved id to the pane hold path"
    );

    let mismatched_command_not_found = resolve_reserved_terminal_spawn(
        43,
        Err::<(), _>(anyhow::Error::new(ZellijError::CommandNotFound {
            terminal_id: 99,
            command: "mismatched-command".to_owned(),
        })),
        |terminal_id| cleared_terminal_ids.push(terminal_id),
    )
    .expect_err("a mismatched CommandNotFound id remains an error");
    assert!(
        mismatched_command_not_found
            .downcast_ref::<ZellijError>()
            .is_none(),
        "a foreign terminal id must be normalized to an ordinary protocol error"
    );
    assert!(
        mismatched_command_not_found
            .to_string()
            .contains("reserved terminal 43"),
        "the normalized error must identify the reservation"
    );
    assert_eq!(
        cleared_terminal_ids,
        vec![41, 43],
        "only CommandNotFound for the exact reservation may retain ownership"
    );
}

// --- Signal delivery tests ---

#[cfg(not(windows))]
#[test]
fn kill_sends_sighup_to_process() {
    let child = long_running_cmd()
        .spawn()
        .expect("failed to spawn long-running process");
    let pid = child.id();
    let waiter = std::thread::spawn(move || child.wait_with_output());

    let server = make_server();

    server.kill(pid).expect("kill should succeed");
    waiter
        .join()
        .expect("child waiter must not panic")
        .expect("child must be reaped after SIGHUP");
}

#[cfg(not(windows))]
#[test]
fn kill_escalates_to_sigkill_and_confirms_exit_when_child_ignores_sighup() {
    let ready_file = tempfile::NamedTempFile::new().expect("create child readiness file");
    let ready_path = ready_file.path().to_path_buf();
    let child = Command::new("sh")
        .args([
            "-c",
            "trap '' HUP; printf ready > \"$1\"; exec sleep 60",
            "vc-frame-test-shell",
        ])
        .arg(&ready_path)
        .spawn()
        .expect("spawn SIGHUP-ignoring child");
    for _ in 0..100 {
        if std::fs::metadata(&ready_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        std::fs::metadata(&ready_path)
            .map(|metadata| metadata.len() > 0)
            .unwrap_or(false),
        "child must install its SIGHUP disposition before the probe"
    );

    let pid = child.id();
    let waiter = std::thread::spawn(move || child.wait_with_output());
    let server = make_server();
    server
        .kill(pid)
        .expect("ignored SIGHUP must escalate and confirm exact process exit");
    let output = waiter
        .join()
        .expect("child waiter must not panic")
        .expect("child waiter must reap the escalated process");
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "a child ignoring SIGHUP must be terminated by the bounded SIGKILL escalation"
    );
}

#[cfg(not(windows))]
#[test]
fn force_kill_sends_sigkill_to_process() {
    let mut child = long_running_cmd()
        .spawn()
        .expect("failed to spawn long-running process");
    let pid = child.id();

    let server = make_server();

    server.force_kill(pid).expect("force_kill should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = child.wait();
}

#[cfg(not(windows))]
#[test]
fn send_sigint_to_process() {
    let mut child = stdin_reader_cmd()
        .spawn()
        .expect("failed to spawn stdin-reader process");
    let pid = child.id();

    let server = make_server();

    server.send_sigint(pid).expect("send_sigint should succeed");

    std::thread::sleep(std::time::Duration::from_millis(100));
    let _ = child.wait();
}

#[test]
fn spawn_and_read_output() {
    use crate::panes::PaneId;
    use zellij_utils::input::command::TerminalAction;

    let server = make_server();
    let test_message = "hello_zellij_test";

    #[cfg(not(windows))]
    let cmd = RunCommand {
        command: PathBuf::from("echo"),
        args: vec![test_message.to_string()],
        ..Default::default()
    };
    #[cfg(windows)]
    let cmd = RunCommand {
        command: PathBuf::from("cmd"),
        args: vec![
            "/K".to_string(),
            "echo".to_string(),
            test_message.to_string(),
        ],
        ..Default::default()
    };

    let action = TerminalAction::RunCommand(cmd);
    let quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send> =
        Box::new(|_pane_id, _exit_status, _run_command| {});

    let (_terminal_id, mut reader, _child_pid) = server
        .spawn_terminal(action, quit_cb, None)
        .expect("spawn_terminal should succeed");

    // Read output from the spawned terminal
    let mut output = Vec::new();
    let mut buf = [0u8; 4096];
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    rt.block_on(async {
        loop {
            if std::time::Instant::now() > deadline {
                break;
            }
            match tokio::time::timeout(std::time::Duration::from_millis(500), reader.read(&mut buf))
                .await
            {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    output.extend_from_slice(&buf[..n]);
                    let s = String::from_utf8_lossy(&output);
                    if s.contains(test_message) {
                        break;
                    }
                },
                Ok(Err(_)) => break,
                Err(_) => {
                    // timeout — check if we already have enough
                    let s = String::from_utf8_lossy(&output);
                    if s.contains(test_message) {
                        break;
                    }
                },
            }
        }
    });

    let output_str = String::from_utf8_lossy(&output);
    assert!(
        output_str.contains(test_message),
        "expected output to contain '{}', got: '{}'",
        test_message,
        output_str
    );
}

#[cfg(unix)]
#[test]
fn send_to_client_fails_closed_after_peer_hangup() {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::time::{Duration, Instant};
    use zellij_utils::ipc::ServerToClientMsg;

    let sock = unix_test_socket("hup");
    let listener = ListenerOptions::new()
        .name(
            sock.path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name"),
        )
        .create_sync()
        .expect("bind");

    let connect_path = sock.path.clone();
    let client = std::thread::spawn(move || {
        let stream = interprocess::local_socket::Stream::connect(
            connect_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("connect name"),
        )
        .expect("connect");
        std::thread::sleep(Duration::from_millis(30));
        drop(stream);
    });

    let stream = listener
        .incoming()
        .next()
        .expect("incoming")
        .expect("accept");
    let mut server = make_server();
    server.new_client(1, stream).expect("register client");
    client.join().expect("client thread");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_hangup = false;
    while Instant::now() < deadline {
        match server.send_to_client(1, ServerToClientMsg::UnblockInputThread) {
            Ok(()) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                saw_hangup = true;
                break;
            },
        }
    }
    assert!(
        saw_hangup,
        "a hung-up client must fail send_to_client so the 5000-deep render buffer is dropped"
    );

    server
        .send_to_client(1, ServerToClientMsg::UnblockInputThread)
        .expect("missing sender is a no-op after hangup eviction");
}

#[cfg(unix)]
#[test]
fn send_to_client_keeps_sender_on_backpressure() {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::time::{Duration, Instant};
    use zellij_utils::ipc::ServerToClientMsg;

    let sock = unix_test_socket("bp");
    let listener = ListenerOptions::new()
        .name(
            sock.path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name"),
        )
        .create_sync()
        .expect("bind");

    let connect_path = sock.path.clone();
    let client = std::thread::spawn(move || {
        let stream = interprocess::local_socket::Stream::connect(
            connect_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("connect name"),
        )
        .expect("connect");
        // Stay connected and silent so the 5000-slot pump backs up.
        std::thread::sleep(Duration::from_secs(3));
        drop(stream);
    });

    let stream = listener
        .incoming()
        .next()
        .expect("incoming")
        .expect("accept");
    let mut server = make_server();
    server.new_client(1, stream).expect("register client");

    let bulky = ServerToClientMsg::Render {
        content: "x".repeat(8192),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_backpressure = false;
    while Instant::now() < deadline {
        match server.send_to_client(1, bulky.clone()) {
            Ok(()) => {},
            Err(error) => {
                assert!(
                    super::client_send_is_backpressure(&error),
                    "buffer-full must be ClientTooSlow, got {error:?}"
                );
                saw_backpressure = true;
                break;
            },
        }
    }
    assert!(
        saw_backpressure,
        "a silent live peer must back up the buffer"
    );

    let again = server.send_to_client(1, bulky);
    assert!(
        again.is_err(),
        "sender must remain registered after backpressure"
    );
    assert!(
        super::client_send_is_backpressure(&again.unwrap_err()),
        "a second overflow is still backpressure, not a missing-sender no-op"
    );

    client.join().expect("client thread");
}

fn render_delta(tag: &str) -> ServerToClientMsg {
    ServerToClientMsg::Render {
        content: tag.to_owned(),
    }
}

fn control_log(tag: &str) -> ServerToClientMsg {
    ServerToClientMsg::Log {
        lines: vec![tag.to_owned()],
    }
}

fn resync_render() -> ServerToClientMsg {
    ServerToClientMsg::Render {
        content: "\u{1b}[2JSYNC".to_owned(),
    }
}

#[test]
fn client_mailbox_evicts_display_to_deliver_unblock() {
    use super::{ClientMailbox, MailboxEnqueue};

    let mailbox = ClientMailbox::with_capacity(3);
    assert_eq!(
        mailbox.try_enqueue(render_delta("a")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(render_delta("b")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(render_delta("c")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: true
        }
    );
    assert_eq!(mailbox.queued_len(), 3, "capacity must stay 3");
    assert!(mailbox.take_dropped_render());

    let first = mailbox.recv().expect("oldest remaining delta");
    assert_eq!(first, render_delta("b"), "oldest Render was evicted");
    assert_eq!(mailbox.recv(), Some(render_delta("c")));
    assert_eq!(
        mailbox.recv(),
        Some(ServerToClientMsg::UnblockInputThread),
        "control must arrive after remaining deltas, not be dropped"
    );
}

#[test]
fn client_mailbox_drops_incoming_render_and_stays_bounded() {
    use super::{ClientMailbox, MailboxEnqueue};

    let mailbox = ClientMailbox::with_capacity(2);
    mailbox.try_enqueue(render_delta("a"));
    mailbox.try_enqueue(render_delta("b"));
    assert_eq!(
        mailbox.try_enqueue(render_delta("c")),
        MailboxEnqueue::Congested {
            dropped_render: true
        }
    );
    assert_eq!(mailbox.queued_len(), 2);
    mailbox.try_enqueue(render_delta("d"));
    mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread);
    mailbox.try_enqueue(render_delta("e"));
    assert!(
        mailbox.queued_len() <= 2,
        "mailbox must never grow past capacity, got {}",
        mailbox.queued_len()
    );
}

#[test]
fn client_mailbox_coalesces_duplicate_unblock_when_full_of_progress() {
    use super::{ClientMailbox, MailboxEnqueue};

    let mailbox = ClientMailbox::with_capacity(1);
    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(mailbox.queued_len(), 1);
}

#[test]
fn client_mailbox_hangup_abandons_queued_memory() {
    use super::ClientMailbox;

    let mailbox = ClientMailbox::with_capacity(4);
    mailbox.try_enqueue(render_delta("a"));
    mailbox.try_enqueue(render_delta("b"));
    mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread);
    assert_eq!(mailbox.queued_len(), 3);
    mailbox.abandon_queue();
    assert_eq!(mailbox.queued_len(), 0);
    assert!(mailbox.is_closed());
    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        super::MailboxEnqueue::Closed
    );
    assert!(mailbox.recv().is_none());
}

#[test]
fn client_mailbox_latches_unblock_when_full_of_non_display_controls() {
    use super::{ClientMailbox, MailboxEnqueue};

    let mailbox = ClientMailbox::with_capacity(2);
    assert_eq!(
        mailbox.try_enqueue(control_log("a")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("b")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("c")),
        MailboxEnqueue::Congested {
            dropped_render: false
        },
        "non-progress must fail observably, not enqueue past capacity"
    );
    assert_eq!(mailbox.queued_len(), 2);
    assert_eq!(mailbox.latched_progress_count(), 0);

    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        },
        "input waiter must latch rather than Congested"
    );
    assert_eq!(
        mailbox.queued_len(),
        2,
        "latch is bounded memory, not a side queue"
    );
    assert!(mailbox.latched_unblock_input());
    assert_eq!(mailbox.latched_progress_count(), 1);

    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        },
        "duplicate Unblock coalesces on the latch"
    );
    assert_eq!(mailbox.latched_progress_count(), 1);

    assert_eq!(
        mailbox.recv(),
        Some(control_log("a")),
        "oldest control drains first"
    );
    assert!(
        mailbox.latched_unblock_input(),
        "recv-pop does not lower occupancy; latch must wait for finish slack"
    );
    assert_eq!(
        mailbox.queued_len() + mailbox.in_flight_len(),
        2,
        "queue+in_flight stay at capacity across the pop"
    );
    mailbox.finish_in_flight();
    assert!(
        !mailbox.latched_unblock_input(),
        "finish slack services already-accepted Unblock before the next recv"
    );
    assert_eq!(mailbox.recv(), Some(control_log("b")));
    assert_eq!(
        mailbox.recv(),
        Some(ServerToClientMsg::UnblockInputThread),
        "latched progress is delivered after occupancy slack, not lost"
    );
}

#[test]
fn client_mailbox_counts_in_flight_against_capacity() {
    use super::{ClientMailbox, MailboxEnqueue};

    let mailbox = ClientMailbox::with_capacity(2);
    assert_eq!(
        mailbox.try_enqueue(control_log("a")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("b")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("c")),
        MailboxEnqueue::Congested {
            dropped_render: false
        }
    );

    assert_eq!(mailbox.recv(), Some(control_log("a")));
    assert_eq!(mailbox.queued_len(), 1);
    assert_eq!(
        mailbox.in_flight_len(),
        1,
        "pump pop occupies capacity until send_server_msg returns"
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("overflow")),
        MailboxEnqueue::Congested {
            dropped_render: false
        },
        "a popped control still in flight occupies capacity; queue-only accounting would report success"
    );
    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        },
        "progress must still latch when outstanding is at capacity"
    );
    mailbox.finish_in_flight();
    assert_eq!(mailbox.in_flight_len(), 0);
    // Fair contract: already-accepted Unblock claims the finish slack.
    // The previous expectation that after-send Enqueues here is enqueue-first
    // starvation — that slot would refill forever and Unblock never reach
    // the client (frozen input under continuous output).
    assert!(
        !mailbox.latched_unblock_input(),
        "finish slack services the already-accepted Unblock"
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("after-send")),
        MailboxEnqueue::Congested {
            dropped_render: false
        },
        "incoming must not win the freed slot over already-accepted Unblock"
    );
    assert_eq!(
        mailbox.occupied_len(),
        2,
        "queued b plus flushed Unblock occupy the bound"
    );
    assert_eq!(mailbox.recv(), Some(control_log("b")));
    mailbox.finish_in_flight();
    assert_eq!(
        mailbox.recv(),
        Some(ServerToClientMsg::UnblockInputThread),
        "Enqueued Unblock is a delivery obligation, not a dropped control"
    );
    mailbox.finish_in_flight();
    assert_eq!(
        mailbox.try_enqueue(control_log("after-send")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        },
        "capacity returns after the pending control is drained and its in-flight send completes"
    );
    assert_eq!(mailbox.occupied_len(), 1);
}

#[test]
fn client_mailbox_latched_unblock_survives_sustained_producer() {
    use super::{ClientMailbox, MailboxEnqueue};

    // Deterministic starvation schedule at capacity 2 under enqueue-first:
    // queued b, inflight a, pending Unblock. finish(a) leaves a slot;
    // incoming fills it; recv(b) leaves occupied full; finish(b) leaves a
    // slot; next producer fills it; repeat. Unblock was Enqueued but never
    // reaches the client — frozen input under continuous output.
    //
    // Fair contract: already-accepted Unblock claims finish slack. A
    // sustained producer must not prevent delivery within a bounded number
    // of pump turns (finish + offer + recv), without producer quiescence.
    let mailbox = ClientMailbox::with_capacity(2);
    assert_eq!(
        mailbox.try_enqueue(control_log("a")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("b")),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        }
    );
    assert_eq!(mailbox.recv(), Some(control_log("a")));
    assert_eq!(mailbox.queued_len(), 1);
    assert_eq!(mailbox.in_flight_len(), 1);
    assert_eq!(mailbox.occupied_len(), 2);

    assert_eq!(
        mailbox.try_enqueue(ServerToClientMsg::UnblockInputThread),
        MailboxEnqueue::Enqueued {
            dropped_render: false
        },
        "progress latches while queue+in_flight are at capacity"
    );
    assert!(mailbox.latched_unblock_input());
    assert_eq!(
        mailbox.queued_len(),
        1,
        "latch is not a side queue and must not grow occupancy"
    );
    assert_eq!(
        mailbox.try_enqueue(control_log("overflow")),
        MailboxEnqueue::Congested {
            dropped_render: false
        },
        "non-progress past occupied capacity stays an honest failure"
    );

    const BOUND: usize = 4; // 2 * capacity; enqueue-first never delivers
    let mut delivered = false;
    for turn in 0..BOUND {
        mailbox.finish_in_flight();
        match mailbox.try_enqueue(control_log(&format!("p{turn}"))) {
            MailboxEnqueue::Congested { .. } | MailboxEnqueue::Enqueued { .. } => {}
            MailboxEnqueue::Closed => panic!("mailbox closed under a live producer"),
        }
        assert!(
            mailbox.occupied_len() <= 2,
            "queue+in_flight must stay at capacity, got {}",
            mailbox.occupied_len()
        );
        match mailbox.recv() {
            Some(ServerToClientMsg::UnblockInputThread) => {
                delivered = true;
                break;
            },
            Some(_) => {},
            None => panic!("mailbox closed before Unblock"),
        }
        assert!(
            mailbox.occupied_len() <= 2,
            "queue+in_flight must stay at capacity after recv, got {}",
            mailbox.occupied_len()
        );
    }
    assert!(
        delivered,
        "Unblock must be delivered within {BOUND} pump turns under a sustained producer"
    );
    assert!(mailbox.occupied_len() <= 2);
}

#[cfg(unix)]
#[test]
fn send_to_client_delivers_control_after_peer_drains_then_resync_render() {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use zellij_utils::ipc::{IpcReceiverWithContext, ServerToClientMsg};

    let sock = unix_test_socket("drn");
    let listener = ListenerOptions::new()
        .name(
            sock.path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name"),
        )
        .create_sync()
        .expect("bind");

    let connect_path = sock.path.clone();
    let start_drain = Arc::new(AtomicBool::new(false));
    let client_start = start_drain.clone();
    let client =
        std::thread::spawn(move || {
            let stream = interprocess::local_socket::Stream::connect(
                connect_path
                    .as_path()
                    .to_fs_name::<GenericFilePath>()
                    .expect("connect name"),
            )
            .expect("connect");
            while !client_start.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut receiver = IpcReceiverWithContext::<ServerToClientMsg>::new(stream);
            let mut got = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if let Some((msg, _)) = receiver.recv_server_msg() {
                    let saw_unblock = matches!(msg, ServerToClientMsg::UnblockInputThread);
                    let saw_resync = matches!(
                        &msg,
                        ServerToClientMsg::Render { content } if content.contains("\u{1b}[2J")
                    );
                    got.push(msg);
                    if saw_unblock && got.iter().any(|queued| {
                        matches!(
                            queued,
                            ServerToClientMsg::Render { content } if content.contains("\u{1b}[2J")
                        )
                    }) || saw_resync
                        && got
                            .iter()
                            .any(|queued| matches!(queued, ServerToClientMsg::UnblockInputThread))
                    {
                        break;
                    }
                }
            }
            got
        });

    let stream = listener
        .incoming()
        .next()
        .expect("incoming")
        .expect("accept");
    let mut server = make_server();
    const CAPACITY: usize = 8;
    server
        .register_client_with_capacity(1, stream, CAPACITY)
        .expect("register client");

    let bulky = ServerToClientMsg::Render {
        content: "x".repeat(256),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_backpressure = false;
    while Instant::now() < deadline {
        match server.send_to_client(1, bulky.clone()) {
            Ok(()) => {},
            Err(error) => {
                assert!(
                    super::client_send_is_backpressure(&error),
                    "buffer-full must be ClientTooSlow, got {error:?}"
                );
                saw_backpressure = true;
                break;
            },
        }
    }
    assert!(
        saw_backpressure,
        "a silent live peer must back up the buffer"
    );
    let queued = server
        .client_queue_len(1)
        .expect("sender must remain registered through congestion");
    assert!(
        queued <= CAPACITY,
        "queued {queued} exceeded capacity {CAPACITY}"
    );
    assert!(
        server.display_resync_pending(),
        "dropping a Render delta must request CSI-2J resync"
    );

    server
        .send_to_client(1, ServerToClientMsg::UnblockInputThread)
        .expect("progress control must be enqueued by evicting a display delta");
    assert!(server.client_queue_len(1).expect("owner sender stays") <= CAPACITY);

    start_drain.store(true, Ordering::SeqCst);

    // Queue-only wait races the in-flight write: one recv drops queued_len
    // below capacity while occupied is still full, so a resync Render is
    // Congested. Wait for queue+in_flight slack, not send completion by sleep.
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < drain_deadline {
        if server
            .client_occupied_len(1)
            .map(|len| len < CAPACITY)
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    server
        .send_to_client(1, resync_render())
        .expect("after the peer drains, a clear+repaint Render must enqueue");
    std::thread::sleep(Duration::from_millis(200));
    server
        .remove_client(1)
        .expect("closing the pump unblocks a late recv");
    assert!(
        server.client_queue_len(1).is_none(),
        "hangup/remove must free the mailbox, not keep a zombie sender"
    );

    let received = client.join().expect("client thread");
    assert!(
        received
            .iter()
            .any(|msg| matches!(msg, ServerToClientMsg::UnblockInputThread)),
        "UnblockInputThread must arrive after congestion, got {received:?}"
    );
    assert!(
        received.iter().any(|msg| matches!(
            msg,
            ServerToClientMsg::Render { content } if content.contains("\u{1b}[2J")
        )),
        "usable coherent rendering is CSI-2J plus a later paint, got {received:?}"
    );
}

#[cfg(unix)]
#[test]
fn send_to_client_reports_honest_progress_when_full_of_controls() {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use zellij_utils::ipc::{IpcReceiverWithContext, ServerToClientMsg};

    let sock = unix_test_socket("ctl");
    let listener = ListenerOptions::new()
        .name(
            sock.path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name"),
        )
        .create_sync()
        .expect("bind");

    let connect_path = sock.path.clone();
    let start_drain = Arc::new(AtomicBool::new(false));
    let client_start = start_drain.clone();
    let client = std::thread::spawn(move || {
        let stream = interprocess::local_socket::Stream::connect(
            connect_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("connect name"),
        )
        .expect("connect");
        while !client_start.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut receiver = IpcReceiverWithContext::<ServerToClientMsg>::new(stream);
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some((msg, _)) = receiver.recv_server_msg() {
                let saw_unblock = matches!(msg, ServerToClientMsg::UnblockInputThread);
                got.push(msg);
                if saw_unblock {
                    break;
                }
            }
        }
        got
    });

    let stream = listener
        .incoming()
        .next()
        .expect("incoming")
        .expect("accept");
    let mut server = make_server();
    const CAPACITY: usize = 2;
    server
        .register_client_with_capacity(1, stream, CAPACITY)
        .expect("register client");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_backpressure = false;
    let mut n = 0usize;
    while Instant::now() < deadline {
        match server.send_to_client(1, control_log(&n.to_string())) {
            Ok(()) => n += 1,
            Err(error) => {
                assert!(
                    super::client_send_is_backpressure(&error),
                    "control saturation must be ClientTooSlow, got {error:?}"
                );
                saw_backpressure = true;
                break;
            },
        }
    }
    assert!(
        saw_backpressure,
        "a silent live peer must back up a mailbox of non-display controls"
    );
    let lost = server.send_to_client(1, control_log("overflow"));
    assert!(
        lost.is_err(),
        "a non-progress control past capacity must not report success"
    );
    assert!(
        super::client_send_is_backpressure(&lost.unwrap_err()),
        "observable failure is ClientTooSlow, not a healthy no-op"
    );
    server
        .send_to_client(1, ServerToClientMsg::UnblockInputThread)
        .expect("progress must latch as Ok so let _ = send cannot hang the waiter");
    assert!(
        server
            .client_queue_len(1)
            .expect("owner sender stays registered")
            <= CAPACITY
    );

    start_drain.store(true, Ordering::SeqCst);
    server
        .remove_client(1)
        .expect("closing the pump unblocks a late recv");
    let received = client.join().expect("client thread");
    assert!(
        received
            .iter()
            .any(|msg| matches!(msg, ServerToClientMsg::UnblockInputThread)),
        "latched UnblockInputThread must arrive after controls drain, got {received:?}"
    );
}

#[cfg(unix)]
#[test]
fn send_to_client_schedules_resync_after_direct_display_drop() {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use zellij_utils::ipc::ServerToClientMsg;

    let sock = unix_test_socket("rsy");
    let listener = ListenerOptions::new()
        .name(
            sock.path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("socket name"),
        )
        .create_sync()
        .expect("bind");

    let connect_path = sock.path.clone();
    let client = std::thread::spawn(move || {
        let stream = interprocess::local_socket::Stream::connect(
            connect_path
                .as_path()
                .to_fs_name::<GenericFilePath>()
                .expect("connect name"),
        )
        .expect("connect");
        std::thread::sleep(Duration::from_secs(2));
        drop(stream);
    });

    let stream = listener
        .incoming()
        .next()
        .expect("incoming")
        .expect("accept");
    let scheduled = Arc::new(AtomicUsize::new(0));
    let mut server = make_server();
    server.bind_resync_render({
        let scheduled = scheduled.clone();
        Arc::new(move || {
            scheduled.fetch_add(1, Ordering::SeqCst);
        })
    });
    const CAPACITY: usize = 4;
    server
        .register_client_with_capacity(1, stream, CAPACITY)
        .expect("register client");

    let bulky = ServerToClientMsg::Render {
        content: "x".repeat(256),
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw_backpressure = false;
    while Instant::now() < deadline {
        match server.send_to_client(1, bulky.clone()) {
            Ok(()) => {},
            Err(error) => {
                assert!(
                    super::client_send_is_backpressure(&error),
                    "buffer-full must be ClientTooSlow, got {error:?}"
                );
                saw_backpressure = true;
                break;
            },
        }
    }
    assert!(
        saw_backpressure,
        "a silent live peer must back up the buffer"
    );
    assert!(
        server.display_resync_pending(),
        "direct send_to_client must mark CSI-2J resync"
    );
    assert!(
        scheduled.load(Ordering::SeqCst) > 0,
        "direct send_to_client must schedule existing render authority without a later user action"
    );

    server.remove_client(1).expect("remove");
    client.join().expect("client thread");
}
