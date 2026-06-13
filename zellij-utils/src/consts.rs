//! Zellij program-wide constants.

use crate::home::find_default_config_dir;
use directories::ProjectDirs;
use include_dir::{Dir, include_dir};
use lazy_static::lazy_static;
use std::{
    path::{Path, PathBuf},
    sync::OnceLock,
};
use uuid::Uuid;

pub const ZELLIJ_CONFIG_FILE_ENV: &str = "ZELLIJ_CONFIG_FILE";
pub const ZELLIJ_CONFIG_DIR_ENV: &str = "ZELLIJ_CONFIG_DIR";
pub const ZELLIJ_LAYOUT_DIR_ENV: &str = "ZELLIJ_LAYOUT_DIR";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_SCROLL_BUFFER_SIZE: usize = 10_000;
pub static SCROLL_BUFFER_SIZE: OnceLock<usize> = OnceLock::new();
pub static DEBUG_MODE: OnceLock<bool> = OnceLock::new();

#[cfg(not(windows))]
pub const SYSTEM_DEFAULT_CONFIG_DIR: &str = "/etc/zellij";
#[cfg(windows)]
pub const SYSTEM_DEFAULT_CONFIG_DIR: &str = "C:\\ProgramData\\Zellij";
pub const SYSTEM_DEFAULT_DATA_DIR_PREFIX: &str = system_default_data_dir();

pub static ZELLIJ_DEFAULT_THEMES: Dir = include_dir!("$CARGO_MANIFEST_DIR/assets/themes");

pub const CLIENT_SERVER_CONTRACT_VERSION: usize = 1;

const VC_FRAME_PROJECT_QUALIFIER: &str = "io";
const VC_FRAME_PROJECT_ORGANIZATION: &str = "VetCoders";
const VC_FRAME_PROJECT_APPLICATION: &str = "vc-frame";
const LEGACY_ZELLIJ_PROJECT_QUALIFIER: &str = "org";
const LEGACY_ZELLIJ_PROJECT_ORGANIZATION: &str = concat!("Zellij ", "Contributors");
const LEGACY_ZELLIJ_PROJECT_APPLICATION: &str = "Zellij";

pub fn session_info_cache_file_name(session_name: &str) -> PathBuf {
    session_info_folder_for_session(session_name).join("session-metadata.kdl")
}

pub fn session_layout_cache_file_name(session_name: &str) -> PathBuf {
    session_info_folder_for_session(session_name).join("session-layout.kdl")
}

pub fn session_info_folder_for_session(session_name: &str) -> PathBuf {
    ZELLIJ_SESSION_INFO_CACHE_DIR.join(session_name)
}

pub fn create_config_and_cache_folders() {
    migrate_legacy_project_dirs();

    if let Err(e) = std::fs::create_dir_all(ZELLIJ_CACHE_DIR.as_path()) {
        log::error!("Failed to create cache dir: {:?}", e);
    }
    if let Some(config_dir) = find_default_config_dir()
        && let Err(e) = std::fs::create_dir_all(config_dir.as_path())
    {
        log::error!("Failed to create config dir: {:?}", e);
    }
    // while session_info is a child of cache currently, it won't necessarily always be this way,
    // and so it's explicitly created here
    if let Err(e) = std::fs::create_dir_all(ZELLIJ_SESSION_INFO_CACHE_DIR.as_path()) {
        log::error!("Failed to create session_info cache dir: {:?}", e);
    }
    prune_empty_session_info_folders();
}

fn vc_frame_project_dirs() -> ProjectDirs {
    ProjectDirs::from(
        VC_FRAME_PROJECT_QUALIFIER,
        VC_FRAME_PROJECT_ORGANIZATION,
        VC_FRAME_PROJECT_APPLICATION,
    )
    .unwrap()
}

