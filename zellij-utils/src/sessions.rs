use crate::{
    consts::{
        ZELLIJ_SESSION_INFO_CACHE_DIR, ZELLIJ_SOCK_DIR, is_ipc_socket,
        session_info_folder_for_session, session_layout_cache_file_name,
    },
    envs,
    input::layout::Layout,
    ipc::{ClientToServerMsg, IpcReceiverWithContext, IpcSenderWithContext, ServerToClientMsg},
};
use anyhow;
use humantime::format_duration;
use lev_distance::find_best_match_for_name;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};
use std::{fs, io, process};

#[cfg(unix)]
use std::{
    ffi::{CString, OsString},
    fs::{File, OpenOptions},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
};

pub fn get_sessions() -> Result<Vec<(String, Duration)>, io::ErrorKind> {
    match fs::read_dir(&*ZELLIJ_SOCK_DIR) {
        Ok(files) => {
            let mut sessions = Vec::new();
            files.for_each(|file| {
                if let Ok(file) = file {
                    let file_name = file.file_name().into_string().unwrap();
                    // try to get creation time, fall back to modification time on platforms where it's not supported (e.g., musl)
                    // for session creation time these are almost always identical (notable
                    // exceptions are session name changes)
                    let ctime = std::fs::metadata(file.path())
                        .ok()
                        .and_then(|f| f.created().ok().or_else(|| f.modified().ok()))
                        .and_then(|d| d.elapsed().ok())
                        .unwrap_or_default();
                    let duration = Duration::from_secs(ctime.as_secs());
                    if is_ipc_socket(&file.file_type().unwrap()) && assert_socket(&file_name) {
                        sessions.push((file_name, duration));
                    }
                }
            });
            Ok(sessions)
        },
        Err(err) if io::ErrorKind::NotFound != err.kind() => Err(err.kind()),
        Err(_) => Ok(Vec::with_capacity(0)),
    }
}

pub fn get_resurrectable_sessions() -> Vec<(String, Duration)> {
    match fs::read_dir(&*ZELLIJ_SESSION_INFO_CACHE_DIR) {
        Ok(files_in_session_info_folder) => {
            let files_that_are_folders = files_in_session_info_folder
                .filter_map(|f| f.ok().map(|f| f.path()))
                .filter(|f| f.is_dir());
            files_that_are_folders
                .filter_map(|folder_name| {
                    let layout_file_name =
                        session_layout_cache_file_name(&folder_name.display().to_string());
                    // Try to get creation time, fall back to modification time on platforms where it's not supported (e.g., musl)
                    let ctime = std::fs::metadata(&layout_file_name)
                        .ok()
                        .and_then(|metadata| {
                            metadata.created().ok().or_else(|| metadata.modified().ok())
                        });
                    let elapsed_duration = ctime
                        .map(|ctime| {
                            Duration::from_secs(ctime.elapsed().ok().unwrap_or_default().as_secs())
                        })
                        .unwrap_or_default();
                    let session_name = folder_name
                        .file_name()
                        .map(|f| std::path::PathBuf::from(f).display().to_string())?;
                    if std::path::Path::new(&layout_file_name).exists() {
                        Some((session_name, elapsed_duration))
                    } else {
                        None
                    }
                })
                .collect()
        },
        Err(e) => {
            log::error!(
                "Failed to read session_info cache folder: \"{:?}\": {:?}",
                &*ZELLIJ_SESSION_INFO_CACHE_DIR,
                e
            );
            vec![]
        },
    }
}

pub fn get_resurrectable_session_names() -> Vec<String> {
    match fs::read_dir(&*ZELLIJ_SESSION_INFO_CACHE_DIR) {
        Ok(files_in_session_info_folder) => {
            let files_that_are_folders = files_in_session_info_folder
                .filter_map(|f| f.ok().map(|f| f.path()))
                .filter(|f| f.is_dir());
            files_that_are_folders
                .filter_map(|folder_name| {
                    let folder = folder_name.display().to_string();
                    let resurrection_layout_file = session_layout_cache_file_name(&folder);
                    if std::path::Path::new(&resurrection_layout_file).exists() {
                        folder_name
                            .file_name()
                            .map(|f| format!("{}", f.to_string_lossy()))
                    } else {
                        None
                    }
                })
                .collect()
        },
        Err(e) => {
            log::error!(
                "Failed to read session_info cache folder: \"{:?}\": {:?}",
                &*ZELLIJ_SESSION_INFO_CACHE_DIR,
                e
            );
            vec![]
        },
    }
}

pub fn get_sessions_sorted_by_mtime() -> anyhow::Result<Vec<String>> {
    match fs::read_dir(&*ZELLIJ_SOCK_DIR) {
        Ok(files) => {
            let mut sessions_with_mtime: Vec<(String, SystemTime)> = Vec::new();
            for file in files {
                let file = file?;
                let file_name = file.file_name().into_string().unwrap();
                let file_modified_at = file.metadata()?.modified()?;
                if is_ipc_socket(&file.file_type()?) && assert_socket(&file_name) {
                    sessions_with_mtime.push((file_name, file_modified_at));
                }
            }
            sessions_with_mtime.sort_by_key(|x| x.1); // the oldest one will be the first

            let sessions = sessions_with_mtime.iter().map(|x| x.0.clone()).collect();
            Ok(sessions)
        },
        Err(err) if io::ErrorKind::NotFound != err.kind() => Err(err.into()),
        Err(_) => Ok(Vec::with_capacity(0)),
    }
}

/// Probe a session socket to check if a server is alive.
///
/// On Unix, connects and sends a `ConnStatus` message to verify the server responds.
/// On Windows, reads the server PID from the marker file and checks process liveness.
#[cfg(unix)]
const SESSION_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const KILL_SESSION_ACK_TIMEOUT: Duration = Duration::from_secs(6);

async fn await_kill_session_ack(
    path: &std::path::Path,
) -> Result<io::Result<()>, tokio::time::error::Elapsed> {
    tokio::time::timeout(
        KILL_SESSION_ACK_TIMEOUT,
        crate::ipc::async_send_kill_and_await(path),
    )
    .await
}

#[cfg(unix)]
fn probe_socket_stream(stream: interprocess::local_socket::Stream, timeout: Duration) -> bool {
    use interprocess::local_socket::traits::Stream as _;

    if stream.set_recv_timeout(Some(timeout)).is_err()
        || stream.set_send_timeout(Some(timeout)).is_err()
    {
        return false;
    }

    let mut sender: IpcSenderWithContext<ClientToServerMsg> = IpcSenderWithContext::new(stream);
    if sender
        .send_client_msg(ClientToServerMsg::ConnStatus)
        .is_err()
    {
        return false;
    }
    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> = sender.get_receiver();
    matches!(
        receiver.recv_server_msg(),
        Some((ServerToClientMsg::Connected, _))
    )
}

