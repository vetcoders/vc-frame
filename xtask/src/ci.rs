//! Tasks related to zellij CI
use crate::{
    build,
    flags::{self, BuildRelease, CiCmd, Cross, E2e},
    metadata,
};
use anyhow::Context;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use xshell::{Shell, cmd};

pub fn main(sh: &Shell, flags: flags::Ci) -> anyhow::Result<()> {
    let err_context = "failed to run CI task";

    match flags.subcommand {
        CiCmd::E2e(E2e {
            build: false,
            test: false,
            ..
        }) => Err(anyhow::anyhow!(
            "either '--build' or '--test' must be provided!"
        )),
        CiCmd::E2e(E2e {
            build: true,
            test: true,
            ..
        }) => Err(anyhow::anyhow!(
            "flags '--build' and '--test' are mutually exclusive!"
        )),
        CiCmd::E2e(E2e {
            build: true,
            test: false,
            ..
        }) => e2e_build(sh),
        CiCmd::E2e(E2e {
            build: false,
            test: true,
            args,
        }) => e2e_test(sh, args),
        CiCmd::Cross(Cross { triple, no_web }) => cross_compile(sh, &triple, no_web),
        CiCmd::BuildRelease(BuildRelease { no_web }) => build_release(sh, no_web),
    }
    .context(err_context)
}

/// Copy exactly the `expected` plugins from the fresh builder output into
/// `<data_dir>/plugins`, replacing any previous data dir. A missing or empty
/// plugin is an error naming it; nothing is skipped.
fn stage_e2e_plugins(
    built_plugins: &Path,
    data_dir: &Path,
    expected: &[&str],
) -> anyhow::Result<()> {
    let mut sources = Vec::with_capacity(expected.len());
    for name in expected {
        let source = built_plugins.join(format!("{name}.wasm"));
        let len = std::fs::metadata(&source)
            .with_context(|| format!("built plugin '{name}' not found at '{}'", source.display()))?
            .len();
        anyhow::ensure!(
            len > 0,
            "built plugin '{name}' is empty: '{}'",
            source.display()
        );
        sources.push((source, format!("{name}.wasm")));
    }
    anyhow::ensure!(
        !sources.is_empty(),
        "no plugins expected for the E2E data dir"
    );
    let plugin_dir = data_dir.join("plugins");
    match std::fs::remove_dir_all(data_dir) {
        Ok(()) => {},
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
        Err(e) => return Err(e).context("failed to clear the E2E data dir"),
    }
    std::fs::create_dir_all(&plugin_dir).context("failed to create the E2E plugin dir")?;
    for (source, file_name) in sources {
        std::fs::copy(&source, plugin_dir.join(&file_name))
            .with_context(|| format!("failed to stage '{}'", source.display()))?;
    }
    Ok(())
}

fn e2e_build(sh: &Shell) -> anyhow::Result<()> {
    let err_context = "failed to build E2E binary";

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

    // Stage the plugins this build just derived (target/<..>/vc-frame-plugins);
    // zellij-utils/assets/plugins is retired and is not a source of truth.
    let project_root = crate::project_root();
    let data_dir = project_root.join("target").join("e2e-data");
    stage_e2e_plugins(
        &build::plugin_release_dir(),
        &data_dir,
        &build::plugin_member_names()?,
    )
    .context(err_context)?;

    let _pd = sh.push_dir(project_root);
    // On Windows, build natively (MSVC).
    // On macOS: use cargo-zigbuild which handles musl cross-compilation for any arch without
    //   needing a separate linker toolchain. The target arch matches the host so the Docker
    //   container runs natively without emulation (arm64 on Apple Silicon, amd64 on Intel).
    //   Install with: cargo install cargo-zigbuild && brew install zig
    // On Linux: cross-compile for musl.
    if cfg!(windows) {
        crate::cargo()
            .and_then(|cargo| {
                cmd!(sh, "{cargo} build --release")
                    .run()
                    .map_err(anyhow::Error::new)
            })
            .context(err_context)
    } else if cfg!(target_os = "macos") {
        let target = if cfg!(target_arch = "aarch64") {
            "aarch64-unknown-linux-musl"
        } else {
            "x86_64-unknown-linux-musl"
        };
        crate::cargo()
            .and_then(|cargo| {
                if which::which("cargo-zigbuild").is_err() {
                    eprintln!("!! 'cargo-zigbuild' wasn't found but is needed on macOS.");
                    eprintln!("!! Please install it with:");
                    eprintln!("!!   cargo install cargo-zigbuild");
                    eprintln!("!!   brew install zig");
                    return Err(anyhow::anyhow!("couldn't find 'cargo-zigbuild'"));
                }
                cmd!(sh, "rustup target add {target}")
                    .run()
                    .map_err(anyhow::Error::new)?;
                cmd!(sh, "{cargo} zigbuild --release --target {target}")
                    .run()
                    .map_err(anyhow::Error::new)
            })
            .context(err_context)
    } else {
        crate::cargo()
            .and_then(|cargo| {
                cmd!(
                    sh,
                    "{cargo} build --release --target x86_64-unknown-linux-musl"
                )
                .run()
                .map_err(anyhow::Error::new)
            })
            .context(err_context)
    }
}