fn legacy_zellij_project_dirs() -> ProjectDirs {
    if cfg!(windows) {
        ProjectDirs::from("", "", LEGACY_ZELLIJ_PROJECT_APPLICATION).unwrap()
    } else {
        ProjectDirs::from(
            LEGACY_ZELLIJ_PROJECT_QUALIFIER,
            LEGACY_ZELLIJ_PROJECT_ORGANIZATION,
            LEGACY_ZELLIJ_PROJECT_APPLICATION,
        )
        .unwrap()
    }
}

fn migrate_legacy_project_dirs() {
    let legacy_dirs = legacy_zellij_project_dirs();
    migrate_legacy_path(legacy_dirs.config_dir(), ZELLIJ_PROJ_DIR.config_dir());
    migrate_legacy_path(legacy_dirs.cache_dir(), ZELLIJ_PROJ_DIR.cache_dir());
    migrate_legacy_path(legacy_dirs.data_dir(), ZELLIJ_PROJ_DIR.data_dir());
    if let (Some(legacy_state_dir), Some(vc_frame_state_dir)) =
        (legacy_dirs.state_dir(), ZELLIJ_PROJ_DIR.state_dir())
    {
        migrate_legacy_path(legacy_state_dir, vc_frame_state_dir);
    }
}

fn migrate_legacy_path(legacy_path: &Path, vc_frame_path: &Path) {
    if let Err(e) = copy_path_if_target_absent(legacy_path, vc_frame_path) {
        log::debug!(
            "Failed to migrate legacy vc-frame path {:?} to {:?}: {:?}",
            legacy_path,
            vc_frame_path,
            e
        );
    }
}

fn copy_path_if_target_absent(source: &Path, target: &Path) -> std::io::Result<()> {
    if !source.exists() || target.exists() {
        return Ok(());
    }

    if source.is_dir() {
        copy_dir_recursively(source, target)
    } else {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, target).map(|_| ())
    }
}

fn copy_dir_recursively(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let target_path = target.join(entry.file_name());
        copy_path_if_target_absent(&source_path, &target_path)?;
    }
    Ok(())
}

fn prune_empty_session_info_folders() {
    let Ok(entries) = std::fs::read_dir(&*ZELLIJ_SESSION_INFO_CACHE_DIR) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let is_empty = std::fs::read_dir(&path)
            .ok()
            .is_some_and(|mut iter| iter.next().is_none());
        if is_empty
            && let Err(e) = std::fs::remove_dir(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            log::debug!("Failed to prune empty session folder {:?}: {:?}", path, e);
        }
    }
}

const fn system_default_data_dir() -> &'static str {
    if let Some(data_dir) = std::option_env!("PREFIX") {
        data_dir
    } else if cfg!(windows) {
        "C:\\ProgramData\\Zellij"
    } else {
        "/usr"
    }
}

lazy_static! {
    pub static ref CLIENT_SERVER_CONTRACT_DIR: String =
        format!("contract_version_{}", CLIENT_SERVER_CONTRACT_VERSION);
    pub static ref ZELLIJ_PROJ_DIR: ProjectDirs = vc_frame_project_dirs();
    pub static ref ZELLIJ_CACHE_DIR: PathBuf = ZELLIJ_PROJ_DIR.cache_dir().to_path_buf();
    pub static ref ZELLIJ_SESSION_CACHE_DIR: PathBuf = ZELLIJ_PROJ_DIR
        .cache_dir()
        .to_path_buf()
        .join(format!("{}", Uuid::new_v4()));
    pub static ref ZELLIJ_PLUGIN_PERMISSIONS_CACHE: PathBuf =
        ZELLIJ_CACHE_DIR.join("permissions.kdl");
    pub static ref ZELLIJ_SESSION_INFO_CACHE_DIR: PathBuf = ZELLIJ_CACHE_DIR
        .join(CLIENT_SERVER_CONTRACT_DIR.clone())
        .join("session_info");
    pub static ref ZELLIJ_PLUGIN_ARTIFACT_DIR: PathBuf = ZELLIJ_CACHE_DIR.join(VERSION);
    pub static ref ZELLIJ_SEEN_RELEASE_NOTES_CACHE_FILE: PathBuf =
        ZELLIJ_CACHE_DIR.join(VERSION).join("seen_release_notes");
}