#[cfg(unix)]
fn assert_socket(name: &str) -> bool {
    use crate::consts::ipc_connect_timeout;
    let path = &*ZELLIJ_SOCK_DIR.join(name);
    match ipc_connect_timeout(path, SESSION_PROBE_TIMEOUT) {
        Ok(stream) => probe_socket_stream(stream, SESSION_PROBE_TIMEOUT),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            // A server may be between stale cleanup and bind while holding the
            // ownership lease. Discovery is allowed to hide that not-yet-live
            // session, but it must never unlink the path out from under it.
            let _ = remove_stale_socket_if_unowned(path);
            false
        },
        Err(_) => false,
    }
}

/// On Windows, reads the server PID from the marker file and checks whether
/// the process is still alive via `OpenProcess`. Cleans up stale marker files.
#[cfg(windows)]
fn assert_socket(name: &str) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    let path = &*ZELLIJ_SOCK_DIR.join(name);
    let pid_str = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => {
            drop(fs::remove_file(path));
            return false;
        },
    };
    let pid: u32 = match pid_str.trim().parse() {
        Ok(p) => p,
        Err(_) => {
            // Marker file exists but has no valid PID (e.g. empty from old version).
            // Treat as stale.
            drop(fs::remove_file(path));
            return false;
        },
    };
    let alive = unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            false
        } else {
            CloseHandle(handle);
            true
        }
    };
    if !alive {
        drop(fs::remove_file(path));
    }
    alive
}

#[cfg(not(any(unix, windows)))]
fn assert_socket(_name: &str) -> bool {
    true
}

/// Whether a session socket path is currently held by a live server.
///
/// This asks a deliberately different question than [`assert_socket`]: not "is
/// the server behind this socket healthy" but "may this path be unlinked and
/// re-bound". A server that is alive yet too busy to answer a `ConnStatus`
/// probe within the discovery deadline must never lose its own socket — the
/// old process keeps running, unreachable and clientless, and nothing reaps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketOwnership {
    /// Nothing is listening: no file at all, or a leftover from a crashed
    /// server. Removing it and binding is legal cleanup.
    Vacant,
    /// A process is listening on this path right now.
    Live,
    /// The path could not be classified. Treated as occupied by callers,
    /// because guessing wrong destroys a running session.
    Unknown(String),
}

/// Deadline for the ownership probe. Longer than `SESSION_PROBE_TIMEOUT`
/// because the answer decides whether another server gets evicted, and the
/// call happens exactly once per server start.
#[cfg(unix)]
pub const SOCKET_OWNERSHIP_PROBE_TIMEOUT: Duration = Duration::from_millis(1000);

/// On Unix, a successful `connect()` means some process holds the listening
/// end. That is enough: whether it replies to `ConnStatus` in time says
/// something about its health, not about its ownership of the name.
#[cfg(unix)]
pub fn probe_socket_ownership(path: &std::path::Path) -> SocketOwnership {
    use crate::consts::ipc_connect_timeout;
    match fs::symlink_metadata(path) {
        // Nothing is there, or what is there cannot be a listening socket —
        // either way nobody can be reached through it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return SocketOwnership::Vacant,
        Ok(metadata) if !is_ipc_socket(&metadata.file_type()) => return SocketOwnership::Vacant,
        _ => {},
    }
    match ipc_connect_timeout(path, SOCKET_OWNERSHIP_PROBE_TIMEOUT) {
        Ok(_stream) => SocketOwnership::Live,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            SocketOwnership::Vacant
        },
        Err(e) => SocketOwnership::Unknown(e.to_string()),
    }
}

/// Filesystem identity captured immediately after binding a Unix session
/// socket. Paths are mutable names; `(dev, ino)` is the ownership proof used by
/// teardown so an old process cannot unlink a successor's listener.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Shared lifetime owner for one Unix session socket name.
///
/// The advisory lock is intentionally retained for the whole server lifetime,
/// not merely around `probe -> unlink -> bind`. That gives startup, discovery,
/// forced stale cleanup, rename, and teardown one writer token. The listener's
/// own automatic name reclamation is disabled by [`bind`](Self::bind), making
/// this lease the only code allowed to remove the bound path.
#[cfg(unix)]
#[derive(Debug)]
pub struct SessionSocketLease {
    socket_path: PathBuf,
    _lock_file: File,
    bound_identity: Option<SocketIdentity>,
}

#[cfg(unix)]
impl SessionSocketLease {
    /// Acquire the non-blocking cross-process writer lease for `socket_path`.
    /// A `WouldBlock` error means another cooperative server owns this name.
    pub fn acquire(socket_path: &Path) -> io::Result<Self> {
        let lock_path = socket_lease_path(socket_path)?;
        let lock_file = acquire_socket_lock(&lock_path)?;
        Ok(Self {
            socket_path: socket_path.to_path_buf(),
            _lock_file: lock_file,
            bound_identity: None,
        })
    }

