use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use vte;
use zellij_server::panes::sixel::SixelImageStore;
use zellij_server::panes::{LinkHandler, TerminalPane};
use zellij_utils::data::{Palette, Style};
use zellij_utils::pane_size::{Dimension, PaneGeom, Size, SizeInPixels};

use ssh2::Session;
use std::io::prelude::*;
use std::net::TcpStream;

use std::path::{Path, PathBuf};

use std::cell::RefCell;
use std::rc::Rc;

const ZELLIJ_EXECUTABLE_LOCATION: &str = "/usr/src/zellij/zellij";
const SET_ENV_VARIABLES: &str = "EDITOR=/usr/bin/vi";
const E2E_RUNTIME_ROOT: &str = "/tmp/vc-frame-e2e";
const E2E_SOCKET_DIR: &str = "/tmp/vc-frame-e2e/sockets";
const E2E_CACHE_DIR: &str = "/tmp/vc-frame-e2e/cache";
const ZELLIJ_CONFIG_PATH: &str = "/usr/src/zellij/fixtures/configs";
const ZELLIJ_CONFIG_DIRS_PATH: &str = "/usr/src/zellij/fixtures/config-dirs";
const ZELLIJ_DATA_DIR: &str = "/usr/src/zellij/e2e-data";
const ZELLIJ_FIXTURE_PATH: &str = "/usr/src/zellij/fixtures";
const E2E_DEFAULT_LAYOUT: &str = "/usr/src/zellij/fixtures/e2e-default.kdl";
const CONNECTION_STRING: &str = "127.0.0.1:2222";
const CONNECTION_USERNAME: &str = "test";
/// Points at the private key whose public half was handed to the e2e ssh
/// container. Set by the workflow and by the local docker-compose flow
/// (see CONTRIBUTING.md).
const SSH_KEY_ENV: &str = "ZELLIJ_E2E_SSH_KEY";
const SESSION_NAME: &str = "e2e-test";
const RETRIES: usize = 10;

/// Public-key only. There is deliberately no password fallback: a static
/// `test`/`test` credential on this service container is exactly what was
/// abused on 2026-07-30 to plant a cryptominer on the runner.
fn authenticate(sess: &ssh2::Session) {
    let key_path = std::env::var(SSH_KEY_ENV).unwrap_or_else(|_| {
        panic!(
            "{SSH_KEY_ENV} is not set. It must point at the private key whose public half \
             was given to the e2e ssh container (see CONTRIBUTING.md)."
        )
    });
    let private_key = PathBuf::from(&key_path);
    let public_key = PathBuf::from(format!("{key_path}.pub"));
    let public_key = public_key.exists().then_some(public_key);
    sess.userauth_pubkey_file(
        CONNECTION_USERNAME,
        public_key.as_deref(),
        &private_key,
        None,
    )
    .unwrap_or_else(|error| panic!("ssh public-key auth failed with {key_path}: {error}"));
    assert!(
        sess.authenticated(),
        "ssh session did not authenticate with {key_path}"
    );
}

fn ssh_connect() -> ssh2::Session {
    let tcp = TcpStream::connect(CONNECTION_STRING).unwrap();
    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    sess.handshake().unwrap();
    authenticate(&sess);
    sess
}

fn ssh_connect_without_timeout() -> ssh2::Session {
    let tcp = TcpStream::connect(CONNECTION_STRING).unwrap();
    let mut sess = Session::new().unwrap();
    sess.set_tcp_stream(tcp);
    sess.handshake().unwrap();
    authenticate(&sess);
    sess
}

