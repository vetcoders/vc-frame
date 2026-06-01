use async_trait::async_trait;
use std::{env, io, path::PathBuf};
use zellij_utils::input::command::RunCommand;

/// Check whether a candidate path refers to an executable file, considering
/// PATHEXT extensions on Windows (eg. `.exe`, `.cmd`).
fn find_executable(candidate: &std::path::Path) -> Option<PathBuf> {
    if candidate.exists() && candidate.is_file() {
        return Some(candidate.to_path_buf());
    }
    #[cfg(windows)]
    {
        if let Some(pathext) = env::var_os("PATHEXT") {
            let pathext = pathext.to_string_lossy();
            for ext in pathext.split(';') {
                let ext = ext.trim();
                if ext.is_empty() {
                    continue;
                }
                let mut with_ext = candidate.as_os_str().to_os_string();
                with_ext.push(ext);
                let with_ext_path = PathBuf::from(with_ext);
                if with_ext_path.exists() && with_ext_path.is_file() {
                    return Some(with_ext_path);
                }
            }
        }
    }
    None
}

/// Resolve a command to its absolute path, searching the working directory,
/// then PATH (and PATHEXT on Windows).
pub(crate) fn resolve_command(cmd: &RunCommand) -> Option<PathBuf> {
    let command = &cmd.command;
    match cmd.cwd.as_ref() {
        Some(cwd) => {
            if let Some(resolved) = find_executable(&cwd.join(command)) {
                return Some(resolved);
            }
        },
        None => {
            if let Some(resolved) = find_executable(command) {
                return Some(resolved);
            }
        },
    }
    if let Some(paths) = env::var_os("PATH") {
        for path in env::split_paths(&paths) {
            if let Some(resolved) = find_executable(&path.join(command)) {
                return Some(resolved);
            }
        }
    }
    None
}

#[cfg(not(windows))]
pub(crate) fn command_exists(cmd: &RunCommand) -> bool {
    resolve_command(cmd).is_some()
}

/// A null `AsyncReader` for held panes (produces EOF immediately).
pub(crate) struct NullAsyncReader;

// async fn in traits is not supported by rust, so dtolnay's excellent async_trait macro is being
// used. See https://smallcultfollowing.com/babysteps/blog/2019/10/26/async-fn-in-traits-are-hard/
#[async_trait]
pub trait AsyncReader: Send + Sync {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error>;
}

#[async_trait]
impl AsyncReader for NullAsyncReader {
    async fn read(&mut self, _buf: &mut [u8]) -> Result<usize, io::Error> {
        Ok(0)
    }
}