    /// Clean a stale name and bind while the lifetime lease is held.
    ///
    /// The live-owner probe remains necessary during migration: an older
    /// vc-frame binary can own the socket without owning the new lockfile.
    pub fn bind(&mut self) -> io::Result<interprocess::local_socket::Listener> {
        if self.bound_identity.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "session socket lease is already bound",
            ));
        }

        self.bind_owned_listener()
    }

    /// Re-publish the owned endpoint when its filesystem pathname disappeared.
    ///
    /// An open Unix listener remains alive after its pathname (or containing
    /// contract directory) is unlinked, but no new client can discover or
    /// connect to it. The lifetime lease survives outside that volatile
    /// contract directory, so the same server can safely recreate the root and
    /// bind a replacement listener while duplicate starters remain excluded.
    pub fn ensure_discoverable(
        &mut self,
    ) -> io::Result<Option<interprocess::local_socket::Listener>> {
        if let Some(identity) = self.bound_identity
            && path_has_identity(&self.socket_path, identity)?
        {
            return Ok(None);
        }

        let previous_identity = self.bound_identity.take();
        match self.bind_owned_listener() {
            Ok(listener) => Ok(Some(listener)),
            Err(error) => {
                self.bound_identity = previous_identity;
                Err(error)
            },
        }
    }

    fn bind_owned_listener(&mut self) -> io::Result<interprocess::local_socket::Listener> {
        use interprocess::local_socket::prelude::*;

        match probe_socket_ownership(&self.socket_path) {
            SocketOwnership::Vacant => {
                remove_path_if_unchanged(&self.socket_path)?;
            },
            SocketOwnership::Live => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another server is alive on {}", self.socket_path.display()),
                ));
            },
            SocketOwnership::Unknown(reason) => {
                return Err(io::Error::other(format!(
                    "cannot determine ownership of {}: {reason}",
                    self.socket_path.display()
                )));
            },
        }

        ensure_socket_parent(&self.socket_path)?;
        let mut listener = crate::consts::ipc_bind(&self.socket_path)?;
        // `interprocess` otherwise unlinks the original pathname from its Drop
        // implementation without checking inode identity. That can erase a
        // replacement listener after rename or an ownership race.
        listener.do_not_reclaim_name_on_drop();
        fs::set_permissions(&self.socket_path, fs::Permissions::from_mode(0o1700))?;

        let metadata = fs::symlink_metadata(&self.socket_path)?;
        if !metadata.file_type().is_socket() {
            return Err(io::Error::other(format!(
                "bound session path is not a socket: {}",
                self.socket_path.display()
            )));
        }
        self.bound_identity = Some(SocketIdentity::from_metadata(&metadata));
        Ok(listener)
    }

    /// Move the live socket and its writer lease to a new session name.
    ///
    /// The target lease is acquired before any pathname mutation. The old
    /// lease remains held until the rename and identity capture have completed,
    /// so concurrent A->B and B->A attempts fail closed instead of deadlocking.
    pub fn rename_owned_socket(&mut self, new_socket_path: &Path) -> io::Result<()> {
        if self.socket_path == new_socket_path {
            return Ok(());
        }
        let expected_identity = self.bound_identity.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "cannot rename a session socket before it is bound",
            )
        })?;
        if !path_has_identity(&self.socket_path, expected_identity)? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "refusing to rename session socket whose ownership changed: {}",
                    self.socket_path.display()
                ),
            ));
        }

        let new_lock_path = socket_lease_path(new_socket_path)?;
        let new_lock_file = acquire_socket_lock(&new_lock_path)?;
        match probe_socket_ownership(new_socket_path) {
            SocketOwnership::Vacant => {
                remove_path_if_unchanged(new_socket_path)?;
            },
            SocketOwnership::Live => {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another server is alive on {}", new_socket_path.display()),
                ));
            },
            SocketOwnership::Unknown(reason) => {
                return Err(io::Error::other(format!(
                    "cannot determine ownership of {}: {reason}",
                    new_socket_path.display()
                )));
            },
        }

        rename_path_noreplace(&self.socket_path, new_socket_path)?;
        let renamed_metadata = fs::symlink_metadata(new_socket_path)?;
        let renamed_identity = SocketIdentity::from_metadata(&renamed_metadata);
        if renamed_identity != expected_identity || !renamed_metadata.file_type().is_socket() {
            return Err(io::Error::other(format!(
                "renamed session socket identity changed unexpectedly: {}",
                new_socket_path.display()
            )));
        }

        self.socket_path = new_socket_path.to_path_buf();
        self._lock_file = new_lock_file;
        self.bound_identity = Some(renamed_identity);
        Ok(())
    }

    /// Remove the current pathname only when it is still this lease's socket.
    pub fn remove_if_owned(&mut self) -> io::Result<bool> {
        let Some(expected_identity) = self.bound_identity else {
            return Ok(false);
        };
        if !path_has_identity(&self.socket_path, expected_identity)? {
            self.bound_identity = None;
            return Ok(false);
        }
        match fs::remove_file(&self.socket_path) {
            Ok(()) => {
                self.bound_identity = None;
                Ok(true)
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.bound_identity = None;
                Ok(false)
            },
            Err(error) => Err(error),
        }
    }
}

/// Remove a stale Unix session socket only when no lifetime owner is active.
#[cfg(unix)]
pub fn remove_stale_socket_if_unowned(socket_path: &Path) -> io::Result<bool> {
    let _lease = match SessionSocketLease::acquire(socket_path) {
        Ok(lease) => lease,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
        Err(error) => return Err(error),
    };
    if probe_socket_ownership(socket_path) != SocketOwnership::Vacant {
        return Ok(false);
    }
    remove_path_if_unchanged(socket_path)
}

#[cfg(unix)]
fn socket_lease_path(socket_path: &Path) -> io::Result<PathBuf> {
    let parent = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session socket has no parent: {}", socket_path.display()),
        )
    })?;
    let file_name = socket_path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session socket has no file name: {}", socket_path.display()),
        )
    })?;
    // Keep the lock outside the versioned socket namespace. Deleting
    // `contract_version_N` must not create a second lock inode that allows a
    // duplicate server to start while the original listener is still alive.
    let namespace = parent.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session socket namespace has no name: {}", parent.display()),
        )
    })?;
    let lease_parent = parent.parent().unwrap_or(parent);
    let lock_root = lease_parent.join(".vc-frame-socket-leases");
    create_private_directory(&lock_root)?;
    let lock_dir = lock_root.join(namespace);
    create_private_directory(&lock_dir)?;
    let mut lock_name = OsString::from(file_name);
    lock_name.push(".lock");
    Ok(lock_dir.join(lock_name))
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir_all(path) {
        Ok(()) => {},
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::other(format!(
            "session ownership directory is not a real directory: {}",
            path.display()
        )));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
fn ensure_socket_parent(socket_path: &Path) -> io::Result<()> {
    let parent = socket_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("session socket has no parent: {}", socket_path.display()),
        )
    })?;
    if !parent.exists() {
        create_private_directory(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_socket_lock(lock_path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(lock_path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other(format!(
            "session socket lease is not a regular file: {}",
            lock_path.display()
        )));
    }
    // SAFETY: `file` owns a valid descriptor for the duration of this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN)
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "session socket lease is already held: {}",
                    lock_path.display()
                ),
            ));
        }
        return Err(error);
    }
    Ok(file)
}

#[cfg(unix)]
fn remove_path_if_unchanged(path: &Path) -> io::Result<bool> {
    let expected = match fs::symlink_metadata(path) {
        Ok(metadata) => SocketIdentity::from_metadata(&metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !path_has_identity(path, expected)? {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn path_has_identity(path: &Path, expected: SocketIdentity) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(SocketIdentity::from_metadata(&metadata) == expected),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn path_to_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "session socket path contains a NUL byte: {}",
                path.display()
            ),
        )
    })
}

