//! Zellij logging utility functions.

use std::{
    fmt, fs,
    io::{self, prelude::*},
    path::{Path, PathBuf},
    sync::Mutex,
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
        Self {
            path,
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
        Ok(build_rolling_file_appender(&self.path)?)
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
            .append(record)
    }

    fn flush(&self) {
        if let Ok(guard) = self.inner.lock()
            && let Some(inner) = guard.as_ref()
        {
            inner.flush();
        }
    }
}

fn build_rolling_file_appender(path: &Path) -> io::Result<RollingFileAppender> {
    build_rolling_file_appender_with_limit(path, LOG_MAX_BYTES)
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

pub fn atomic_create_file(file_name: &Path) -> io::Result<()> {
    let _ = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(file_name)?;
    set_permissions(file_name, 0o600)
}

pub fn atomic_create_dir(dir_name: &Path) -> io::Result<()> {
    let result = if let Err(e) = fs::create_dir(dir_name) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Ok(())
        } else {
            Err(e)
        }
    } else {
        Ok(())
    };
    if result.is_ok() {
        set_permissions(dir_name, 0o700)?;
    }
    result
}

pub fn debug_to_file(message: &[u8], terminal_id: i32) -> io::Result<()> {
    let mut path = PathBuf::new();
    path.push(&*ZELLIJ_TMP_LOG_DIR);
    path.push(format!("pane-{}.log", terminal_id));
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
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
    }

    #[test]
    fn process_scoped_log_rolls_beside_the_active_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let client_dir = root.path().join("client-1");
        ensure_private_dir(&client_dir).expect("client dir");
        let log_file = client_dir.join("vc-frame.log");
        let appender = build_rolling_file_appender_with_limit(&log_file, 256).expect("appender");

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
    }

    #[cfg(unix)]
    #[test]
    fn ensure_private_dir_tightens_uid_tmp_root_created_at_0755() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("tempdir");
        let uid_dir = root.path().join("vc-frame-503");
        let sock_dir = uid_dir.join("contract_version_2");
        fs::create_dir_all(&sock_dir).expect("simulate socket create_dir_all");
        let mut open = fs::metadata(&uid_dir).expect("uid meta").permissions();
        open.set_mode(0o755);
        fs::set_permissions(&uid_dir, open).expect("force 0755");
        assert_eq!(dir_mode(&uid_dir), 0o755);

        ensure_private_dir(&sock_dir).expect("leaf");
        ensure_private_dir(&uid_dir).expect("tmp root");
        assert_eq!(dir_mode(&sock_dir), 0o700);
        assert_eq!(
            dir_mode(&uid_dir),
            0o700,
            "uid tmp root must not stay at umask 0755"
        );
    }
}
