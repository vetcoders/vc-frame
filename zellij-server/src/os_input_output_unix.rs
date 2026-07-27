use crate::os_input_output_api::{AsyncReader, command_exists};
use crate::panes::PaneId;

use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    pty::{OpenptyResult, Winsize, openpty},
    sys::{
        signal::{Signal, kill},
        termios,
    },
    unistd,
};
use tokio::io::unix::AsyncFd;

use libc::{self, TIOCSWINSZ, ioctl};
use signal_hook::consts::*;

use std::{
    collections::BTreeMap,
    fs::File,
    io,
    os::fd::FromRawFd,
    os::unix::{
        io::{AsRawFd, RawFd},
        process::CommandExt,
    },
    process::{Child, Command},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
    },
    thread,
    time::Duration,
};

use zellij_utils::{envs, errors::prelude::*, input::command::RunCommand};

pub use async_trait::async_trait;

const PROCESS_REAP_CONFIRMATION_ATTEMPTS: usize = 25;
const PROCESS_REAP_CONFIRMATION_INTERVAL: Duration = Duration::from_millis(10);

/// An `AsyncReader` that wraps a `RawFd` using epoll via `AsyncFd`.
///
/// Construction sets O_NONBLOCK but defers `AsyncFd` registration to the first
/// `read()` call, because `AsyncFd::new()` requires a live Tokio reactor and
/// `spawn_terminal` runs on the plain PTY thread (outside the runtime).
struct RawFdAsyncReader {
    /// Holds the file before reactor registration; `None` after promotion.
    pending: Option<File>,
    /// Populated on first `read()` inside the Tokio runtime.
    async_fd: Option<AsyncFd<File>>,
}

impl RawFdAsyncReader {
    fn new(file: File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        // Set O_NONBLOCK so AsyncFd can use epoll correctly
        let flags =
            fcntl(fd, FcntlArg::F_GETFL).map_err(|e| io::Error::from_raw_os_error(e as i32))?;
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(fd, FcntlArg::F_SETFL(oflags)).map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        Ok(Self {
            pending: Some(file),
            async_fd: None,
        })
    }

    /// Lazily register with the Tokio reactor on first use.
    fn get_async_fd(&mut self) -> io::Result<&mut AsyncFd<File>> {
        if self.async_fd.is_none() {
            let file = self
                .pending
                .take()
                .ok_or_else(|| io::Error::other("RawFdAsyncReader used after init"))?;
            self.async_fd = Some(AsyncFd::new(file)?);
        }
        self.async_fd
            .as_mut()
            .ok_or_else(|| io::Error::other("RawFdAsyncReader initialization lost its file"))
    }
}

#[async_trait]
impl AsyncReader for RawFdAsyncReader {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let async_fd = self.get_async_fd()?;
        loop {
            let mut guard = async_fd.readable().await?;
            match guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let ret =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if ret < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(ret as usize)
                }
            }) {
                Ok(result) => return result,
                Err(_would_block) => continue,
            }
        }
    }
}

fn set_terminal_size_using_fd(
    fd: RawFd,
    columns: u16,
    rows: u16,
    width_in_pixels: Option<u16>,
    height_in_pixels: Option<u16>,
) {
    // TODO: do this with the nix ioctl
    let ws_xpixel = width_in_pixels.unwrap_or(0);
    let ws_ypixel = height_in_pixels.unwrap_or(0);
    let winsize = Winsize {
        ws_col: columns,
        ws_row: rows,
        ws_xpixel,
        ws_ypixel,
    };
    // TIOCGWINSZ is an u32, but the second argument to ioctl is u64 on
    // some platforms. When checked on Linux, clippy will complain about
    // useless conversion.
    #[allow(clippy::useless_conversion)]
    unsafe {
        ioctl(fd, TIOCSWINSZ.into(), &winsize)
    };
}

