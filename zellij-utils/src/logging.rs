//! Zellij logging utility functions.

use std::{
    fmt, fs,
    io::{self, prelude::*},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use log::{LevelFilter, Record};

use log4rs::append::Append;
use log4rs::append::rolling_file::{
    RollingFileAppender,
    policy::compound::{
        CompoundPolicy, roll::fixed_window::FixedWindowRoller, trigger::size::SizeTrigger,
    },
};
use log4rs::config::{Appender, Config, Logger, Root};
use log4rs::encode::pattern::PatternEncoder;

use crate::consts::{ZELLIJ_TMP_DIR, ZELLIJ_TMP_LOG_DIR, ZELLIJ_TMP_LOG_FILE, ZELLIJ_TMP_LOG_ROOT};
use crate::shared::{ensure_private_dir, set_permissions};

const LOG_MAX_BYTES: u64 = 1024 * 1024 * 16; // 16 MiB per log
const PLUGIN_EVENT_DIAGNOSTICS_ENV: &str = "VC_FRAME_PLUGIN_EVENT_DIAGNOSTICS";
const PLUGIN_EVENT_DIAGNOSTICS_TARGET: &str = "vc_frame::plugin_event_rate";

/// Rolling file appender that creates its directory and opens the file on the
/// first record, not at process start.
///
/// Short-lived CLI clients (`--help`, `--version`, `list-sessions`, `action`)
/// otherwise leave an empty `client-<pid>/` directory in `/tmp` on every
/// invocation. Process-owned paths stay: servers and clients that actually log
/// still do not rotate one shared inode.
struct LazyRollingFileAppender {
    path: PathBuf,
    max_bytes: u64,
    inner: Mutex<Option<RollingFileAppender>>,
}

impl fmt::Debug for LazyRollingFileAppender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyRollingFileAppender")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl LazyRollingFileAppender {
    fn new(path: PathBuf) -> Self {
        Self::with_limit(path, LOG_MAX_BYTES)
    }

    fn with_limit(path: PathBuf, max_bytes: u64) -> Self {
        Self {
            path,
            max_bytes,
            inner: Mutex::new(None),
        }
    }

    fn materialize(&self) -> anyhow::Result<RollingFileAppender> {
        if let Some(dir) = self.path.parent() {
            ensure_private_dir(dir)?;
        }
        // Socket code may have created the uid tmp root with create_dir_all
        // (umask 0o755). Tighten it when this log lives under that root.
        if self.path.starts_with(ZELLIJ_TMP_DIR.as_path()) {
            ensure_private_dir(&ZELLIJ_TMP_DIR)?;
            ensure_private_dir(&ZELLIJ_TMP_LOG_ROOT)?;
        }
        let appender = build_rolling_file_appender_with_limit(&self.path, self.max_bytes)?;
        // RollingFileAppender::build opens the file with create(true) and the
        // process umask (typically 0644). Restore the owner-only contract the
        // previous atomic_create_file(0o600) enforced.
        set_permissions(&self.path, 0o600)?;
        Ok(appender)
    }

    fn tighten_process_log_modes(&self) -> anyhow::Result<()> {
        // log4rs post-process rotation renames the active file away and does
        // not recreate it until the next write, so ENOENT here is expected.
        tighten_mode_if_exists(&self.path)?;
        if let Some(dir) = self.path.parent() {
            tighten_mode_if_exists(&dir.join("vc-frame.log.old.0"))?;
        }
        Ok(())
    }
}

fn tighten_mode_if_exists(path: &Path) -> anyhow::Result<()> {
    match set_permissions(path, 0o600) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

impl Append for LazyRollingFileAppender {
    fn append(&self, record: &Record) -> anyhow::Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(self.materialize()?);
        }
        guard
            .as_ref()
            .expect("log file appender materialized")
            .append(record)?;
        // Fixed-window rollover creates the next active inode with umask 0644
        // (and may rename a just-created file to vc-frame.log.old.0). Re-apply
        // 0600 on whatever process-owned log files exist after the write.
        self.tighten_process_log_modes()
    }

    fn flush(&self) {
        if let Ok(guard) = self.inner.lock()
            && let Some(inner) = guard.as_ref()
        {
            inner.flush();
        }
    }
}

