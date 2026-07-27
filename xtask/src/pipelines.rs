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
        let mut hooks = ProductionInstallHooks { shell: _sh };
        let outcome = install_binary_unix(source, destination, &mut hooks)?;
        outcome.report(destination);
        Ok(())
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
struct ProductionInstallHooks<'a> {
    shell: &'a Shell,
}

#[cfg(unix)]
impl InstallHooks for ProductionInstallHooks<'_> {
    fn prepare_staged(&mut self, staged: &Path) -> anyhow::Result<()> {
        // On macOS (Apple Silicon especially), `cargo build --release` with
        // `strip = true` produces a stripped, ad-hoc linker-signed Mach-O.
        // codesign may replace its input inode, so it works only inside the
        // private staging directory. The installer adopts that resulting inode
        // through the retained directory descriptor before verification.
        #[cfg(target_os = "macos")]
        {
            let shell = self.shell;
            cmd!(shell, "/usr/bin/codesign --force --sign - {staged}")
                .run()
                .context("failed to sign staged vc-frame binary")?;
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (self.shell, staged);
        Ok(())
    }

    fn verify_staged(&mut self, staged: &Path) -> anyhow::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let shell = self.shell;
            cmd!(shell, "/usr/bin/codesign --verify --strict {staged}")
                .run()
                .context("failed to verify staged vc-frame signature")?;
        }

        #[cfg(not(target_os = "macos"))]
        let _ = (self.shell, staged);
        Ok(())
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
trait InstallHooks {
    fn after_source_open(&mut self, _source: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn prepare_staged(&mut self, _staged: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn verify_staged(&mut self, _staged: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn before_publish(&mut self, _staged: &Path, _destination: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    fn sync_parent(&mut self, parent: &std::fs::File) -> std::io::Result<()> {
        parent.sync_all()
    }
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum InstallDurability {
    Confirmed,
    Uncertain,
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct InstallOutcome {
    durability: InstallDurability,
    warnings: Vec<String>,
}

#[cfg(unix)]
impl InstallOutcome {
    fn report(&self, destination: &Path) {
        if self.durability == InstallDurability::Uncertain {
            eprintln!(
                "warning: vc-frame is published at {}, but directory durability is uncertain",
                destination.display()
            );
        }
        for warning in &self.warnings {
            eprintln!("warning: {warning}");
        }
    }
}

#[cfg(unix)]
struct OpenedInstallSource {
    path: PathBuf,
    file: std::fs::File,
    mode: rustix::fs::Mode,
}

#[cfg(unix)]
impl OpenedInstallSource {
    fn open(source: &Path) -> anyhow::Result<Self> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};

        let file_name = source
            .file_name()
            .context("vc-frame install source has no file name")?;
        let parent = source
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .canonicalize()
            .with_context(|| {
                format!(
                    "failed to resolve vc-frame install source parent for {}",
                    source.display()
                )
            })?;
        let parent_directory = open_directory_no_follow(&parent).with_context(|| {
            format!(
                "failed to open vc-frame install source parent {}",
                parent.display()
            )
        })?;
        revalidate_absolute_path(&parent, &parent_directory, FileType::Directory)
            .context("vc-frame install source parent changed identity while opening it")?;

        let source_fd = rustix::fs::openat(
            &parent_directory,
            file_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| {
            format!(
                "failed to open vc-frame install source without following symlinks: {}",
                parent.join(file_name).display()
            )
        })?;
        let file = std::fs::File::from(source_fd);
        let source_stat =
            rustix::fs::fstat(&file).context("failed to inspect open vc-frame install source")?;
        if FileType::from_raw_mode(source_stat.st_mode) != FileType::RegularFile {
            anyhow::bail!(
                "vc-frame install source is not a regular file: {}",
                parent.join(file_name).display()
            );
        }
        let entry_stat =
            rustix::fs::statat(&parent_directory, file_name, AtFlags::SYMLINK_NOFOLLOW)
                .context("failed to revalidate vc-frame install source entry")?;
        if !same_file_identity(&source_stat, &entry_stat)
            || FileType::from_raw_mode(entry_stat.st_mode) != FileType::RegularFile
        {
            anyhow::bail!(
                "vc-frame install source changed identity while it was being opened: {}",
                parent.join(file_name).display()
            );
        }

        Ok(Self {
            path: parent.join(file_name),
            file,
            mode: Mode::from_raw_mode(source_stat.st_mode),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
enum DestinationEntryState {
    Missing,
    Existing(rustix::fs::Stat),
}

#[cfg(unix)]
struct StagedInstall {
    parent_path: PathBuf,
    parent_directory: std::fs::File,
    destination_name: std::ffi::OsString,
    expected_destination: DestinationEntryState,
    stage_name: std::ffi::OsString,
    stage_path: PathBuf,
    stage_directory: std::fs::File,
    payload_path: PathBuf,
    payload_file: Option<std::fs::File>,
    expected_payload_mode: Option<rustix::fs::Mode>,
    expected_owner: rustix::process::Uid,
    payload_active: bool,
    directory_active: bool,
}

#[cfg(unix)]
impl StagedInstall {
    fn reserve(destination: &ResolvedInstallDestination) -> anyhow::Result<Self> {
        use rustix::fs::{FileType, Mode, OFlags};

        let parent_path = destination.parent().to_path_buf();
        let destination_name = destination
            .path()
            .file_name()
            .context("vc-frame install destination has no file name")?
            .to_os_string();
        let parent_directory = open_directory_no_follow(&parent_path).with_context(|| {
            format!("failed to open install directory {}", parent_path.display())
        })?;
        revalidate_absolute_path(&parent_path, &parent_directory, FileType::Directory)
            .context("vc-frame install destination parent changed identity while opening it")?;
        let expected_destination = match statat_optional(&parent_directory, &destination_name)? {
            Some(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                DestinationEntryState::Existing(stat)
            },
            Some(_) => anyhow::bail!(
                "vc-frame install destination is not a regular file: {}",
                destination.path().display()
            ),
            None => DestinationEntryState::Missing,
        };

        // A random name plus mode 0700 prevents cross-user staging attacks and
        // accidental installer collisions. A malicious process with the same
        // effective uid remains inside the operator's trust boundary.
        for attempt in 0..32 {
            let stage_name = std::ffi::OsString::from(format!(
                ".vc-frame.install-{}-{attempt}",
                uuid::Uuid::new_v4().simple()
            ));
            match rustix::fs::mkdirat(&parent_directory, &stage_name, Mode::RWXU) {
                Ok(()) => {
                    let stage_fd = rustix::fs::openat(
                        &parent_directory,
                        &stage_name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .with_context(|| {
                        format!(
                            "failed to open newly created private install directory {}",
                            parent_path.join(&stage_name).display()
                        )
                    })?;
                    let stage_directory = std::fs::File::from(stage_fd);
                    let stage_path = parent_path.join(&stage_name);
                    let payload_path = stage_path.join("payload");
                    let mut staged = Self {
                        parent_path,
                        parent_directory,
                        destination_name,
                        expected_destination,
                        stage_name,
                        stage_path,
                        stage_directory,
                        payload_path,
                        payload_file: None,
                        expected_payload_mode: None,
                        expected_owner: rustix::process::geteuid(),
                        payload_active: false,
                        directory_active: true,
                    };
                    staged.harden_private_directory()?;
                    staged.create_payload()?;
                    return Ok(staged);
                },
                Err(rustix::io::Errno::EXIST) => {},
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to reserve private install directory beside {}",
                            destination.path().display()
                        )
                    });
                },
            }
        }

        anyhow::bail!(
            "failed to reserve a private staged vc-frame install directory beside {}",
            destination.path().display()
        )
    }

    fn path(&self) -> &Path {
        &self.payload_path
    }

    fn harden_private_directory(&mut self) -> anyhow::Result<()> {
        use rustix::fs::Mode;

        rustix::fs::fchmod(&self.stage_directory, Mode::RWXU).with_context(|| {
            format!(
                "failed to set private install directory mode on {}",
                self.stage_path.display()
            )
        })?;
        self.revalidate_stage_directory()
    }

    fn create_payload(&mut self) -> anyhow::Result<()> {
        use rustix::fs::{Mode, OFlags};

        let payload_fd = rustix::fs::openat(
            &self.stage_directory,
            "payload",
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RWXU,
        )
        .with_context(|| {
            format!(
                "failed to reserve private staged install payload {}",
                self.payload_path.display()
            )
        })?;
        self.payload_file = Some(std::fs::File::from(payload_fd));
        self.payload_active = true;
        self.revalidate_payload()
    }

    fn copy_source(&mut self, source: &mut OpenedInstallSource) -> anyhow::Result<()> {
        let payload_path = self.payload_path.clone();
        let payload = self.payload_file_mut()?;
        std::io::copy(&mut source.file, payload).with_context(|| {
            format!(
                "failed to copy vc-frame install source {}",
                source.path().display()
            )
        })?;
        rustix::fs::fchmod(payload, source.mode).with_context(|| {
            format!(
                "failed to set source permissions on {}",
                payload_path.display()
            )
        })?;
        self.expected_payload_mode = Some(source.mode);
        self.revalidate_payload()?;
        self.sync_payload()
    }

    fn adopt_payload_after_preparation(&mut self) -> anyhow::Result<()> {
        use rustix::fs::{Mode, OFlags};

        let payload_fd = rustix::fs::openat(
            &self.stage_directory,
            "payload",
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| {
            format!(
                "failed to adopt prepared staged install payload {}",
                self.payload_path.display()
            )
        })?;
        self.payload_file = Some(std::fs::File::from(payload_fd));
        self.revalidate_payload()
            .context("prepared staged install payload failed identity validation")
    }

    fn payload_file(&self) -> anyhow::Result<&std::fs::File> {
        self.payload_file
            .as_ref()
            .context("staged install payload file is not open")
    }

    fn payload_file_mut(&mut self) -> anyhow::Result<&mut std::fs::File> {
        self.payload_file
            .as_mut()
            .context("staged install payload file is not open")
    }

    fn sync_payload(&self) -> anyhow::Result<()> {
        self.payload_file()?
            .sync_all()
            .with_context(|| format!("failed to sync {}", self.payload_path.display()))
    }

    fn revalidate_payload(&self) -> anyhow::Result<()> {
        use rustix::fs::{FileType, Mode};

        match self.payload_entry_matches_open_file()? {
            Some(true) => {},
            Some(false) => anyhow::bail!(
                "staged vc-frame install payload changed identity before publication: {}",
                self.payload_path.display()
            ),
            None => anyhow::bail!(
                "staged vc-frame install payload disappeared before publication: {}",
                self.payload_path.display()
            ),
        };
        let payload_stat = rustix::fs::fstat(self.payload_file()?)
            .context("failed to inspect open staged install payload")?;
        let stage_stat = rustix::fs::fstat(&self.stage_directory)
            .context("failed to inspect open private install directory")?;
        if FileType::from_raw_mode(payload_stat.st_mode) != FileType::RegularFile
            || payload_stat.st_uid != self.expected_owner.as_raw()
            || payload_stat.st_dev != stage_stat.st_dev
            || payload_stat.st_nlink != 1
        {
            anyhow::bail!(
                "staged vc-frame install payload is not a private, same-filesystem regular file owned by the installer: {}",
                self.payload_path.display()
            );
        }
        if let Some(expected_mode) = self.expected_payload_mode
            && Mode::from_raw_mode(payload_stat.st_mode) != expected_mode
        {
            anyhow::bail!(
                "prepared staged vc-frame install payload changed mode before publication: {}",
                self.payload_path.display()
            );
        }
        Ok(())
    }

    fn payload_entry_matches_open_file(&self) -> anyhow::Result<Option<bool>> {
        use rustix::fs::FileType;

        let Some(entry_stat) = statat_optional(&self.stage_directory, "payload")? else {
            return Ok(None);
        };
        let payload_stat = rustix::fs::fstat(self.payload_file()?)
            .context("failed to inspect open staged install payload")?;
        Ok(Some(
            FileType::from_raw_mode(entry_stat.st_mode) == FileType::RegularFile
                && same_file_identity(&entry_stat, &payload_stat),
        ))
    }

    fn revalidate_stage_directory(&self) -> anyhow::Result<()> {
        use rustix::fs::{FileType, Mode};

        let stage_stat = rustix::fs::fstat(&self.stage_directory)
            .context("failed to inspect open private install directory")?;
        let parent_stat = rustix::fs::fstat(&self.parent_directory)
            .context("failed to inspect open install parent directory")?;
        if FileType::from_raw_mode(stage_stat.st_mode) != FileType::Directory
            || stage_stat.st_uid != self.expected_owner.as_raw()
            || Mode::from_raw_mode(stage_stat.st_mode) != Mode::RWXU
            || stage_stat.st_dev != parent_stat.st_dev
        {
            anyhow::bail!(
                "private vc-frame install directory is not mode 0700, installer-owned, and on the destination filesystem: {}",
                self.stage_path.display()
            );
        }
        if !self.stage_entry_matches_open_directory()? {
            anyhow::bail!(
                "private vc-frame install directory changed identity: {}",
                self.stage_path.display()
            );
        }
        Ok(())
    }

    fn stage_entry_matches_open_directory(&self) -> anyhow::Result<bool> {
        use rustix::fs::FileType;

        let Some(entry_stat) = statat_optional(&self.parent_directory, &self.stage_name)? else {
            return Ok(false);
        };
        let directory_stat = rustix::fs::fstat(&self.stage_directory)
            .context("failed to inspect open private install directory")?;
        Ok(
            FileType::from_raw_mode(entry_stat.st_mode) == FileType::Directory
                && same_file_identity(&entry_stat, &directory_stat),
        )
    }

    fn revalidate_destination_entry(&self) -> anyhow::Result<()> {
        use rustix::fs::FileType;

        let current = statat_optional(&self.parent_directory, &self.destination_name)?;
        let unchanged = match (&self.expected_destination, current) {
            (DestinationEntryState::Missing, None) => true,
            (DestinationEntryState::Existing(expected), Some(current)) => {
                FileType::from_raw_mode(current.st_mode) == FileType::RegularFile
                    && same_file_identity(expected, &current)
            },
            _ => false,
        };
        if !unchanged {
            anyhow::bail!(
                "vc-frame install destination changed identity before publication: {}",
                self.parent_path.join(&self.destination_name).display()
            )
        }
        Ok(())
    }

    fn publish(&mut self, hooks: &mut impl InstallHooks) -> anyhow::Result<InstallOutcome> {
        use rustix::fs::AtFlags;

        self.revalidate_payload()?;
        revalidate_absolute_path(
            &self.parent_path,
            &self.parent_directory,
            rustix::fs::FileType::Directory,
        )
        .context("vc-frame install destination directory changed identity before publication")?;
        self.revalidate_stage_directory()?;
        self.revalidate_destination_entry()?;

        rustix::fs::renameat(
            &self.stage_directory,
            "payload",
            &self.parent_directory,
            &self.destination_name,
        )
        .with_context(|| {
            format!(
                "failed to atomically publish {} over {}",
                self.payload_path.display(),
                self.parent_path.join(&self.destination_name).display()
            )
        })?;
        self.payload_active = false;

        let mut warnings = Vec::new();
        match self.stage_entry_matches_open_directory() {
            Ok(true) => match rustix::fs::unlinkat(
                &self.parent_directory,
                &self.stage_name,
                AtFlags::REMOVEDIR,
            ) {
                Ok(()) => self.directory_active = false,
                Err(error) => warnings.push(format!(
                    "vc-frame was published, but its empty private staging directory {} could not be removed: {error}",
                    self.stage_path.display()
                )),
            },
            Ok(false) => warnings.push(format!(
                "vc-frame was published, but its private staging directory {} changed identity before cleanup",
                self.stage_path.display()
            )),
            Err(error) => warnings.push(format!(
                "vc-frame was published, but its private staging directory {} could not be revalidated for cleanup: {error:#}",
                self.stage_path.display()
            )),
        }

        let durability = match hooks.sync_parent(&self.parent_directory) {
            Ok(()) => InstallDurability::Confirmed,
            Err(error) => {
                warnings.push(format!(
                    "failed to sync install directory {} after publication: {error}",
                    self.parent_path.display()
                ));
                InstallDurability::Uncertain
            },
        };

        Ok(InstallOutcome {
            durability,
            warnings,
        })
    }

    fn cleanup(&mut self) -> anyhow::Result<()> {
        use rustix::fs::AtFlags;

        if self.payload_active {
            match self.payload_entry_matches_open_file()? {
                Some(true) => {
                    rustix::fs::unlinkat(&self.stage_directory, "payload", AtFlags::empty())
                        .with_context(|| {
                            format!(
                                "failed to remove staged install payload {}",
                                self.payload_path.display()
                            )
                        })?;
                    self.payload_active = false;
                },
                Some(false) => anyhow::bail!(
                    "refusing to remove staged install payload after its identity changed: {}",
                    self.payload_path.display()
                ),
                None => self.payload_active = false,
            }
        }

        if self.directory_active {
            self.stage_directory.sync_all().with_context(|| {
                format!(
                    "failed to sync private install directory {} during cleanup",
                    self.stage_path.display()
                )
            })?;
            if !self.stage_entry_matches_open_directory()? {
                anyhow::bail!(
                    "refusing to remove private install directory after its identity changed: {}",
                    self.stage_path.display()
                );
            }
            rustix::fs::unlinkat(&self.parent_directory, &self.stage_name, AtFlags::REMOVEDIR)
                .with_context(|| {
                    format!(
                        "failed to remove private install directory {}",
                        self.stage_path.display()
                    )
                })?;
            self.directory_active = false;
            self.parent_directory.sync_all().with_context(|| {
                format!(
                    "failed to sync install directory {} after cleanup",
                    self.parent_path.display()
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for StagedInstall {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

#[cfg(unix)]
fn install_binary_unix(
    source: &Path,
    destination: &Path,
    hooks: &mut impl InstallHooks,
) -> anyhow::Result<InstallOutcome> {
    let destination = effective_install_destination(source, destination)?;
    let mut source = OpenedInstallSource::open(source)?;
    hooks.after_source_open(source.path())?;
    let mut staged = StagedInstall::reserve(&destination)?;
    let install_result = (|| {
        staged.copy_source(&mut source)?;
        hooks.prepare_staged(staged.path())?;
        staged.adopt_payload_after_preparation()?;
        hooks.verify_staged(staged.path())?;
        staged.revalidate_payload()?;
        staged.sync_payload()?;
        hooks.before_publish(staged.path(), destination.path())?;
        staged.publish(hooks)
    })();

    match install_result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            if let Err(cleanup_error) = staged.cleanup() {
                return Err(error.context(format!(
                    "also failed to clean private staged vc-frame install {}: {cleanup_error:#}",
                    staged.path().display()
                )));
            }
            Err(error)
        },
    }
}

#[cfg(unix)]
fn open_directory_no_follow(path: &Path) -> anyhow::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let fd = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(std::fs::File::from(fd))
}

#[cfg(unix)]
fn revalidate_absolute_path(
    path: &Path,
    file: &std::fs::File,
    expected_type: rustix::fs::FileType,
) -> anyhow::Result<()> {
    let path_stat =
        rustix::fs::statat(rustix::fs::CWD, path, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
    let file_stat = rustix::fs::fstat(file)
        .with_context(|| format!("failed to inspect open {}", path.display()))?;
    if rustix::fs::FileType::from_raw_mode(path_stat.st_mode) != expected_type
        || rustix::fs::FileType::from_raw_mode(file_stat.st_mode) != expected_type
        || !same_file_identity(&path_stat, &file_stat)
    {
        anyhow::bail!("path changed identity while open: {}", path.display());
    }
    Ok(())
}

#[cfg(unix)]
fn statat_optional(
    directory: &std::fs::File,
    name: impl rustix::path::Arg,
) -> anyhow::Result<Option<rustix::fs::Stat>> {
    match rustix::fs::statat(directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(stat)),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error).context("failed to inspect fd-relative install entry"),
    }
}

#[cfg(unix)]
fn same_file_identity(left: &rustix::fs::Stat, right: &rustix::fs::Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(all(test, unix))]
mod install_tests {
    use super::*;
    use std::io::Read;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    type PathHook = Box<dyn FnMut(&Path) -> anyhow::Result<()>>;
    type PublishHook = Box<dyn FnMut(&Path, &Path) -> anyhow::Result<()>>;

    #[derive(Default)]
    struct TestInstallHooks {
        after_source_open: Option<PathHook>,
        prepare_staged: Option<PathHook>,
        verify_staged: Option<PathHook>,
        before_publish: Option<PublishHook>,
        fail_parent_sync: bool,
    }

    impl InstallHooks for TestInstallHooks {
        fn after_source_open(&mut self, source: &Path) -> anyhow::Result<()> {
            match &mut self.after_source_open {
                Some(hook) => hook(source),
                None => Ok(()),
            }
        }

        fn prepare_staged(&mut self, staged: &Path) -> anyhow::Result<()> {
            match &mut self.prepare_staged {
                Some(hook) => hook(staged),
                None => Ok(()),
            }
        }

        fn verify_staged(&mut self, staged: &Path) -> anyhow::Result<()> {
            match &mut self.verify_staged {
                Some(hook) => hook(staged),
                None => Ok(()),
            }
        }

        fn before_publish(&mut self, staged: &Path, destination: &Path) -> anyhow::Result<()> {
            match &mut self.before_publish {
                Some(hook) => hook(staged, destination),
                None => Ok(()),
            }
        }

        fn sync_parent(&mut self, parent: &std::fs::File) -> std::io::Result<()> {
            if self.fail_parent_sync {
                Err(std::io::Error::other("injected parent sync failure"))
            } else {
                parent.sync_all()
            }
        }
    }

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
        let mut hooks = TestInstallHooks::default();
        let outcome = install_binary_unix(source, destination, &mut hooks)?;
        if outcome.durability != InstallDurability::Confirmed || !outcome.warnings.is_empty() {
            anyhow::bail!("unexpected install outcome: {outcome:?}");
        }
        Ok(())
    }

    fn staged_entries(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .expect("read test directory")
            .filter_map(|entry| {
                let path = entry.expect("read test entry").path();
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().contains(".vc-frame.install-"))
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
    fn staged_preparation_failure_preserves_destination_and_cleans_private_stage() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");

        let mut hooks = TestInstallHooks {
            prepare_staged: Some(Box::new(|_| anyhow::bail!("injected signing failure"))),
            ..TestInstallHooks::default()
        };
        let error = install_binary_unix(&source, &destination, &mut hooks)
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
    fn source_replacement_after_open_does_not_change_copied_bytes() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let replacement = directory.join("replacement-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"opened runtime").expect("write source");
        std::fs::write(&replacement, b"path replacement").expect("write replacement");
        std::fs::write(&destination, b"old runtime").expect("write destination");

        let replacement_for_hook = replacement.clone();
        let mut hooks = TestInstallHooks {
            after_source_open: Some(Box::new(move |source| {
                std::fs::rename(&replacement_for_hook, source)
                    .expect("replace source after retained fd is open");
                Ok(())
            })),
            ..TestInstallHooks::default()
        };
        let outcome = install_binary_unix(&source, &destination, &mut hooks)
            .expect("retained source fd must install opened bytes");

        assert_eq!(outcome.durability, InstallDurability::Confirmed);
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read published destination"),
            "opened runtime"
        );
        assert_eq!(
            std::fs::read_to_string(&source).expect("read replaced source path"),
            "path replacement"
        );
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn staged_inode_replacement_is_adopted_and_verified_before_publication() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"unsigned runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let verified = Arc::new(AtomicBool::new(false));
        let verified_for_hook = Arc::clone(&verified);
        let mut hooks = TestInstallHooks {
            prepare_staged: Some(Box::new(|staged| {
                let replacement = staged.with_extension("codesign-replacement");
                std::fs::write(&replacement, b"signed runtime").expect("write signed inode");
                std::fs::rename(&replacement, staged)
                    .expect("replace staged inode like codesign --force");
                Ok(())
            })),
            verify_staged: Some(Box::new(move |staged| {
                assert_eq!(
                    std::fs::read_to_string(staged).expect("read adopted staged inode"),
                    "signed runtime"
                );
                verified_for_hook.store(true, Ordering::SeqCst);
                Ok(())
            })),
            ..TestInstallHooks::default()
        };

        let outcome = install_binary_unix(&source, &destination, &mut hooks)
            .expect("adopt codesign replacement inode");

        assert_eq!(outcome.durability, InstallDurability::Confirmed);
        assert!(verified.load(Ordering::SeqCst));
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read signed destination"),
            "signed runtime"
        );
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn private_stage_directory_is_mode_0700_and_installer_owned() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        let inspected = Arc::new(AtomicBool::new(false));
        let inspected_for_hook = Arc::clone(&inspected);
        let mut hooks = TestInstallHooks {
            prepare_staged: Some(Box::new(move |staged| {
                let stage_directory = staged.parent().expect("staged payload parent");
                let metadata = std::fs::metadata(stage_directory).expect("private stage metadata");
                assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
                assert_eq!(metadata.uid(), rustix::process::geteuid().as_raw());
                inspected_for_hook.store(true, Ordering::SeqCst);
                Ok(())
            })),
            ..TestInstallHooks::default()
        };

        install_binary_unix(&source, &destination, &mut hooks).expect("private stage install");

        assert!(inspected.load(Ordering::SeqCst));
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn staged_payload_replacement_before_publish_is_rejected_and_not_deleted() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let mut hooks = TestInstallHooks {
            before_publish: Some(Box::new(|staged, _| {
                std::fs::remove_file(staged).expect("unlink adopted staged payload");
                std::fs::write(staged, b"unowned replacement").expect("replace staged payload");
                Ok(())
            })),
            ..TestInstallHooks::default()
        };

        let error = install_binary_unix(&source, &destination, &mut hooks)
            .expect_err("staged payload race must fail");

        assert!(format!("{error:#}").contains("changed identity"));
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read preserved destination"),
            "old runtime"
        );
        let stages = staged_entries(&directory);
        assert_eq!(stages.len(), 1);
        assert_eq!(
            std::fs::read_to_string(stages[0].join("payload")).expect("read retained replacement"),
            "unowned replacement"
        );
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn parent_replacement_before_publish_is_rejected_and_old_destination_survives() {
        let directory = test_directory();
        let build_directory = directory.join("build");
        let install_directory = directory.join("bin");
        let moved_install_directory = directory.join("original-bin");
        std::fs::create_dir(&build_directory).expect("create build directory");
        std::fs::create_dir(&install_directory).expect("create install directory");
        let source = build_directory.join("vc-frame");
        let destination = install_directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let install_directory_for_hook = install_directory.clone();
        let moved_install_directory_for_hook = moved_install_directory.clone();
        let mut hooks = TestInstallHooks {
            before_publish: Some(Box::new(move |_, _| {
                std::fs::rename(
                    &install_directory_for_hook,
                    &moved_install_directory_for_hook,
                )
                .expect("replace install parent");
                std::fs::create_dir(&install_directory_for_hook)
                    .expect("create replacement install parent");
                Ok(())
            })),
            ..TestInstallHooks::default()
        };

        let error = install_binary_unix(&source, &destination, &mut hooks)
            .expect_err("parent replacement race must fail");

        assert!(format!("{error:#}").contains("destination directory changed identity"));
        assert!(!destination.exists());
        assert_eq!(
            std::fs::read_to_string(moved_install_directory.join("vc-frame"))
                .expect("read old destination in original parent"),
            "old runtime"
        );
        assert!(staged_entries(&moved_install_directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn destination_replacement_before_publish_is_rejected_without_overwriting_it() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let mut hooks = TestInstallHooks {
            before_publish: Some(Box::new(|_, destination| {
                let replacement = destination.with_extension("concurrent");
                std::fs::write(&replacement, b"concurrent runtime")
                    .expect("write concurrent destination");
                std::fs::rename(&replacement, destination)
                    .expect("replace destination before publish");
                Ok(())
            })),
            ..TestInstallHooks::default()
        };

        let error = install_binary_unix(&source, &destination, &mut hooks)
            .expect_err("destination replacement race must fail");

        assert!(format!("{error:#}").contains("destination changed identity"));
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read concurrent destination"),
            "concurrent runtime"
        );
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[test]
    fn parent_sync_failure_reports_published_but_durability_uncertain() {
        let directory = test_directory();
        let source = directory.join("new-vc-frame");
        let destination = directory.join("vc-frame");
        std::fs::write(&source, b"new runtime").expect("write source");
        std::fs::write(&destination, b"old runtime").expect("write destination");
        let mut hooks = TestInstallHooks {
            fail_parent_sync: true,
            ..TestInstallHooks::default()
        };

        let outcome = install_binary_unix(&source, &destination, &mut hooks)
            .expect("publication remains successful after parent sync failure");

        assert_eq!(outcome.durability, InstallDurability::Uncertain);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("failed to sync install directory"))
        );
        assert_eq!(
            std::fs::read_to_string(&destination).expect("read published destination"),
            "new runtime"
        );
        assert!(staged_entries(&directory).is_empty());
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_real_macho_is_signed_installed_and_strictly_verified() {
        let directory = test_directory();
        let source = directory.join("vc-frame-source");
        let destination = directory.join("vc-frame");
        std::fs::copy(
            std::env::current_exe().expect("current test executable"),
            &source,
        )
        .expect("copy real Mach-O test executable");
        let shell = Shell::new().expect("create shell");

        install_binary(&shell, &source, &destination)
            .expect("sign and install real Mach-O executable");

        let verification = std::process::Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict"])
            .arg(&destination)
            .status()
            .expect("run codesign verification");
        assert!(verification.success());
        assert!(staged_entries(&directory).is_empty());
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