/// Handle some signals for the child process. This will loop until the child
/// process exits.
fn handle_command_exit(child: &mut Child) -> Result<Option<i32>> {
    let id = child.id();
    let err_context = || {
        format!(
            "failed to handle signals and command exit for child process pid {}",
            id
        )
    };

    // returns the exit status, if any
    let mut should_exit = false;
    let mut attempts = 3;
    let mut signals =
        signal_hook::iterator::Signals::new([SIGINT, SIGTERM]).with_context(err_context)?;
    'handle_exit: loop {
        // test whether the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                // if the child process has exited, break outside of the loop
                // and exit this function
                // TODO: handle errors?
                break 'handle_exit Ok(status.code());
            },
            Ok(None) => {
                thread::sleep(Duration::from_millis(10));
            },
            Err(e) => return Err(e).with_context(err_context),
        }

        if !should_exit {
            for signal in signals.pending() {
                if signal == SIGINT || signal == SIGTERM {
                    should_exit = true;
                }
            }
        } else if attempts > 0 {
            // let's try nicely first...
            attempts -= 1;
            kill(
                unistd::Pid::from_raw(child.id() as i32),
                Some(Signal::SIGTERM),
            )
            .with_context(err_context)?;
            continue;
        } else {
            // when I say whoa, I mean WHOA!
            let _ = child.kill();
            child.wait().with_context(err_context)?;
            break 'handle_exit Ok(None);
        }
    }
}

#[cfg(test)]
static FAIL_MONITOR_SPAWN_FOR_TERMINAL: AtomicU32 = AtomicU32::new(u32::MAX);
#[cfg(test)]
static UNIX_CHILD_GUARD_CLEANUPS: AtomicU32 = AtomicU32::new(0);
#[cfg(test)]
static LAST_REAPED_UNIX_CHILD: AtomicU32 = AtomicU32::new(0);

struct UnixChildMonitor {
    child: Option<Child>,
    secondary_fd: Option<RawFd>,
    cmd: Option<RunCommand>,
    quit_cb: Option<Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>>,
    terminal_id: u32,
}

impl std::fmt::Debug for UnixChildMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixChildMonitor")
            .field("child_id", &self.child.as_ref().map(Child::id))
            .field("secondary_fd", &self.secondary_fd)
            .field("terminal_id", &self.terminal_id)
            .finish_non_exhaustive()
    }
}

impl UnixChildMonitor {
    fn new(
        child: Child,
        secondary_fd: RawFd,
        cmd: RunCommand,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
    ) -> Self {
        Self {
            child: Some(child),
            secondary_fd: Some(secondary_fd),
            cmd: Some(cmd),
            quit_cb: Some(quit_cb),
            terminal_id,
        }
    }

