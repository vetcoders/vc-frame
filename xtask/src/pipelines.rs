//! Composite pipelines for the build system.
//!
//! Defines multiple "pipelines" that run specific individual steps in sequence.
use crate::{WorkspaceMember, flags};
use crate::{build, clippy, format, metadata, test};
use anyhow::Context;
use std::path::{Path, PathBuf};
use xshell::{Shell, cmd};

/// Perform a default build.
///
/// Runs the following steps in sequence:
///
/// - format
/// - build
/// - test
/// - clippy
pub fn make(sh: &Shell, flags: flags::Make) -> anyhow::Result<()> {
    let err_context = || format!("failed to run pipeline 'make' with args {flags:?}");

    if flags.clean {
        crate::cargo()
            .and_then(|cargo| cmd!(sh, "{cargo} clean").run().map_err(anyhow::Error::new))
            .with_context(err_context)?;
    }

    format::format(sh, flags::Format { check: false })
        .and_then(|_| {
            build::build(
                sh,
                flags::Build {
                    release: flags.release,
                    no_plugins: false,
                    plugins_only: false,
                    no_web: flags.no_web,
                },
            )
        })
        .and_then(|_| {
            test::test(
                sh,
                flags::Test {
                    args: vec![],
                    no_web: flags.no_web,
                },
            )
        })
        .and_then(|_| clippy::clippy(sh, flags::Clippy {}))
        .with_context(err_context)
}

/// Generate a runnable executable.
///
/// Runs the following steps in sequence:
///
/// - [`build`](build::build) (release, plugins only), unless `--no-plugins`
/// - [`build`](build::build) (release, without plugins)
/// - [`manpage`](build::manpage)
/// - Copy the executable to [target file](flags::Install::destination)
pub fn install(sh: &Shell, flags: flags::Install) -> anyhow::Result<()> {
    let err_context = || format!("failed to run pipeline 'install' with args {flags:?}");

    if !flags.no_plugins {
        // Build and optimize plugins
        build::build(
            sh,
            flags::Build {
                release: true,
                no_plugins: false,
                plugins_only: true,
                no_web: flags.no_web,
            },
        )
        .with_context(err_context)?;
    }

    // Build the main executable
    build::build(
        sh,
        flags::Build {
            release: true,
            no_plugins: true,
            plugins_only: false,
            no_web: flags.no_web,
        },
    )
    .and_then(|_| {
        // Generate man page
        build::manpage(sh)
    })
    .with_context(err_context)?;

    // Copy binary to destination
    let destination = if flags.destination.is_absolute() {
        flags.destination.clone()
    } else {
        std::env::current_dir()
            .context("Can't determine current working directory")?
            .join(&flags.destination)
    };
    sh.change_dir(crate::project_root());
    let source = crate::project_root().join("target/release/vc-frame");
    install_binary(sh, &source, &destination).with_context(err_context)?;

    Ok(())
}

fn install_binary(_sh: &Shell, source: &Path, destination: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        install_binary_unix(source, destination, |staged| {
            // On macOS (Apple Silicon especially), `cargo build --release` with
            // `strip = true` produces a stripped, ad-hoc "linker-signed" Mach-O.
            // Sign the staged inode before publication so neither existing
            // servers nor a concurrent new launch can observe a half-signed
            // executable.
            #[cfg(target_os = "macos")]
            cmd!(_sh, "codesign --force --sign - {staged}")
                .run()
                .context("failed to sign staged vc-frame binary")?;

            #[cfg(not(target_os = "macos"))]
            let _ = staged;
            Ok(())
        })
    }

    #[cfg(not(unix))]
    {
        // Windows keeps the previous copy fallback: it is neither an atomic
        // replacement nor guaranteed to overwrite a currently running binary.
        let destination = effective_install_destination(source, destination)?;
        _sh.copy_file(source, destination.path())
            .context("failed to install vc-frame binary")
    }
}

#[cfg(unix)]
struct ResolvedInstallSource(PathBuf);

