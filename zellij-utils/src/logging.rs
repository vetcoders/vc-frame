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

use crate::consts::{ZELLIJ_TMP_LOG_DIR, ZELLIJ_TMP_LOG_FILE};
use crate::shared::set_permissions;

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
        ensure_missing_parents(&self.path)?;
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
    let trigger = SizeTrigger::new(LOG_MAX_BYTES);
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

/// Create only the ancestors of `path` that do not exist yet, chmod 0o700 each.
fn ensure_missing_parents(path: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.as_os_str().is_empty() || dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        current = dir.parent();
    }
    for dir in missing.into_iter().rev() {
        atomic_create_dir(&dir)?;
    }
    Ok(())
}

pub fn debug_to_file(message: &[u8], terminal_id: i32) -> io::Result<()> {
    let mut path = PathBuf::new();
    path.push(&*ZELLIJ_TMP_LOG_DIR);
    path.push(format!("pane-{}.log", terminal_id));
    ensure_missing_parents(&path)?;

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
            .append(
                &Record::builder()
                    .args(format_args!("first write"))
                    .level(Level::Info)
                    .target("vc_frame::logging_test")
                    .module_path(Some("vc_frame::logging_test"))
                    .file(Some("logging.rs"))
                    .line(Some(1))
                    .build(),
            )
            .expect("first record materializes the log");

        assert!(client_dir.is_dir(), "first record creates the client dir");
        let bytes = fs::read(&log_file).expect("log file after first record");
        assert!(
            !bytes.is_empty(),
            "first record must write bytes, not a 0-length placeholder"
        );
    }

    #[test]
    fn ensure_missing_parents_is_a_no_op_when_ancestors_exist() {
        let root = tempfile::tempdir().expect("tempdir");
        let log_file = root.path().join("vc-frame.log");
        ensure_missing_parents(&log_file).expect("existing parent");
        assert!(
            root.path()
                .read_dir()
                .expect("read tempdir")
                .next()
                .is_none(),
            "must not create the log file itself"
        );
    }
}
