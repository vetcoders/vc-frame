//! Who is behind a pid — and is it still the same process?
//!
//! A pid is a recyclable number, not an identity. The clinic must be able to
//! say "the process that answered on this socket a second ago is still the one
//! I am about to act on", so identity here is `(pid, start time)`: the kernel
//! hands a reused pid to a process that started later, and that difference is
//! what [`ProcessIdentity::is_same_process`] checks before anything mutates.
//!
//! The peer of a session socket comes from the kernel too
//! ([`socket_peer_pid`]), never from `ps` argv: argv keeps the socket path the
//! server was *started* with, and a renamed session no longer lives there.
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

use std::io;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// What the kernel says about one live process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub uid: u32,
    /// Process start, at the kernel's resolution (microseconds on macOS,
    /// clock ticks on Linux).
    pub started_at: SystemTime,
    /// The image the process executes, as the kernel resolves it now. `None`
    /// when the kernel cannot name it (unlinked image, permissions).
    pub executable: Option<PathBuf>,
}

impl ProcessIdentity {
    /// Read the identity of `pid`. `Ok(None)` means there is no such process.
    pub fn of(pid: i32) -> io::Result<Option<Self>> {
        if pid <= 0 {
            return Ok(None);
        }
        platform::identity_of(pid)
    }

    /// True only when `pid` is still alive AND started at the same instant.
    /// A recycled pid fails here, which is the point.
    pub fn is_same_process(&self) -> io::Result<bool> {
        Ok(Self::of(self.pid)?.is_some_and(|now| now.started_at == self.started_at))
    }

    /// Seconds since the process started, saturating at zero.
    pub fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.started_at)
            .unwrap_or_default()
    }
}

/// The pid of the process holding the other end of a connected Unix stream.
#[cfg(unix)]
pub fn socket_peer_pid(stream: &std::os::unix::net::UnixStream) -> io::Result<i32> {
    platform::peer_pid(stream)
}

/// The current user's uid.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: getuid has no failure mode and touches no memory.
    unsafe { libc::getuid() }
}

#[cfg(target_vendor = "apple")]
mod platform {
    use super::ProcessIdentity;
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    pub(super) fn identity_of(pid: i32) -> io::Result<Option<ProcessIdentity>> {
        // SAFETY: proc_bsdinfo is plain old data; the kernel fills at most
        // `size` bytes of it and returns the count written.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let written = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        if written <= 0 {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                // ESRCH: gone. A zero-byte answer with no errno is the same.
                Some(libc::ESRCH) | Some(0) | None => Ok(None),
                _ => Err(error),
            };
        }
        if written < size {
            return Err(io::Error::other(format!(
                "proc_pidinfo returned a short record for pid {pid}"
            )));
        }
        let started_at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(info.pbi_start_tvsec)
            + Duration::from_micros(info.pbi_start_tvusec);
        Ok(Some(ProcessIdentity {
            pid,
            uid: info.pbi_uid,
            started_at,
            executable: executable_of(pid),
        }))
    }

    fn executable_of(pid: i32) -> Option<PathBuf> {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: the buffer is PROC_PIDPATHINFO_MAXSIZE bytes long, as the
        // call requires; the return value is the number of bytes written.
        let written =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if written <= 0 {
            return None;
        }
        buffer.truncate(written as usize);
        Some(PathBuf::from(OsString::from_vec(buffer)))
    }

    pub(super) fn peer_pid(stream: &std::os::unix::net::UnixStream) -> io::Result<i32> {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: `pid` is a valid out-parameter of the size we declare.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pid)
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod platform {
    use super::ProcessIdentity;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::io::AsRawFd;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    pub(super) fn identity_of(pid: i32) -> io::Result<Option<ProcessIdentity>> {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        let stat = match std::fs::read_to_string(proc_dir.join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        // The command name is parenthesised and may hold spaces; fields
        // resume after the LAST ')'. starttime is field 22 overall.
        let after_comm = stat
            .rfind(')')
            .map(|index| &stat[index + 1..])
            .ok_or_else(|| io::Error::other("malformed /proc stat"))?;
        let start_ticks: u64 = after_comm
            .split_whitespace()
            .nth(19)
            .and_then(|field| field.parse().ok())
            .ok_or_else(|| io::Error::other("malformed /proc stat starttime"))?;
        let boot_secs: u64 = std::fs::read_to_string("/proc/stat")?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| io::Error::other("no btime in /proc/stat"))?;
        // SAFETY: sysconf has no memory side effects.
        let ticks_per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
        let started_at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(boot_secs)
            + Duration::from_micros(start_ticks * 1_000_000 / ticks_per_sec);
        let uid = std::fs::metadata(&proc_dir)?.uid();
        let executable = std::fs::read_link(proc_dir.join("exe")).ok().map(|path| {
            // An unlinked image reads back as "<path> (deleted)".
            let text = path.to_string_lossy();
            PathBuf::from(text.strip_suffix(" (deleted)").unwrap_or(&text).to_owned())
        });
        Ok(Some(ProcessIdentity {
            pid,
            uid,
            started_at,
            executable,
        }))
    }

    pub(super) fn peer_pid(stream: &std::os::unix::net::UnixStream) -> io::Result<i32> {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `credentials` is a valid out-parameter of the declared size.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credentials).cast(),
                &mut len,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credentials.pid)
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux", target_os = "android")))]
mod platform {
    use super::ProcessIdentity;
    use std::io;

    pub(super) fn identity_of(_pid: i32) -> io::Result<Option<ProcessIdentity>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process identity is not implemented on this platform",
        ))
    }

    #[cfg(unix)]
    pub(super) fn peer_pid(_stream: &std::os::unix::net::UnixStream) -> io::Result<i32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket peer identity is not implemented on this platform",
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn this_process_has_an_identity_that_names_its_own_image() {
        let identity = ProcessIdentity::of(std::process::id() as i32)
            .unwrap()
            .expect("the test process exists");
        assert_eq!(identity.uid, current_uid());
        assert!(identity.age() < Duration::from_secs(24 * 3600));
        let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
        let named = identity
            .executable
            .clone()
            .expect("the kernel names our image");
        assert_eq!(named.canonicalize().unwrap(), exe);
        assert!(identity.is_same_process().unwrap());
    }

    #[test]
    fn a_reaped_child_has_no_identity_and_a_forged_start_is_not_the_same_process() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert_eq!(ProcessIdentity::of(pid).unwrap(), None);

        // PID reuse, simulated: same pid, different start → never the same.
        let mut forged = ProcessIdentity::of(std::process::id() as i32)
            .unwrap()
            .unwrap();
        forged.started_at -= Duration::from_secs(1);
        assert!(!forged.is_same_process().unwrap());
    }

    #[test]
    fn the_kernel_names_the_listener_behind_a_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let _accepted = listener.accept().unwrap();
        assert_eq!(socket_peer_pid(&stream).unwrap(), std::process::id() as i32);
    }
}
