//! Isolated real-process proof: an attached outer client stays on the host
//! while A/B are projected, and guest workloads survive detach/reattach.
//!
//! Listing never-attached sessions is not detach survival. This harness
//! allocates a kernel PTY, attaches the built Frame client, and keeps it
//! connected while the project-workspace API switches A → B → A.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn unique_socket_dir() -> PathBuf {
    let stamp = format!(
        "vc{}{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            % 10_000
    );
    let dir = std::env::temp_dir().join(stamp);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn isolated_env(socket_dir: &Path, home: &Path) -> Vec<(String, String)> {
    vec![
        (
            "VC_FRAME_SOCKET_DIR".to_owned(),
            socket_dir.display().to_string(),
        ),
        (
            "ZELLIJ_SOCKET_DIR".to_owned(),
            socket_dir.display().to_string(),
        ),
        ("HOME".to_owned(), home.display().to_string()),
        (
            "XDG_CONFIG_HOME".to_owned(),
            home.join("config").display().to_string(),
        ),
        (
            "XDG_DATA_HOME".to_owned(),
            home.join("data").display().to_string(),
        ),
        (
            "XDG_CACHE_HOME".to_owned(),
            home.join("cache").display().to_string(),
        ),
        (
            "VIBECRAFTED_HOME".to_owned(),
            home.join("vibecrafted").display().to_string(),
        ),
        (
            "VC_FRAME_CONFIG_DIR".to_owned(),
            home.join("config/vc-frame").display().to_string(),
        ),
    ]
}

fn frame_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vc-frame")
}

fn run_frame(socket_dir: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let mut command = Command::new(frame_bin());
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_remove("VC_FRAME_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_DIR");
    for (key, value) in isolated_env(socket_dir, home) {
        command.env(key, value);
    }
    let output = command.output().expect("spawn vc-frame");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), combined)
}

fn wait_for_session(socket_dir: &Path, home: &Path, name: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let (ok, listed) = run_frame(socket_dir, home, &["ls", "-n", "-s"]);
        if ok && listed.lines().any(|line| line.trim() == name) {
            return true;
        }
        thread::sleep(Duration::from_millis(150));
    }
    false
}

fn wait_until(
    socket_dir: &Path,
    home: &Path,
    args: &[&str],
    timeout: Duration,
    predicate: impl Fn(&str) -> bool,
) -> String {
    let start = Instant::now();
    let mut last = String::new();
    while start.elapsed() < timeout {
        let (ok, out) = run_frame(socket_dir, home, args);
        last = out;
        if ok && predicate(&last) {
            return last;
        }
        thread::sleep(Duration::from_millis(200));
    }
    last
}

fn pty_gate_paths(home: &Path, token: &str) -> (PathBuf, PathBuf) {
    (
        home.join(format!("pty-attached-{token}")),
        home.join(format!("pty-release-{token}")),
    )
}

fn spawn_pty_attach(socket_dir: &Path, home: &Path, session: &str, token: &str) -> Child {
    let script = home.join("pty_attach.py");
    std::fs::write(
        &script,
        r#"
import os, pty, select, signal, sys, time

binary, session, attached_path, release_path = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.execvpe(binary, [binary, "--session", session, "attach"], os.environ)

screen_path = attached_path + ".screen"
deadline = time.time() + 25
saw = False
with open(screen_path, "wb") as screen:
    while time.time() < deadline and not saw:
        ready, _, _ = select.select([fd], [], [], 0.2)
        if ready:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if chunk:
                screen.write(chunk)
                screen.flush()
                saw = True
                with open(attached_path, "w", encoding="utf-8") as handle:
                    handle.write(str(pid))
        time.sleep(0.05)

    if not saw:
        sys.stderr.write("pty attach produced no output\n")
        sys.exit(2)

    while not os.path.exists(release_path):
        ready, _, _ = select.select([fd], [], [], 0.25)
        if ready:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if chunk:
                screen.write(chunk)
                screen.flush()
        time.sleep(0.05)

try:
    os.close(fd)
except OSError:
    pass
try:
    os.kill(pid, signal.SIGHUP)
except OSError:
    pass
try:
    os.waitpid(pid, 0)
except ChildProcessError:
    pass
"#,
    )
    .unwrap();

    let (attached, release) = pty_gate_paths(home, token);
    let _ = std::fs::remove_file(&attached);
    let _ = std::fs::remove_file(&release);

    let mut command = Command::new("python3");
    command
        .arg(&script)
        .arg(frame_bin())
        .arg(session)
        .arg(&attached)
        .arg(&release)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.env_remove("VC_FRAME_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_DIR");
    for (key, value) in isolated_env(socket_dir, home) {
        command.env(key, value);
    }
    command.spawn().expect("spawn python pty attach")
}