fn build_rolling_file_appender_with_limit(
    path: &Path,
    max_bytes: u64,
) -> io::Result<RollingFileAppender> {
    let trigger = SizeTrigger::new(max_bytes);
    let roll_pattern = path.parent().unwrap_or(path).join("vc-frame.log.old.{}");
    let roller = FixedWindowRoller::builder()
        .build(
            roll_pattern.to_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "log roll pattern is not valid UTF-8",
                )
            })?,
            1,
        )
        .map_err(io::Error::other)?;

    // {n} means platform dependent newline
    // module is padded to exactly 25 bytes and thread is padded to be between 10 and 15 bytes.
    let file_pattern = "{highlight({level:<6})} |{module:<25.25}| {date(%Y-%m-%d %H:%M:%S.%3f)} [{thread:<10.15}] {file}:{line}: {message} {n}";

    RollingFileAppender::builder()
        .encoder(Box::new(PatternEncoder::new(file_pattern)))
        .build(
            path,
            Box::new(CompoundPolicy::new(Box::new(trigger), Box::new(roller))),
        )
}

pub fn configure_logger() {
    // Directory and file creation is deferred until the first log record.
    // RollingFileAppender::build() otherwise create_dir_all + opens a 0-byte
    // file during `--help` / `list-sessions` / `action`.
    let log_file = LazyRollingFileAppender::new(ZELLIJ_TMP_LOG_FILE.clone());

    // Set the default logging level to "info" and log it to the process-owned
    // vc-frame.log file. One appender owns rotation; independent appenders and
    // server processes must never rename a shared inode underneath each other.
    // Decrease verbosity for `wasmtime_wasi` module because it has a lot of useless info logs
    // `zellij_server::logging_pipe` already formats plugin identity in its message.
    let mut config_builder = Config::builder()
        .appender(Appender::builder().build("logFile", Box::new(log_file)))
        // reduce the verbosity of isahc, otherwise it logs on every failed web request
        .logger(
            Logger::builder()
                .appender("logFile")
                .build("isahc", LevelFilter::Error),
        )
        .logger(
            Logger::builder()
                .appender("logFile")
                .build("wasmtime_wasi", LevelFilter::Warn),
        )
        .logger(
            Logger::builder()
                .appender("logFile")
                .additive(false)
                .build("zellij_server::logging_pipe", LevelFilter::Trace),
        );
    if std::env::var_os(PLUGIN_EVENT_DIAGNOSTICS_ENV).is_some() {
        config_builder = config_builder.logger(
            Logger::builder()
                .appender("logFile")
                .additive(false)
                .build(PLUGIN_EVENT_DIAGNOSTICS_TARGET, LevelFilter::Debug),
        );
    }
    let config = config_builder
        .build(Root::builder().appender("logFile").build(LevelFilter::Info))
        .unwrap();

    let _ = log4rs::init_config(config).unwrap();
}

static DEBUG_LOG_DIRS_READY: OnceLock<()> = OnceLock::new();

fn ensure_debug_log_dirs(process_dir: &Path) -> io::Result<()> {
    if DEBUG_LOG_DIRS_READY.get().is_some() {
        return Ok(());
    }
    // `--debug` pane capture can run before the first log record. Tighten the
    // uid tmp root and log root, not only the process leaf (create_dir_all
    // otherwise leaves ancestors at umask 0755).
    if process_dir.starts_with(ZELLIJ_TMP_DIR.as_path()) {
        ensure_private_dir(&ZELLIJ_TMP_DIR)?;
        ensure_private_dir(&ZELLIJ_TMP_LOG_ROOT)?;
    }
    ensure_private_dir(process_dir)?;
    let _ = DEBUG_LOG_DIRS_READY.set(());
    Ok(())
}

