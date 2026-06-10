//! Subcommands for building.
//!
//! Currently has the following functions:
//!
//! - [`build`]: Builds general cargo projects (i.e. vc-frame components) with `cargo build`
//! - [`manpage`]: Builds the manpage with `mandown`
use crate::{WorkspaceMember, flags, metadata};
use anyhow::Context;
use std::path::{Path, PathBuf};
use xshell::{Shell, cmd};

/// Build members of the vc-frame workspace.
///
/// Build behavior is controlled by the [`flags`](flags::Build). Calls some variation of `cargo
/// build` under the hood.
pub fn build(sh: &Shell, flags: flags::Build) -> anyhow::Result<()> {
    let _pd = sh.push_dir(crate::project_root());

    let cargo = crate::cargo()?;
    if flags.no_plugins && flags.plugins_only {
        eprintln!("Cannot use both '--no-plugins' and '--plugins-only'");
        std::process::exit(1);
    }

    // zellij-utils requires protobuf definition files to be present. Usually these are
    // auto-generated with `build.rs`-files, but this is currently broken for us.
    // See [this PR][1] for details.
    //
    // [1]: https://github.com/zellij-org/zellij/pull/2711#issuecomment-1695015818
    run_proto_codegen(sh);

    // Build all plugins in a single invocation so Cargo can unify transitive dependency
    // features across all of them and compile shared crates (e.g. zellij-utils) only once.
    if !flags.no_plugins {
        let plugin_members: Vec<&WorkspaceMember> = crate::workspace_members()
            .iter()
            .filter(|m| m.build && m.crate_name.contains("plugins"))
            .collect();

        if !plugin_members.is_empty() {
            println!();
            let msg = ">> Building plugins";
            crate::status(msg);
            println!("{}", msg);

            let mut base_cmd = cmd!(sh, "{cargo} build --target wasm32-wasip1");
            if flags.release {
                base_cmd = base_cmd.arg("--release");
            }
            for member in &plugin_members {
                let plugin_name = member
                    .crate_name
                    .rsplit_once('/')
                    .context("Cannot determine plugin name from crate path")?
                    .1;
                base_cmd = base_cmd.args(["-p", plugin_name]);
            }
            base_cmd.run().context("failed to build plugins")?;

            if flags.release {
                for member in &plugin_members {
                    let plugin_name = member
                        .crate_name
                        .rsplit_once('/')
                        .context("Cannot determine plugin name from crate path")?
                        .1;
                    move_plugin_to_assets(sh, plugin_name)?;
                }
            }
        }
    }

    // Build non-plugin crates (native target).
    if !flags.plugins_only {
        for WorkspaceMember { crate_name, .. } in crate::workspace_members()
            .iter()
            .filter(|member| member.build && !member.crate_name.contains("plugins"))
        {
            let err_context = || format!("failed to build '{crate_name}'");

            let _pd = sh.push_dir(Path::new(crate_name));
            println!();
            let msg = format!(">> Building '{crate_name}'");
            crate::status(&msg);
            println!("{}", msg);

            let mut base_cmd = cmd!(sh, "{cargo} build");
            if flags.release {
                base_cmd = base_cmd.arg("--release");
            } else {
                base_cmd = base_cmd.args(["--profile", "dev-opt"]);
            }
            if flags.no_web {
                // Check if this crate has web features that need modification
                match metadata::get_no_web_features(sh, crate_name)
                    .context("Failed to check web features")?
                {
                    Some(features) => {
                        base_cmd = base_cmd.arg("--no-default-features");
                        if !features.is_empty() {
                            base_cmd = base_cmd.arg("--features");
                            base_cmd = base_cmd.arg(features);
                        }
                    },
                    None => {
                        // Crate doesn't have web features, build normally
                    },
                }
            }
            base_cmd.run().with_context(err_context)?;
        }
    }

    Ok(())
}

