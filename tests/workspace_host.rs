//! Isolated real-process proof that A/B are distinct guest sessions under one
//! host identity, and that closing the outer client does not kill them.

use std::path::PathBuf;
use std::process::{Command, Stdio};
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

fn isolated_env(socket_dir: &std::path::Path, home: &std::path::Path) -> Vec<(String, String)> {
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

fn run_frame(
    socket_dir: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
) -> (bool, String) {
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

fn wait_for_session(
    socket_dir: &std::path::Path,
    home: &std::path::Path,
    name: &str,
    timeout: Duration,
) -> bool {
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

#[test]
fn two_guest_workspaces_survive_outer_host_detach() {
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

    for (name, layout) in [
        ("workspace-a", "vibecrafted"),
        ("workspace-b", "vibecrafted"),
    ] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &[
                "--layout",
                layout,
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

    let (list_ok, listed) = run_frame(&socket_dir, &home, &["ls", "-n", "-s"]);
    assert!(list_ok, "list failed:\n{listed}");
    let names: Vec<&str> = listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert!(names.contains(&"workspace-a"), "missing A in {names:?}");
    assert!(names.contains(&"workspace-b"), "missing B in {names:?}");
    assert!(names.contains(&"frame-host"), "missing host in {names:?}");
    assert_eq!(
        names.iter().filter(|name| **name == "workspace-a").count(),
        1,
        "A must be one process, not duplicated: {names:?}"
    );
    assert_eq!(
        names.iter().filter(|name| **name == "workspace-b").count(),
        1,
        "B must be one process, not duplicated: {names:?}"
    );

    // Closing the outer view: never attach an interactive client. Both guests
    // must remain after the host exists only as a detached server.
    thread::sleep(Duration::from_millis(200));
    let (again_ok, again) = run_frame(&socket_dir, &home, &["ls", "-n", "-s"]);
    assert!(again_ok, "re-list failed:\n{again}");
    assert!(again.contains("workspace-a"));
    assert!(again.contains("workspace-b"));
    assert!(again.contains("frame-host"));

    // Same-name create must refuse before mutating the live guest.
    // `attach -b` is create-detached; it does not reconnect an existing server.
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
    assert_eq!(
        run_frame(&socket_dir, &home, &["ls", "-n", "-s"])
            .1
            .lines()
            .map(str::trim)
            .filter(|line| *line == "workspace-a")
            .count(),
        1,
        "duplicate create must not spawn a second A"
    );

    let _ = run_frame(&socket_dir, &home, &["ka", "-y"]);
    let _ = std::fs::remove_dir_all(&socket_dir);
}