/// Native release build: builds plugins, generates protobufs, and runs
/// `cargo build --release` without cross-compilation.
fn build_release(sh: &Shell, no_web: bool) -> anyhow::Result<()> {
    let err_context = "failed to perform native release build";

    // Build plugins and generate protobufs
    build::build(
        sh,
        flags::Build {
            release: true,
            no_plugins: false,
            plugins_only: true,
            no_web,
        },
    )
    .context(err_context)?;

    // Build the main binary natively
    let _pd = sh.push_dir(crate::project_root());
    crate::cargo()
        .and_then(|cargo| {
            if no_web {
                match metadata::get_no_web_features(sh, ".")
                    .context("Failed to check web features for build-release")?
                {
                    Some(features) => {
                        let mut cmd = cmd!(
                            sh,
                            "{cargo} build --verbose --release --no-default-features"
                        );
                        if !features.is_empty() {
                            cmd = cmd.arg("--features").arg(features);
                        }
                        cmd.run().map_err(anyhow::Error::new)
                    },
                    None => cmd!(sh, "{cargo} build --verbose --release")
                        .run()
                        .map_err(anyhow::Error::new),
                }
            } else {
                cmd!(sh, "{cargo} build --verbose --release")
                    .run()
                    .map_err(anyhow::Error::new)
            }
        })
        .context(err_context)
}

fn e2e_test(sh: &Shell, args: Vec<OsString>) -> anyhow::Result<()> {
    let err_context = "failed to run E2E tests";

    e2e_build(sh).context(err_context)?;

    let _pd = sh.push_dir(crate::project_root());

    // set --no-default-features while zellij-utils/build.rs still guarantees
    // current-source plugins in the derived plugin target directory
    crate::cargo()
        .and_then(|cargo| {
            // e2e tests
            cmd!(
                sh,
                "{cargo} test --no-default-features -- --ignored --nocapture --test-threads 1"
            )
            .args(args.clone())
            .run()
            .map_err(anyhow::Error::new)?;

            // plugin system tests are run here because they're medium-slow
            let _pd = sh.push_dir(Path::new("zellij-server"));
            println!();
            let msg = ">> Testing Plugin System".to_string();
            crate::status(&msg);
            println!("{}", msg);

            cmd!(sh, "{cargo} test -- --ignored --nocapture --test-threads 1")
                .args(args.clone())
                .run()
                .with_context(|| "Failed to run tests for the Plugin System".to_string())?;
            Ok(())
        })
        .context(err_context)
}