/// Atomically move a socket pathname without replacing a destination that
/// appeared after the ownership probe. This closes the legacy-server race at
/// the actual filesystem mutation rather than relying only on cooperative
/// lease holders.
#[cfg(target_vendor = "apple")]
fn rename_path_noreplace(old_path: &Path, new_path: &Path) -> io::Result<()> {
    let old_path = path_to_cstring(old_path)?;
    let new_path = path_to_cstring(new_path)?;
    // SAFETY: both C strings remain alive for the call and contain no interior
    // NUL bytes. RENAME_EXCL asks the kernel to fail if `new_path` exists.
    let result = unsafe {
        libc::renameatx_np(
            libc::AT_FDCWD,
            old_path.as_ptr(),
            libc::AT_FDCWD,
            new_path.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn rename_path_noreplace(old_path: &Path, new_path: &Path) -> io::Result<()> {
    let old_path = path_to_cstring(old_path)?;
    let new_path = path_to_cstring(new_path)?;
    // SAFETY: both C strings remain alive for the call and contain no interior
    // NUL bytes. The raw syscall is required because libc does not expose its
    // renameat2 wrapper on musl. RENAME_NOREPLACE asks the kernel to fail if
    // `new_path` exists.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            old_path.as_ptr(),
            libc::AT_FDCWD,
            new_path.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(all(
    unix,
    not(target_vendor = "apple"),
    not(any(target_os = "linux", target_os = "android"))
))]
fn rename_path_noreplace(old_path: &Path, new_path: &Path) -> io::Result<()> {
    // POSIX link is the portable no-clobber primitive for the remaining Unix
    // targets. Session names share one socket directory, so this cannot cross
    // filesystems. A brief second link is harmless: both names address the same
    // listener inode until the old name is removed.
    fs::hard_link(old_path, new_path)?;
    if let Err(error) = fs::remove_file(old_path) {
        let _ = fs::remove_file(new_path);
        return Err(error);
    }
    Ok(())
}

// Deliberately no non-Unix implementation. Off Unix the session path is a
// marker file and the listener is a named pipe whose name the OS refuses to
// hand out twice, so `ipc_bind` — which writes the marker only after that bind
// succeeds — already answers the ownership question without a probe. Guessing
// from the marker's PID would be strictly worse: a PID recycled after a crash
// reads as live and strands the session name, while connecting to the pipe to
// check would occupy the target server's accept loop, which pairs every
// accepted stream with a blocking accept on the reply pipe.

#[cfg(all(test, unix))]
mod session_probe_timeout_tests {
    use super::*;
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    use std::{process::Command, thread, time::Instant};

    const LEASE_CHILD_MODE: &str = "VC_FRAME_SOCKET_LEASE_CHILD_MODE";
    const LEASE_CHILD_SOCKET: &str = "VC_FRAME_SOCKET_LEASE_CHILD_SOCKET";
    const LEASE_CHILD_READY: &str = "VC_FRAME_SOCKET_LEASE_CHILD_READY";
    const LEASE_CHILD_RELEASE: &str = "VC_FRAME_SOCKET_LEASE_CHILD_RELEASE";

    fn wait_for_path(path: &Path, description: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "timed out waiting for {description}");
    }

    /// `flock` lives on the open file description, and sibling tests spawn
    /// children through `Command`: between fork and exec such a child holds an
    /// inherited copy of every parent fd — including this lease's lock fd —
    /// until `O_CLOEXEC` closes it. Dropping a lease therefore releases the
    /// lock "any moment now", not atomically-now, so post-drop expectations
    /// must poll briefly instead of asserting on the first attempt.
    fn acquire_after_release(socket: &Path, description: &str) -> SessionSocketLease {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match SessionSocketLease::acquire(socket) {
                Ok(lease) => return lease,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "timed out waiting for {description}"
                    );
                    thread::sleep(Duration::from_millis(10));
                },
                Err(error) => panic!("{description}: {error}"),
            }
        }
    }

    /// Same fork-window caveat as [`acquire_after_release`], for the cleanup
    /// path: a transiently inherited lock makes the guard defer (`Ok(false)`)
    /// even though no real owner remains.
    fn eventually_removes_stale(socket: &Path, description: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if remove_stale_socket_if_unowned(socket).expect(description) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn lease_child(mode: &str, socket: &Path, ready: &Path, release: &Path) -> std::process::Child {
        Command::new(std::env::current_exe().expect("current test binary"))
            .arg("--exact")
            .arg("sessions::session_probe_timeout_tests::session_socket_lease_child_helper")
            .arg("--nocapture")
            .env(LEASE_CHILD_MODE, mode)
            .env(LEASE_CHILD_SOCKET, socket)
            .env(LEASE_CHILD_READY, ready)
            .env(LEASE_CHILD_RELEASE, release)
            .spawn()
            .expect("spawn lease child")
    }

    #[test]
    fn session_socket_lease_child_helper() {
        let Ok(mode) = std::env::var(LEASE_CHILD_MODE) else {
            return;
        };
        let socket = PathBuf::from(std::env::var_os(LEASE_CHILD_SOCKET).expect("child socket"));
        let ready = PathBuf::from(std::env::var_os(LEASE_CHILD_READY).expect("child ready"));
        let release = PathBuf::from(std::env::var_os(LEASE_CHILD_RELEASE).expect("child release"));

        match mode.as_str() {
            "hold" => {
                let mut lease = SessionSocketLease::acquire(&socket).expect("child lease");
                let _listener = lease.bind().expect("child bind");
                fs::write(&ready, b"ready").expect("publish child readiness");
                wait_for_path(&release, "parent release");
                assert!(lease.remove_if_owned().expect("child teardown"));
            },
            "expect-busy" => {
                let error = SessionSocketLease::acquire(&socket)
                    .expect_err("second process must observe the held lease");
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                fs::write(&ready, b"busy").expect("publish busy result");
            },
            other => panic!("unknown lease child mode: {other}"),
        }
    }

    #[test]
    fn silent_session_socket_is_rejected_within_the_probe_deadline() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("silent-session.sock");
        let listener = ListenerOptions::new()
            .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .expect("bind silent socket");

        let server = std::thread::spawn(move || {
            let _stream = listener
                .incoming()
                .next()
                .expect("incoming connection")
                .expect("accept silent client");
            std::thread::sleep(Duration::from_secs(2));
        });

        let stream = crate::consts::ipc_connect_timeout(&socket, Duration::from_millis(75))
            .expect("connect silent socket");
        let started = Instant::now();
        let alive = probe_socket_stream(stream, Duration::from_millis(75));

        assert!(
            !alive,
            "a server that never answers ConnStatus is not healthy"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "session discovery must not block on a silent socket"
        );
        server.join().expect("silent server thread");
    }

    #[test]
    fn a_busy_but_listening_socket_still_belongs_to_its_server() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("busy-session.sock");
        let listener = ListenerOptions::new()
            .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .expect("bind busy socket");
        // A server that never answers ConnStatus: `assert_socket` reports it as
        // gone, which is exactly the misread that used to cost it its socket.
        let server = std::thread::spawn(move || {
            let _listener = listener;
            std::thread::sleep(Duration::from_millis(500));
        });

        assert_eq!(probe_socket_ownership(&socket), SocketOwnership::Live);
        server.join().expect("busy server thread");
    }

    #[test]
    fn a_stale_socket_file_is_vacant() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("stale-session.sock");
        {
            let listener = ListenerOptions::new()
                .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
                .create_sync()
                .expect("bind stale socket");
            drop(listener);
        }

        assert_eq!(probe_socket_ownership(&socket), SocketOwnership::Vacant);
        assert_eq!(
            probe_socket_ownership(&dir.path().join("never-existed.sock")),
            SocketOwnership::Vacant
        );

        // Junk left at a session path must not block that session name
        // forever just because connect() reports an unfamiliar error.
        let not_a_socket = dir.path().join("not-a-socket");
        std::fs::write(&not_a_socket, b"stale").expect("write junk");
        assert_eq!(
            probe_socket_ownership(&not_a_socket),
            SocketOwnership::Vacant
        );
    }

    #[test]
    fn lifetime_lease_serializes_owners_and_releases_on_drop() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("leased-session.sock");
        let first = SessionSocketLease::acquire(&socket).expect("first lease");

        let error = SessionSocketLease::acquire(&socket).expect_err("second lease must fail");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        drop(first);
        acquire_after_release(&socket, "lease released on final drop");
    }

    #[test]
    fn live_owner_rebinds_after_contract_namespace_is_unlinked() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let namespace = root.path().join("contract_version_2");
        fs::create_dir(&namespace).expect("create socket namespace");
        let socket = namespace.join("durable-session");
        let mut lease = SessionSocketLease::acquire(&socket).expect("owner lease");
        let original_listener = lease.bind().expect("original listener");

        fs::remove_dir_all(&namespace).expect("unlink live socket namespace");
        let duplicate_error = SessionSocketLease::acquire(&socket)
            .expect_err("namespace loss must not create a second owner");
        assert_eq!(duplicate_error.kind(), io::ErrorKind::WouldBlock);

        let rebound_listener = lease
            .ensure_discoverable()
            .expect("recover discovery endpoint")
            .expect("missing endpoint must be rebound");
        assert!(socket.exists(), "recovery recreates the discoverable path");
        crate::consts::ipc_connect_timeout(&socket, Duration::from_millis(250))
            .expect("rebound endpoint accepts new clients");
        assert!(
            lease
                .ensure_discoverable()
                .expect("stable endpoint")
                .is_none(),
            "an intact endpoint is not rebound repeatedly"
        );

        drop(original_listener);
        assert!(
            socket.exists(),
            "old listener drop cannot unlink replacement"
        );
        drop(rebound_listener);
        assert!(lease.remove_if_owned().expect("owned teardown"));
    }

    #[test]
    fn dead_owner_reclaims_after_contract_namespace_loss() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let namespace = root.path().join("contract_version_2");
        fs::create_dir(&namespace).expect("create socket namespace");
        let socket = namespace.join("reclaimed-session");
        let mut original_lease = SessionSocketLease::acquire(&socket).expect("original lease");
        let original_listener = original_lease.bind().expect("original listener");

        fs::remove_dir_all(&namespace).expect("unlink live socket namespace");
        drop(original_listener);
        drop(original_lease);

        let mut successor = acquire_after_release(&socket, "dead-owner lease reclaim");
        let successor_listener = successor
            .bind()
            .expect("successor bind recreates namespace");
        assert!(socket.exists());
        drop(successor_listener);
        assert!(successor.remove_if_owned().expect("successor teardown"));
    }

    #[test]
    fn lifetime_lease_serializes_independent_processes() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("cross-process-session.sock");
        let holder_ready = dir.path().join("holder.ready");
        let contender_ready = dir.path().join("contender.ready");
        let release = dir.path().join("release");

        let mut holder = lease_child("hold", &socket, &holder_ready, &release);
        wait_for_path(&holder_ready, "holder readiness");
        crate::consts::ipc_connect_timeout(&socket, Duration::from_millis(250))
            .expect("winning listener remains connectable");

        let mut contender = lease_child("expect-busy", &socket, &contender_ready, &release);
        wait_for_path(&contender_ready, "contender result");
        assert!(contender.wait().expect("wait contender").success());

        fs::write(&release, b"release").expect("release holder");
        assert!(holder.wait().expect("wait holder").success());
        assert!(!socket.exists(), "winner performs its own teardown");
    }

    #[test]
    fn socket_lease_descriptor_is_closed_by_exec() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("cloexec-session.sock");
        let lease = SessionSocketLease::acquire(&socket).expect("lease");
        let mut child = Command::new("/bin/sleep")
            .arg("2")
            .spawn()
            .expect("spawn exec child");
        drop(lease);

        let deadline = Instant::now() + Duration::from_millis(750);
        let reacquired = loop {
            match SessionSocketLease::acquire(&socket) {
                Ok(lease) => break Some(lease),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(10));
                },
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break None,
                Err(error) => panic!("unexpected reacquire error: {error}"),
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            reacquired.is_some(),
            "an exec child must not prolong the socket lease descriptor"
        );
    }

    #[test]
    fn lease_bind_preserves_a_live_legacy_listener() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("legacy-session.sock");
        let listener = ListenerOptions::new()
            .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .expect("bind legacy listener");
        let mut lease = SessionSocketLease::acquire(&socket).expect("lease");

        let error = lease.bind().expect_err("live legacy listener must win");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(
            socket.exists(),
            "the live legacy socket must not be unlinked"
        );
        drop(listener);
    }

    #[test]
    fn teardown_does_not_unlink_a_replacement_inode() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("replacement-session.sock");
        let mut lease = SessionSocketLease::acquire(&socket).expect("lease");
        let listener = lease.bind().expect("bind owned listener");

        fs::remove_file(&socket).expect("simulate hostile unlink");
        let mut replacement = ListenerOptions::new()
            .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .expect("bind replacement listener");
        replacement.do_not_reclaim_name_on_drop();

        assert!(!lease.remove_if_owned().expect("identity-safe teardown"));
        assert!(
            socket.exists(),
            "replacement socket must survive old teardown"
        );
        drop(listener);
        assert!(
            socket.exists(),
            "owned listener Drop must not reclaim by pathname"
        );
        drop(replacement);
        fs::remove_file(&socket).expect("test cleanup");
    }

    #[test]
    fn rename_transfers_lease_identity_and_teardown_target() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let old_socket = dir.path().join("old-session.sock");
        let new_socket = dir.path().join("new-session.sock");
        let mut lease = SessionSocketLease::acquire(&old_socket).expect("lease");
        let listener = lease.bind().expect("bind owned listener");

        lease
            .rename_owned_socket(&new_socket)
            .expect("transfer socket lease");
        assert!(!old_socket.exists());
        assert!(new_socket.exists());
        assert!(SessionSocketLease::acquire(&new_socket).is_err());

        assert!(lease.remove_if_owned().expect("remove renamed socket"));
        assert!(!new_socket.exists());
        drop(listener);
        assert!(
            !new_socket.exists(),
            "listener Drop must not recreate or reclaim"
        );
    }

    #[test]
    fn rename_refuses_a_live_destination_without_changing_the_source() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let old_socket = dir.path().join("source-session.sock");
        let new_socket = dir.path().join("occupied-session.sock");
        let mut lease = SessionSocketLease::acquire(&old_socket).expect("source lease");
        let source_listener = lease.bind().expect("bind source listener");
        let destination_listener = ListenerOptions::new()
            .name(
                new_socket
                    .as_path()
                    .to_fs_name::<GenericFilePath>()
                    .unwrap(),
            )
            .create_sync()
            .expect("bind occupied destination");

        let error = lease
            .rename_owned_socket(&new_socket)
            .expect_err("occupied destination must win");
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(old_socket.exists(), "source ownership remains intact");
        assert!(new_socket.exists(), "destination remains intact");

        assert!(lease.remove_if_owned().expect("source teardown"));
        drop(source_listener);
        assert!(
            new_socket.exists(),
            "source teardown must not touch destination"
        );
        drop(destination_listener);
    }

    #[test]
    fn atomic_no_replace_rename_preserves_a_late_destination() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let source = dir.path().join("source");
        let destination = dir.path().join("destination");
        fs::write(&source, b"source").expect("write source");
        fs::write(&destination, b"destination").expect("write destination");

        let error = rename_path_noreplace(&source, &destination)
            .expect_err("no-replace rename must reject an existing destination");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).expect("read source"), b"source");
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"destination"
        );
    }

    #[test]
    fn stale_cleanup_defers_to_an_active_lifetime_lease() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let socket = dir.path().join("starting-session.sock");
        let mut stale = ListenerOptions::new()
            .name(socket.as_path().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .expect("bind stale socket");
        stale.do_not_reclaim_name_on_drop();
        drop(stale);
        let lease = SessionSocketLease::acquire(&socket).expect("startup lease");

        assert!(!remove_stale_socket_if_unowned(&socket).expect("defer cleanup"));
        assert!(socket.exists(), "cleanup must not race the startup lease");

        drop(lease);
        eventually_removes_stale(&socket, "clean stale socket");
        assert!(!socket.exists());
    }

    #[test]
    fn kill_ack_timer_is_entered_inside_its_runtime() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let missing_socket = dir.path().join("missing-session.sock");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("shutdown runtime");

        let result = runtime.block_on(await_kill_session_ack(&missing_socket));

        assert!(
            matches!(result, Ok(Err(_))),
            "a missing socket should return its transport error without panicking outside Tokio"
        );
    }
}

