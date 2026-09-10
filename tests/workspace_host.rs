//! Isolated real-process proof: an attached outer client stays on the host
//! while A/B are projected, and guest workloads survive detach/reattach.
//!
//! Listing never-attached sessions is not detach survival. This harness
//! allocates a kernel PTY, attaches the built Frame client, and keeps it
//! connected while the project-workspace API switches A → B → A.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn unique_socket_dir() -> PathBuf {
    // Exclusive creation prevents another process from pre-seeding the fixture
    // namespace. Keep it for failure and success receipts after cleanup.
    tempfile::Builder::new()
        .prefix("vcp")
        .tempdir()
        .unwrap()
        .into_path()
}

/// Session cleanup is confined to the fixture's private socket namespace,
/// including assertion failures. Keep failed fixture files for diagnosis.
struct FixtureCleanup {
    socket_dir: PathBuf,
    home: PathBuf,
}

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        if self.socket_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&self.home) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(token) = name.strip_prefix("pty-attached-")
                        && !token.ends_with(".screen")
                    {
                        let _ =
                            std::fs::write(self.home.join(format!("pty-release-{token}")), "1");
                    }
                }
            }
            let outcome = cleanup_fixture_processes(&self.socket_dir, &self.home);
            fixture_receipt(
                &self.home,
                serde_json::json!({
                    "event": "fixture_cleanup_drop",
                    "all_owned_processes_absent": outcome.all_owned_processes_absent,
                    "survivors": outcome.survivors,
                    "discovery_error": outcome.discovery_error,
                }),
            );
        }
    }
}

#[derive(Clone, Debug)]
struct FixtureProcess {
    pid: i32,
    parent_pid: i32,
    lstart: String,
    state: String,
    executable: String,
    command: String,
}

impl FixtureProcess {
    fn receipt(&self) -> serde_json::Value {
        serde_json::json!({
            "pid": self.pid,
            "parent_pid": self.parent_pid,
            "lstart": self.lstart,
            "state": self.state,
            "executable": self.executable,
            "command": self.command,
        })
    }
}

struct FixtureCleanupOutcome {
    all_owned_processes_absent: bool,
    survivors: Vec<serde_json::Value>,
    discovery_error: Option<String>,
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
        // Keep the server as a fixture-owned child. The normal Unix
        // double-fork leaves the launcher waiting on an inherited pipe and
        // makes both startup and teardown attribution nondeterministic.
        ("VC_FRAME_SERVER_FOREGROUND".to_owned(), "1".to_owned()),
    ]
}

fn frame_bin() -> &'static str {
    env!("CARGO_BIN_EXE_vc-frame")
}

fn clear_ambient_session_env(command: &mut Command) {
    // A layout is otherwise interpreted as a new tab for the test runner's
    // enclosing Frame session. The fixture owns a distinct socket namespace.
    for key in [
        "ZELLIJ",
        "VC_FRAME",
        "ZELLIJ_SESSION_NAME",
        "VC_FRAME_SESSION_NAME",
        "ZELLIJ_PANE_ID",
        "VC_FRAME_PANE_ID",
    ] {
        command.env_remove(key);
    }
}

fn fixture_receipt(home: &Path, value: serde_json::Value) {
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("commands.jsonl"))
        .expect("open fixture receipt");
    writeln!(log, "{value}").expect("write fixture receipt");
}

fn run_frame(socket_dir: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    static NEXT_COMMAND: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_COMMAND.fetch_add(1, Ordering::Relaxed);
    let stdout_path = home.join(format!("command-{id}.stdout"));
    let stderr_path = home.join(format!("command-{id}.stderr"));
    let mut command = Command::new(frame_bin());
    // File-backed streams cannot fill a pipe while this thread polls the child.
    // They also retain partial output when the CLI watchdog or fixture expires.
    command
        .args(args)
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap());
    command.env_remove("VC_FRAME_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_FILE");
    command.env_remove("ZELLIJ_CONFIG_DIR");
    clear_ambient_session_env(&mut command);
    for (key, value) in isolated_env(socket_dir, home) {
        command.env(key, value);
    }
    command.env("VC_FRAME_ACTION_TTL_SECONDS", "20");
    command.env("VC_FRAME_CALLER", "workspace-project-cli");
    let mut child = command.spawn().expect("spawn vc-frame");
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "request", "id": id, "pid": child.id(), "args": args,
            "binary": frame_bin(), "socket_dir": socket_dir,
            "stdout": stdout_path, "stderr": stderr_path,
        }),
    );
    let started = Instant::now();
    let deadline = started + Duration::from_secs(45);
    let mut timed_out = false;
    while child.try_wait().expect("poll vc-frame").is_none() {
        if Instant::now() >= deadline {
            timed_out = true;
            child.kill().expect("kill only this test command");
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let status = child.wait().expect("reap vc-frame");
    let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "response", "id": id, "exit_code": status.code(),
            "timed_out": timed_out, "elapsed_ms": started.elapsed().as_millis(),
            "stdout": stdout, "stderr": stderr,
        }),
    );
    let combined = format!("{stdout}{stderr}");
    if timed_out {
        return (
            false,
            format!(
                "test command timed out after 45s: {args:?}; socket_dir={}; output={combined}",
                socket_dir.display()
            ),
        );
    }
    (status.success(), combined)
}