pub const FEATURES: &[&str] = &[
    #[cfg(feature = "disable_automatic_asset_installation")]
    "disable_automatic_asset_installation",
];

#[cfg(not(target_family = "wasm"))]
pub use not_wasm::*;

#[cfg(not(target_family = "wasm"))]
mod not_wasm {
    use lazy_static::lazy_static;
    use std::collections::HashMap;
    use std::path::PathBuf;

    // Convenience macro to add plugins to the asset map (see `ASSET_MAP`)
    //
    // Plugins are taken from:
    //
    // - `zellij-utils/assets/plugins`: When building in release mode OR when the
    //   `plugins_from_target` feature IS NOT set
    // - `zellij-utils/../target/wasm32-wasip1/debug`: When building in debug mode AND the
    //   `plugins_from_target` feature IS set
    macro_rules! add_plugin {
        ($assets:expr_2021, $plugin:literal) => {
            $assets.insert(
                PathBuf::from("plugins").join($plugin),
                #[cfg(any(not(feature = "plugins_from_target"), not(debug_assertions)))]
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/assets/plugins/",
                    $plugin
                ))
                .to_vec(),
                #[cfg(all(feature = "plugins_from_target", debug_assertions))]
                include_bytes!(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../target/wasm32-wasip1/debug/",
                    $plugin
                ))
                .to_vec(),
            );
        };
    }

    lazy_static! {
        // Zellij asset map
        pub static ref ASSET_MAP: HashMap<PathBuf, Vec<u8>> = {
            let mut assets = std::collections::HashMap::new();
            add_plugin!(assets, "compact-bar.wasm");
            add_plugin!(assets, "status-bar.wasm");
            add_plugin!(assets, "tab-bar.wasm");
            add_plugin!(assets, "strider.wasm");
            add_plugin!(assets, "session-manager.wasm");
            add_plugin!(assets, "configuration.wasm");
            add_plugin!(assets, "plugin-manager.wasm");
            add_plugin!(assets, "about.wasm");
            add_plugin!(assets, "share.wasm");
            add_plugin!(assets, "multiple-select.wasm");
            add_plugin!(assets, "layout-manager.wasm");
            add_plugin!(assets, "link.wasm");
            assets
        };
    }
}

/// Check if a filesystem entry is an IPC socket.
///
/// On Unix, this checks `FileTypeExt::is_socket()`. On non-Unix platforms,
/// this checks `is_file()` to detect marker files created by `ipc_bind()`
/// and `ipc_bind_async()` alongside kernel-level named pipes.
#[cfg(unix)]
pub fn is_ipc_socket(file_type: &std::fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    file_type.is_socket()
}

#[cfg(not(unix))]
pub fn is_ipc_socket(file_type: &std::fs::FileType) -> bool {
    file_type.is_file()
}

/// Connect to an IPC socket at the given path.
///
/// On Unix, this uses Unix domain sockets via `GenericFilePath`.
/// On Windows, this uses named pipes via `GenericNamespaced`.
#[cfg(unix)]
pub fn ipc_connect(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::{GenericFilePath, Stream as LocalSocketStream, prelude::*};
    let fs_name = path.to_fs_name::<GenericFilePath>()?;
    LocalSocketStream::connect(fs_name)
}

#[cfg(windows)]
pub fn ipc_connect(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::{GenericNamespaced, Stream as LocalSocketStream, prelude::*};
    let name = path.to_string_lossy().to_string();
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    LocalSocketStream::connect(ns_name)
}