pub fn print_sessions(
    mut sessions: Vec<(String, Duration, bool)>,
    no_formatting: bool,
    short: bool,
    reverse: bool,
) {
    // (session_name, timestamp, is_dead)
    let curr_session = envs::get_session_name().unwrap_or_else(|_| "".into());
    sessions.sort_by(|a, b| {
        if reverse {
            // sort by `Duration` ascending (newest would be first)
            a.1.cmp(&b.1)
        } else {
            b.1.cmp(&a.1)
        }
    });
    sessions
        .iter()
        .for_each(|(session_name, timestamp, is_dead)| {
            if short {
                println!("{}", session_name);
                return;
            }
            if no_formatting {
                let suffix = if curr_session == *session_name {
                    "(current)".to_string()
                } else if *is_dead {
                    "(EXITED - attach to resurrect)".to_string()
                } else {
                    String::new()
                };
                let timestamp = format!("[Created {} ago]", format_duration(*timestamp));
                println!("{} {} {}", session_name, timestamp, suffix);
            } else {
                let formatted_session_name = format!("\u{1b}[32;1m{}\u{1b}[m", session_name);
                let suffix = if curr_session == *session_name {
                    "(current)".to_string()
                } else if *is_dead {
                    "(\u{1b}[31;1mEXITED\u{1b}[m - attach to resurrect)".to_string()
                } else {
                    String::new()
                };
                let timestamp = format!(
                    "[Created \u{1b}[35;1m{}\u{1b}[m ago]",
                    format_duration(*timestamp)
                );
                println!("{} {} {}", formatted_session_name, timestamp, suffix);
            }
        })
}