fn cross_compile(sh: &Shell, target: &OsString, no_web: bool) -> anyhow::Result<()> {
    let err_context = || format!("failed to cross-compile for {target:?}");

    crate::cargo()
        .and_then(|cargo| {
            cmd!(sh, "{cargo} install mandown").run()?;
            Ok(cargo)
        })
        .and_then(|cargo| {
            cmd!(sh, "{cargo} install cross")
                .run()
                .map_err(anyhow::Error::new)
        })
        .with_context(err_context)?;

    build::build(
        sh,
        flags::Build {
            release: true,
            no_plugins: false,
            plugins_only: true,
            no_web,
        },
    )
    .and_then(|_| build::manpage(sh))
    .with_context(err_context)?;

    cross()
        .and_then(|cross| {
            if no_web {
                match metadata::get_no_web_features(sh, ".")
                    .context("Failed to check web features for cross compilation")?
                {
                    Some(features) => {
                        let mut cmd = cmd!(sh, "{cross} build --verbose --release --target {target} --no-default-features");
                        if !features.is_empty() {
                            cmd = cmd.arg("--features").arg(features);
                        }
                        cmd.run().map_err(anyhow::Error::new)
                    },
                    None => {
                        // Main crate doesn't have web_server_capability, build normally
                        cmd!(sh, "{cross} build --verbose --release --target {target}")
                            .run()
                            .map_err(anyhow::Error::new)
                    },
                }
            } else {
                cmd!(sh, "{cross} build --verbose --release --target {target}")
                    .run()
                    .map_err(anyhow::Error::new)
            }
        })
        .with_context(err_context)
}

fn cross() -> anyhow::Result<PathBuf> {
    match which::which("cross") {
        Ok(path) => Ok(path),
        Err(e) => {
            eprintln!("!! 'cross' wasn't found but is needed for this build step.");
            eprintln!("!! Please install it with: `cargo install cross`");
            Err(e).context("couldn't find 'cross' executable")
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xtask-e2e-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn built(dir: &Path, names: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for name in names {
            std::fs::write(dir.join(format!("{name}.wasm")), b"\0asm").unwrap();
        }
        // artifacts that must never be staged
        std::fs::write(dir.join("notes.txt"), b"x").unwrap();
        std::fs::write(dir.join("stray.wasm"), b"\0asm").unwrap();
    }

    #[test]
    fn retired_source_asset_dir_is_not_a_plugin_source() {
        let retired = crate::asset_dir().join("plugins");
        assert!(!retired.exists(), "plugins are derived, not tracked assets");
        let scratch = scratch("retired");
        let error = stage_e2e_plugins(&retired, &scratch.join("data"), &["strider"]);
        assert!(error.is_err());
        assert!(format!("{:#}", error.unwrap_err()).contains("strider"));
    }

    #[test]
    fn stages_exactly_the_expected_derived_plugins() {
        let scratch = scratch("stage");
        let derived = scratch.join("custom-target/vc-frame-plugins/wasm32-wasip1/release");
        built(&derived, &["strider", "fixture-plugin-for-tests"]);
        let data = scratch.join("data");
        std::fs::create_dir_all(data.join("plugins")).unwrap();
        std::fs::write(data.join("plugins/old.wasm"), b"old").unwrap();
        stage_e2e_plugins(&derived, &data, &["strider", "fixture-plugin-for-tests"]).unwrap();
        let mut staged: Vec<String> = std::fs::read_dir(data.join("plugins"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        staged.sort();
        assert_eq!(staged, ["fixture-plugin-for-tests.wasm", "strider.wasm"]);
    }

    #[test]
    fn a_missing_or_empty_plugin_fails_by_name_and_stages_nothing() {
        let scratch = scratch("missing");
        let derived = scratch.join("release");
        built(&derived, &["strider"]);
        let data = scratch.join("data");
        let missing = stage_e2e_plugins(&derived, &data, &["strider", "compact-bar"]);
        assert!(format!("{:#}", missing.unwrap_err()).contains("compact-bar"));
        assert!(!data.exists(), "no partial staging");
        std::fs::write(derived.join("strider.wasm"), b"").unwrap();
        let empty = stage_e2e_plugins(&derived, &data, &["strider"]);
        assert!(format!("{:#}", empty.unwrap_err()).contains("empty"));
        assert!(stage_e2e_plugins(&derived, &data, &[]).is_err());
    }
}