fn projection_diagnostics(socket_dir: &Path, home: &Path, session: &str) -> String {
    let panes = run_frame(
        socket_dir,
        home,
        &[
            "--session",
            session,
            "action",
            "list-panes",
            "--json",
            "--command",
        ],
    )
    .1;
    let clients = run_frame(
        socket_dir,
        home,
        &["--session", session, "action", "list-clients"],
    )
    .1;
    let (dump_ok, screen) = dump_session_screen(socket_dir, home, session);
    format!("panes:\n{panes}\nclients:\n{clients}\ndump_ok={dump_ok}\nscreen:\n{screen}")
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

/// `list-panes` title is `Pane::current_title()`: rename (`pane_name`) else
/// VTE OSC 0/2 (`Grid::title`) else the initial `pane_title` (launch command).
/// A visit pane starts as the full `--workspace-projection` command and later
/// receives an OSC title. Duplicate refusal must be baselined only after that
/// ownership has left the launch command.
fn host_projection_titles_have_settled(listed: &str) -> bool {
    let Ok(panes) = serde_json::from_str::<Vec<serde_json::Value>>(listed) else {
        return false;
    };
    panes.iter().all(|pane| {
        let Some(command) = pane.get("terminal_command").and_then(|v| v.as_str()) else {
            return true;
        };
        if !command.contains("--workspace-projection") {
            return true;
        }
        pane.get("title").and_then(|v| v.as_str()) != Some(command)
    })
}

/// Proven volatile on a live visit terminal: the VTE cursor advances while the
/// host surface stays put. W2 `frame-broadcast-retry-w2.log` at
/// `999837ef716ff97005cd3283800046c2dc84f4c4` reached this later assertion
/// after broadcast `activate_tab` completed; the before/after JSON differed
/// only on terminal pane id 5 `cursor_coordinates_in_pane` `[1,1]` vs `[69,3]`.
/// The ambiguous two-client refusal earlier in the same fixture reproduces it
/// independently: `frame-ece7-ambiguity-diff.json` at
/// `ece7dfc1f57d111cd4159264d0106405a02508e7` records four panes whose
/// before/after snapshots differ on nothing but the last pane's cursor
/// (`Null` vs `[69,3]`), equal once that single field is removed.
/// That is not runtime mutation. Strip this one field and keep IDs, type,
/// command, workspace/tab, geometry, focus, suppression, and lifecycle.
const HOST_SURFACE_VOLATILE_CURSOR_FIELD: &str = "cursor_coordinates_in_pane";

fn pane_host_surface_identity(pane: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = pane else {
        return pane.clone();
    };
    let mut retained = map.clone();
    retained.remove(HOST_SURFACE_VOLATILE_CURSOR_FIELD);
    serde_json::Value::Object(retained)
}

fn host_surface_identity(snapshot: &serde_json::Value) -> serde_json::Value {
    match snapshot {
        serde_json::Value::Array(panes) => {
            serde_json::Value::Array(panes.iter().map(pane_host_surface_identity).collect())
        },
        other => pane_host_surface_identity(other),
    }
}

fn fixture_client_ids(listing: &str) -> std::collections::BTreeSet<u16> {
    listing
        .lines()
        .filter_map(|line| line.split_whitespace().next()?.parse::<u16>().ok())
        .collect()
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
import fcntl, os, pty, select, signal, struct, sys, termios, time

binary, session, attached_path, release_path = sys.argv[1:5]
pid, fd = pty.fork()
if pid == 0:
    fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.environ["TERM"] = "xterm-256color"
    os.execvpe(binary, [binary, "attach", session], os.environ)

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
                # Usage/error output from an exited CLI is not an attach.
                time.sleep(0.5)
                ended, status = os.waitpid(pid, os.WNOHANG)
                if ended:
                    sys.stderr.write("pty client exited during startup: " + str(status) + "\n")
                    sys.exit(2)
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
            if not chunk:
                break
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
    clear_ambient_session_env(&mut command);
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

fn dump_session_screen(socket_dir: &Path, home: &Path, session: &str) -> (bool, String) {
    run_frame(
        socket_dir,
        home,
        &["--session", session, "action", "dump-screen"],
    )
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
    if session == "workspace-a" {
        let script = format!(
            "printf '%s\\n' GUEST_A_TAB_TWO_VISIBLE; echo $$ > {}; exec sleep 10000",
            home.join("guest-a-tab-two.pid").display()
        );
        let (ok, out) = run_frame(
            socket_dir,
            home,
            &[
                "--session",
                session,
                "action",
                "new-tab",
                "--name",
                "A second tab",
                "--no-focus",
                "--",
                "sh",
                "-c",
                &script,
            ],
        );
        assert!(ok, "create second guest tab failed: {out}");
    }
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

fn fixture_socket_owner_pids(socket_dir: &Path) -> Result<BTreeSet<i32>, String> {
    fixture_socket_owner_pids_for(socket_dir, None)
}

fn fixture_socket_owner_pids_for(
    socket_dir: &Path,
    expected_socket: Option<&Path>,
) -> Result<BTreeSet<i32>, String> {
    let output = Command::new("lsof")
        .args(["-n", "-P", "-U", "-Fpn"])
        .output()
        .map_err(|error| format!("cannot execute lsof for fixture ownership: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "lsof fixture ownership failed with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let canonical_root = socket_dir
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize fixture socket namespace: {error}"))?;
    let mut process = None;
    let mut owners = BTreeSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(pid) = line.strip_prefix('p') {
            process = pid.parse::<i32>().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            // `-F n` preserves a pathname verbatim, including spaces. lsof
            // appends an arrow only for linked endpoint displays.
            let path = Path::new(
                name.split_once(" -> ")
                    .map(|(path, _)| path)
                    .unwrap_or(name),
            );
            if let Ok(path) = path.canonicalize()
                && path.starts_with(&canonical_root)
                && expected_socket.is_none_or(|expected| path == expected)
                && let Some(pid) = process
            {
                owners.insert(pid);
            }
        }
    }
    Ok(owners)
}

fn process_table() -> Result<BTreeMap<i32, FixtureProcess>, String> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,lstart=,state=,command="])
        .output()
        .map_err(|error| format!("cannot execute ps for fixture ownership: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "ps fixture ownership failed with {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let parent_pid = fields.next()?.parse().ok()?;
            let lstart = (0..5)
                .map(|_| fields.next())
                .collect::<Option<Vec<_>>>()?
                .join(" ");
            let state = fields.next()?.to_owned();
            let command = fields.collect::<Vec<_>>().join(" ");
            let executable = command.split_whitespace().next()?.to_owned();
            Some((
                pid,
                FixtureProcess {
                    pid,
                    parent_pid,
                    lstart,
                    state,
                    executable,
                    command,
                },
            ))
        })
        .collect())
}

/// Roots come only from sockets inside this fixture's private namespace. Every
/// descendant is then captured by its PID/start-time identity, so cleanup can
/// never match a Founder server or an unrelated process by argv text.
fn fixture_owned_processes(socket_dir: &Path) -> Result<Vec<FixtureProcess>, String> {
    let roots = fixture_socket_owner_pids(socket_dir)?;
    let table = process_table()?;
    let mut owned = roots;
    loop {
        let descendants: BTreeSet<i32> = table
            .values()
            .filter(|process| owned.contains(&process.parent_pid))
            .map(|process| process.pid)
            .collect();
        let before = owned.len();
        owned.extend(descendants);
        if owned.len() == before {
            break;
        }
    }
    Ok(owned
        .into_iter()
        .filter_map(|pid| table.get(&pid).cloned())
        .collect())
}

fn same_fixture_processes(owned: &[FixtureProcess]) -> Result<Vec<FixtureProcess>, String> {
    let table = process_table()?;
    // Start time plus executable distinguishes PID reuse. Stopped children are
    // sent TERM before CONT below, so they cannot exec into a different image
    // between identity check and termination.
    Ok(owned
        .iter()
        .filter(|process| {
            table.get(&process.pid).is_some_and(|current| {
                current.lstart == process.lstart
                    && current.executable == process.executable
                    && !current.state.starts_with('Z')
            })
        })
        .cloned()
        .collect())
}

fn zombie_fixture_processes(owned: &[FixtureProcess]) -> Result<Vec<FixtureProcess>, String> {
    let table = process_table()?;
    Ok(owned
        .iter()
        .filter_map(|process| {
            table.get(&process.pid).filter(|current| {
                current.lstart == process.lstart
                    && current.executable == process.executable
                    && current.state.starts_with('Z')
            })
        })
        .cloned()
        .collect())
}

fn signal_if_same(process: &FixtureProcess, signal: &str) -> Result<bool, String> {
    if !same_fixture_processes(std::slice::from_ref(process))?.is_empty() {
        return Command::new("kill")
            .args([format!("-{signal}"), process.pid.to_string()])
            .status()
            .map(|status| status.success())
            .map_err(|error| format!("cannot signal fixture pid {}: {error}", process.pid));
    }
    Ok(false)
}

fn wait_for_fixture_processes_to_exit(
    owned: &[FixtureProcess],
    timeout: Duration,
) -> Result<Vec<FixtureProcess>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = same_fixture_processes(owned)?;
        if remaining.is_empty() || Instant::now() >= deadline {
            return Ok(remaining);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn reap_fixture_processes(
    owned: &[FixtureProcess],
    home: &Path,
) -> Result<Vec<FixtureProcess>, String> {
    let remaining = wait_for_fixture_processes_to_exit(owned, Duration::from_secs(3))?;
    for process in &remaining {
        let terminated = signal_if_same(process, "TERM")?;
        let continued = signal_if_same(process, "CONT")?;
        fixture_receipt(
            home,
            serde_json::json!({
                "event": "fixture_cleanup_signal",
                "signal": "TERM+CONT",
                "process": process.receipt(),
                "terminated": terminated,
                "continued": continued,
            }),
        );
    }
    let remaining = wait_for_fixture_processes_to_exit(owned, Duration::from_secs(3))?;
    for process in &remaining {
        let killed = signal_if_same(process, "KILL")?;
        fixture_receipt(
            home,
            serde_json::json!({
                "event": "fixture_cleanup_signal",
                "signal": "KILL",
                "process": process.receipt(),
                "killed": killed,
            }),
        );
    }
    let survivors = wait_for_fixture_processes_to_exit(owned, Duration::from_secs(1))?;
    let zombies = zombie_fixture_processes(owned)?;
    if !zombies.is_empty() {
        fixture_receipt(
            home,
            serde_json::json!({
                "event": "fixture_cleanup_terminal_zombies",
                "zombies": zombies.iter().map(FixtureProcess::receipt).collect::<Vec<_>>(),
            }),
        );
    }
    Ok(survivors)
}

fn cleanup_fixture_processes(socket_dir: &Path, home: &Path) -> FixtureCleanupOutcome {
    let owned = match fixture_owned_processes(socket_dir) {
        Ok(owned) => owned,
        Err(error) => {
            fixture_receipt(
                home,
                serde_json::json!({"event": "fixture_cleanup_discovery_failed", "error": error}),
            );
            return FixtureCleanupOutcome {
                all_owned_processes_absent: false,
                survivors: vec![],
                discovery_error: Some(error),
            };
        },
    };
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "fixture_cleanup_inventory",
            "owned": owned.iter().map(FixtureProcess::receipt).collect::<Vec<_>>(),
        }),
    );
    if owned.is_empty() {
        fixture_receipt(
            home,
            serde_json::json!({"event": "fixture_cleanup_no_owned_processes"}),
        );
    }

    let (kill_all_ok, kill_all_output) = run_frame(socket_dir, home, &["ka", "-y"]);
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "fixture_cleanup_session_shutdown",
            "ok": kill_all_ok,
            "output": kill_all_output,
        }),
    );

    let (survivors, discovery_error) = match reap_fixture_processes(&owned, home) {
        Ok(survivors) => (survivors, None),
        Err(error) => {
            fixture_receipt(
                home,
                serde_json::json!({"event": "fixture_cleanup_discovery_failed", "error": error}),
            );
            (vec![], Some(error))
        },
    };
    let survivors = survivors
        .iter()
        .map(FixtureProcess::receipt)
        .collect::<Vec<_>>();
    let outcome = FixtureCleanupOutcome {
        all_owned_processes_absent: discovery_error.is_none() && survivors.is_empty(),
        survivors,
        discovery_error,
    };
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "fixture_cleanup_complete",
            "all_owned_processes_absent": outcome.all_owned_processes_absent,
            "survivors": &outcome.survivors,
            "discovery_error": &outcome.discovery_error,
        }),
    );
    outcome
}