#[cfg(unix)]
impl ResolvedInstallSource {
    fn resolve(source: &Path) -> anyhow::Result<Self> {
        let source = source.canonicalize().with_context(|| {
            format!(
                "failed to resolve vc-frame install source {}",
                source.display()
            )
        })?;
        let metadata = source.metadata().with_context(|| {
            format!(
                "failed to inspect vc-frame install source {}",
                source.display()
            )
        })?;
        if !metadata.is_file() {
            anyhow::bail!(
                "vc-frame install source is not a regular file: {}",
                source.display()
            );
        }
        Ok(Self(source))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

struct ResolvedInstallDestination {
    path: PathBuf,
    #[cfg(unix)]
    parent: PathBuf,
}

impl ResolvedInstallDestination {
    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn parent(&self) -> &Path {
        &self.parent
    }
}

/// Resolve the install contract without creating directories:
/// existing directories receive the source file name, existing final symlinks
/// are preserved while their regular-file target is replaced, and the final
/// parent must already exist.
fn effective_install_destination(
    source: &Path,
    destination: &Path,
) -> anyhow::Result<ResolvedInstallDestination> {
    let requested_destination = match std::fs::metadata(destination) {
        Ok(metadata) if metadata.is_dir() => destination.join(
            source
                .file_name()
                .context("vc-frame source path has no file name")?,
        ),
        Ok(_) => destination.to_path_buf(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => destination.to_path_buf(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect vc-frame install destination {}",
                    destination.display()
                )
            });
        },
    };

    let destination = match std::fs::symlink_metadata(&requested_destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            requested_destination.canonicalize().with_context(|| {
                format!(
                    "failed to resolve existing vc-frame install destination symlink {}",
                    requested_destination.display()
                )
            })?
        },
        Ok(metadata) if metadata.is_dir() => anyhow::bail!(
            "resolved vc-frame install destination is a directory: {}",
            requested_destination.display()
        ),
        Ok(metadata) if !metadata.is_file() => anyhow::bail!(
            "vc-frame install destination is not a regular file: {}",
            requested_destination.display()
        ),
        Ok(_) => requested_destination,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => requested_destination,
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to inspect resolved vc-frame install destination {}",
                    requested_destination.display()
                )
            });
        },
    };

    let file_name = destination
        .file_name()
        .context("vc-frame install destination has no file name")?;
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = parent.canonicalize().with_context(|| {
        format!(
            "vc-frame install destination parent does not exist or cannot be resolved: {}",
            parent.display()
        )
    })?;
    if !parent
        .metadata()
        .with_context(|| {
            format!(
                "failed to inspect vc-frame install destination parent {}",
                parent.display()
            )
        })?
        .is_dir()
    {
        anyhow::bail!(
            "vc-frame install destination parent is not a directory: {}",
            parent.display()
        );
    }

    let destination = parent.join(file_name);
    match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_symlink() => anyhow::bail!(
            "vc-frame install destination changed to a symlink while it was being resolved: {}",
            destination.display()
        ),
        Ok(metadata) if !metadata.is_file() => anyhow::bail!(
            "vc-frame install destination is not a regular file: {}",
            destination.display()
        ),
        Ok(_) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "failed to revalidate vc-frame install destination {}",
                    destination.display()
                )
            });
        },
    }
    Ok(ResolvedInstallDestination {
        path: destination,
        #[cfg(unix)]
        parent,
    })
}

#[cfg(unix)]
fn install_binary_unix(
    source: &Path,
    destination: &Path,
    sign_staged: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let destination = effective_install_destination(source, destination)?;
    let source = ResolvedInstallSource::resolve(source)?;
    let mut staged = StagedInstall::reserve(&destination)?;
    let install_result = (|| {
        staged.copy_source(&source)?;
        sign_staged(staged.path())?;
        // `codesign` must have modified the inode we reserved, rather than
        // swapping the path to a different file before publication.
        staged.revalidate_path_identity()?;
        staged.sync_file()?;
        staged.publish(&destination)
    })();

    match install_result {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Err(cleanup_error) = staged.cleanup() {
                return Err(error.context(format!(
                    "also failed to clean staged vc-frame install {}: {cleanup_error:#}",
                    staged.path().display()
                )));
            }
            Err(error)
        },
    }
}

#[cfg(unix)]
struct StagedInstall {
    path: PathBuf,
    file: std::fs::File,
    parent_path: PathBuf,
    parent_directory: std::fs::File,
    active: bool,
}