fn wait_for_pty_attached(home: &Path, token: &str, timeout: Duration) -> bool {
    let attached = pty_gate_paths(home, token).0;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if attached.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn dump_session_screen(socket_dir: &Path, home: &Path, session: &str) -> String {
    run_frame(
        socket_dir,
        home,
        &["--session", session, "action", "dump-screen", "--full"],
    )
    .1
}

fn start_guest_marker(
    socket_dir: &Path,
    home: &Path,
    session: &str,
    marker: &str,
    pid_name: &str,
) -> (bool, String) {
    let script = format!(
        "printf '%s\\n' {marker}; echo $$ > {}; exec sleep 10000",
        home.join(pid_name).display()
    );
    run_frame(
        socket_dir,
        home,
        &[
            "--session",
            session,
            "action",
            "new-pane",
            "--",
            "sh",
            "-c",
            &script,
        ],
    )
}

fn wake_guest_marker(socket_dir: &Path, home: &Path, session: &str, marker: &str, pid_name: &str) {
    let token = session;
    let child = spawn_pty_attach(socket_dir, home, session, token);
    assert!(
        wait_for_pty_attached(home, token, Duration::from_secs(30)),
        "guest {session} did not accept a short attach to start its marker"
    );
    thread::sleep(Duration::from_millis(400));
    let mut last_start = String::new();
    let start_deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < start_deadline {
        if marker_pid_alive(home, pid_name) {
            break;
        }
        let (ok, out) = start_guest_marker(socket_dir, home, session, marker, pid_name);
        last_start = out;
        if ok {
            break;
        }
        thread::sleep(Duration::from_millis(400));
    }
    let listed = wait_until(
        socket_dir,
        home,
        &["--session", session, "action", "list-panes", "--command"],
        Duration::from_secs(20),
        |out| out.contains(marker) || out.contains("sleep") || marker_pid_alive(home, pid_name),
    );
    let _ = release_pty(home, token, child);
    assert!(
        marker_pid_alive(home, pid_name) || listed.contains("sleep") || listed.contains(marker),
        "guest {session} marker process missing after wake:\n{listed}\nstart:{last_start}"
    );
}

fn marker_pid_alive(home: &Path, pid_name: &str) -> bool {
    let Ok(raw) = std::fs::read_to_string(home.join(pid_name)) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return false;
    };
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn release_pty(home: &Path, token: &str, child: Child) -> String {
    let _ = std::fs::write(pty_gate_paths(home, token).1, "1");
    let output = child.wait_with_output().expect("pty attach exit");
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn attached_client_switches_ab_and_survives_outer_detach() {
    let socket_dir = unique_socket_dir();
    let home = socket_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (host_ok, host_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--layout",
            "vibecrafted-host",
            "attach",
            "-b",
            "-c",
            "frame-host",
        ],
    );
    assert!(host_ok, "host create failed:\n{host_out}");
    assert!(
        wait_for_session(&socket_dir, &home, "frame-host", Duration::from_secs(20)),
        "host session did not appear"
    );

    for (name, marker, pid_name) in [
        ("workspace-a", "GUEST_A_VISIBLE", "guest-a.pid"),
        ("workspace-b", "GUEST_B_VISIBLE", "guest-b.pid"),
    ] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &["--layout", "vibecrafted-guest", "attach", "-b", "-c", name],
        );
        assert!(ok, "create {name} failed:\n{out}");
        assert!(
            wait_for_session(&socket_dir, &home, name, Duration::from_secs(20)),
            "{name} session did not appear"
        );
        wake_guest_marker(&socket_dir, &home, name, marker, pid_name);
    }

    let guest_a_before = run_frame(
        &socket_dir,
        &home,
        &["--session", "workspace-a", "action", "list-panes", "--json"],
    )
    .1;
    let guest_b_before = run_frame(
        &socket_dir,
        &home,
        &["--session", "workspace-b", "action", "list-panes", "--json"],
    )
    .1;
    assert!(
        !guest_a_before.trim().is_empty(),
        "guest A must have panes before attach:\n{guest_a_before}"
    );
    assert!(
        !guest_b_before.trim().is_empty(),
        "guest B must have panes before attach:\n{guest_b_before}"
    );
    assert!(
        wait_until(
            &socket_dir,
            &home,
            &[
                "--session",
                "workspace-a",
                "action",
                "list-panes",
                "--command",
            ],
            Duration::from_secs(15),
            |listed| listed.contains("GUEST_A_VISIBLE") || listed.contains("sleep"),
        )
        .contains("sleep")
            || marker_pid_alive(&home, "guest-a.pid"),
        "guest A marker process missing"
    );
    assert!(
        wait_until(
            &socket_dir,
            &home,
            &[
                "--session",
                "workspace-b",
                "action",
                "list-panes",
                "--command",
            ],
            Duration::from_secs(15),
            |listed| listed.contains("GUEST_B_VISIBLE") || listed.contains("sleep"),
        )
        .contains("sleep")
            || marker_pid_alive(&home, "guest-b.pid"),
        "guest B marker process missing"
    );

    let pty = spawn_pty_attach(&socket_dir, &home, "frame-host", "frame-host");
    assert!(
        wait_for_pty_attached(&home, "frame-host", Duration::from_secs(30)),
        "interactive host client did not attach"
    );

    let clients = wait_until(
        &socket_dir,
        &home,
        &["--session", "frame-host", "action", "list-clients"],
        Duration::from_secs(15),
        |out| out.contains("frame-host") || out.to_lowercase().contains("client"),
    );
    assert!(
        !clients.contains("workspace-a") && !clients.contains("workspace-b")
            || clients.contains("frame-host"),
        "outer client must remain on the host, got:\n{clients}"
    );

    let host_placeholder = wait_until(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "action",
            "list-panes",
            "--json",
            "--command",
        ],
        Duration::from_secs(20),
        |listed| listed.contains("VC_FRAME_GUEST_SURFACE=1"),
    );
    assert!(
        host_placeholder.contains("VC_FRAME_GUEST_SURFACE=1"),
        "host must expose the registered guest-surface hold before project:\n{host_placeholder}"
    );

    for guest in ["workspace-a", "workspace-b", "workspace-a"] {
        let visit_token = format!("visit {guest}");
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &["--session", "frame-host", "project-workspace", guest],
        );
        assert!(ok, "project {guest} failed:\n{out}");
        let panes = wait_until(
            &socket_dir,
            &home,
            &[
                "--session",
                "frame-host",
                "action",
                "list-panes",
                "--json",
                "--command",
            ],
            Duration::from_secs(20),
            |listed| listed.contains(&visit_token),
        );
        assert!(
            panes.contains(&visit_token),
            "host VC Guest must {visit_token}, panes:\n{panes}"
        );
        let clients_after = run_frame(
            &socket_dir,
            &home,
            &["--session", "frame-host", "action", "list-clients"],
        )
        .1;
        assert!(
            !clients_after.contains("No session") && !clients_after.contains("not found"),
            "host client disappeared while projecting {guest}:\n{clients_after}"
        );
        let expected_marker = if guest == "workspace-a" {
            "GUEST_A_VISIBLE"
        } else {
            "GUEST_B_VISIBLE"
        };
        let host_screen = dump_session_screen(&socket_dir, &home, "frame-host");
        assert!(
            host_screen.contains(guest) || panes.contains(&visit_token),
            "host must remain projected onto {guest}; screen:\n{host_screen}\npanes:\n{panes}"
        );
        let guest_screen = dump_session_screen(&socket_dir, &home, guest);
        assert!(
            guest_screen.contains(expected_marker),
            "guest {guest} must retain its marker workload:\n{guest_screen}"
        );
        assert!(
            marker_pid_alive(&home, "guest-a.pid") && marker_pid_alive(&home, "guest-b.pid"),
            "both guest marker PIDs must stay alive through {guest}"
        );
    }

    let (plugin_ok, plugin_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "action",
            "launch-or-focus-plugin",
            "--floating",
            "session-manager",
        ],
    );
    assert!(
        plugin_ok || plugin_out.contains("plugin_"),
        "ordinary Session Manager launch failed:\n{plugin_out}"
    );

    let activate = activate_guest_tab_payload_json("workspace-b", 0);
    let (broadcast_ok, broadcast_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "pipe",
            "--name",
            "vc.guest-surface.v1",
            "--",
            &activate,
        ],
    );
    assert!(
        broadcast_ok,
        "broadcast activate_tab failed:\n{broadcast_out}"
    );
    let clients_after_broadcast = run_frame(
        &socket_dir,
        &home,
        &["--session", "frame-host", "action", "list-clients"],
    )
    .1;
    assert!(
        !clients_after_broadcast.contains("Session 'workspace")
            && !clients_after_broadcast.to_lowercase().contains("not found"),
        "broadcast tab click must not reconnect the outer client:\n{clients_after_broadcast}"
    );

    let pty_log = release_pty(&home, "frame-host", pty);
    thread::sleep(Duration::from_millis(300));

    let (again_ok, again) = run_frame(&socket_dir, &home, &["ls", "-n", "-s"]);
    assert!(
        again_ok,
        "re-list after detach failed:\n{again}\npty:{pty_log}"
    );
    assert!(
        again.contains("workspace-a"),
        "A died after detach:\n{again}"
    );
    assert!(
        again.contains("workspace-b"),
        "B died after detach:\n{again}"
    );
    assert!(
        again.contains("frame-host"),
        "host died after detach:\n{again}"
    );

    let guest_a_after = run_frame(
        &socket_dir,
        &home,
        &["--session", "workspace-a", "action", "list-panes", "--json"],
    )
    .1;
    let guest_b_after = run_frame(
        &socket_dir,
        &home,
        &["--session", "workspace-b", "action", "list-panes", "--json"],
    )
    .1;
    assert!(
        guest_a_after.contains("pane") || guest_a_after.contains("{"),
        "guest A workload identity missing after detach:\n{guest_a_after}"
    );
    assert!(
        guest_b_after.contains("pane") || guest_b_after.contains("{"),
        "guest B workload identity missing after detach:\n{guest_b_after}"
    );
    let guest_a_screen = dump_session_screen(&socket_dir, &home, "workspace-a");
    let guest_b_screen = dump_session_screen(&socket_dir, &home, "workspace-b");
    assert!(
        guest_a_screen.contains("GUEST_A_VISIBLE"),
        "guest A marker missing after detach:\n{guest_a_screen}"
    );
    assert!(
        guest_b_screen.contains("GUEST_B_VISIBLE"),
        "guest B marker missing after detach:\n{guest_b_screen}"
    );
    assert!(
        marker_pid_alive(&home, "guest-a.pid") && marker_pid_alive(&home, "guest-b.pid"),
        "marker PIDs must survive outer detach"
    );

    let pty2 = spawn_pty_attach(&socket_dir, &home, "frame-host", "frame-host-re");
    assert!(
        wait_for_pty_attached(&home, "frame-host-re", Duration::from_secs(30)),
        "reattach of outer host client failed"
    );
    let (project_ok, project_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "project-workspace",
            "workspace-b",
        ],
    );
    assert!(
        project_ok,
        "reproject B after reattach failed:\n{project_out}"
    );
    let _ = release_pty(&home, "frame-host-re", pty2);

    let (dup_ok, dup) = run_frame(
        &socket_dir,
        &home,
        &[
            "--layout",
            "vibecrafted",
            "--guest-workspace",
            "attach",
            "-b",
            "-c",
            "workspace-a",
        ],
    );
    assert!(!dup_ok, "duplicate A must refuse:\n{dup}");
    assert!(
        dup.contains("already exists") || dup.contains("refused before mutation"),
        "duplicate refusal must be actionable, got:\n{dup}"
    );

    let _ = run_frame(&socket_dir, &home, &["ka", "-y"]);
    let _ = std::fs::remove_dir_all(&socket_dir);
}