fn fixture_project_data_dir(home: &Path) -> PathBuf {
    // Match ProjectDirs::from("io", "vetcoders", "vc-frame") under the
    // isolated HOME. Creating it first also stops get_default_data_dir
    // from preferring a host system_data_dir that happens to exist.
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/io.vetcoders.vc-frame")
    } else if cfg!(windows) {
        home.join("AppData/Roaming/vetcoders/vc-frame")
    } else {
        home.join("data/vc-frame")
    }
}

fn dump_fixture_plugins(socket_dir: &Path, home: &Path) {
    // Isolated HOME + disable_automatic_asset_installation cannot load
    // session-manager.wasm from ASSET_MAP. Dump the same bytes the child
    // server will resolve from its data dir so activation is real.
    let data_dir = fixture_project_data_dir(home);
    std::fs::create_dir_all(&data_dir).unwrap();
    let data_dir_arg = data_dir.display().to_string();
    let (ok, out) = run_frame(
        socket_dir,
        home,
        &["setup", "--dump-plugins", &data_dir_arg],
    );
    assert!(
        ok,
        "fixture must materialize builtin plugins for real activation:\n{out}"
    );
    let session_manager = data_dir.join("plugins/session-manager.wasm");
    assert!(
        session_manager.is_file(),
        "configured host rail WASM must exist at {} after dump:\n{out}",
        session_manager.display()
    );
}

