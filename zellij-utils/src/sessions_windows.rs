//! Non-mutating Windows session ownership precheck. A live PID marker is followed
//! by the caller's IPC inventory/incarnation check before project admission.
//! Discovery may clean stale markers; this ownership probe must never do so.

use super::SocketOwnership;
use std::io::{self, Read};
use std::path::Path;

#[cfg(windows)]
pub fn probe_socket_ownership(path: &Path) -> SocketOwnership {
    probe_pid_marker(path, process_is_running)
}

#[cfg(windows)]
pub(super) fn assert_socket(path: &Path) -> bool {
    discovered_marker_is_live(path, process_is_running)
}

fn discovered_marker_is_live(
    path: &Path,
    process_probe: impl FnOnce(u32) -> io::Result<bool>,
) -> bool {
    match probe_pid_marker(path, process_probe) {
        SocketOwnership::Live => true,
        SocketOwnership::Vacant => {
            drop(std::fs::remove_file(path));
            false
        },
        SocketOwnership::Unknown(_) => false,
    }
}

fn probe_pid_marker(
    path: &Path,
    process_probe: impl FnOnce(u32) -> io::Result<bool>,
) -> SocketOwnership {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return SocketOwnership::Vacant,
        Err(error) => return SocketOwnership::Unknown(error.to_string()),
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return SocketOwnership::Unknown("session PID marker is not a regular file".to_owned());
        },
        Ok(_) => {},
    }
    let mut raw = String::new();
    let read = std::fs::File::open(path).and_then(|file| file.take(33).read_to_string(&mut raw));
    if let Err(error) = read {
        return if error.kind() == io::ErrorKind::NotFound {
            SocketOwnership::Vacant
        } else {
            SocketOwnership::Unknown(error.to_string())
        };
    }
    let pid = match raw.trim().parse::<u32>() {
        Ok(pid) if pid != 0 && raw.len() <= 32 => pid,
        _ => return SocketOwnership::Unknown("session PID marker is invalid".to_owned()),
    };
    match process_probe(pid) {
        Ok(true) => SocketOwnership::Live,
        Ok(false) => SocketOwnership::Vacant,
        Err(error) => SocketOwnership::Unknown(error.to_string()),
    }
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // OpenProcess can also open a process that has terminated while another
    // handle still refers to it. Its exit status, not the handle alone, decides
    // whether the marker can describe a live server.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(false)
            } else {
                Err(error)
            };
        }
        let mut code = 0;
        let status = GetExitCodeProcess(handle, &mut code);
        let result = if status == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(code == STILL_ACTIVE as u32)
        };
        CloseHandle(handle);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_preserves_marker_when_process_query_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("protected");
        std::fs::write(&path, b"1234").unwrap();
        assert!(!discovered_marker_is_live(&path, |_| Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "process query failed"
        ))));
        assert_eq!(std::fs::read(path).unwrap(), b"1234");
    }

    #[test]
    fn discovery_cleans_only_a_confirmed_dead_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead");
        std::fs::write(&path, b"1234").unwrap();
        assert!(!discovered_marker_is_live(&path, |_| Ok(false)));
        assert!(!path.exists());
    }

    #[test]
    fn discovery_preserves_a_live_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live");
        std::fs::write(&path, b"1234").unwrap();
        assert!(discovered_marker_is_live(&path, |_| Ok(true)));
        assert_eq!(std::fs::read(path).unwrap(), b"1234");
    }

    #[test]
    fn live_pid_marker_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live");
        let body = b"1234\r\n";
        std::fs::write(&path, body).unwrap();
        assert_eq!(
            probe_pid_marker(&path, |pid| Ok(pid == 1234)),
            SocketOwnership::Live
        );
        assert_eq!(std::fs::read(path).unwrap(), body);
    }

    #[test]
    fn dead_pid_marker_is_classified_without_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dead");
        std::fs::write(&path, b"1234").unwrap();
        assert_eq!(
            probe_pid_marker(&path, |_| Ok(false)),
            SocketOwnership::Vacant
        );
        assert_eq!(std::fs::read(path).unwrap(), b"1234");
    }

    #[test]
    fn missing_marker_is_vacant_without_a_process_query() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_pid_marker(&dir.path().join("absent"), |_| panic!("no PID")),
            SocketOwnership::Vacant
        );
    }

    #[test]
    fn malformed_markers_remain_unknown_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown");
        for body in [
            b"".to_vec(),
            b"0".to_vec(),
            b"4294967296".to_vec(),
            b"12\n34".to_vec(),
            vec![b'1'; 33],
            vec![255],
        ] {
            std::fs::write(&path, &body).unwrap();
            assert!(matches!(
                probe_pid_marker(&path, |_| panic!("invalid PID")),
                SocketOwnership::Unknown(_)
            ));
            assert_eq!(std::fs::read(&path).unwrap(), body);
        }
    }

    #[test]
    fn inaccessible_process_is_unknown_and_never_removes_its_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("protected");
        std::fs::write(&path, b"1234").unwrap();
        assert!(matches!(
            probe_pid_marker(&path, |_| Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "denied"
            ))),
            SocketOwnership::Unknown(_)
        ));
        assert_eq!(std::fs::read(path).unwrap(), b"1234");
    }

    #[test]
    fn directory_marker_is_unknown_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe_pid_marker(dir.path(), |_| panic!("not a marker")),
            SocketOwnership::Unknown(_)
        ));
        assert!(dir.path().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn linked_marker_is_unknown_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, b"1234").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            probe_pid_marker(&link, |_| panic!("linked marker")),
            SocketOwnership::Unknown(_)
        ));
        assert!(link.is_symlink());
        assert_eq!(std::fs::read(target).unwrap(), b"1234");
    }

    #[cfg(windows)]
    #[test]
    fn native_probe_recognizes_its_running_process() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-live");
        std::fs::write(&path, std::process::id().to_string()).unwrap();
        assert_eq!(probe_socket_ownership(&path), SocketOwnership::Live);
        assert!(path.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn native_probe_recognizes_a_nonexistent_process_without_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("native-absent");
        std::fs::write(&path, u32::MAX.to_string()).unwrap();
        assert_eq!(probe_socket_ownership(&path), SocketOwnership::Vacant);
        assert!(path.is_file());
    }
}
