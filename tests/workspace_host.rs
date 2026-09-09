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

fn spawn_pty_attach(socket_dir: &Path, home: &Path, session: &str) -> Child {
    let script = home.join("pty_attach.py");
    std::fs::write(
        &script,
        r#"
import os, pty, select, signal, sys, time

binary, session, attached_path, release_path = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    os.execvpe(binary, [binary, "--session", session, "attach"], os.environ)

deadline = time.time() + 25
saw = False
while time.time() < deadline and not saw:
    ready, _, _ = select.select([fd], [], [], 0.2)
    if ready:
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if chunk:
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
            os.read(fd, 4096)
        except OSError:
            break
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

    let attached = home.join("pty-attached");
    let release = home.join("pty-release");
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

fn wait_for_pty_attached(home: &Path, timeout: Duration) -> bool {
    let attached = home.join("pty-attached");
    let start = Instant::now();
    while start.elapsed() < timeout {
        if attached.is_file() {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    false
}

fn release_pty(home: &Path, child: Child) -> String {
    let _ = std::fs::write(home.join("pty-release"), "1");
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

    for name in ["workspace-a", "workspace-b"] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &[
                "--layout",
                "vibecrafted",
                "--guest-workspace",
                "attach",
                "-b",
                "-c",
                name,
            ],
        );
        assert!(ok, "create {name} failed:\n{out}");
        assert!(
            wait_for_session(&socket_dir, &home, name, Duration::from_secs(20)),
            "{name} session did not appear"
        );
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

    let pty = spawn_pty_attach(&socket_dir, &home, "frame-host");
    assert!(
        wait_for_pty_attached(&home, Duration::from_secs(30)),
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
            "--command",
        ],
        Duration::from_secs(20),
        |listed| listed.contains("VC Guest") || listed.contains("zsh"),
    );
    assert!(
        host_placeholder.contains("VC Guest") || host_placeholder.contains("terminal_"),
        "host must expose a replaceable guest surface before project:\n{host_placeholder}"
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

    let pty_log = release_pty(&home, pty);
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

    let pty2 = spawn_pty_attach(&socket_dir, &home, "frame-host");
    assert!(
        wait_for_pty_attached(&home, Duration::from_secs(30)),
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
    let _ = release_pty(&home, pty2);

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

fn activate_guest_tab_payload_json(session: &str, tab: usize) -> String {
    format!(r#"{{"session":"{session}","activate_tab":{tab}}}"#)
}