fn run_proto_codegen(sh: &Shell) {
    let zellij_utils_basedir = crate::project_root().join("zellij-utils");
    let _pd = sh.push_dir(&zellij_utils_basedir);

    let specs: &[(&str, &str, &str)] = &[
        ("assets/prost", "src/plugin_api", "generated_plugin_api.rs"),
        (
            "assets/prost_ipc",
            "src/client_server_contract",
            "generated_client_server_api.rs",
        ),
        (
            "assets/prost_web_server",
            "src/web_server_contract",
            "generated_web_server_api.rs",
        ),
    ];

    for (out_subdir, src_subdir, include_file) in specs {
        let out_dir = sh.current_dir().join(out_subdir);
        let src_dir = sh.current_dir().join(src_subdir);
        std::fs::create_dir_all(&out_dir).unwrap();

        let last_generated = out_dir
            .join(include_file)
            .metadata()
            .and_then(|m| m.modified());
        let mut proto_files = vec![];
        let mut needs_regeneration = false;

        for entry in std::fs::read_dir(&src_dir).unwrap() {
            let entry_path = entry.unwrap().path();
            if entry_path.is_file()
                && entry_path
                    .extension()
                    .map(|e| e == "proto")
                    .unwrap_or(false)
            {
                let modified = entry_path.metadata().and_then(|m| m.modified());
                needs_regeneration |= match (&last_generated, modified) {
                    (Ok(last_generated), Ok(modified)) => modified > *last_generated,
                    // Couldn't read some metadata, assume needs update
                    _ => true,
                };
                proto_files.push(entry_path.display().to_string());
            }
        }
        proto_files.sort();

        if needs_regeneration {
            let mut prost = prost_build::Config::new();
            prost.out_dir(&out_dir);
            prost.include_file(include_file);
            configure_clippy_clean_prost(&mut prost);
            prost.compile_protos(&proto_files, &[src_dir]).unwrap();
        }
        postprocess_prost_for_clippy(&out_dir, include_file);
    }
}

fn configure_clippy_clean_prost(prost: &mut prost_build::Config) {
    for path in [
        ".client_server_contract.Action.action_type.new_tab",
        ".api.action.Action.optional_payload.new_tab_payload",
        ".api.event.Event.payload.mode_update_payload",
        ".api.event.Event.payload.action_complete_payload",
        ".api.plugin_command.PluginCommand.payload.run_action_payload",
    ] {
        prost.boxed(path);
    }
}

fn postprocess_prost_for_clippy(out_dir: &Path, include_file: &str) {
    let include_path = out_dir.join(include_file);
    if let Ok(mut include_contents) = std::fs::read_to_string(&include_path) {
        include_contents = include_contents
            .replace(
                "pub mod action {\n    include!(\"api.action.rs\");\n}",
                "pub mod action_api {\n    include!(\"api.action.rs\");\n}\npub use action_api as action;",
            )
            .replace(
                "pub mod event {\n    include!(\"api.event.rs\");\n}",
                "pub mod event_api {\n    include!(\"api.event.rs\");\n}\npub use event_api as event;",
            )
            .replace(
                "pub mod key {\n    include!(\"api.key.rs\");\n}",
                "pub mod key_api {\n    include!(\"api.key.rs\");\n}\npub use key_api as key;",
            )
            .replace(
                "pub mod plugin_command {\n    include!(\"api.plugin_command.rs\");\n}",
                "pub mod plugin_command_api {\n    include!(\"api.plugin_command.rs\");\n}\npub use plugin_command_api as plugin_command;",
            )
            .replace(
                "pub mod client_server_contract {\n    include!(\"client_server_contract.rs\");\n}",
                "pub mod client_server_contract_api {\n    include!(\"client_server_contract.rs\");\n}\npub use client_server_contract_api as client_server_contract;",
            )
            .replace(
                "pub mod web_server_contract {\n    include!(\"web_server_contract.rs\");\n}",
                "pub mod web_server_contract_api {\n    include!(\"web_server_contract.rs\");\n}\npub use web_server_contract_api as web_server_contract;",
            );
        std::fs::write(include_path, include_contents).unwrap();
    }

    let event_path = out_dir.join("api.event.rs");
    if let Ok(mut event_contents) = std::fs::read_to_string(&event_path) {
        let kdl_error_tail = "    #[prost(string, optional, tag=\"4\")]\n    pub help_message: ::core::option::Option<::prost::alloc::string::String>,\n}\n";
        let kdl_error_tail_with_is_empty = "    #[prost(string, optional, tag=\"4\")]\n    pub help_message: ::core::option::Option<::prost::alloc::string::String>,\n}\nimpl KdlError {\n    pub fn is_empty(&self) -> bool {\n        self.len == Some(0)\n    }\n}\n";
        if event_contents.contains(kdl_error_tail)
            && !event_contents.contains("impl KdlError {\n    pub fn is_empty")
        {
            event_contents = event_contents.replace(kdl_error_tail, kdl_error_tail_with_is_empty);
            std::fs::write(event_path, event_contents).unwrap();
        }
    }
}