fn setup_remote_environment(channel: &mut ssh2::Channel, win_size: Size) {
    let (columns, rows) = (win_size.cols as u32, win_size.rows as u32);
    channel
        .request_pty("xterm", None, Some((columns, rows, 0, 0)))
        .unwrap();
    channel.shell().unwrap();
    channel.write_all(b"export PS1=\"$ \"\n").unwrap();
    channel
        .write_all(
            format!(
                "export VC_FRAME_SOCKET_DIR={E2E_SOCKET_DIR}\n\
                 export XDG_CACHE_HOME={E2E_CACHE_DIR}\n\
                 mkdir -p \"$VC_FRAME_SOCKET_DIR\" \"$XDG_CACHE_HOME\"\n"
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn cleanup_remote_runtime_command() -> String {
    format!(
        r#"export VC_FRAME_SOCKET_DIR={E2E_SOCKET_DIR}
export ZELLIJ_SOCKET_DIR="$VC_FRAME_SOCKET_DIR"
export XDG_CACHE_HOME={E2E_CACHE_DIR}
mkdir -p "$VC_FRAME_SOCKET_DIR" "$XDG_CACHE_HOME"
if [ -x {ZELLIJ_EXECUTABLE_LOCATION} ]; then
  {ZELLIJ_EXECUTABLE_LOCATION} kill-all-sessions --yes >/dev/null 2>&1 || true
fi
cleanup_attempt=0
while ps -eo args= | grep -Eq '([v]c-frame|[z]ellij) --server .*/vc-frame-e2e/sockets/' && [ "$cleanup_attempt" -lt 100 ]; do
  sleep 0.1
  cleanup_attempt=$((cleanup_attempt + 1))
done
if ps -eo args= | grep -Eq '([v]c-frame|[z]ellij) --server .*/vc-frame-e2e/sockets/'; then
  printf 'vc-frame e2e cleanup failed: an isolated server did not stop gracefully\n' >&2
  exit 1
fi
rm -rf {E2E_SOCKET_DIR} {E2E_CACHE_DIR}
mkdir -p {E2E_SOCKET_DIR} {E2E_CACHE_DIR}
ln -sf /usr/src/zellij/$(uname -m)-unknown-linux-musl/release/vc-frame {ZELLIJ_EXECUTABLE_LOCATION}
"#
    )
}

fn cleanup_remote_runtime(sess: &ssh2::Session) -> Result<(), String> {
    let mut channel = sess
        .channel_session()
        .map_err(|error| format!("failed to open remote cleanup channel: {error}"))?;
    channel
        .exec(&cleanup_remote_runtime_command())
        .map_err(|error| format!("failed to execute remote cleanup: {error}"))?;

    let mut stdout = String::new();
    channel
        .write_all(b"rm -rf ~/.cache/zellij/permissions.kdl\n")
        .unwrap();
    // NOTE: the arch-independent /usr/src/zellij/zellij symlink is created on the HOST
    // — by the workflow step "Publish arch-stable binary path for the container", or
    // by the local docker-compose flow in CONTRIBUTING.md. The mount is :ro, so the
    // container must not try to write it.
}

fn start_zellij(channel: &mut ssh2::Channel) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} --new-session-with-layout {} options --show-release-notes false --show-startup-tips false\n",
                SET_ENV_VARIABLES,
                ZELLIJ_EXECUTABLE_LOCATION,
                SESSION_NAME,
                ZELLIJ_DATA_DIR,
                E2E_DEFAULT_LAYOUT
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_with_config_dir(channel: &mut ssh2::Channel, config_dir: &str) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} --config-dir {}/{} options --show-release-notes false --show-startup-tips false\n",
                SET_ENV_VARIABLES, ZELLIJ_EXECUTABLE_LOCATION, SESSION_NAME, ZELLIJ_DATA_DIR, ZELLIJ_CONFIG_DIRS_PATH, config_dir
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_mirrored_session(channel: &mut ssh2::Channel) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} options --show-release-notes false --show-startup-tips false --mirror-session true --serialization-interval 1\n",
                SET_ENV_VARIABLES, ZELLIJ_EXECUTABLE_LOCATION, SESSION_NAME, ZELLIJ_DATA_DIR
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_mirrored_session_with_layout(channel: &mut ssh2::Channel, layout_file_name: &str) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} --new-session-with-layout {}/{} options --show-release-notes false --show-startup-tips false --mirror-session true --serialization-interval 1\n",
                SET_ENV_VARIABLES,
                ZELLIJ_EXECUTABLE_LOCATION,
                SESSION_NAME,
                ZELLIJ_DATA_DIR,
                ZELLIJ_FIXTURE_PATH,
                layout_file_name
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_mirrored_session_with_layout_and_viewport_serialization(
    channel: &mut ssh2::Channel,
    layout_file_name: &str,
) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} --new-session-with-layout {}/{} options --show-release-notes false --show-startup-tips false --mirror-session true --serialize-pane-viewport true --serialization-interval 1\n",
                SET_ENV_VARIABLES,
                ZELLIJ_EXECUTABLE_LOCATION,
                SESSION_NAME,
                ZELLIJ_DATA_DIR,
                ZELLIJ_FIXTURE_PATH,
                layout_file_name
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_in_session(channel: &mut ssh2::Channel, session_name: &str, mirrored: bool) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} options --show-release-notes false --show-startup-tips false --mirror-session {}\n",
                SET_ENV_VARIABLES,
                ZELLIJ_EXECUTABLE_LOCATION,
                session_name,
                ZELLIJ_DATA_DIR,
                mirrored
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn attach_to_existing_session(channel: &mut ssh2::Channel, session_name: &str) {
    channel
        .write_all(
            format!(
                "{} {} attach {}\n",
                SET_ENV_VARIABLES, ZELLIJ_EXECUTABLE_LOCATION, session_name
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn watch_existing_session(channel: &mut ssh2::Channel, session_name: &str) {
    channel
        .write_all(
            format!(
                "{} {} watch {}\n",
                SET_ENV_VARIABLES, ZELLIJ_EXECUTABLE_LOCATION, session_name
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_without_frames(channel: &mut ssh2::Channel) {
    channel
        .write_all(
            format!(
                "{} {} --session {} --data-dir {} options --show-release-notes false --show-startup-tips false --pane-frames false\n",
                SET_ENV_VARIABLES, ZELLIJ_EXECUTABLE_LOCATION, SESSION_NAME, ZELLIJ_DATA_DIR
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn start_zellij_with_config(channel: &mut ssh2::Channel, config_path: &str) {
    channel
        .write_all(
            format!(
                "{} {} --config {} --session {} --data-dir {} options --show-release-notes false --show-startup-tips false\n",
                SET_ENV_VARIABLES,
                ZELLIJ_EXECUTABLE_LOCATION,
                config_path,
                SESSION_NAME,
                ZELLIJ_DATA_DIR
            )
            .as_bytes(),
        )
        .unwrap();
    channel.flush().unwrap();
}

fn wait_for_startup(last_snapshot: &Arc<Mutex<String>>) {
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(10);
    loop {
        if last_snapshot.lock().unwrap().contains("Ctrl +") {
            break;
        }
        if start.elapsed() > timeout {
            break; // timed out — let the test proceed and fail naturally
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn read_from_channel(
    channel: &Arc<Mutex<ssh2::Channel>>,
    last_snapshot: &Arc<Mutex<String>>,
    cursor_coordinates: &Arc<Mutex<(usize, usize)>>,
    pane_geom: &PaneGeom,
) -> (Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let should_keep_running = Arc::new(AtomicBool::new(true));
    let thread = std::thread::Builder::new()
        .name("read_thread".into())
        .spawn({
            let pane_geom = *pane_geom;
            let should_keep_running = should_keep_running.clone();
            let channel = channel.clone();
            let last_snapshot = last_snapshot.clone();
            let cursor_coordinates = cursor_coordinates.clone();
            move || {
                let mut retries_left = 3;
                let mut should_sleep = false;
                let mut vte_parser = vte::Parser::new();
                let character_cell_size = Rc::new(RefCell::new(Some(SizeInPixels {
                    height: 21,
                    width: 8,
                })));
                let sixel_image_store = Rc::new(RefCell::new(SixelImageStore::default()));
                let debug = false;
                let arrow_fonts = true;
                let styled_underlines = true;
                let explicitly_disable_kitty_keyboard_protocol = false;
                let mut terminal_output = TerminalPane::new(
                    0,
                    pane_geom,
                    Style::default(),
                    0,
                    String::new(),
                    Rc::new(RefCell::new(LinkHandler::new())),
                    character_cell_size,
                    sixel_image_store,
                    Rc::new(RefCell::new(Palette::default())),
                    Rc::new(RefCell::new(HashMap::new())),
                    None,
                    None,
                    debug,
                    arrow_fonts,
                    styled_underlines,
                    true, // osc8_hyperlinks
                    explicitly_disable_kitty_keyboard_protocol,
                    None,
                ); // 0 is the pane index
                loop {
                    if !should_keep_running.load(Ordering::SeqCst) {
                        break;
                    }
                    if should_sleep {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        should_sleep = false;
                    }
                    let mut buf = [0u8; 1280000];
                    match channel.lock().unwrap().read(&mut buf) {
                        Ok(0) => {
                            let current_snapshot = take_snapshot(&mut terminal_output);
                            let mut last_snapshot = last_snapshot.lock().unwrap();
                            *cursor_coordinates.lock().unwrap() = terminal_output
                                .cursor_coordinates()
                                .map(|(x, y, _)| (x, y))
                                .unwrap_or((0, 0));
                            *last_snapshot = current_snapshot;
                            should_sleep = true;
                        },
                        Ok(count) => {
                            for byte in buf.iter().take(count) {
                                vte_parser.advance(&mut terminal_output.grid, *byte);
                            }
                            let current_snapshot = take_snapshot(&mut terminal_output);
                            let mut last_snapshot = last_snapshot.lock().unwrap();
                            *cursor_coordinates.lock().unwrap() = terminal_output
                                .grid
                                .cursor_coordinates()
                                .map(|(x, y, _)| (x, y))
                                .unwrap_or((0, 0));
                            *last_snapshot = current_snapshot;
                            should_sleep = true;
                        },
                        Err(e) => {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                let current_snapshot = take_snapshot(&mut terminal_output);
                                let mut last_snapshot = last_snapshot.lock().unwrap();
                                *cursor_coordinates.lock().unwrap() = terminal_output
                                    .cursor_coordinates()
                                    .map(|(x, y, _)| (x, y))
                                    .unwrap_or((0, 0));
                                *last_snapshot = current_snapshot;
                                should_sleep = true;
                            } else if retries_left > 0 {
                                retries_left -= 1;
                            } else {
                                break;
                            }
                        },
                    }
                }
            }
        })
        .unwrap();
    (should_keep_running, thread)
}

pub fn take_snapshot(terminal_output: &mut TerminalPane) -> String {
    let output_lines = terminal_output.read_buffer_as_lines();
    let cursor_coordinates = terminal_output
        .cursor_coordinates()
        .and_then(|(x, y, visible)| if visible { Some((x, y)) } else { None });
    let mut snapshot = String::new();
    for (line_index, line) in output_lines.iter().enumerate() {
        for (character_index, terminal_character) in line.iter().enumerate() {
            if let Some((cursor_x, cursor_y)) = cursor_coordinates
                && line_index == cursor_y
                && character_index == cursor_x
            {
                snapshot.push('█');
                continue;
            }
            snapshot.push(terminal_character.character);
        }
        if line_index != output_lines.len() - 1 {
            snapshot.push('\n');
        }
    }
    snapshot
}

pub struct RemoteTerminal {
    channel: Arc<Mutex<ssh2::Channel>>,
    cursor_x: usize,
    cursor_y: usize,
    last_snapshot: Arc<Mutex<String>>,
}

impl std::fmt::Debug for RemoteTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "cursor x: {}\ncursor_y: {}\ncurrent_snapshot:\n{}",
            self.cursor_x,
            self.cursor_y,
            *self.last_snapshot.lock().unwrap()
        )
    }
}

impl RemoteTerminal {
    pub fn cursor_position_is(&self, x: usize, y: usize) -> bool {
        x == self.cursor_x && y == self.cursor_y
    }
    pub fn status_bar_appears(&self) -> bool {
        self.last_snapshot.lock().unwrap().contains("Ctrl +")
            && self.last_snapshot.lock().unwrap().contains("LOCK")
    }
    pub fn ctrl_plus_appears(&self) -> bool {
        self.last_snapshot.lock().unwrap().contains("Ctrl +")
    }
    pub fn tab_bar_appears(&self) -> bool {
        self.last_snapshot.lock().unwrap().contains("Tab #1")
    }
    pub fn snapshot_contains(&self, text: &str) -> bool {
        self.last_snapshot.lock().unwrap().contains(text)
    }
    pub fn lines(&self) -> Vec<String> {
        let s = self.last_snapshot.lock().unwrap();
        s.lines().map(|s| s.to_owned()).collect::<Vec<_>>()
    }
    #[allow(unused)]
    pub fn current_snapshot(&self) -> String {
        // convenience method for writing tests,
        // this should only be used when developing,
        // please prefer "snapsht_contains" instead
        self.last_snapshot.lock().unwrap().clone()
    }
    #[allow(unused)]
    pub fn current_cursor_position(&self) -> String {
        // convenience method for writing tests,
        // this should only be used when developing,
        // please prefer "cursor_position_is" instead
        format!("x: {}, y: {}", self.cursor_x, self.cursor_y)
    }
    pub fn send_key(&mut self, key: &[u8]) {
        let mut channel = self.channel.lock().unwrap();
        channel.write_all(key).unwrap();
        channel.flush().unwrap();
    }
    pub fn change_size(&mut self, cols: u32, rows: u32) {
        self.channel
            .lock()
            .unwrap()
            .request_pty_size(cols, rows, Some(cols), Some(rows))
            .unwrap();
    }
    pub fn attach_to_original_session(&mut self) {
        {
            let mut channel = self.channel.lock().unwrap();
            channel
                .write_all(
                    format!("{} attach {}\n", ZELLIJ_EXECUTABLE_LOCATION, SESSION_NAME).as_bytes(),
                )
                .unwrap();
            channel.flush().unwrap();
        } // release mutex before sleeping so the reader thread can process Zellij's startup output
        std::thread::sleep(std::time::Duration::from_secs(1)); // wait until Zellij stops parsing startup ANSI codes from the terminal STDIN
    }
    pub fn run_zellij_action(&mut self, action_and_arguments: &str) {
        let mut channel = self.channel.lock().unwrap();
        channel
            .write_all(
                format!(
                    "{} action {}",
                    ZELLIJ_EXECUTABLE_LOCATION, action_and_arguments
                )
                .as_bytes(),
            )
            .unwrap();
        channel.flush().unwrap();
    }
    pub fn send_command_through_the_cli(&mut self, command: &str) {
        let mut channel = self.channel.lock().unwrap();
        channel
            .write_all(
                // note that this is run with the -s flag that suspends the command on startup
                format!("{} run -s -- \"{}\"", ZELLIJ_EXECUTABLE_LOCATION, command).as_bytes(),
            )
            .unwrap();
        channel.flush().unwrap();
    }
    pub fn send_blocking_command_through_the_cli(&mut self, command: &str) {
        let mut channel = self.channel.lock().unwrap();
        channel
            .write_all(
                format!(
                    "{} run --blocking --floating --close-on-exit -- {}",
                    ZELLIJ_EXECUTABLE_LOCATION, command
                )
                .as_bytes(),
            )
            .unwrap();
        channel.flush().unwrap();
    }
    pub fn path_to_fixture_folder(&self) -> String {
        ZELLIJ_FIXTURE_PATH.to_string()
    }
    pub fn load_fixture(&mut self, name: &str) {
        let mut channel = self.channel.lock().unwrap();
        channel
            .write_all(format!("cat {ZELLIJ_FIXTURE_PATH}/{name}\n").as_bytes())
            .unwrap();
        channel.flush().unwrap();
    }
}

#[derive(Clone)]
pub struct Step {
    pub instruction: fn(RemoteTerminal) -> bool,
    pub name: &'static str,
}

pub struct RemoteRunner {
    steps: Vec<Step>,
    current_step_index: usize,
    channel: Arc<Mutex<ssh2::Channel>>,
    currently_running_step: Option<String>,
    retries_left: usize,
    retry_pause_ms: usize,
    panic_on_no_retries_left: bool,
    last_snapshot: Arc<Mutex<String>>,
    cursor_coordinates: Arc<Mutex<(usize, usize)>>, // x, y
    reader_thread: (Arc<AtomicBool>, std::thread::JoinHandle<()>),
    pub test_timed_out: bool,
}

impl RemoteRunner {
    pub fn new(win_size: Size) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij(&mut channel);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_with_config_dir(win_size: Size, config_dir_name: &str) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_with_config_dir(&mut channel, config_dir_name);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_mirrored_session(win_size: Size) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_mirrored_session(&mut channel);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_mirrored_session_with_layout(win_size: Size, layout_file_name: &str) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_mirrored_session_with_layout(&mut channel, layout_file_name);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_mirrored_session_with_layout_and_viewport_serialization(
        win_size: Size,
        layout_file_name: &str,
    ) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_mirrored_session_with_layout_and_viewport_serialization(
            &mut channel,
            layout_file_name,
        );
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn kill_running_sessions(_win_size: Size) {
        let sess = ssh_connect();
        cleanup_remote_runtime(&sess)
            .unwrap_or_else(|error| panic!("remote E2E cleanup failed: {error}"));
    }
    pub fn new_with_session_name(win_size: Size, session_name: &str, mirrored: bool) -> Self {
        // notice that this method does not have a timeout, so use with caution!
        let sess = ssh_connect_without_timeout();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_in_session(&mut channel, session_name, mirrored);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_existing_session(win_size: Size, session_name: &str) -> Self {
        let sess = ssh_connect_without_timeout();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        attach_to_existing_session(&mut channel, session_name);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_watcher_session(win_size: Size, session_name: &str) -> Self {
        let sess = ssh_connect_without_timeout();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        watch_existing_session(&mut channel, session_name);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_without_frames(win_size: Size) -> Self {
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_without_frames(&mut channel);
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn new_with_config(win_size: Size, config_file_name: &'static str) -> Self {
        let remote_path = Path::new(ZELLIJ_CONFIG_PATH).join(config_file_name);
        let sess = ssh_connect();
        let mut channel = sess.channel_session().unwrap();
        let mut rows = Dimension::fixed(win_size.rows);
        let mut cols = Dimension::fixed(win_size.cols);
        rows.set_inner(win_size.rows);
        cols.set_inner(win_size.cols);
        let pane_geom = PaneGeom {
            x: 0,
            y: 0,
            rows,
            cols,
            stacked: None,
            is_pinned: false,
            logical_position: None,
        };
        setup_remote_environment(&mut channel, win_size);
        start_zellij_with_config(&mut channel, &remote_path.to_string_lossy());
        let channel = Arc::new(Mutex::new(channel));
        let last_snapshot = Arc::new(Mutex::new(String::new()));
        let cursor_coordinates = Arc::new(Mutex::new((0, 0)));
        sess.set_blocking(false);
        let reader_thread =
            read_from_channel(&channel, &last_snapshot, &cursor_coordinates, &pane_geom);
        wait_for_startup(&last_snapshot);
        RemoteRunner {
            steps: vec![],
            channel,
            currently_running_step: None,
            current_step_index: 0,
            retries_left: RETRIES,
            retry_pause_ms: 100,
            test_timed_out: false,
            panic_on_no_retries_left: true,
            last_snapshot,
            cursor_coordinates,
            reader_thread,
        }
    }
    pub fn dont_panic(mut self) -> Self {
        self.panic_on_no_retries_left = false;
        self
    }
    #[allow(unused)]
    pub fn retry_pause_ms(mut self, retry_pause_ms: usize) -> Self {
        self.retry_pause_ms = retry_pause_ms;
        self
    }
    pub fn add_step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }
    pub fn run_next_step(&mut self) {
        if let Some(next_step) = self.steps.get(self.current_step_index) {
            println!(
                "running step: {}, retries left: {}",
                next_step.name, self.retries_left
            );
            let (cursor_x, cursor_y) = *self.cursor_coordinates.lock().unwrap();
            let remote_terminal = RemoteTerminal {
                cursor_x,
                cursor_y,
                last_snapshot: self.last_snapshot.clone(),
                channel: self.channel.clone(),
            };
            let instruction = next_step.instruction;
            self.currently_running_step = Some(String::from(next_step.name));
            if instruction(remote_terminal) {
                self.retries_left = RETRIES;
                self.current_step_index += 1;
            } else {
                self.retries_left = self.retries_left.saturating_sub(1);
                std::thread::sleep(std::time::Duration::from_millis(self.retry_pause_ms as u64));
            }
        }
    }
    pub fn steps_left(&self) -> bool {
        self.steps.get(self.current_step_index).is_some()
    }
    pub fn take_snapshot_after(&mut self, step: Step) -> String {
        let mut retries_left = RETRIES;
        let instruction = step.instruction;
        loop {
            println!(
                "taking snapshot: {}, retries left: {}",
                step.name, retries_left
            );
            if retries_left == 0 {
                self.test_timed_out = true;
                return self.last_snapshot.lock().unwrap().clone();
            }
            let (cursor_x, cursor_y) = *self.cursor_coordinates.lock().unwrap();
            let remote_terminal = RemoteTerminal {
                cursor_x,
                cursor_y,
                last_snapshot: self.last_snapshot.clone(),
                channel: self.channel.clone(),
            };
            if instruction(remote_terminal) {
                return self.last_snapshot.lock().unwrap().clone();
            } else {
                retries_left -= 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
        }
    }
    pub fn run_all_steps(&mut self) {
        println!();
        loop {
            self.run_next_step();
            if !self.steps_left() {
                break;
            } else if self.retries_left == 0 {
                self.test_timed_out = true;
                break;
            }
        }
    }
}

impl Drop for RemoteRunner {
    fn drop(&mut self) {
        let reader_thread_running = &mut self.reader_thread.0;
        reader_thread_running.store(false, Ordering::SeqCst);
        let mut channel = match self.channel.lock() {
            Ok(channel) => channel,
            Err(poisoned) => poisoned.into_inner(),
        };
        let _ = channel.close();
    }
}

#[cfg(test)]
mod cleanup_contract_tests {
    use super::*;

    #[test]
    fn cleanup_is_scoped_graceful_and_waits_for_server_exit() {
        let command = cleanup_remote_runtime_command();

        assert!(command.contains("kill-all-sessions --yes"));
        assert!(command.contains("cleanup_attempt"));
        assert!(command.contains(E2E_SOCKET_DIR));
        assert!(command.contains(E2E_CACHE_DIR));
        assert!(command.contains("export VC_FRAME_SOCKET_DIR="));
        assert!(command.contains("export ZELLIJ_SOCKET_DIR=\"$VC_FRAME_SOCKET_DIR\""));
        assert!(!command.contains("killall"));
        assert!(!command.contains("-KILL"));
        assert!(!command.contains("rm -rf /tmp/*"));
    }

    #[test]
    fn runtime_root_is_an_explicit_non_global_tmp_subdirectory() {
        assert_eq!(E2E_RUNTIME_ROOT, "/tmp/vc-frame-e2e");
        assert!(E2E_SOCKET_DIR.starts_with(&format!("{E2E_RUNTIME_ROOT}/")));
        assert!(E2E_CACHE_DIR.starts_with(&format!("{E2E_RUNTIME_ROOT}/")));
    }

    #[test]
    fn default_runner_pins_the_legacy_e2e_layout() {
        let layout = include_str!("../fixtures/e2e-default.kdl");

        assert_eq!(
            E2E_DEFAULT_LAYOUT,
            "/usr/src/zellij/fixtures/e2e-default.kdl"
        );
        assert!(layout.contains("plugin location=\"tab-bar\""));
        assert!(layout.contains("plugin location=\"status-bar\""));
        assert!(!layout.contains("session-manager"));
    }

    #[test]
    fn insta_snapshots_follow_the_current_binary_crate_name() {
        let snapshot_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tests/e2e/snapshots");
        let expected_prefix = format!("{}__", env!("CARGO_PKG_NAME").replace('-', "_"));
        let snapshot_names: Vec<_> = std::fs::read_dir(snapshot_dir)
            .expect("E2E snapshot directory must exist")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".snap"))
            .collect();

        assert!(!snapshot_names.is_empty(), "E2E snapshots must be tracked");
        assert!(
            snapshot_names
                .iter()
                .all(|name| name.starts_with(&expected_prefix)),
            "E2E snapshots must start with {expected_prefix:?}; found {snapshot_names:?}"
        );
    }
}