pub fn print_sessions_with_index(sessions: Vec<String>) {
    let curr_session = envs::get_session_name().unwrap_or_else(|_| "".into());
    for (i, session) in sessions.iter().enumerate() {
        let suffix = if curr_session == *session {
            " (current)"
        } else {
            ""
        };
        println!("{}: {}{}", i, session, suffix);
    }
}

pub enum ActiveSession {
    None,
    One(String),
    Many,
}

pub fn get_active_session() -> ActiveSession {
    match get_sessions() {
        Ok(sessions) if sessions.is_empty() => ActiveSession::None,
        Ok(mut sessions) if sessions.len() == 1 => ActiveSession::One(sessions.pop().unwrap().0),
        Ok(_) => ActiveSession::Many,
        Err(e) => {
            eprintln!("Error occurred: {:?}", e);
            process::exit(1);
        },
    }
}

pub fn kill_session(name: &str, force: bool) {
    if let Err(error) = validate_session_name(name) {
        eprintln!("{error}");
        process::exit(1);
    }
    let path = &*ZELLIJ_SOCK_DIR.join(name);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("Cannot create shutdown runtime: {error}");
            process::exit(1);
        });
    // Poll the async helper from inside the runtime. Constructing
    // `tokio::time::timeout` before `block_on` panics because no reactor is
    // entered yet.
    let shutdown_result = runtime.block_on(await_kill_session_ack(path));
    match shutdown_result {
        Ok(Ok(())) => {},
        Ok(Err(error)) => {
            // Dead transport: the server is already gone and only its socket
            // remains. With --force the kill is idempotent — report success
            // and clean the stale socket so the name stops resolving.
            let already_dead = matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            );
            if force && already_dead {
                #[cfg(unix)]
                let cleaned = remove_stale_socket_if_unowned(path).unwrap_or(false);
                #[cfg(not(unix))]
                let cleaned = std::fs::remove_file(path).is_ok();
                if cleaned {
                    eprintln!("Session {name} was already dead — cleaned up its stale socket.");
                } else {
                    eprintln!(
                        "Session {name} was already dead — socket cleanup deferred to its owner."
                    );
                }
            } else {
                eprintln!("Failed to kill session {name}: {error}");
                process::exit(1);
            }
        },
        Err(_) => {
            eprintln!(
                "Session {name} did not acknowledge shutdown within {:.1}s",
                KILL_SESSION_ACK_TIMEOUT.as_secs_f64()
            );
            if force {
                eprintln!(
                    "--force cannot reach an unresponsive server; find it with: ps aux | grep 'vc-frame --server' | grep '{name}'"
                );
            }
            process::exit(1);
        },
    }
}

