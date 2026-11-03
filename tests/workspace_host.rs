//! Isolated native-peer proof using the built Frame client on a kernel PTY.
//! The legacy project-workspace CLI moves that frontend between complete
//! sessions. Peer-local chrome, tabs, panes and workloads survive navigation
//! and outer detach/reattach. Receipts and raw PTY output remain private.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
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
                        let _ = std::fs::write(self.home.join(format!("pty-release-{token}")), "1");
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
        ("TMPDIR".to_owned(), home.join("tmp").display().to_string()),
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

fn frame_bin() -> PathBuf {
    std::env::var_os("FRAME_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_vc-frame")))
}

fn clear_ambient_session_env(command: &mut Command) {
    // Keep tool lookup and locale, but no enclosing Frame/Runtime Pack policy.
    command.env_clear();
    for key in ["PATH", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.env("SHELL", "/bin/sh");
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

/// A terminal's cursor may advance independently of a pane inventory request.
/// Refusal/duplicate checks retain every other field, including focus, geometry,
/// suppression and lifecycle. Reconnect comparisons below explicitly retain only
/// stable ownership fields, since attachment legitimately resizes the viewport.
const PEER_SURFACE_VOLATILE_CURSOR_FIELD: &str = "cursor_coordinates_in_pane";

fn pane_peer_surface_identity(pane: &serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(map) = pane else {
        return pane.clone();
    };
    let mut retained = map.clone();
    retained.remove(PEER_SURFACE_VOLATILE_CURSOR_FIELD);
    serde_json::Value::Object(retained)
}

fn peer_surface_identity(snapshot: &serde_json::Value) -> serde_json::Value {
    match snapshot {
        serde_json::Value::Array(panes) => {
            serde_json::Value::Array(panes.iter().map(pane_peer_surface_identity).collect())
        },
        other => pane_peer_surface_identity(other),
    }
}

fn fixture_client_ids(listing: &str) -> BTreeSet<u16> {
    let mut lines = listing.lines();
    assert_eq!(
        lines
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["CLIENT_ID", "ZELLIJ_PANE_ID", "RUNNING_COMMAND"],
        "client schema: {listing}"
    );
    lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split_whitespace()
                .next()
                .unwrap()
                .parse::<u16>()
                .unwrap_or_else(|error| panic!("invalid interactive client: {error}: {line}"))
        })
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

    // Runtime PATH may begin with a Python door requiring inherited runtime
    // policy. Use its already selected interpreter directly in the private env.
    let python = std::env::var_os("VIBECRAFTED_PYTHON").unwrap_or_else(|| "python3".into());
    let mut command = Command::new(python);
    command
        .arg(&script)
        .arg(frame_bin())
        .arg(session)
        .arg(&attached)
        .arg(&release)
        .stdout(std::fs::File::create(home.join(format!("pty-helper-{token}.stdout"))).unwrap())
        .stderr(std::fs::File::create(home.join(format!("pty-helper-{token}.stderr"))).unwrap());
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

fn start_peer_marker(
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

fn wake_peer_marker(socket_dir: &Path, home: &Path, session: &str, marker: &str, pid_name: &str) {
    let token = session;
    let child = spawn_pty_attach(socket_dir, home, session, token);
    assert!(
        wait_for_pty_attached(home, token, Duration::from_secs(30)),
        "peer {session} did not accept a short attach to start its marker"
    );
    thread::sleep(Duration::from_millis(400));
    let mut last_start = String::new();
    let start_deadline = Instant::now() + Duration::from_secs(12);
    while Instant::now() < start_deadline {
        if marker_pid_alive(home, pid_name) {
            break;
        }
        let (ok, out) = start_peer_marker(socket_dir, home, session, marker, pid_name);
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
            "printf '%s\\n' PEER_A_EXTRA_TAB_VISIBLE; echo $$ > {}; exec sleep 10000",
            home.join("peer-a-extra.pid").display()
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
                "A extra tab",
                "--no-focus",
                "--",
                "sh",
                "-c",
                &script,
            ],
        );
        assert!(ok, "create extra peer tab failed: {out}");
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker_pid_alive(home, pid_name)
        || (session == "workspace-a" && !marker_pid_alive(home, "peer-a-extra.pid"))
    {
        assert!(
            Instant::now() < deadline,
            "peer marker exec did not complete: {session}"
        );
        thread::sleep(Duration::from_millis(100));
    }
    let _ = release_pty(home, token, child);
    assert!(
        marker_pid_alive(home, pid_name),
        "peer {session} marker process missing after wake:\n{listed}\nstart:{last_start}"
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
    std::fs::create_dir_all(home.join("tmp")).unwrap();
    let config_dir = home.join("config/vc-frame");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.kdl"),
        "show_startup_tips false\ndefault_shell \"/bin/sh\"\n",
    )
    .unwrap();
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
    let socket = socket_dir
        .join(zellij_utils::consts::CLIENT_SERVER_CONTRACT_DIR.as_str())
        .join(session);
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

fn peer_panes(socket_dir: &Path, home: &Path, session: &str) -> serde_json::Value {
    let (ok, out) = run_frame(
        socket_dir,
        home,
        &[
            "--session",
            session,
            "action",
            "list-panes",
            "--all",
            "--json",
            "--command",
        ],
    );
    assert!(ok, "list {session} panes: {out}");
    serde_json::from_str(&out).unwrap_or_else(|error| panic!("{session} pane JSON: {error}: {out}"))
}

/// Attach/resize/focus may change viewport geometry. IDs, commands, tab ownership,
/// plugin ownership and lifecycle must remain identical across native reconnects.
fn peer_inventory_identity(panes: &serde_json::Value) -> serde_json::Value {
    serde_json::Value::Array(
        panes
            .as_array()
            .expect("pane array")
            .iter()
            .map(|pane| {
                let mut stable = serde_json::Map::new();
                for field in [
                    "id",
                    "is_plugin",
                    "plugin_url",
                    "plugin_runtime_id",
                    "pane_command",
                    "terminal_command",
                    "tab_id",
                    "tab_name",
                    "tab_position",
                    "is_suppressed",
                    "exited",
                    "exit_status",
                ] {
                    stable.insert(field.to_owned(), pane[field].clone());
                }
                serde_json::Value::Object(stable)
            })
            .collect(),
    )
}

fn assert_complete_peer(panes: &serde_json::Value, expected_tabs: &[&str]) {
    let rows = panes.as_array().expect("peer pane array");
    for plugin in ["session-manager", "compact-bar", "status-bar"] {
        // Session canvas mounts one runtime owner through per-tab projector
        // panes. Match collect_pane_list's explicit plugin_runtime_id contract.
        let chrome: Vec<_> = rows
            .iter()
            .filter(|pane| {
                pane["is_plugin"] == true
                    && pane["plugin_url"]
                        .as_str()
                        .is_some_and(|url| url.rsplit(':').next() == Some(plugin))
            })
            .collect();
        assert!(!chrome.is_empty(), "peer must own {plugin}: {panes}");
        let owners: BTreeSet<_> = chrome
            .iter()
            .map(|pane| {
                pane["plugin_runtime_id"]
                    .as_u64()
                    .expect("chrome runtime identity")
            })
            .collect();
        assert_eq!(
            owners.len(),
            1,
            "one runtime owner for peer {plugin}: {panes}"
        );
        assert!(
            chrome
                .iter()
                .all(|pane| pane["exited"] == false && pane["is_suppressed"] == false),
            "peer chrome stays live and visible: {panes}"
        );
    }
    let tabs: BTreeSet<_> = rows
        .iter()
        .filter(|pane| pane["is_plugin"] == false)
        .filter_map(|pane| pane["tab_name"].as_str())
        .collect();
    assert_eq!(
        tabs,
        expected_tabs.iter().copied().collect(),
        "complete peer tabs: {panes}"
    );
    assert!(
        rows.iter()
            .all(|pane| !pane.to_string().contains("--workspace-projection")),
        "no nested visitor pane: {panes}"
    );
}

fn client_ids(socket_dir: &Path, home: &Path, session: &str) -> BTreeSet<u16> {
    let (ok, out) = run_frame(
        socket_dir,
        home,
        &["--session", session, "action", "list-clients"],
    );
    assert!(ok, "list {session} clients: {out}");
    fixture_client_ids(&out)
}

fn await_client_count(
    socket_dir: &Path,
    home: &Path,
    session: &str,
    count: usize,
) -> BTreeSet<u16> {
    let listing = wait_until(
        socket_dir,
        home,
        &["--session", session, "action", "list-clients"],
        Duration::from_secs(15),
        |listing| listing.starts_with("CLIENT_ID") && fixture_client_ids(listing).len() == count,
    );
    let ids = fixture_client_ids(&listing);
    assert_eq!(ids.len(), count, "{session} client memberships: {listing}");
    ids
}

fn pty_output_path(home: &Path, token: &str) -> PathBuf {
    home.join(format!("pty-attached-{token}.screen"))
}

fn pty_output_len(home: &Path, token: &str) -> usize {
    std::fs::read(pty_output_path(home, token)).unwrap().len()
}

fn assert_fresh_frontend_output(home: &Path, token: &str, offset: usize, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let bytes = std::fs::read(pty_output_path(home, token)).unwrap();
        let fresh = String::from_utf8_lossy(&bytes[offset..]);
        if fresh.contains(marker) {
            fixture_receipt(
                home,
                serde_json::json!({"event": "frontend_render", "token": token,
                "offset": offset, "end": bytes.len(), "marker": marker}),
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "frontend did not render {marker} after offset {offset}: {fresh}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

fn assert_workloads(home: &Path, names: &[&str], expected: &[String]) {
    let current: Vec<_> = names
        .iter()
        .map(|name| {
            assert!(
                marker_pid_alive(home, name),
                "workload {name} must remain alive"
            );
            std::fs::read_to_string(home.join(name)).unwrap()
        })
        .collect();
    assert_eq!(current, expected, "exact peer workload PIDs must survive");
}

fn switch_peer(socket_dir: &Path, home: &Path, source: &str, target: &str, tab: &str) {
    let (ok, out) = run_frame(
        socket_dir,
        home,
        &[
            "--session",
            source,
            "project-workspace",
            target,
            "--tab",
            tab,
        ],
    );
    assert!(ok, "native {source} -> {target}/{tab}: {out}");
}

#[test]
fn attached_frontend_navigates_complete_peers_and_survives_outer_detach() {
    let socket_dir = unique_socket_dir();
    let home = socket_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    eprintln!(
        "workspace_host native-peer fixture: {}",
        socket_dir.display()
    );
    let _cleanup = FixtureCleanup {
        socket_dir: socket_dir.clone(),
        home: home.clone(),
    };
    dump_fixture_plugins(&socket_dir, &home);
    for (session, layout) in [
        ("frame-host", "vibecrafted-host"),
        ("workspace-a", "vibecrafted-guest"),
        ("workspace-b", "vibecrafted-guest"),
    ] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &["--layout", layout, "attach", "-b", "-c", session],
        );
        assert!(ok, "create {session}: {out}");
        assert!(wait_for_session(
            &socket_dir,
            &home,
            session,
            Duration::from_secs(20)
        ));
    }
    for (session, marker, pid) in [
        ("workspace-a", "PEER_A_VISIBLE", "peer-a.pid"),
        ("workspace-b", "PEER_B_VISIBLE", "peer-b.pid"),
    ] {
        wake_peer_marker(&socket_dir, &home, session, marker, pid);
        await_client_count(&socket_dir, &home, session, 0);
    }
    // A role-marked session with no actual frontend is still ineligible.
    let (ok, out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "frame-host",
            "project-workspace",
            "workspace-a",
        ],
    );
    assert!(
        !ok && out.contains("no attached interactive client"),
        "no-origin refusal: {out}"
    );

    let pty = spawn_pty_attach(&socket_dir, &home, "frame-host", "outer");
    assert!(wait_for_pty_attached(
        &home,
        "outer",
        Duration::from_secs(30)
    ));
    await_client_count(&socket_dir, &home, "frame-host", 1);
    // Every peer has now had a real 160x40 frontend: compare stable inventory
    // across later reconnects, while retaining full raw snapshots in receipts.
    let sessions = ["frame-host", "workspace-a", "workspace-b"];
    let before = sessions.map(|session| peer_panes(&socket_dir, &home, session));
    assert_complete_peer(&before[0], &["Launchpad"]);
    assert_complete_peer(&before[1], &["Launchpad", "Agents", "Shell", "A extra tab"]);
    assert_complete_peer(&before[2], &["Launchpad", "Agents", "Shell"]);
    let servers = sessions.map(|session| fixture_session_server_pid(&socket_dir, &home, session));
    let marker_names = ["peer-a.pid", "peer-a-extra.pid", "peer-b.pid"];
    let marker_pids: Vec<_> = marker_names
        .iter()
        .map(|name| std::fs::read_to_string(home.join(name)).unwrap())
        .collect();
    fixture_receipt(
        &home,
        serde_json::json!({"event": "peers_before", "panes": before,
        "servers": servers, "marker_pids": marker_pids}),
    );

    for (source, target, tab, marker) in [
        ("frame-host", "workspace-a", "1", "PEER_A_VISIBLE"),
        (
            "workspace-a",
            "workspace-a",
            "4",
            "PEER_A_EXTRA_TAB_VISIBLE",
        ),
        ("workspace-a", "workspace-a", "1", "PEER_A_VISIBLE"),
        ("workspace-a", "workspace-b", "1", "PEER_B_VISIBLE"),
        ("workspace-b", "workspace-a", "1", "PEER_A_VISIBLE"),
    ] {
        let offset = pty_output_len(&home, "outer");
        switch_peer(&socket_dir, &home, source, target, tab);
        for session in sessions {
            await_client_count(&socket_dir, &home, session, usize::from(session == target));
        }
        assert_fresh_frontend_output(&home, "outer", offset, marker);
        let screen = wait_until(
            &socket_dir,
            &home,
            &["--session", target, "action", "dump-screen"],
            Duration::from_secs(15),
            |screen| screen.contains(marker),
        );
        assert!(screen.contains(marker), "target content: {screen}");
        for other in [
            "PEER_A_VISIBLE",
            "PEER_A_EXTRA_TAB_VISIBLE",
            "PEER_B_VISIBLE",
        ] {
            if other != marker {
                assert!(!screen.contains(other), "stale peer/tab content: {screen}");
            }
        }
        for (index, session) in sessions.iter().enumerate() {
            let current = peer_panes(&socket_dir, &home, session);
            assert_eq!(
                peer_inventory_identity(&before[index]),
                peer_inventory_identity(&current),
                "{session} retains its own rail, panes and tabs through {source} -> {target}"
            );
            assert_eq!(
                servers[index],
                fixture_session_server_pid(&socket_dir, &home, session)
            );
        }
        assert_workloads(&home, &marker_names, &marker_pids);
        fixture_receipt(
            &home,
            serde_json::json!({"event": "native_transition", "source": source,
            "target": target, "tab": tab, "clients": client_ids(&socket_dir, &home, target), "screen": screen}),
        );
    }

    let original = await_client_count(&socket_dir, &home, "workspace-a", 1);
    let second = spawn_pty_attach(&socket_dir, &home, "workspace-a", "second");
    assert!(wait_for_pty_attached(
        &home,
        "second",
        Duration::from_secs(30)
    ));
    let two = await_client_count(&socket_dir, &home, "workspace-a", 2);
    let before_refusal = peer_panes(&socket_dir, &home, "workspace-a");
    for selector in [None, Some("65535")] {
        let mut args = vec!["--session", "workspace-a"];
        if let Some(id) = selector {
            args.extend(["--client-id", id]);
        }
        args.extend(["project-workspace", "workspace-b", "--tab", "1"]);
        let (ok, out) = run_frame(&socket_dir, &home, &args);
        let reason = if selector.is_none() {
            "multiple attached clients"
        } else {
            "not attached to this session"
        };
        assert!(
            !ok && out.contains(reason),
            "native targeting refusal: {out}"
        );
        assert_eq!(client_ids(&socket_dir, &home, "workspace-a"), two);
        assert!(client_ids(&socket_dir, &home, "workspace-b").is_empty());
        assert_eq!(
            peer_surface_identity(&before_refusal),
            peer_surface_identity(&peer_panes(&socket_dir, &home, "workspace-a"))
        );
        assert_workloads(&home, &marker_names, &marker_pids);
    }
    let selected = original.first().unwrap().to_string();
    let outer_offset = pty_output_len(&home, "outer");
    let (ok, out) = run_frame(
        &socket_dir,
        &home,
        &[
            "--session",
            "workspace-a",
            "--client-id",
            &selected,
            "project-workspace",
            "workspace-b",
            "--tab",
            "1",
        ],
    );
    assert!(ok, "explicit original frontend selection: {out}");
    assert_eq!(
        await_client_count(&socket_dir, &home, "workspace-a", 1),
        &two - &original
    );
    await_client_count(&socket_dir, &home, "workspace-b", 1);
    assert_fresh_frontend_output(&home, "outer", outer_offset, "PEER_B_VISIBLE");
    let _ = release_pty(&home, "second", second);
    await_client_count(&socket_dir, &home, "workspace-a", 0);

    // Duplicate creation must preserve every complete peer, not a projected host.
    let before_duplicate = sessions.map(|session| peer_panes(&socket_dir, &home, session));
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
            "workspace-a",
        ],
    );
    assert!(
        !ok && out.contains("already exists"),
        "duplicate refusal: {out}"
    );
    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(
            peer_surface_identity(&before_duplicate[index]),
            peer_surface_identity(&peer_panes(&socket_dir, &home, session))
        );
    }
    let _ = release_pty(&home, "outer", pty);
    for session in sessions {
        await_client_count(&socket_dir, &home, session, 0);
    }
    assert_workloads(&home, &marker_names, &marker_pids);
    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(
            servers[index],
            fixture_session_server_pid(&socket_dir, &home, session)
        );
        assert_eq!(
            peer_inventory_identity(&before[index]),
            peer_inventory_identity(&peer_panes(&socket_dir, &home, session))
        );
    }
    fixture_receipt(
        &home,
        serde_json::json!({"event": "outer_detached", "servers": servers, "marker_pids": marker_pids}),
    );
    let reattached = spawn_pty_attach(&socket_dir, &home, "workspace-a", "reattached");
    assert!(wait_for_pty_attached(
        &home,
        "reattached",
        Duration::from_secs(30)
    ));
    await_client_count(&socket_dir, &home, "workspace-a", 1);
    assert_fresh_frontend_output(&home, "reattached", 0, "PEER_A_VISIBLE");
    let offset = pty_output_len(&home, "reattached");
    switch_peer(&socket_dir, &home, "workspace-a", "workspace-b", "1");
    await_client_count(&socket_dir, &home, "workspace-a", 0);
    await_client_count(&socket_dir, &home, "workspace-b", 1);
    assert_fresh_frontend_output(&home, "reattached", offset, "PEER_B_VISIBLE");
    assert_workloads(&home, &marker_names, &marker_pids);
    let after_reattach = sessions.map(|session| peer_panes(&socket_dir, &home, session));
    for (index, session) in sessions.iter().enumerate() {
        assert_eq!(
            servers[index],
            fixture_session_server_pid(&socket_dir, &home, session)
        );
        assert_eq!(
            peer_inventory_identity(&before[index]),
            peer_inventory_identity(&after_reattach[index]),
            "{session} retains pane/tab/runtime ownership after outer reattach"
        );
    }
    fixture_receipt(
        &home,
        serde_json::json!({"event": "peers_after_reattach", "panes": after_reattach,
        "servers": servers, "marker_pids": marker_pids}),
    );
    let _ = release_pty(&home, "reattached", reattached);
    assert_fixture_cleanup(&socket_dir, &home);
    eprintln!("retained native-peer receipts: {}", home.display());
}