/// Create an IPC listener bound to the given path.
///
/// On Unix, this uses Unix domain sockets via `GenericFilePath`.
/// On Windows, this uses named pipes via `GenericNamespaced` and creates
/// a marker file for session discovery.
#[cfg(unix)]
pub fn ipc_bind(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    let fs_name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new().name(fs_name).create_sync()
}

#[cfg(windows)]
pub fn ipc_bind(path: &std::path::Path) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, prelude::*};
    let name = path.to_string_lossy().to_string();
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(ns_name).create_sync()?;
    std::fs::write(path, std::process::id().to_string())?;
    Ok(listener)
}

/// Create an async (tokio) IPC listener bound to the given path.
///
/// On Unix, this uses Unix domain sockets via `GenericFilePath`.
/// On Windows, this uses named pipes via `GenericNamespaced` and creates
/// a marker file for session discovery.
#[cfg(unix)]
pub fn ipc_bind_async(
    path: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::{GenericFilePath, ListenerOptions, prelude::*};
    let fs_name = path.to_fs_name::<GenericFilePath>()?;
    ListenerOptions::new().name(fs_name).create_tokio()
}

#[cfg(windows)]
pub fn ipc_bind_async(
    path: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::tokio::Listener> {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, prelude::*};
    let name = path.to_string_lossy().to_string();
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    let listener = ListenerOptions::new().name(ns_name).create_tokio()?;
    std::fs::write(path, std::process::id().to_string())?;
    Ok(listener)
}