pub fn delete_session(name: &str, force: bool) {
    if force {
        use crate::consts::ipc_connect;
        let path = &*ZELLIJ_SOCK_DIR.join(name);
        let _ = ipc_connect(path).ok().map(|stream| {
            #[cfg(windows)]
            {
                let reply = crate::consts::ipc_connect_reply(path);
                let _ = IpcSenderWithContext::<ClientToServerMsg>::new(stream)
                    .send_client_msg(ClientToServerMsg::KillSession);
                if let Ok(reply_stream) = reply {
                    let mut receiver: IpcReceiverWithContext<ServerToClientMsg> =
                        IpcReceiverWithContext::new(reply_stream);
                    let _ = receiver.recv_server_msg();
                }
            }
            #[cfg(not(windows))]
            {
                IpcSenderWithContext::<ClientToServerMsg>::new(stream)
                    .send_client_msg(ClientToServerMsg::KillSession)
                    .ok();
            }
        });
    }
    if let Err(e) = std::fs::remove_dir_all(session_info_folder_for_session(name)) {
        if e.kind() == std::io::ErrorKind::NotFound {
            eprintln!("Session: {:?} not found.", name);
            process::exit(2);
        } else {
            log::error!("Failed to remove session {:?}: {:?}", name, e);
        }
    } else {
        println!("Session: {:?} successfully deleted.", name);
    }
}

pub fn list_sessions(no_formatting: bool, short: bool, reverse: bool) {
    let exit_code = match get_sessions() {
        Ok(running_sessions) => {
            let resurrectable_sessions = get_resurrectable_sessions();
            let mut all_sessions: HashMap<String, (Duration, bool)> = resurrectable_sessions
                .iter()
                .map(|(name, timestamp)| (name.clone(), (*timestamp, true)))
                .collect();
            for (session_name, duration) in running_sessions {
                all_sessions.insert(session_name.clone(), (duration, false));
            }
            if all_sessions.is_empty() {
                eprintln!("No active vc-frame sessions found.");
                1
            } else {
                print_sessions(
                    all_sessions
                        .iter()
                        .map(|(name, (timestamp, is_dead))| (name.clone(), *timestamp, *is_dead))
                        .collect(),
                    no_formatting,
                    short,
                    reverse,
                );
                0
            }
        },
        Err(e) => {
            eprintln!("Error occurred: {:?}", e);
            1
        },
    };
    process::exit(exit_code);
}

#[derive(Debug, Clone)]
pub enum SessionNameMatch {
    AmbiguousPrefix(Vec<String>),
    UniquePrefix(String),
    Exact(String),
    None,
}

pub fn match_session_name(prefix: &str) -> Result<SessionNameMatch, io::ErrorKind> {
    let sessions = get_sessions()?;

    let filtered_sessions: Vec<_> = sessions
        .iter()
        .filter(|s| s.0.starts_with(prefix))
        .collect();

    if filtered_sessions.iter().any(|s| s.0 == prefix) {
        return Ok(SessionNameMatch::Exact(prefix.to_string()));
    }

    Ok({
        match &filtered_sessions[..] {
            [] => SessionNameMatch::None,
            [s] => SessionNameMatch::UniquePrefix(s.0.to_string()),
            _ => SessionNameMatch::AmbiguousPrefix(
                filtered_sessions.into_iter().map(|s| s.0.clone()).collect(),
            ),
        }
    })
}

pub fn session_exists(name: &str) -> Result<bool, io::ErrorKind> {
    match match_session_name(name) {
        Ok(SessionNameMatch::Exact(_)) => Ok(true),
        Ok(_) => Ok(false),
        Err(e) => Err(e),
    }
}

// if the session is resurrecable, the returned layout is the one to be used to resurrect it
pub fn resurrection_layout(session_name_to_resurrect: &str) -> Result<Option<Layout>, String> {
    let layout_file_name = session_layout_cache_file_name(session_name_to_resurrect);
    let raw_layout = match std::fs::read_to_string(&layout_file_name) {
        Ok(raw_layout) => raw_layout,
        Err(_e) => {
            return Ok(None);
        },
    };
    match Layout::from_kdl(
        &raw_layout,
        Some(layout_file_name.display().to_string()),
        None,
        None,
    ) {
        Ok(layout) => Ok(Some(layout)),
        Err(e) => {
            log::error!(
                "Failed to parse resurrection layout file {}: {}",
                layout_file_name.display(),
                e
            );
            Err(format!(
                "Failed to parse resurrection layout file {}: {}.",
                layout_file_name.display(),
                e
            ))
        },
    }
}

pub fn assert_session(name: &str) {
    match session_exists(name) {
        Ok(result) => {
            if result {
                return;
            } else {
                println!("No session named {:?} found.", name);
                let session_names = get_sessions()
                    .unwrap()
                    .into_iter()
                    .map(|session| session.0)
                    .collect::<Vec<_>>();
                if let Some(sugg) = find_best_match_for_name(session_names.iter(), name, None) {
                    println!("  help: Did you mean `{}`?", sugg);
                }
            }
        },
        Err(e) => {
            eprintln!("Error occurred: {:?}", e);
        },
    };
    process::exit(1);
}

pub fn assert_dead_session(name: &str, force: bool) {
    match session_exists(name) {
        Ok(exists) => {
            if exists && !force {
                println!(
                    "A session by the name {:?} exists and is active, use --force to delete it.",
                    name
                )
            } else if exists && force {
                println!(
                    "A session by the name {:?} exists and is active, but will be force killed and deleted.",
                    name
                );
                return;
            } else {
                return;
            }
        },
        Err(e) => {
            eprintln!("Error occurred: {:?}", e);
        },
    };
    process::exit(1);
}