fn assert_fixture_cleanup(socket_dir: &Path, home: &Path) {
    let outcome = cleanup_fixture_processes(socket_dir, home);
    assert!(
        outcome.all_owned_processes_absent,
        "fixture cleanup did not prove owned processes absent; survivors={:?}; discovery_error={:?}",
        outcome.survivors, outcome.discovery_error,
    );
}

/// Resolve the live owner of this fixture's bound Unix socket, never an argv
/// substring or a process from the Founder's session namespace.
fn fixture_session_server_pid(socket_dir: &Path, home: &Path, session: &str) -> u32 {
    let socket = socket_dir.join("contract_version_2").join(session);
    let socket = socket
        .canonicalize()
        .expect("fixture session socket exists");
    let owners = fixture_socket_owner_pids_for(socket_dir, Some(&socket))
        .expect("lsof fixture ownership discovery must succeed")
        .into_iter()
        .collect::<BTreeSet<_>>();
    fixture_receipt(
        home,
        serde_json::json!({
            "event": "socket_owners", "session": session, "pids": owners,
        }),
    );
    assert_eq!(
        owners.len(),
        1,
        "expected one live socket owner for {session}: {owners:?}"
    );
    *owners.first().unwrap() as u32
}

fn release_pty(home: &Path, token: &str, child: Child) -> String {
    let _ = std::fs::write(pty_gate_paths(home, token).1, "1");
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("poll fixture PTY").is_none() {
        if Instant::now() >= deadline {
            child.kill().expect("kill fixture PTY helper");
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
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
    eprintln!("workspace_host fixture: {}", socket_dir.display());
    let _cleanup = FixtureCleanup {
        socket_dir: socket_dir.clone(),
        home: home.clone(),
    };
    dump_fixture_plugins(&socket_dir, &home);

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

    let marker_pids_before = ["guest-a.pid", "guest-b.pid"]
        .map(|name| std::fs::read_to_string(home.join(name)).expect("marker PID receipt"));
    fixture_receipt(
        &home,
        serde_json::json!({"event": "marker_pids_before", "pids": marker_pids_before}),
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
        |listed| {
            listed.contains("session-manager")
                && listed.contains("VC Guest")
                && listed.contains("frame-host")
        },
    );
    assert!(
        host_placeholder.contains("session-manager")
            && host_placeholder.contains("VC Guest")
            && host_placeholder.contains("frame-host"),
        "host rail and surface must exist before project:\n{host_placeholder}"
    );

    let host_server_before = fixture_session_server_pid(&socket_dir, &home, "frame-host");
    let mut owner_client = None;
    let mut request_ids = std::collections::HashSet::new();
    let mut last_projected_pane = String::new();
    for (guest, tab, expected_marker) in [
        ("workspace-a", "1", "GUEST_A_VISIBLE"),
        ("workspace-a", "2", "GUEST_A_TAB_TWO_VISIBLE"),
        ("workspace-a", "1", "GUEST_A_VISIBLE"),
        ("workspace-b", "1", "GUEST_B_VISIBLE"),
        ("workspace-a", "1", "GUEST_A_VISIBLE"),
    ] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &[
                "--session",
                "frame-host",
                "project-workspace",
                guest,
                "--tab",
                tab,
            ],
        );
        if !ok {
            let diagnostics = projection_diagnostics(&socket_dir, &home, "frame-host");
            panic!(
                "project {guest} failed:\n{out}\n{diagnostics}\nreceipts: {}",
                home.display()
            );
        }
        let receipt: serde_json::Value = out
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|value| value.get("request_id").is_some())
            .unwrap_or_else(|| panic!("project must return owner acknowledgment: {out}"));
        assert_eq!(receipt["status"], "Handled", "{receipt}");
        assert_eq!(receipt["guest"], guest, "{receipt}");
        assert_eq!(
            receipt["tab"],
            tab.parse::<usize>().unwrap() - 1,
            "{receipt}"
        );
        let request_id = receipt["request_id"].as_str().expect("request identity");
        assert!(
            !request_id.is_empty() && request_ids.insert(request_id.to_owned()),
            "fresh request identity: {receipt}"
        );
        let client = receipt["client_id"].as_u64().expect("recipient client");
        if let Some(expected) = owner_client {
            assert_eq!(client, expected, "same attached recipient");
        }
        owner_client = Some(client);
        assert!(
            receipt["plugin_id"].as_u64().is_some(),
            "owner plugin: {receipt}"
        );
        let projected_pane = format!(
            "terminal_{}",
            receipt["pane_id"]
                .as_u64()
                .expect("projected pane identity")
        );
        last_projected_pane = projected_pane.clone();
        let panes = run_frame(
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
        )
        .1;
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
        let host_screen = wait_until(
            &socket_dir,
            &home,
            &[
                "--session",
                "frame-host",
                "action",
                "dump-screen",
                "--pane-id",
                &projected_pane,
            ],
            Duration::from_secs(20),
            |screen| screen.contains(expected_marker),
        );
        assert!(
            host_screen.contains(expected_marker),
            "host must remain projected onto {guest}; screen:\n{host_screen}\npanes:\n{panes}"
        );
        for other_marker in [
            "GUEST_A_VISIBLE",
            "GUEST_A_TAB_TWO_VISIBLE",
            "GUEST_B_VISIBLE",
        ] {
            if other_marker != expected_marker {
                assert!(
                    !host_screen.contains(other_marker),
                    "current projected viewport must exclude prior guest/tab {other_marker}: {host_screen}"
                );
            }
        }
        let (guest_ok, guest_screen) = dump_session_screen(&socket_dir, &home, guest);
        assert!(
            guest_ok && guest_screen.contains(expected_marker),
            "guest {guest} must retain its marker workload (ok={guest_ok}):\n{guest_screen}"
        );
        assert!(
            marker_pid_alive(&home, "guest-a.pid") && marker_pid_alive(&home, "guest-b.pid"),
            "both guest marker PIDs must stay alive through {guest}"
        );
    }

    // CLI invocation has no interactive-client selector. A second attached
    // owner must produce refusal, never choose either client's shared surface.
    let second_pty = spawn_pty_attach(&socket_dir, &home, "frame-host", "frame-host-second");
    assert!(
        wait_for_pty_attached(&home, "frame-host-second", Duration::from_secs(30)),
        "second host client must actually attach"
    );
    let two_clients = wait_until(
        &socket_dir,
        &home,
        &["--session", "frame-host", "action", "list-clients"],
        Duration::from_secs(15),
        |listing| fixture_client_ids(listing).len() == 2,
    );
    assert_eq!(
        fixture_client_ids(&two_clients).len(),
        2,
        "must observe two current interactive clients: {two_clients}"
    );
    let panes_before_two_clients = run_frame(
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
    )
    .1;
    let marker_names = ["guest-a.pid", "guest-a-tab-two.pid", "guest-b.pid"];
    let marker_pids_before_refusal = marker_names.map(|name| {
        std::fs::read_to_string(home.join(name)).expect("marker identity before ambiguous request")
    });
    let (ambiguous_ok, ambiguous_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "project-workspace",
            "workspace-b",
            "--tab",
            "1",
        ],
    );
    assert!(
        !ambiguous_ok,
        "two-client projection must refuse: {ambiguous_out}"
    );
    let refusal: serde_json::Value = ambiguous_out
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|value| value.get("request_id").is_some())
        .unwrap_or_else(|| {
            panic!("two-client refusal requires a correlated receipt: {ambiguous_out}")
        });
    assert_eq!(refusal["status"], "Refused", "{refusal}");
    assert_eq!(refusal["guest"], "workspace-b", "{refusal}");
    assert_eq!(refusal["tab"], 0, "{refusal}");
    assert!(
        refusal["pane_id"].is_null(),
        "refusal cannot report a replacement: {refusal}"
    );
    let refusal_request = refusal["request_id"]
        .as_str()
        .expect("refusal request identity");
    assert!(
        !refusal_request.is_empty() && request_ids.insert(refusal_request.to_owned()),
        "refusal identity must be fresh: {refusal}"
    );
    let panes_after_two_clients = run_frame(
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
    )
    .1;
    let before_two_clients: serde_json::Value = serde_json::from_str(&panes_before_two_clients)
        .expect("pane snapshot before ambiguous request");
    let after_two_clients: serde_json::Value = serde_json::from_str(&panes_after_two_clients)
        .expect("pane snapshot after ambiguous request");
    assert_eq!(
        host_surface_identity(&before_two_clients),
        host_surface_identity(&after_two_clients),
        "ambiguous client refusal must leave shared surface unchanged; cursor_coordinates_in_pane is excluded as proven VTE motion, not a structural change"
    );
    let marker_pids_after_refusal = marker_names.map(|name| {
        std::fs::read_to_string(home.join(name)).expect("marker identity after ambiguous request")
    });
    assert_eq!(
        marker_pids_before_refusal, marker_pids_after_refusal,
        "ambiguous client refusal must preserve exact guest workload PIDs"
    );
    assert!(
        marker_names
            .iter()
            .all(|name| marker_pid_alive(&home, name)),
        "all guest workloads survive ambiguous client refusal"
    );
    let viewport_after_refusal = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "action",
            "dump-screen",
            "--pane-id",
            &last_projected_pane,
        ],
    )
    .1;
    assert!(
        viewport_after_refusal.contains("GUEST_A_VISIBLE")
            && !viewport_after_refusal.contains("GUEST_B_VISIBLE"),
        "ambiguous request must preserve current A viewport: {viewport_after_refusal}"
    );
    let _ = release_pty(&home, "frame-host-second", second_pty);
    let surviving_clients = wait_until(
        &socket_dir,
        &home,
        &["--session", "frame-host", "action", "list-clients"],
        Duration::from_secs(15),
        |listing| fixture_client_ids(listing).len() == 1,
    );
    let expected_client = u16::try_from(owner_client.expect("first owner receipt")).unwrap();
    assert_eq!(
        fixture_client_ids(&surviving_clients),
        std::collections::BTreeSet::from([expected_client]),
        "releasing second client must preserve the original owner: {surviving_clients}"
    );

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
    assert!(
        !broadcast_out.contains("warden.expired_client")
            && !broadcast_out.contains("client_self_retired"),
        "CLI activate_tab must complete on the owning rail, not time out:\n{broadcast_out}"
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

    let panes_before_duplicate = wait_until(
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
        host_projection_titles_have_settled,
    );
    assert!(
        host_projection_titles_have_settled(&panes_before_duplicate),
        "projection titles must leave the launch-command title before duplicate refusal:\n{panes_before_duplicate}"
    );
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

    let panes_after_duplicate = run_frame(
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
    )
    .1;
    let before: serde_json::Value =
        serde_json::from_str(&panes_before_duplicate).expect("pane snapshot before duplicate");
    let after: serde_json::Value =
        serde_json::from_str(&panes_after_duplicate).expect("pane snapshot after duplicate");
    assert_eq!(
        host_surface_identity(&before),
        host_surface_identity(&after),
        "duplicate refusal must not mutate the attached host surface; cursor_coordinates_in_pane is excluded as proven VTE motion, not a structural change"
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
    let (guest_a_ok, guest_a_screen) = dump_session_screen(&socket_dir, &home, "workspace-a");
    let (guest_b_ok, guest_b_screen) = dump_session_screen(&socket_dir, &home, "workspace-b");
    assert!(
        guest_a_ok && guest_a_screen.contains("GUEST_A_VISIBLE"),
        "guest A marker missing after detach (ok={guest_a_ok}):\n{guest_a_screen}"
    );
    assert!(
        guest_b_ok && guest_b_screen.contains("GUEST_B_VISIBLE"),
        "guest B marker missing after detach (ok={guest_b_ok}):\n{guest_b_screen}"
    );
    assert!(
        marker_pid_alive(&home, "guest-a.pid") && marker_pid_alive(&home, "guest-b.pid"),
        "marker PIDs must survive outer detach"
    );

    let host_server_after = fixture_session_server_pid(&socket_dir, &home, "frame-host");
    assert_eq!(
        host_server_before, host_server_after,
        "host server must retain its PID across detach"
    );
    let marker_pids_after = ["guest-a.pid", "guest-b.pid"].map(|name| {
        std::fs::read_to_string(home.join(name)).expect("surviving marker PID receipt")
    });
    assert_eq!(
        marker_pids_before, marker_pids_after,
        "guest workloads must retain exact PIDs across detach"
    );
    fixture_receipt(
        &home,
        serde_json::json!({"event": "marker_pids_after_detach", "pids": marker_pids_after}),
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

    assert_fixture_cleanup(&socket_dir, &home);
    eprintln!("retained workspace_host receipts: {}", home.display());
}

#[test]
fn ordinary_session_with_focused_marker_refuses_projection() {
    let socket_dir = unique_socket_dir();
    let home = socket_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    eprintln!("workspace_host fixture: {}", socket_dir.display());
    let _cleanup = FixtureCleanup {
        socket_dir: socket_dir.clone(),
        home: home.clone(),
    };

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

    let ordinary_pty = spawn_pty_attach(&socket_dir, &home, "ordinary-shell", "ordinary-shell");
    assert!(
        wait_for_pty_attached(&home, "ordinary-shell", Duration::from_secs(30)),
        "ordinary client must actually attach before spoof refusal"
    );
    let spoof_script = format!(
        "VC_FRAME_GUEST_SURFACE=1; FRAME_PLUGIN=frame-host; printf '%s\\n' SPOOF_SURFACE_VISIBLE; echo $$ > {}; exec sleep 10000",
        home.join("spoof.pid").display()
    );
    let (spoof_ok, spoof_out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "ordinary-shell",
            "action",
            "new-pane",
            "--",
            "sh",
            "-c",
            &spoof_script,
        ],
    );
    assert!(spoof_ok, "create spoofed command pane: {spoof_out}");

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

    assert!(
        marker_pid_alive(&home, "spoof.pid"),
        "spoofed ordinary workload survives refusal"
    );
    let (screen_ok, screen) = dump_session_screen(&socket_dir, &home, "ordinary-shell");
    assert!(
        screen_ok && screen.contains("SPOOF_SURFACE_VISIBLE"),
        "refusal preserves focused ordinary surface (ok={screen_ok}): {screen}"
    );
    let _ = release_pty(&home, "ordinary-shell", ordinary_pty);

    assert_fixture_cleanup(&socket_dir, &home);
    eprintln!("retained workspace_host receipts: {}", home.display());
}

#[test]
fn fixture_reaper_resumes_a_stopped_owned_child_before_termination() {
    let temp = tempfile::tempdir().expect("stopped-child fixture home");
    let mut child = Command::new("sh")
        .args(["-c", "kill -STOP $$; exec sleep 10000"])
        .spawn()
        .expect("spawn stopped fixture child");
    let deadline = Instant::now() + Duration::from_secs(2);
    let process = loop {
        if let Some(process) = process_table()
            .expect("process discovery for stopped child")
            .get(&(child.id() as i32))
            .cloned()
        {
            let state = Command::new("ps")
                .args(["-p", &child.id().to_string(), "-o", "state="])
                .output()
                .expect("read stopped child state");
            if String::from_utf8_lossy(&state.stdout)
                .trim_start()
                .starts_with('T')
            {
                break process;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fixture child did not stop itself"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let result = reap_fixture_processes(std::slice::from_ref(&process), temp.path());
    let reaper_reaped_child = child
        .try_wait()
        .expect("poll stopped fixture child after reaper")
        .is_some();
    let reaper_succeeded = result.as_ref().is_ok_and(|survivors| survivors.is_empty());
    if !reaper_succeeded || !reaper_reaped_child {
        let _ = Command::new("kill")
            .args(["-CONT".to_owned(), child.id().to_string()])
            .status();
        let _ = child.kill();
        let _ = child.wait();
    }
    // Emergency cleanup only prevents a failed test from leaking a process; it
    // is never acceptance evidence for the reaper under test.
    let survivors = result.expect("stopped fixture child cleanup discovery");
    assert!(
        survivors.is_empty(),
        "stopped fixture child survived cleanup"
    );
    assert!(
        reaper_reaped_child,
        "reaper did not terminate the stopped child"
    );
}

#[test]
fn host_surface_identity_accepts_cursor_drift_and_rejects_structure() {
    let baseline = serde_json::json!([
        {
            "id": 3,
            "is_plugin": true,
            "is_focused": true,
            "is_suppressed": false,
            "exited": false,
            "exit_status": null,
            "plugin_url": "frame-host",
            "tab_id": 0,
            "tab_name": "Workspace",
            "tab_position": 0,
            "pane_x": 0,
            "pane_y": 1,
            "pane_columns": 24,
            "pane_rows": 38,
            "terminal_command": null,
            "cursor_coordinates_in_pane": null
        },
        {
            "id": 5,
            "is_plugin": false,
            "is_focused": false,
            "is_suppressed": false,
            "exited": false,
            "exit_status": null,
            "plugin_url": null,
            "tab_id": 0,
            "tab_name": "Workspace",
            "tab_position": 0,
            "pane_x": 24,
            "pane_y": 1,
            "pane_columns": 136,
            "pane_rows": 38,
            "terminal_command": "vc-frame --workspace-projection visit workspace-b --tab 1",
            "cursor_coordinates_in_pane": [1, 1]
        }
    ]);
    let mut cursor_only = baseline.clone();
    cursor_only[1]["cursor_coordinates_in_pane"] = serde_json::json!([69, 3]);
    assert_eq!(
        host_surface_identity(&baseline),
        host_surface_identity(&cursor_only),
        "asynchronous terminal cursor motion is not host-surface mutation"
    );

    let mut id_changed = baseline.clone();
    id_changed[1]["id"] = serde_json::json!(6);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&id_changed),
        "a pane id change is host-surface mutation"
    );

    let mut geometry_changed = baseline.clone();
    geometry_changed[1]["pane_columns"] = serde_json::json!(80);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&geometry_changed),
        "a geometry change is host-surface mutation"
    );

    let mut focus_changed = baseline.clone();
    focus_changed[1]["is_focused"] = serde_json::json!(true);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&focus_changed),
        "a focus change is host-surface mutation"
    );

    let mut command_changed = baseline.clone();
    command_changed[1]["terminal_command"] = serde_json::json!("visit workspace-a --tab 1");
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&command_changed),
        "a command change is host-surface mutation"
    );

    let mut suppression_changed = baseline.clone();
    suppression_changed[0]["is_suppressed"] = serde_json::json!(true);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&suppression_changed),
        "a suppression change is host-surface mutation"
    );

    let mut lifecycle_changed = baseline.clone();
    lifecycle_changed[1]["exited"] = serde_json::json!(true);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&lifecycle_changed),
        "a lifecycle change is host-surface mutation"
    );

    let dropped_pane = serde_json::json!([baseline[0].clone()]);
    assert_ne!(
        host_surface_identity(&baseline),
        host_surface_identity(&dropped_pane),
        "dropping a pane is host-surface mutation, not a count-only compare"
    );
}

fn activate_guest_tab_payload_json(session: &str, tab: usize) -> String {
    format!(r#"{{"session":"{session}","activate_tab":{tab}}}"#)
}