/// Connect to the reply pipe for a given IPC path (Windows only).
///
/// Uses `path-reply` as the named pipe for the server→client direction.
#[cfg(windows)]
pub fn ipc_connect_reply(
    path: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::Stream> {
    use interprocess::local_socket::{GenericNamespaced, Stream as LocalSocketStream, prelude::*};
    let name = format!("{}-reply", path.to_string_lossy());
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    LocalSocketStream::connect(ns_name)
}

/// Create an IPC listener for the reply pipe (Windows only).
///
/// Binds to `path-reply` as the named pipe for the server→client direction.
#[cfg(windows)]
pub fn ipc_bind_reply(
    path: &std::path::Path,
) -> std::io::Result<interprocess::local_socket::Listener> {
    use interprocess::local_socket::{GenericNamespaced, ListenerOptions, prelude::*};
    let name = format!("{}-reply", path.to_string_lossy());
    let ns_name = name.to_ns_name::<GenericNamespaced>()?;
    ListenerOptions::new().name(ns_name).create_sync()
}

#[cfg(unix)]
pub use unix_only::*;

#[cfg(unix)]
mod unix_only {
    use super::*;
    use crate::envs;
    pub use crate::shared::set_permissions;
    use lazy_static::lazy_static;
    use nix::unistd::Uid;
    use std::env::temp_dir;

    // Maximum length of a Unix domain socket path (from sockaddr_un.sun_path).
    // macOS (and other BSDs) use 104, Linux/Android/Solaris use 108.
    // The not(target_os = "macos") fallback of 108 is used for all other Unix
    // platforms — this is correct for Linux/Android/Solaris and only 4 bytes
    // over for BSDs, which would cause a slightly late error rather than a
    // missed one.
    #[cfg(target_os = "macos")]
    pub const ZELLIJ_SOCK_MAX_LENGTH: usize = 104;
    #[cfg(not(target_os = "macos"))]
    pub const ZELLIJ_SOCK_MAX_LENGTH: usize = 108;

    lazy_static! {
        static ref UID: Uid = Uid::current();
        pub static ref ZELLIJ_TMP_DIR: PathBuf = temp_dir().join(format!("vc-frame-{}", *UID));
        pub static ref ZELLIJ_TMP_LOG_DIR: PathBuf = ZELLIJ_TMP_DIR.join("vc-frame-log");
        pub static ref ZELLIJ_TMP_LOG_FILE: PathBuf = ZELLIJ_TMP_LOG_DIR.join("zellij.log");
        pub static ref ZELLIJ_SOCK_DIR: PathBuf = {
            let mut ipc_dir = envs::get_socket_dir().map_or_else(
                |_| {
                    ZELLIJ_PROJ_DIR
                        .runtime_dir()
                        .map_or_else(|| ZELLIJ_TMP_DIR.clone(), |p| p.to_owned())
                },
                PathBuf::from,
            );
            ipc_dir.push(CLIENT_SERVER_CONTRACT_DIR.clone());
            ipc_dir
        };
        pub static ref WEBSERVER_SOCKET_PATH: PathBuf = ZELLIJ_SOCK_DIR.join("web_server_bus");
    }
}

#[cfg(not(unix))]
pub use not_unix::*;

#[cfg(not(unix))]
mod not_unix {
    use super::*;
    use crate::envs;
    pub use crate::shared::set_permissions;
    #[cfg(windows)]
    use dunce;
    use lazy_static::lazy_static;
    use std::env::temp_dir;

    #[cfg(windows)]
    fn canonicalize_path(path: PathBuf) -> PathBuf {
        dunce::canonicalize(&path).unwrap_or(path)
    }

    #[cfg(not(windows))]
    fn canonicalize_path(path: PathBuf) -> PathBuf {
        path
    }

    pub const ZELLIJ_SOCK_MAX_LENGTH: usize = 256;

    lazy_static! {
        pub static ref ZELLIJ_TMP_DIR: PathBuf = {
            let tmp_dir = canonicalize_path(temp_dir());
            tmp_dir.join("vc-frame")
        };
        pub static ref ZELLIJ_TMP_LOG_DIR: PathBuf = ZELLIJ_TMP_DIR.join("vc-frame-log");
        pub static ref ZELLIJ_TMP_LOG_FILE: PathBuf = ZELLIJ_TMP_LOG_DIR.join("zellij.log");
        pub static ref ZELLIJ_SOCK_DIR: PathBuf = {
            let mut ipc_dir = canonicalize_path(envs::get_socket_dir().map_or_else(
                |_| {
                    ZELLIJ_PROJ_DIR
                        .runtime_dir()
                        .map_or_else(|| ZELLIJ_TMP_DIR.clone(), |p| p.to_owned())
                },
                PathBuf::from,
            ));
            ipc_dir.push(CLIENT_SERVER_CONTRACT_DIR.clone());
            ipc_dir
        };
        pub static ref WEBSERVER_SOCKET_PATH: PathBuf = ZELLIJ_SOCK_DIR.join("web_server_bus");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vc_frame_project_dirs_use_owned_namespace() {
        let cache_dir = vc_frame_project_dirs()
            .cache_dir()
            .to_string_lossy()
            .to_string();

        assert!(cache_dir.contains(VC_FRAME_PROJECT_APPLICATION));
        assert!(!cache_dir.contains(LEGACY_ZELLIJ_PROJECT_APPLICATION));

        #[cfg(target_os = "macos")]
        assert!(cache_dir.contains("io.VetCoders.vc-frame"));
    }

    #[test]
    fn copy_path_if_target_absent_copies_recursively_without_overwriting() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let source = tmp_dir.path().join("source");
        let target = tmp_dir.path().join("target");
        let nested = source.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("token.txt"), "legacy").unwrap();

        copy_path_if_target_absent(&source, &target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("nested").join("token.txt")).unwrap(),
            "legacy"
        );

        std::fs::write(target.join("nested").join("token.txt"), "owned").unwrap();
        copy_path_if_target_absent(&source, &target).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("nested").join("token.txt")).unwrap(),
            "owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_tmp_dir_uses_vc_frame_namespace() {
        assert!(
            ZELLIJ_TMP_DIR
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("vc-frame-")
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_tmp_dir_uses_vc_frame_namespace() {
        assert_eq!(
            ZELLIJ_TMP_DIR.file_name().unwrap().to_string_lossy(),
            "vc-frame"
        );
    }
}