#[cfg(unix)]
impl StagedInstall {
    fn reserve(destination: &ResolvedInstallDestination) -> anyhow::Result<Self> {
        use std::ffi::OsString;
        use std::fs::OpenOptions;
        use std::time::{SystemTime, UNIX_EPOCH};

        let parent_path = destination.parent().to_path_buf();
        let file_name = destination
            .path()
            .file_name()
            .context("vc-frame install destination has no file name")?;
        let parent_directory = std::fs::File::open(&parent_path).with_context(|| {
            format!("failed to open install directory {}", parent_path.display())
        })?;
        if !parent_directory
            .metadata()
            .context("failed to inspect open install directory")?
            .is_dir()
        {
            anyhow::bail!(
                "vc-frame install destination parent is not a directory: {}",
                parent_path.display()
            );
        }
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_nanos();

        for attempt in 0..32 {
            let mut staged_name = OsString::from(".");
            staged_name.push(file_name);
            staged_name.push(format!(".install-{}-{nonce}-{attempt}", std::process::id()));
            let path = parent_path.join(staged_name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file,
                        parent_path,
                        parent_directory,
                        active: true,
                    });
                },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("failed to reserve {}", path.display()));
                },
            }
        }

        anyhow::bail!(
            "failed to reserve a staged vc-frame install path beside {}",
            destination.path().display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn copy_source(&mut self, source: &ResolvedInstallSource) -> anyhow::Result<()> {
        let mut source_file = std::fs::File::open(source.path()).with_context(|| {
            format!(
                "failed to open vc-frame install source {}",
                source.path().display()
            )
        })?;
        let source_metadata = source_file.metadata().with_context(|| {
            format!(
                "failed to inspect vc-frame install source {}",
                source.path().display()
            )
        })?;
        if !source_metadata.is_file() {
            anyhow::bail!(
                "vc-frame install source is not a regular file: {}",
                source.path().display()
            );
        }
        std::io::copy(&mut source_file, &mut self.file).with_context(|| {
            format!(
                "failed to copy vc-frame install source {}",
                source.path().display()
            )
        })?;
        self.file
            .set_permissions(source_metadata.permissions())
            .with_context(|| format!("failed to set permissions on {}", self.path.display()))?;
        self.sync_file()
    }

    fn sync_file(&self) -> anyhow::Result<()> {
        self.file
            .sync_all()
            .with_context(|| format!("failed to sync {}", self.path.display()))
    }

    fn revalidate_path_identity(&self) -> anyhow::Result<()> {
        match self.path_matches_open_file()? {
            Some(true) => Ok(()),
            Some(false) => anyhow::bail!(
                "staged vc-frame install path changed inode before publication: {}",
                self.path.display()
            ),
            None => anyhow::bail!(
                "staged vc-frame install path disappeared before publication: {}",
                self.path.display()
            ),
        }
    }

    fn path_matches_open_file(&self) -> anyhow::Result<Option<bool>> {
        use std::os::unix::fs::MetadataExt;

        let path_metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to inspect staged install {}", self.path.display())
                });
            },
        };
        if !path_metadata.file_type().is_file() {
            return Ok(Some(false));
        }
        let file_metadata = self
            .file
            .metadata()
            .context("failed to inspect open staged install file")?;
        Ok(Some(
            path_metadata.dev() == file_metadata.dev()
                && path_metadata.ino() == file_metadata.ino(),
        ))
    }

    fn parent_path_matches_open_directory(&self) -> anyhow::Result<bool> {
        use std::os::unix::fs::MetadataExt;

        let path_metadata = match std::fs::symlink_metadata(&self.parent_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect staged install directory {}",
                        self.parent_path.display()
                    )
                });
            },
        };
        let directory_metadata = self
            .parent_directory
            .metadata()
            .context("failed to inspect open staged install directory")?;
        Ok(path_metadata.file_type().is_dir()
            && path_metadata.dev() == directory_metadata.dev()
            && path_metadata.ino() == directory_metadata.ino())
    }

    fn publish(&mut self, destination: &ResolvedInstallDestination) -> anyhow::Result<()> {
        self.revalidate_path_identity()?;
        if !self.parent_path_matches_open_directory()? {
            anyhow::bail!(
                "vc-frame install destination directory changed identity before publication: {}",
                self.parent_path.display()
            );
        }
        std::fs::rename(&self.path, destination.path()).with_context(|| {
            format!(
                "failed to atomically publish {} over {}",
                self.path.display(),
                destination.path().display()
            )
        })?;
        self.active = false;
        self.parent_directory.sync_all().with_context(|| {
            format!(
                "failed to sync install directory {}",
                self.parent_path.display()
            )
        })
    }

    fn cleanup(&mut self) -> anyhow::Result<()> {
        if !self.active {
            return Ok(());
        }
        let Some(path_matches) = self.path_matches_open_file()? else {
            self.active = false;
            return Ok(());
        };
        if !path_matches || !self.parent_path_matches_open_directory()? {
            anyhow::bail!(
                "refusing to remove staged install after its path or parent changed identity: {}",
                self.path.display()
            );
        }
        std::fs::remove_file(&self.path)
            .with_context(|| format!("failed to remove staged install {}", self.path.display()))?;
        self.active = false;
        self.parent_directory.sync_all().with_context(|| {
            format!(
                "failed to sync install directory {} after cleanup",
                self.parent_path.display()
            )
        })
    }
}