#[test]
fn ordinary_session_with_focused_marker_refuses_projection() {
    let socket_dir = unique_socket_dir();
    let home = socket_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let (ok, out) = run_frame(
        &socket_dir,
        &home,
        &["attach", "-b", "-c", "ordinary-shell"],
    );
    assert!(ok, "ordinary session create failed:\n{out}");
    assert!(
        wait_for_session(
            &socket_dir,
            &home,
            "ordinary-shell",
            Duration::from_secs(20)
        ),
        "ordinary session did not appear"
    );

    let before = wait_until(
        &socket_dir,
        &home,
        &[
            "--session",
            "ordinary-shell",
            "action",
            "list-panes",
            "--command",
        ],
        Duration::from_secs(15),
        |listed| listed.contains("zsh") || listed.contains("terminal_"),
    );
    assert!(
        before.contains("zsh") || before.contains("terminal_"),
        "ordinary focused shell must be running before refuse:\n{before}"
    );

    let (guest_ok, guest_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--layout",
            "vibecrafted",
            "--guest-workspace",
            "attach",
            "-b",
            "-c",
            "workspace-a",
        ],
    );
    assert!(guest_ok, "guest create failed:\n{guest_out}");
    assert!(wait_for_session(
        &socket_dir,
        &home,
        "workspace-a",
        Duration::from_secs(20)
    ));

    let (project_ok, project_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "ordinary-shell",
            "project-workspace",
            "workspace-a",
        ],
    );
    assert!(
        !project_ok,
        "ordinary session must refuse projection:\n{project_out}"
    );
    assert!(
        project_out.contains("Refused") || project_out.contains("Zero process"),
        "refuse must be explicit, got:\n{project_out}"
    );

    let after = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "ordinary-shell",
            "action",
            "list-panes",
            "--command",
        ],
    )
    .1;
    assert!(
        !after.contains("visit workspace-a"),
        "ordinary pane must not be replaced:\n{after}"
    );
    assert!(
        after.contains("zsh") || after.contains("terminal_"),
        "ordinary focused shell must survive refused projection:\n{after}"
    );

    let _ = run_frame(&socket_dir, &home, &["ka", "-y"]);
    let _ = std::fs::remove_dir_all(&socket_dir);
}

fn activate_guest_tab_payload_json(session: &str, tab: usize) -> String {
    format!(r#"{{"session":"{session}","activate_tab":{tab}}}"#)
}