pub fn debug_to_file(message: &[u8], terminal_id: i32) -> io::Result<()> {
    let mut path = PathBuf::new();
    path.push(&*ZELLIJ_TMP_LOG_DIR);
    path.push(format!("pane-{}.log", terminal_id));
    if let Some(dir) = path.parent() {
        ensure_debug_log_dirs(dir)?;
    }

    let mut file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)?;
    set_permissions(&path, 0o600)?;
    file.write_all(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Level;

    macro_rules! info_record {
        ($msg:expr) => {
            Record::builder()
                .args(format_args!("{}", $msg))
                .level(Level::Info)
                .target("vc_frame::logging_test")
                .module_path(Some("vc_frame::logging_test"))
                .file(Some("logging.rs"))
                .line(Some(1))
                .build()
        };
    }

    #[cfg(unix)]
    fn dir_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn lazy_appender_does_not_create_dir_until_first_record() {
        let root = tempfile::tempdir().expect("tempdir");
        let log_file = root
            .path()
            .join("vc-frame-log")
            .join("client-1")
            .join("vc-frame.log");
        let client_dir = log_file.parent().expect("client dir");

        let appender = LazyRollingFileAppender::new(log_file.clone());
        assert!(
            !client_dir.exists(),
            "constructing the appender must not mkdir client-*"
        );
        assert!(
            !log_file.exists(),
            "constructing the appender must not open the log"
        );

        appender
            .append(&info_record!("first write"))
            .expect("first record materializes the log");

        assert!(client_dir.is_dir(), "first record creates the client dir");
        let bytes = fs::read(&log_file).expect("log file after first record");
        assert!(
            !bytes.is_empty(),
            "first record must write bytes, not a 0-length placeholder"
        );
        #[cfg(unix)]
        assert_eq!(dir_mode(client_dir), 0o700, "client log dir must be 0700");
        #[cfg(unix)]
        assert_eq!(
            dir_mode(&log_file),
            0o600,
            "active vc-frame.log must be 0600, not umask 0644"
        );
    }

    #[test]
    fn process_scoped_log_rolls_beside_the_active_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let client_dir = root.path().join("client-1");
        let log_file = client_dir.join("vc-frame.log");
        let appender = LazyRollingFileAppender::with_limit(log_file.clone(), 256);

        for i in 0..80 {
            appender
                .append(&info_record!(format!("rotation-payload-{i:04}")))
                .expect("write");
        }

        assert!(
            client_dir.join("vc-frame.log.old.0").is_file(),
            "fixed-window roller must archive into the process directory"
        );
        assert!(
            fs::metadata(&log_file).expect("active log").len() > 0,
            "active log continues after rotation"
        );
        #[cfg(unix)]
        {
            assert_eq!(
                dir_mode(&log_file),
                0o600,
                "active log must stay 0600 after rollover, not umask 0644"
            );
            assert_eq!(
                dir_mode(&client_dir.join("vc-frame.log.old.0")),
                0o600,
                "rolled archive must be 0600, not leftover umask 0644"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn socket_mkdir_sequence_tightens_uid_tmp_root_created_at_0755() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let uid_dir = root.path().join("vc-frame-503");
        let sock_dir = uid_dir.join("contract_version_2");
        fs::create_dir_all(&sock_dir).expect("simulate socket create_dir_all");
        let mut open = fs::metadata(&uid_dir).expect("uid meta").permissions();
        open.set_mode(0o755);
        fs::set_permissions(&uid_dir, open).expect("force 0755");
        assert_eq!(dir_mode(&uid_dir), 0o755);

        crate::shared::ensure_socket_runtime_dirs_in(&sock_dir, &uid_dir)
            .expect("socket mkdir sequence");
        assert_eq!(dir_mode(&sock_dir), 0o700);
        assert_eq!(
            dir_mode(&uid_dir),
            0o700,
            "uid tmp root must not stay at umask 0755"
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_mkdir_outside_tmp_root_does_not_create_the_tmp_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let uid_dir = root.path().join("vc-frame-503");
        let sock_dir = root.path().join("xdg-runtime").join("contract_version_2");

        crate::shared::ensure_socket_runtime_dirs_in(&sock_dir, &uid_dir).expect("xdg socket dir");
        assert!(sock_dir.is_dir());
        assert!(
            !uid_dir.exists(),
            "Linux XDG / VC_FRAME_SOCKET_DIR must not mkdir /tmp/vc-frame-<uid>"
        );
        assert_eq!(dir_mode(&sock_dir), 0o700);
    }
}