fn move_plugin_to_assets(sh: &Shell, plugin_name: &str) -> anyhow::Result<()> {
    let err_context = || format!("failed to move plugin '{plugin_name}' to assets folder");

    // Get asset path
    let asset_name = crate::asset_dir()
        .join("plugins")
        .join(plugin_name)
        .with_extension("wasm");

    // Get plugin path
    let plugin = PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR")
            .unwrap_or(crate::project_root().join("target").into_os_string()),
    )
    .join("wasm32-wasip1")
    .join("release")
    .join(plugin_name)
    .with_extension("wasm");

    if !plugin.is_file() {
        return Err(anyhow::anyhow!("No plugin found at '{}'", plugin.display()))
            .with_context(err_context);
    }

    // This is a plugin we want to move
    let from = plugin.as_path();
    let to = asset_name.as_path();
    sh.copy_file(from, to).with_context(err_context)
}

/// Build the manpage with `mandown`.
//      mkdir -p ${root_dir}/assets/man
//      mandown ${root_dir}/docs/MANPAGE.md 1 > ${root_dir}/assets/man/vc-frame.1
pub fn manpage(sh: &Shell) -> anyhow::Result<()> {
    let err_context = "failed to generate manpage";

    let mandown = mandown(sh).context(err_context)?;

    let project_root = crate::project_root();
    let asset_dir = &project_root.join("assets").join("man");
    sh.create_dir(asset_dir).context(err_context)?;
    let _pd = sh.push_dir(asset_dir);

    let text = cmd!(sh, "{mandown} {project_root}/docs/MANPAGE.md 1")
        .read()
        .context(err_context)?;
    if text.trim().is_empty() {
        // A broken mandown can emit zero bytes with exit code 0; never let
        // that silently clobber a good committed manpage with an empty file.
        let existing = asset_dir.join("vc-frame.1");
        if std::fs::metadata(&existing).is_ok_and(|m| m.len() > 0) {
            eprintln!("!! mandown produced empty output; keeping existing assets/man/vc-frame.1");
            return Ok(());
        }
        anyhow::bail!(
            "mandown produced an empty manpage and no usable assets/man/vc-frame.1 exists"
        );
    }
    sh.write_file("vc-frame.1", text).context(err_context)
}

/// Get the path to a `mandown` executable.
///
/// If the executable isn't found, an error is returned instead.
fn mandown(_sh: &Shell) -> anyhow::Result<PathBuf> {
    match which::which("mandown") {
        Ok(path) => Ok(path),
        Err(e) => {
            eprintln!("!! 'mandown' wasn't found but is needed for this build step.");
            eprintln!("!! Please install it with: `cargo install mandown`");
            Err(e).context("Couldn't find 'mandown' executable")
        },
    }
}