    fn child_id(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    fn kill_and_reap(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        #[cfg(test)]
        let child_id = child.id();
        #[cfg(test)]
        UNIX_CHILD_GUARD_CLEANUPS.fetch_add(1, Ordering::SeqCst);

        match child.try_wait() {
            Ok(Some(_)) => {},
            Ok(None) | Err(_) => {
                if let Err(error) = child.kill() {
                    log::debug!(
                        "failed to kill child process {} during spawn cleanup: {}",
                        child.id(),
                        error
                    );
                }
                if let Err(error) = child.wait() {
                    log::error!(
                        "failed to reap child process {} during spawn cleanup: {}",
                        child.id(),
                        error
                    );
                }
            },
        }
        #[cfg(test)]
        LAST_REAPED_UNIX_CHILD.store(child_id, Ordering::SeqCst);
    }

    fn close_secondary_fd(&mut self) {
        if let Some(secondary_fd) = self.secondary_fd.take() {
            let _ = unistd::close(secondary_fd);
        }
    }

    fn run(mut self) {
        let command_name = self
            .cmd
            .as_ref()
            .map(|cmd| cmd.command.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<unknown>".to_owned());
        let exit_status = match self.child.as_mut() {
            Some(child) => match handle_command_exit(child) {
                Ok(exit_status) => {
                    self.child.take();
                    exit_status
                },
                Err(error) => {
                    log::error!(
                        "failed to monitor child process for '{}': {:#}",
                        command_name,
                        error
                    );
                    self.kill_and_reap();
                    None
                },
            },
            None => {
                log::error!("child process ownership vanished before monitor start");
                None
            },
        };
        self.close_secondary_fd();

        match (self.quit_cb.take(), self.cmd.take()) {
            (Some(quit_cb), Some(cmd)) => {
                quit_cb(PaneId::Terminal(self.terminal_id), exit_status, cmd);
            },
            _ => {
                log::error!(
                    "child monitor for terminal {} lost its completion callback",
                    self.terminal_id
                );
            },
        }
    }
}

impl Drop for UnixChildMonitor {
    fn drop(&mut self) {
        self.kill_and_reap();
        self.close_secondary_fd();
    }
}

#[derive(Debug)]
struct SpawnedUnixTerminal {
    primary: File,
    monitor: UnixChildMonitor,
}

fn spawn_child_monitor<F>(terminal_id: u32, monitor: F) -> io::Result<thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_MONITOR_SPAWN_FOR_TERMINAL
        .compare_exchange(terminal_id, u32::MAX, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Err(io::Error::other(
            "injected Unix child-monitor thread spawn failure",
        ));
    }

    thread::Builder::new()
        .name(format!("pty-child-monitor-{terminal_id}"))
        .spawn(monitor)
}

unsafe fn spawn_command_in_pty<F>(
    cmd: &RunCommand,
    terminal_id: u32,
    pre_exec: F,
) -> io::Result<Child>
where
    F: FnMut() -> io::Result<()> + Send + Sync + 'static,
{
    let mut command = Command::new(&cmd.command);
    if let Some(current_dir) = &cmd.cwd {
        if current_dir.exists() && current_dir.is_dir() {
            command.current_dir(current_dir);
        } else {
            log::error!(
                "Failed to set CWD for new pane. '{}' does not exist or is not a folder",
                current_dir.display()
            );
        }
    }
    // SAFETY: `pre_exec` runs in the forked child before exec; the caller upholds the
    // contract of `spawn_command_in_pty` (an `unsafe fn`) that the closure is async-signal-safe.
    unsafe {
        command
            .args(&cmd.args)
            .env(envs::VC_FRAME_PANE_ID_ENV_KEY, format!("{}", terminal_id))
            .env(envs::PANE_ID_ENV_KEY, format!("{}", terminal_id))
            .pre_exec(pre_exec)
            .spawn()
    }
}

fn handle_openpty(
    open_pty_res: OpenptyResult,
    cmd: RunCommand,
    quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    terminal_id: u32,
) -> Result<SpawnedUnixTerminal> {
    let err_context = |cmd: &RunCommand| {
        format!(
            "failed to open PTY for command '{}'",
            cmd.command.to_string_lossy()
        )
    };

    // primary side of pty and child fd
    let pid_primary = open_pty_res.master;
    let pid_secondary = open_pty_res.slave;

    let child = match unsafe {
        spawn_command_in_pty(&cmd, terminal_id, move || -> io::Result<()> {
            if libc::login_tty(pid_secondary) != 0 {
                return Err(io::Error::last_os_error());
            }
            close_fds::close_open_fds(3, &[]);
            Ok(())
        })
    } {
        Ok(child) => child,
        Err(e) => {
            let _ = unistd::close(pid_primary);
            let _ = unistd::close(pid_secondary);
            return Err(e).with_context(|| err_context(&cmd));
        },
    };

    // SAFETY: ownership of the successfully opened primary descriptor is
    // transferred exactly once to `File`; every return/unwind now closes it.
    let primary = unsafe { File::from_raw_fd(pid_primary) };
    let monitor = UnixChildMonitor::new(child, pid_secondary, cmd, quit_cb, terminal_id);
    Ok(SpawnedUnixTerminal { primary, monitor })
}

/// Spawns a new terminal from the parent terminal with [`termios`](termios::Termios)
/// `orig_termios`.
fn handle_terminal(
    cmd: RunCommand,
    failover_cmd: Option<RunCommand>,
    orig_termios: Option<termios::Termios>,
    quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
    terminal_id: u32,
) -> Result<SpawnedUnixTerminal> {
    let err_context = || "failed to spawn child terminal".to_string();
    if !command_exists(&cmd) {
        return Err(ZellijError::CommandNotFound {
            terminal_id,
            command: cmd.command.to_string_lossy().to_string(),
        })
        .with_context(|| {
            format!(
                "failed to open PTY for command '{}'",
                cmd.command.to_string_lossy()
            )
        });
    }

    // Create a pipe to allow the child the communicate the shell's pid to its
    // parent.
    match openpty(None, &orig_termios) {
        Ok(open_pty_res) => handle_openpty(open_pty_res, cmd, quit_cb, terminal_id),
        Err(e) => match failover_cmd {
            Some(failover_cmd) => {
                handle_terminal(failover_cmd, None, orig_termios, quit_cb, terminal_id)
                    .with_context(err_context)
            },
            None => Err::<SpawnedUnixTerminal, _>(e)
                .context("failed to start pty")
                .with_context(err_context)
                .to_log(),
        },
    }
}

/// The Unix PTY backend. Manages native PTY file descriptors and signals.
#[derive(Clone)]
pub(crate) struct UnixPtyBackend {
    orig_termios: Arc<Mutex<Option<termios::Termios>>>,
    terminal_id_to_raw_fd: Arc<Mutex<BTreeMap<u32, Option<RawFd>>>>,
    next_terminal_id_counter: Arc<AtomicU32>,
}

/// Try to write as many bytes from `buf` as possible to `fd` without blocking.
///
/// Loops on successful short writes and EINTR to drain as much as the kernel
/// will accept. On EAGAIN (fd buffer full), stops and returns how many bytes
/// were written so far (which may be 0). The caller is expected to re-queue
/// any unwritten remainder.
fn try_write_to_fd(fd: RawFd, buf: &[u8]) -> Result<usize> {
    let mut written = 0;
    while written < buf.len() {
        match unistd::write(fd, &buf[written..]) {
            Ok(0) => break, // fd returned 0 on non-empty buf; treat like EAGAIN
            Ok(n) => written += n,
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::EAGAIN) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(written)
}

impl UnixPtyBackend {
    pub fn new() -> Result<Self, io::Error> {
        let current_termios = termios::tcgetattr(0).ok();
        if current_termios.is_none() {
            log::warn!(
                "Starting a server without a controlling terminal, using the default termios configuration."
            );
        }
        Ok(Self {
            orig_termios: Arc::new(Mutex::new(current_termios)),
            terminal_id_to_raw_fd: Arc::new(Mutex::new(BTreeMap::new())),
            next_terminal_id_counter: Arc::new(AtomicU32::new(0)),
        })
    }

    pub fn spawn_terminal(
        &self,
        cmd: RunCommand,
        failover_cmd: Option<RunCommand>,
        quit_cb: Box<dyn Fn(PaneId, Option<i32>, RunCommand) + Send>,
        terminal_id: u32,
    ) -> Result<(Box<dyn AsyncReader>, RawFd)> {
        {
            let terminal_registry = self
                .terminal_id_to_raw_fd
                .lock()
                .to_anyhow()
                .context("failed to lock terminal registry before spawn")?;
            match terminal_registry.get(&terminal_id) {
                Some(None) => {},
                Some(Some(_)) => {
                    return Err(anyhow!(
                        "terminal {terminal_id} is already active and cannot be spawned again"
                    ));
                },
                None => {
                    return Err(anyhow!(
                        "terminal {terminal_id} was not reserved before spawn"
                    ));
                },
            }
        }
        let orig_termios = self
            .orig_termios
            .lock()
            .to_anyhow()
            .context("failed to lock orig_termios")?
            .clone();
        let spawned = handle_terminal(cmd, failover_cmd, orig_termios, quit_cb, terminal_id)?;
        let child_fd = spawned
            .monitor
            .child_id()
            .ok_or_else(|| anyhow!("child ownership vanished immediately after spawn"))?
            as RawFd;
        let SpawnedUnixTerminal { primary, monitor } = spawned;
        let pid_primary = primary.as_raw_fd();
        let async_reader =
            Box::new(RawFdAsyncReader::new(primary).context("failed to create async reader")?)
                as Box<dyn AsyncReader>;

        let mut terminal_registry = self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .context("failed to lock terminal registry after child spawn")?;
        match terminal_registry.get(&terminal_id) {
            Some(None) => {},
            Some(Some(_)) => {
                return Err(anyhow!(
                    "terminal {terminal_id} became active while its child was spawning"
                ));
            },
            None => {
                return Err(anyhow!(
                    "terminal {terminal_id} reservation vanished while its child was spawning"
                ));
            },
        }
        terminal_registry.insert(terminal_id, Some(pid_primary));

        if let Err(error) = spawn_child_monitor(terminal_id, move || monitor.run()) {
            terminal_registry.insert(terminal_id, None);
            return Err(error).context("failed to spawn Unix child-monitor thread");
        }
        Ok((async_reader, child_fd))
    }

    pub fn set_terminal_size(
        &self,
        terminal_id: u32,
        cols: u16,
        rows: u16,
        width_in_pixels: Option<u16>,
        height_in_pixels: Option<u16>,
    ) -> Result<()> {
        let err_context = || {
            format!(
                "failed to set terminal id {} to size ({}, {})",
                terminal_id, rows, cols
            )
        };
        match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => {
                if cols > 0 && rows > 0 {
                    set_terminal_size_using_fd(*fd, cols, rows, width_in_pixels, height_in_pixels);
                }
            },
            _ => {
                Err::<(), _>(anyhow!("failed to find terminal fd for id {terminal_id}"))
                    .with_context(err_context)
                    .non_fatal();
            },
        }
        Ok(())
    }

    pub fn write_to_tty_stdin(&self, terminal_id: u32, buf: &[u8]) -> Result<usize> {
        let err_context = || format!("failed to write to stdin of TTY ID {}", terminal_id);

        let fd = match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => *fd,
            _ => {
                return Err(anyhow!("could not find raw file descriptor"))
                    .with_context(err_context);
            },
        };

        try_write_to_fd(fd, buf).with_context(err_context)
    }