#[test]
fn ordinary_attached_peer_navigates_without_host_role_and_preserves_workload() {
    let socket_dir = unique_socket_dir();
    let home = socket_dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    eprintln!(
        "workspace_host ordinary-peer fixture: {}",
        socket_dir.display()
    );
    let _cleanup = FixtureCleanup {
        socket_dir: socket_dir.clone(),
        home: home.clone(),
    };
    dump_fixture_plugins(&socket_dir, &home);
    for session in ["ordinary-shell", "target-peer"] {
        let (ok, out) = run_frame(
            &socket_dir,
            &home,
            &["--layout", "vibecrafted", "attach", "-b", "-c", session],
        );
        assert!(ok, "create ordinary peer: {out}");
        assert!(wait_for_session(
            &socket_dir,
            &home,
            session,
            Duration::from_secs(20)
        ));
    }
    wake_peer_marker(
        &socket_dir,
        &home,
        "target-peer",
        "TARGET_PEER_VISIBLE",
        "target.pid",
    );
    let pty = spawn_pty_attach(&socket_dir, &home, "ordinary-shell", "ordinary");
    assert!(wait_for_pty_attached(
        &home,
        "ordinary",
        Duration::from_secs(30)
    ));
    await_client_count(&socket_dir, &home, "ordinary-shell", 1);
    // Command text is not role or navigation authority. Preserve the old focused
    // marker regression as a positive native switch, not a host-role refusal.
    let (ok, out) = start_peer_marker(
        &socket_dir,
        &home,
        "ordinary-shell",
        "VC_FRAME_GUEST_SURFACE=1_FRAME_PLUGIN=frame-host_ORDINARY_VISIBLE",
        "ordinary.pid",
    );
    assert!(ok, "ordinary focused workload: {out}");
    assert_fresh_frontend_output(&home, "ordinary", 0, "ORDINARY_VISIBLE");
    let before = peer_panes(&socket_dir, &home, "ordinary-shell");
    assert_complete_peer(&before, &["Launchpad", "Agents", "Shell"]);
    let target_before = peer_panes(&socket_dir, &home, "target-peer");
    assert_complete_peer(&target_before, &["Launchpad", "Agents", "Shell"]);
    let names = ["ordinary.pid", "target.pid"];
    let pids: Vec<_> = names
        .iter()
        .map(|name| std::fs::read_to_string(home.join(name)).unwrap())
        .collect();
    let offset = pty_output_len(&home, "ordinary");
    switch_peer(&socket_dir, &home, "ordinary-shell", "target-peer", "1");
    await_client_count(&socket_dir, &home, "ordinary-shell", 0);
    await_client_count(&socket_dir, &home, "target-peer", 1);
    assert_fresh_frontend_output(&home, "ordinary", offset, "TARGET_PEER_VISIBLE");
    let offset = pty_output_len(&home, "ordinary");
    switch_peer(&socket_dir, &home, "target-peer", "ordinary-shell", "1");
    await_client_count(&socket_dir, &home, "target-peer", 0);
    await_client_count(&socket_dir, &home, "ordinary-shell", 1);
    assert_fresh_frontend_output(&home, "ordinary", offset, "ORDINARY_VISIBLE");
    let (screen_ok, screen) = dump_session_screen(&socket_dir, &home, "ordinary-shell");
    assert!(
        screen_ok && screen.contains("ORDINARY_VISIBLE"),
        "ordinary peer content after return: {screen}"
    );
    assert_eq!(
        peer_inventory_identity(&before),
        peer_inventory_identity(&peer_panes(&socket_dir, &home, "ordinary-shell"))
    );
    assert_eq!(
        peer_inventory_identity(&target_before),
        peer_inventory_identity(&peer_panes(&socket_dir, &home, "target-peer"))
    );
    assert_workloads(&home, &names, &pids);
    let _ = release_pty(&home, "ordinary", pty);
    assert_fixture_cleanup(&socket_dir, &home);
    eprintln!("retained ordinary-peer receipts: {}", home.display());
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
fn peer_surface_identity_accepts_cursor_drift_and_rejects_structure() {
    let baseline = serde_json::json!([
        {
            "id": 3,
            "is_plugin": true,
            "is_focused": true,
            "is_suppressed": false,
            "exited": false,
            "exit_status": null,
            "plugin_url": "session-manager",
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
            "terminal_command": "sh -c peer-b-workload",
            "cursor_coordinates_in_pane": [1, 1]
        }
    ]);
    let mut cursor_only = baseline.clone();
    cursor_only[1]["cursor_coordinates_in_pane"] = serde_json::json!([69, 3]);
    assert_eq!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&cursor_only),
        "asynchronous terminal cursor motion is not peer-surface mutation"
    );

    let mut id_changed = baseline.clone();
    id_changed[1]["id"] = serde_json::json!(6);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&id_changed),
        "a pane id change is peer-surface mutation"
    );

    let mut geometry_changed = baseline.clone();
    geometry_changed[1]["pane_columns"] = serde_json::json!(80);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&geometry_changed),
        "a geometry change is peer-surface mutation"
    );

    let mut focus_changed = baseline.clone();
    focus_changed[1]["is_focused"] = serde_json::json!(true);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&focus_changed),
        "a focus change is peer-surface mutation"
    );

    let mut command_changed = baseline.clone();
    command_changed[1]["terminal_command"] = serde_json::json!("sh -c peer-a-workload");
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&command_changed),
        "a command change is peer-surface mutation"
    );

    let mut suppression_changed = baseline.clone();
    suppression_changed[0]["is_suppressed"] = serde_json::json!(true);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&suppression_changed),
        "a suppression change is peer-surface mutation"
    );

    let mut lifecycle_changed = baseline.clone();
    lifecycle_changed[1]["exited"] = serde_json::json!(true);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&lifecycle_changed),
        "a lifecycle change is peer-surface mutation"
    );

    let dropped_pane = serde_json::json!([baseline[0].clone()]);
    assert_ne!(
        peer_surface_identity(&baseline),
        peer_surface_identity(&dropped_pane),
        "dropping a pane is peer-surface mutation, not a count-only compare"
    );
}