#[cfg(unix)]
impl Drop for StagedInstall {
    fn drop(&mut self) {
        if self.active
            && self.path_matches_open_file().ok() == Some(Some(true))
            && self.parent_path_matches_open_directory().ok() == Some(true)
            && std::fs::remove_file(&self.path).is_ok()
        {
            let _ = self.parent_directory.sync_all();
        }
    }
}

#[cfg(all(test, unix))]
mod install_tests {
    use super::*;
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_directory() -> PathBuf {
        static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("test clock")
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..32 {
            let directory = std::env::temp_dir().join(format!(
                "vc-frame-atomic-install-{}-{nonce}-{sequence}-{attempt}",
                std::process::id()
            ));
            match std::fs::create_dir(&directory) {
                Ok(()) => return directory,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
                Err(error) => panic!("create test directory: {error}"),
            }
        }
        panic!("could not reserve unique atomic-install test directory")
    }

    fn install_for_test(source: &Path, destination: &Path) -> anyhow::Result<()> {
        install_binary_unix(source, destination, |_| Ok(()))
    }

    fn staged_entries(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .expect("read test directory")
            .filter_map(|entry| {
                let path = entry.expect("read test entry").path();
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".install-"))
                    .then_some(path)
            })
            .collect()
    }

    #[test]
    fn atomic_install_keeps_the_old_inode_alive_for_running_processes() {
        use std::os::unix::fs::PermissionsExt;

        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751))
            .expect("set source permissions");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let mut running_process_view =
            std::fs::File::open(&destination).expect("open old runtime inode");

        install_for_test(&source, &destination).expect("atomic install");

        let mut old_runtime = String::new();
        running_process_view
            .read_to_string(&mut old_runtime)
            .expect("read old inode");
        assert_eq!(old_runtime, "old runtime");
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read published runtime"),
            "new runtime"
        );
        assert_eq!(
            std::fs::metadata(&destination)
                .expect("read published permissions")
                .permissions()
                .mode()
                & 0o777,
            0o751
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn install_preserves_an_existing_destination_symlink() {
        use std::os::unix::fs::symlink;

        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let target = directory.join("versioned-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&target, b"old runtime").expect("write target");
        symlink(&target, &destination).expect("create destination symlink");

        install_for_test(&source, &destination).expect("atomic install");

        assert!(
            destination
                .symlink_metadata()
                .expect("symlink metadata")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read published target"),
            "new runtime"
        );

        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn dangling_destination_symlink_fails_without_replacing_the_link() {
        use std::os::unix::fs::symlink;

        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let missing_target = directory.join("missing-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        symlink(&missing_target, &destination).expect("create dangling destination symlink");

        let error =
            install_for_test(&source, &destination).expect_err("dangling symlink must fail");

        assert!(
            error
                .to_string()
                .contains("failed to resolve existing vc-frame install destination symlink")
        );
        assert!(
            destination
                .symlink_metadata()
                .expect("read dangling symlink")
                .file_type()
                .is_symlink()
        );
        assert!(!missing_target.exists());
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn install_to_directory_uses_the_source_file_name() {
        let directory = test_directory();
        let build_directory = directory.join("build");
        let install_directory = directory.join("bin");
        std::fs::create_dir(&build_directory).expect("create build directory");
        std::fs::create_dir(&install_directory).expect("create install directory");
        let source = build_directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");

        install_for_test(&source, &install_directory).expect("directory install");

        assert_eq!(
            std::fs::read_to_string(install_directory.join("vc-frame"))
                .expect("read directory install"),
            "new runtime"
        );
        assert!(staged_entries(&install_directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn missing_destination_parent_is_an_error_and_is_not_created() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let missing_parent = directory.join("missing");
        let destination = missing_parent.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");

        let error = install_for_test(&source, &destination).expect_err("missing parent must fail");

        assert!(
            error
                .to_string()
                .contains("install destination parent does not exist")
        );
        assert!(!missing_parent.exists());
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn signing_failure_preserves_destination_and_removes_the_stage() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");

        let error = install_binary_unix(&source, &destination, |_| {
            anyhow::bail!("injected signing failure")
        })
        .expect_err("signing failure must fail install");

        assert!(error.to_string().contains("injected signing failure"));
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read preserved destination"),
            "old runtime"
        );
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn staged_path_replacement_is_rejected_without_deleting_the_replacement() {
        let directory = test_directory();
        let destination = directory.join("vc-frame");
        let destination = effective_install_destination(Path::new("vc-frame"), &destination)
            .expect("resolve destination");
        let mut staged = StagedInstall::reserve(&destination).expect("reserve stage");
        let staged_path = staged.path().to_path_buf();
        std::fs::remove_file(&staged_path).expect("unlink owned stage");
        std::fs::write(&staged_path, b"replacement").expect("write replacement");

        let identity_error = staged
            .revalidate_path_identity()
            .expect_err("replacement inode must fail revalidation");
        assert!(identity_error.to_string().contains("changed inode"));
        let cleanup_error = staged
            .cleanup()
            .expect_err("cleanup must not delete an unowned replacement");
        assert!(cleanup_error.to_string().contains("refusing to remove"));
        drop(staged);
        assert_eq!(
            std::fs::read_to_string(&staged_path).expect("read retained replacement"),
            "replacement"
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}

/// Run vc-frame debug build.
pub fn run(sh: &Shell, mut flags: flags::Run) -> anyhow::Result<()> {
    let err_context =
        |flags: &flags::Run| format!("failed to run pipeline 'run' with args {:?}", flags);

    if flags.quick_run {
        if flags.data_dir.is_some() {
            eprintln!("cannot use '--data-dir' and '--quick-run' at the same time!");
            std::process::exit(1);
        }
        flags.data_dir.replace(crate::asset_dir());
    }

    let profile = if flags.disable_deps_optimize {
        "dev"
    } else {
        "dev-opt"
    };

    if let Some(ref data_dir) = flags.data_dir {
        let data_dir = sh.current_dir().join(data_dir);
        let features = if flags.no_web {
            "disable_automatic_asset_installation"
        } else {
            "disable_automatic_asset_installation web_server_capability"
        };

        crate::cargo()
            .and_then(|cargo| {
                cmd!(sh, "{cargo} run")
                    .args(["--package", "zellij"])
                    .args(["--bin", "vc-frame"])
                    .arg("--no-default-features")
                    .args(["--features", features])
                    .args(["--profile", profile])
                    .args(["--", "--data-dir", &format!("{}", data_dir.display())])
                    .args(&flags.args)
                    .run()
                    .map_err(anyhow::Error::new)
            })
            .with_context(|| err_context(&flags))
    } else {
        build::build(
            sh,
            flags::Build {
                release: false,
                no_plugins: false,
                plugins_only: true,
                no_web: flags.no_web,
            },
        )
        .and_then(|_| crate::cargo())
        .and_then(|cargo| {
            if flags.no_web {
                // Use dynamic metadata approach to get the correct features
                match metadata::get_no_web_features(sh, ".")
                    .context("Failed to check web features for main crate")?
                {
                    Some(features) => {
                        let mut cmd = cmd!(sh, "{cargo} run")
                            .args(["--package", "zellij"])
                            .args(["--bin", "vc-frame"])
                            .args(["--no-default-features"]);

                        if !features.is_empty() {
                            cmd = cmd.args(["--features", &features]);
                        }

                        cmd.args(["--profile", profile])
                            .args(["--"])
                            .args(&flags.args)
                            .run()
                            .map_err(anyhow::Error::new)
                    },
                    None => {
                        // Main crate doesn't have web_server_capability, run normally
                        cmd!(sh, "{cargo} run")
                            .args(["--package", "zellij"])
                            .args(["--bin", "vc-frame"])
                            .args(["--profile", profile])
                            .args(["--"])
                            .args(&flags.args)
                            .run()
                            .map_err(anyhow::Error::new)
                    },
                }
            } else {
                cmd!(sh, "{cargo} run")
                    .args(["--package", "zellij"])
                    .args(["--bin", "vc-frame"])
                    .args(["--profile", profile])
                    .args(["--"])
                    .args(&flags.args)
                    .run()
                    .map_err(anyhow::Error::new)
            }
        })
        .with_context(|| err_context(&flags))
    }
}

/// Bundle all distributable content to `target/dist`.
///
/// This includes the optimized vc-frame executable from the [`install`] pipeline, the man page, the
/// `.desktop` file and the application logo.
pub fn dist(sh: &Shell, _flags: flags::Dist) -> anyhow::Result<()> {
    let err_context = || "failed to run pipeline 'dist'";

    sh.change_dir(crate::project_root());
    if sh.path_exists("target/dist") {
        sh.remove_path("target/dist").with_context(err_context)?;
    }
    sh.create_dir("target/dist")
        .map_err(anyhow::Error::new)
        .and_then(|_| {
            install(
                sh,
                flags::Install {
                    destination: crate::project_root().join("./target/dist/vc-frame"),
                    no_plugins: false,
                    no_web: false,
                },
            )
        })
        .with_context(err_context)?;

    sh.create_dir("target/dist/man")
        .and_then(|_| sh.copy_file("assets/man/vc-frame.1", "target/dist/man/vc-frame.1"))
        .and_then(|_| sh.copy_file("assets/vc-frame.desktop", "target/dist/vc-frame.desktop"))
        .and_then(|_| sh.copy_file("assets/logo.png", "target/dist/logo.png"))
        .with_context(err_context)
}

/// Actions for the user to choose from to resolve publishing errors/conflicts.
enum UserAction {
    Retry,
    Abort,
    Ignore,
}

/// Make a zellij release and publish all crates.
pub fn publish(sh: &Shell, flags: flags::Publish) -> anyhow::Result<()> {
    let err_context = "failed to publish zellij";

    // Process flags
    let dry_run = if flags.dry_run {
        Some("--dry-run")
    } else {
        None
    };
    let remote = flags.git_remote.unwrap_or("origin".into());
    let registry = if let Some(ref registry) = flags.cargo_registry {
        Some(format!(
            "--registry={}",
            registry
                .clone()
                .into_string()
                .map_err(|registry| anyhow::Error::msg(format!(
                    "failed to convert '{:?}' to valid registry name",
                    registry
                )))
                .context(err_context)?
        ))
    } else {
        None
    };
    let registry = registry.as_ref();
    if flags.no_push && flags.cargo_registry.is_none() {
        anyhow::bail!("flag '--no-push' can only be used with '--cargo-registry'");
    }

    sh.change_dir(crate::project_root());
    let cargo = crate::cargo().context(err_context)?;
    let project_dir = crate::project_root();
    let manifest = sh
        .read_file(project_dir.join("Cargo.toml"))
        .context(err_context)?
        .parse::<toml::Value>()
        .context(err_context)?;
    // Version of the core crate
    let version = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("package"))
        .and_then(|package| package["version"].as_str())
        .context("failed to read package version from manifest")
        .context(err_context)?;

    let mut skip_build = false;
    if cmd!(sh, "git tag -l")
        .read()
        .context(err_context)?
        .contains(version)
    {
        println!();
        println!("Git tag 'v{version}' is already present.");
        println!("If this is a mistake, delete it with: git tag -d 'v{version}'");
        println!("Skip build phase and continue to publish? [y/n]");

        let stdin = std::io::stdin();
        loop {
            let mut buffer = String::new();
            stdin.read_line(&mut buffer).context(err_context)?;
            match buffer.trim_end() {
                "y" | "Y" => {
                    skip_build = true;
                    break;
                },
                "n" | "N" => {
                    skip_build = false;
                    break;
                },
                _ => {
                    println!(" --> Unknown input '{buffer}', ignoring...");
                    println!();
                    println!("Skip build phase and continue to publish? [y/n]");
                },
            }
        }
    }

    if !skip_build {
        // Clean project
        cmd!(sh, "{cargo} clean").run().context(err_context)?;

        // Build plugins
        build::build(
            sh,
            flags::Build {
                release: true,
                no_plugins: false,
                plugins_only: true,
                no_web: false,
            },
        )
        .context(err_context)?;

        // Update default config
        sh.copy_file(
            project_dir
                .join("zellij-utils")
                .join("assets")
                .join("config")
                .join("default.kdl"),
            project_dir.join("example").join("default.kdl"),
        )
        .context(err_context)?;

        // Commit changes
        cmd!(sh, "git commit -aem")
            .arg(format!("chore(release): v{}", version))
            .run()
            .context(err_context)?;

        // Tag release
        cmd!(sh, "git tag --annotate --message")
            .arg(format!("Version {}", version))
            .arg(format!("v{}", version))
            .run()
            .context(err_context)?;
    }

    let closure = || -> anyhow::Result<()> {
        // Push commit and tag
        if flags.dry_run {
            println!("Skipping push due to dry-run");
        } else if flags.no_push {
            println!("Skipping push due to no-push");
        } else {
            let branch = cmd!(sh, "git rev-parse --abbrev-ref HEAD")
                .read()
                .context(err_context)?;
            cmd!(sh, "git push --atomic {remote} {branch} v{version}")
                .run()
                .context(err_context)?;
        }

        // Publish all the crates
        for WorkspaceMember { crate_name, .. } in crate::workspace_members().iter() {
            if crate_name.contains("plugin") || crate_name.contains("xtask") {
                continue;
            }

            let _pd = sh.push_dir(project_dir.join(crate_name));
            loop {
                let msg = format!(">> Publishing '{crate_name}'");
                crate::status(&msg);
                println!("{}", msg);

                let more_args = match *crate_name {
                    // This is needed for zellij to pick up the plugins from the assets included in
                    // the released zellij-utils binary
                    "." => Some("--no-default-features"),
                    _ => None,
                };

                match cmd!(
                    sh,
                    "{cargo} publish --locked {registry...} {more_args...} {dry_run...}"
                )
                .run()
                .context(err_context)
                {
                    Err(err) => {
                        println!();
                        println!("Publishing crate '{crate_name}' failed with error:");
                        println!("{:?}", err);
                        println!();
                        println!("Please choose what to do: [r]etry/[a]bort/[i]gnore");

                        let stdin = std::io::stdin();
                        let action;

                        loop {
                            let mut buffer = String::new();
                            stdin.read_line(&mut buffer).context(err_context)?;
                            match buffer.trim_end() {
                                "r" | "R" => {
                                    action = UserAction::Retry;
                                    break;
                                },
                                "a" | "A" => {
                                    action = UserAction::Abort;
                                    break;
                                },
                                "i" | "I" => {
                                    action = UserAction::Ignore;
                                    break;
                                },
                                _ => {
                                    println!(" --> Unknown input '{buffer}', ignoring...");
                                    println!();
                                    println!("Please choose what to do: [r]etry/[a]bort/[i]gnore");
                                },
                            }
                        }

                        match action {
                            UserAction::Retry => continue,
                            UserAction::Ignore => break,
                            UserAction::Abort => {
                                eprintln!("Aborting publish for crate '{crate_name}'");
                                return Err::<(), _>(err);
                            },
                        }
                    },
                    _ => {
                        // publish successful, continue to next crate
                        break;
                    },
                }
            }
        }

        println!();
        println!(" +-----------------------------------------------+");
        println!(" | PRAISE THE DEVS, WE HAVE A NEW ZELLIJ RELEASE |");
        println!(" +-----------------------------------------------+");
        Ok(())
    };

    // We run this in a closure so that a failure in any of the commands doesn't abort the whole
    // program. When dry-running we need to undo the release commit first!
    let result = closure();

    if flags.dry_run && !skip_build {
        cmd!(sh, "git reset --hard HEAD~1")
            .run()
            .context(err_context)?;
    }

    result
}