    pub fn tcdrain(&self, terminal_id: u32) -> Result<()> {
        let err_context = || format!("failed to tcdrain to TTY ID {}", terminal_id);

        match self
            .terminal_id_to_raw_fd
            .lock()
            .to_anyhow()
            .with_context(err_context)?
            .get(&terminal_id)
        {
            Some(Some(fd)) => termios::tcdrain(*fd).with_context(err_context),
            _ => Err(anyhow!("could not find raw file descriptor")).with_context(err_context),
        }
    }

    fn wait_for_process_exit(pid: unistd::Pid) -> Result<bool> {
        for attempt in 0..PROCESS_REAP_CONFIRMATION_ATTEMPTS {
            match kill(pid, None) {
                Err(nix::errno::Errno::ESRCH) => return Ok(true),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to confirm child process {pid} termination")
                    });
                },
                Ok(()) if attempt + 1 < PROCESS_REAP_CONFIRMATION_ATTEMPTS => {
                    thread::sleep(PROCESS_REAP_CONFIRMATION_INTERVAL);
                },
                Ok(()) => {},
            }
        }
        Ok(false)
    }

    pub fn kill(&self, pid: u32) -> Result<()> {
        let child_pid = unistd::Pid::from_raw(pid as i32);
        match kill(child_pid, Some(Signal::SIGHUP)) {
            Ok(()) => {},
            Err(nix::errno::Errno::ESRCH) => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to send SIGHUP to child process {child_pid}")
                });
            },
        }

        if Self::wait_for_process_exit(child_pid)? {
            return Ok(());
        }

        self.force_kill(pid)?;
        if Self::wait_for_process_exit(child_pid)? {
            return Ok(());
        }

        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "child process {child_pid} still exists after SIGKILL; exit/reap remains unconfirmed"
            ),
        )
        .into())
    }

    pub fn force_kill(&self, pid: u32) -> Result<()> {
        match kill(unistd::Pid::from_raw(pid as i32), Some(Signal::SIGKILL)) {
            Ok(()) | Err(nix::errno::Errno::ESRCH) => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("failed to send SIGKILL to child process {pid}"))
            },
        }
    }

    pub fn send_sigint(&self, pid: u32) -> Result<()> {
        let _ = kill(unistd::Pid::from_raw(pid as i32), Some(Signal::SIGINT));
        Ok(())
    }

    pub fn reserve_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_raw_fd
            .lock()
            .unwrap_or_else(|poisoned| {
                log::error!("PTY terminal registry was poisoned while reserving; recovering");
                poisoned.into_inner()
            })
            .insert(terminal_id, None);
    }

    pub fn clear_terminal_id(&self, terminal_id: u32) {
        self.terminal_id_to_raw_fd
            .lock()
            .unwrap_or_else(|poisoned| {
                log::error!("PTY terminal registry was poisoned while clearing; recovering");
                poisoned.into_inner()
            })
            .remove(&terminal_id);
    }

    pub fn next_terminal_id(&self) -> Option<u32> {
        Some(
            self.next_terminal_id_counter
                .fetch_add(1, Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::sys::termios;
    use std::io::Read;

    fn open_file_descriptor_count() -> usize {
        let fd_directory = if std::path::Path::new("/proc/self/fd").is_dir() {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        std::fs::read_dir(fd_directory)
            .expect("open file descriptor directory")
            .count()
    }

    #[test]
    fn repeated_missing_commands_do_not_leak_pty_file_descriptors() {
        // The Rust test harness runs this alongside tests which legitimately
        // open process-wide descriptors. Keep a bounded allowance for that
        // noise while still catching the original per-attempt leak: leaking
        // one descriptor for each of the 128 rejected commands exceeds this
        // ceiling by a wide margin.
        const PARALLEL_TEST_FD_ALLOWANCE: usize = 32;
        let before = open_file_descriptor_count();
        for terminal_id in 0..128 {
            let command = RunCommand {
                command: format!("/definitely/not/a/real/vc-frame-command-{terminal_id}").into(),
                ..Default::default()
            };
            let error = handle_terminal(command, None, None, Box::new(|_, _, _| {}), terminal_id)
                .expect_err("a missing executable must be rejected");
            assert!(
                error.downcast_ref::<ZellijError>().is_some(),
                "the missing-command source must be preserved: {error:#}"
            );
        }
        let after = open_file_descriptor_count();
        assert!(
            after <= before + PARALLEL_TEST_FD_ALLOWANCE,
            "128 rejected commands leaked file descriptors: before={before}, after={after}"
        );
    }

    #[test]
    fn exact_spawn_requires_and_preserves_its_reservation_on_command_not_found() {
        let backend = UnixPtyBackend::new().expect("backend");
        let terminal_id = 77;
        let missing_command = RunCommand {
            command: "/definitely/not/a/real/vc-frame-command".into(),
            ..Default::default()
        };

        let unreserved_error = match backend.spawn_terminal(
            missing_command.clone(),
            None,
            Box::new(|_, _, _| {}),
            terminal_id,
        ) {
            Ok(_) => panic!("an exact spawn without a reservation is a protocol error"),
            Err(error) => error,
        };
        assert!(format!("{unreserved_error:#}").contains("was not reserved before spawn"));

        backend.reserve_terminal_id(terminal_id);
        let command_error = match backend.spawn_terminal(
            missing_command,
            None,
            Box::new(|_, _, _| {}),
            terminal_id,
        ) {
            Ok(_) => panic!("the reserved missing command must remain CommandNotFound"),
            Err(error) => error,
        };
        assert!(matches!(
            command_error.downcast_ref::<ZellijError>(),
            Some(ZellijError::CommandNotFound {
                terminal_id: 77,
                ..
            })
        ));
        assert!(matches!(
            backend
                .terminal_id_to_raw_fd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&terminal_id),
            Some(None)
        ));
        backend.clear_terminal_id(terminal_id);
    }

    #[test]
    fn monitor_thread_spawn_failure_reaps_child_and_rolls_back_reservation() {
        let backend = UnixPtyBackend::new().expect("backend");
        let terminal_id = 0xffff_ff00;
        backend.reserve_terminal_id(terminal_id);

        let cleanups_before = UNIX_CHILD_GUARD_CLEANUPS.load(Ordering::SeqCst);
        FAIL_MONITOR_SPAWN_FOR_TERMINAL.store(terminal_id, Ordering::SeqCst);
        let command = RunCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            ..Default::default()
        };

        let error = match backend.spawn_terminal(command, None, Box::new(|_, _, _| {}), terminal_id)
        {
            Ok(_) => panic!("the injected monitor-thread failure must reject the spawn"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("injected Unix child-monitor thread spawn failure"),
            "the injected thread failure must remain visible: {error:#}"
        );
        assert!(
            UNIX_CHILD_GUARD_CLEANUPS.load(Ordering::SeqCst) > cleanups_before,
            "the exact spawned child must pass through the kill-and-reap guard"
        );
        let reaped_child = LAST_REAPED_UNIX_CHILD.load(Ordering::SeqCst);
        assert_ne!(reaped_child, 0, "the guard must record the exact child pid");
        let mut wait_status = 0;
        let wait_result =
            unsafe { libc::waitpid(reaped_child as libc::pid_t, &mut wait_status, libc::WNOHANG) };
        assert_eq!(
            wait_result, -1,
            "the child must already be reaped before spawn_terminal returns"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "waitpid must report that no unreaped child remains"
        );
        assert!(matches!(
            backend
                .terminal_id_to_raw_fd
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(&terminal_id),
            Some(None)
        ));
        backend.clear_terminal_id(terminal_id);
    }

    #[test]
    fn windows_spawn_source_keeps_child_guarded_until_fallible_monitor_handoff() {
        let source = include_str!("os_input_output_windows.rs");
        let do_spawn = source
            .split("    fn do_spawn(")
            .nth(1)
            .and_then(|source| source.split("    pub fn spawn_terminal(").next())
            .expect("Windows do_spawn source");

        assert!(do_spawn.contains("WindowsSpawnGuard::new("));
        assert!(do_spawn.contains("spawn_child_monitor("));
        assert!(!do_spawn.contains("std::thread::spawn("));
        assert!(!do_spawn.contains(".lock().unwrap()"));
        assert!(!do_spawn.contains("CloseHandle(process_handle)"));
        assert!(source.contains("impl Drop for WindowsProcessGuard"));
        assert!(source.contains("thread::Builder::new()"));
        assert!(source.contains("TerminateProcess(handle, 1)"));
        assert!(source.contains("WaitForSingleObject(handle, INFINITE)"));
    }

    #[test]
    fn reservation_cleanup_recovers_a_poisoned_terminal_registry() {
        let backend = UnixPtyBackend::new().expect("backend");
        let terminal_registry = backend.terminal_id_to_raw_fd.clone();
        let registry_to_poison = terminal_registry.clone();

        let poison_result = std::panic::catch_unwind(move || {
            let _guard = registry_to_poison.lock().expect("initial lock");
            panic!("inject terminal registry poison");
        });
        assert!(
            poison_result.is_err(),
            "the registry must actually be poisoned"
        );

        backend.reserve_terminal_id(77);
        assert!(
            terminal_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&77)
        );
        backend.clear_terminal_id(77);
        assert!(
            !terminal_registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(&77),
            "cleanup must recover the poisoned guard instead of panicking again"
        );
    }

    /// Verify that `try_write_to_fd` writes as many bytes as the kernel will
    /// accept in one pass and returns a partial count (not an error) when the
    /// PTY buffer fills up.
    ///
    /// A concurrent reader drains the slave side so some bytes are accepted.
    /// The key assertion: the function returns Ok(n) where n <= buf.len(),
    /// and the caller (PtyWriter) is responsible for re-queuing the rest.
    #[test]
    fn try_write_to_fd_returns_partial_on_full_buffer() {
        let pty = openpty(None, &None).expect("openpty failed");

        let mut attrs = termios::tcgetattr(pty.slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut attrs);
        termios::tcsetattr(pty.slave, termios::SetArg::TCSANOW, &attrs).expect("tcsetattr failed");

        // O_NONBLOCK so write() returns EAGAIN instead of blocking
        let flags = fcntl(pty.master, FcntlArg::F_GETFL).expect("F_GETFL");
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(pty.master, FcntlArg::F_SETFL(oflags)).expect("F_SETFL");

        // Fill most of the buffer, leaving some space
        let chunk = vec![0x42u8; 1024];
        let mut total_filled = 0;
        loop {
            match super::try_write_to_fd(pty.master, &chunk) {
                Ok(0) => break,
                Ok(n) => total_filled += n,
                Err(e) => panic!("unexpected error filling buffer: {e}"),
            }
        }
        assert!(
            total_filled > 0,
            "should have written some bytes to fill buffer"
        );

        // Read a small amount from the slave to free partial space
        let mut drain = vec![0u8; 512];
        let slave_file = unsafe { std::fs::File::from_raw_fd(pty.slave) };
        let mut slave_reader = std::io::BufReader::new(&slave_file);
        let drained = slave_reader.read(&mut drain).expect("slave read failed");
        assert!(drained > 0, "should have drained some bytes");
        // Prevent File from closing the slave fd — we close it manually below
        std::mem::forget(slave_file);

        // Now write more than the freed space — should get a partial write
        let size = 128 * 1024;
        let data: Vec<u8> = (0..size).map(|i| (i % 256) as u8).collect();
        let written = super::try_write_to_fd(pty.master, &data)
            .expect("try_write_to_fd should not error on EAGAIN");

        assert!(
            written > 0 && written < size,
            "expected partial write, got {written}/{size}",
        );

        unsafe {
            libc::close(pty.master);
            libc::close(pty.slave);
        }
    }

    /// Verify that `try_write_to_fd` returns Ok(0) — not an error — when the
    /// fd is completely full and cannot accept any bytes at all.
    #[test]
    fn try_write_to_fd_returns_zero_on_stuck_pty() {
        let pty = openpty(None, &None).expect("openpty failed");

        let mut attrs = termios::tcgetattr(pty.slave).expect("tcgetattr failed");
        termios::cfmakeraw(&mut attrs);
        termios::tcsetattr(pty.slave, termios::SetArg::TCSANOW, &attrs).expect("tcsetattr failed");

        let flags = fcntl(pty.master, FcntlArg::F_GETFL).expect("F_GETFL");
        let mut oflags = OFlag::from_bits_truncate(flags);
        oflags.insert(OFlag::O_NONBLOCK);
        fcntl(pty.master, FcntlArg::F_SETFL(oflags)).expect("F_SETFL");

        // Fill the buffer completely — keep writing until we get Ok(0)
        let fill = vec![0x42u8; 1024];
        loop {
            match super::try_write_to_fd(pty.master, &fill) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) => panic!("unexpected error filling buffer: {e}"),
            }
        }

        // Now the buffer is full — next write should return Ok(0)
        let written = super::try_write_to_fd(pty.master, &[0x01, 0x02, 0x03])
            .expect("try_write_to_fd should not error on EAGAIN");

        assert_eq!(written, 0, "expected zero bytes written on full buffer");

        unsafe {
            libc::close(pty.master);
            libc::close(pty.slave);
        }
    }

    #[test]
    fn spawn_command_in_pty_returns_spawn_errors() {
        let cmd = RunCommand {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 0".into()],
            ..Default::default()
        };

        let err = unsafe {
            spawn_command_in_pty(&cmd, 0, || Err(io::Error::from_raw_os_error(libc::EMFILE)))
        }
        .expect_err("spawn errors should be returned, not panic");

        assert_eq!(err.raw_os_error(), Some(libc::EMFILE));
    }
}