pub fn validate_session_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err(
            "Session name cannot be empty. Please provide a specific session name.".to_string(),
        );
    }
    if name == "." || name == ".." {
        return Err(format!("Invalid session name: \"{}\".", name));
    }
    if name.contains(['/', '\\']) {
        return Err("Session name cannot contain path separators.".to_string());
    }
    let bytes = name.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err("Session name cannot contain a Windows drive prefix.".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod session_name_validation_tests {
    use super::validate_session_name;

    #[test]
    fn session_name_cannot_escape_socket_root() {
        for valid in ["work", "work.tree", "work-night", "work:night"] {
            assert_eq!(validate_session_name(valid), Ok(()), "{valid}");
        }

        for invalid in [
            "",
            " ",
            ".",
            "..",
            "../other",
            "/tmp/other",
            r"..\other",
            r"\other",
            r"C:\other",
            "C:other",
            r"\\server\share",
        ] {
            assert!(
                validate_session_name(invalid).is_err(),
                "{invalid:?} must not escape the session socket root"
            );
        }
    }
}

pub fn assert_session_ne(name: &str) {
    if let Err(e) = validate_session_name(name) {
        eprintln!("{}", e);
        process::exit(1);
    }

    match session_exists(name) {
        Ok(result) if !result => {
            let resurrectable_sessions = get_resurrectable_session_names();
            if resurrectable_sessions.iter().any(|s| s == name) {
                println!(
                    "Session with name {:?} already exists, but is dead. Use the attach command to resurrect it or, the delete-session command to kill it or specify a different name.",
                    name
                );
            } else {
                return;
            }
        },
        Ok(_) => println!(
            "Session with name {:?} already exists. Use attach command to connect to it or specify a different name.",
            name
        ),
        Err(e) => eprintln!("Error occurred: {:?}", e),
    };
    process::exit(1);
}

pub fn generate_unique_session_name() -> Option<String> {
    let sessions = get_sessions().map(|sessions| {
        sessions
            .iter()
            .map(|s| s.0.clone())
            .collect::<Vec<String>>()
    });
    let dead_sessions = get_resurrectable_session_names();
    let Ok(sessions) = sessions else {
        eprintln!("Failed to list existing sessions: {:?}", sessions);
        return None;
    };

    get_name_generator().take(1000).find(|name| {
        session_name_fits_socket_path(name)
            && !sessions.contains(name)
            && !dead_sessions.contains(name)
    })
}

#[cfg(unix)]
fn session_name_fits_socket_path(session_name: &str) -> bool {
    socket_path_fits_limit(
        &ZELLIJ_SOCK_DIR,
        session_name,
        crate::consts::ZELLIJ_SOCK_MAX_LENGTH,
    )
}

#[cfg(unix)]
fn socket_path_fits_limit(
    socket_dir: &std::path::Path,
    session_name: &str,
    max_length: usize,
) -> bool {
    socket_dir.join(session_name).as_os_str().len() < max_length
}

#[cfg(not(unix))]
fn session_name_fits_socket_path(_session_name: &str) -> bool {
    true
}

#[cfg(all(test, unix))]
mod generated_session_name_tests {
    use super::socket_path_fits_limit;
    use std::path::PathBuf;

    #[test]
    fn socket_path_limit_is_exclusive() {
        let socket_dir = PathBuf::from("x".repeat(80));

        assert!(socket_path_fits_limit(&socket_dir, &"y".repeat(22), 104));
        assert!(!socket_path_fits_limit(&socket_dir, &"y".repeat(23), 104));
    }
}

/// Create a new random name generator
///
/// Used to provide a memorable handle for a session when users don't specify a session name when the session is
/// created.
///
/// Uses the list of adjectives and nouns defined below, with the intention of avoiding unfortunate
/// and offensive combinations. Care should be taken when adding or removing to either list due to the birthday paradox/
/// hash collisions, e.g. with 4096 unique names, the likelihood of a collision in 10 session names is 1%.
pub fn get_name_generator() -> impl Iterator<Item = String> {
    names::Generator::new(ADJECTIVES, NOUNS, names::Name::Plain)
}

/// Generates a random human-readable name using curated adjectives and nouns.
/// Returns a single name in the format: AdjectiveNoun (e.g., "BraveRustacean")
pub fn generate_random_name() -> String {
    get_name_generator().next().unwrap()
}

const ADJECTIVES: &[&str] = &[
    "adamant",
    "adept",
    "adventurous",
    "arcadian",
    "auspicious",
    "awesome",
    "blossoming",
    "brave",
    "charming",
    "chatty",
    "circular",
    "considerate",
    "cubic",
    "curious",
    "delighted",
    "didactic",
    "diligent",
    "effulgent",
    "erudite",
    "excellent",
    "exquisite",
    "fabulous",
    "fascinating",
    "friendly",
    "glowing",
    "gracious",
    "gregarious",
    "hopeful",
    "implacable",
    "inventive",
    "joyous",
    "judicious",
    "jumping",
    "kind",
    "likable",
    "loyal",
    "lucky",
    "marvellous",
    "mellifluous",
    "nautical",
    "oblong",
    "outstanding",
    "polished",
    "polite",
    "profound",
    "quadratic",
    "quiet",
    "rectangular",
    "remarkable",
    "rusty",
    "sensible",
    "sincere",
    "sparkling",
    "splendid",
    "stellar",
    "tenacious",
    "tremendous",
    "triangular",
    "undulating",
    "unflappable",
    "unique",
    "verdant",
    "vitreous",
    "wise",
    "zippy",
];

const NOUNS: &[&str] = &[
    "aardvark",
    "accordion",
    "apple",
    "apricot",
    "bee",
    "brachiosaur",
    "cactus",
    "capsicum",
    "clarinet",
    "cowbell",
    "crab",
    "cuckoo",
    "cymbal",
    "diplodocus",
    "donkey",
    "drum",
    "duck",
    "echidna",
    "elephant",
    "foxglove",
    "galaxy",
    "glockenspiel",
    "goose",
    "hill",
    "horse",
    "iguanadon",
    "jellyfish",
    "kangaroo",
    "lake",
    "lemon",
    "lemur",
    "magpie",
    "megalodon",
    "mountain",
    "mouse",
    "muskrat",
    "newt",
    "oboe",
    "ocelot",
    "orange",
    "panda",
    "peach",
    "pepper",
    "petunia",
    "pheasant",
    "piano",
    "pigeon",
    "platypus",
    "quasar",
    "rhinoceros",
    "river",
    "rustacean",
    "salamander",
    "sitar",
    "stegosaurus",
    "tambourine",
    "tiger",
    "tomato",
    "triceratops",
    "ukulele",
    "viola",
    "weasel",
    "xylophone",
    "yak",
    "zebra",
];
