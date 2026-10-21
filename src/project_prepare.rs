//! Native folder selection crosses into the existing selected-runtime workspace
//! owner here. Frame validates the returned destination, but never allocates WES
//! identities, chooses a collision fallback, or creates a replacement itself.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zellij_utils::{
    cli::{CliArgs, PrepareProjectCli},
    consts::{ZELLIJ_CACHE_DIR, ZELLIJ_SOCK_DIR},
    data::LayoutInfo,
    input::{config::Config, layout::Layout},
    sessions::{SocketOwnership, probe_socket_ownership, validate_session_name},
};

const ADMISSION_SCHEMA: &str = "vc-frame.project-admission.v1";

#[derive(Debug, Deserialize, Serialize)]
struct Admission {
    schema: String,
    session_name: String,
    session_incarnation: String,
    socket_dir: String,
    workspace_root: String,
    workspace_id: String,
    workspace_instance_id: String,
    session_id: String,
    build_id: String,
}

struct SelectedRuntime {
    root: PathBuf,
    python: PathBuf,
    frame: PathBuf,
}

#[derive(Deserialize)]
struct OwnerAdmission {
    native_session_name: String,
    session_incarnation: String,
    socket_dir: String,
    #[serde(rename = "VIBECRAFTED_OPERATOR_SESSION")]
    operator_session: String,
    #[serde(rename = "VIBECRAFTED_WORKSPACE_ROOT")]
    workspace_root: String,
    #[serde(rename = "VIBECRAFTED_WORKSPACE_ID")]
    workspace_id: String,
    #[serde(rename = "VIBECRAFTED_WORKSPACE_INSTANCE_ID")]
    workspace_instance_id: String,
    #[serde(rename = "VIBECRAFTED_SESSION_ID")]
    session_id: String,
    #[serde(rename = "VIBECRAFTED_BUILD_ID")]
    build_id: String,
}

pub(crate) fn prepare(opts: &CliArgs, request: &PrepareProjectCli) -> i32 {
    match prepare_project(opts, request) {
        Ok(admission) => match serde_json::to_string(&admission) {
            Ok(json) => {
                println!("{json}");
                0
            },
            Err(error) => {
                eprintln!("Project could not be opened: {error}");
                1
            },
        },
        Err(error) => {
            eprintln!("Project could not be opened: {error}");
            1
        },
    }
}

fn prepare_project(opts: &CliArgs, request: &PrepareProjectCli) -> Result<Admission, String> {
    let runtime = selected_runtime()?;
    let root = project_root(&request.root)?;
    if let Some(name) = &request.requested_session {
        validate_session_name(name)?;
        if name == "vc-host" {
            return Err("the enclosing host is not a project destination".to_owned());
        }
    }
    let layout = retained_layout(opts, request.layout_json.as_deref())?;
    let admission = owner_admission(&runtime, &root, request, layout.as_deref())?;
    validate_admission(&admission, &root, &ZELLIJ_SOCK_DIR)?;
    validate_native_destination(&runtime, &admission)?;
    Ok(admission)
}

fn project_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err("project root must be an absolute directory".to_owned());
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("project root is unavailable: {error}"))?;
    if !root.is_dir() {
        return Err("project root is not a directory".to_owned());
    }
    Ok(root)
}

fn selected_runtime() -> Result<SelectedRuntime, String> {
    let raw = std::env::var_os("VIBECRAFTED_RUNTIME_ROOT")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "a selected Vibecrafted Runtime Pack is required; ordinary native Frame sessions do not claim project ownership".to_owned()
        })?;
    let root = PathBuf::from(raw);
    if !root.is_absolute() {
        return Err("selected runtime root must be an absolute physical generation".to_owned());
    }
    for ancestor in root.ancestors() {
        let metadata = std::fs::symlink_metadata(ancestor)
            .map_err(|error| format!("selected runtime is unavailable: {error}"))?;
        if metadata.file_type().is_symlink() {
            return Err("selected runtime root must not traverse symbolic links".to_owned());
        }
    }
    let root = root
        .canonicalize()
        .map_err(|error| format!("selected runtime is unavailable: {error}"))?;
    let python = root.join("bin/python3");
    let frame = root.join("bin/vc-frame");
    let facade = root.join("vibecrafted-core/vibecrafted_core/runtime/shell/vetcoders.sh");
    if !python.is_file() || !frame.is_file() || !facade.is_file() {
        return Err(
            "selected Runtime Pack is incomplete (Python, Frame or workspace facade missing)"
                .to_owned(),
        );
    }
    for ancestor in facade.ancestors() {
        if std::fs::symlink_metadata(ancestor)
            .map_err(|error| format!("selected runtime facade is unavailable: {error}"))?
            .file_type()
            .is_symlink()
        {
            return Err("selected runtime workspace facade must be physical".to_owned());
        }
    }
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot identify this Frame engine: {error}"))?;
    if frame.canonicalize().ok().as_ref() != Some(&executable) {
        return Err("this Frame engine does not belong to the selected Runtime Pack".to_owned());
    }
    Ok(SelectedRuntime {
        root,
        python,
        frame,
    })
}

fn retained_layout(opts: &CliArgs, serialized: Option<&str>) -> Result<Option<PathBuf>, String> {
    let Some(serialized) = serialized else {
        return Ok(None);
    };
    let selected: LayoutInfo = serde_json::from_str(serialized)
        .map_err(|error| format!("invalid selected layout: {error}"))?;
    let selected = selected.resolve_product_workspace();
    let config = Config::try_from(opts)
        .map_err(|error| format!("cannot resolve selected layout configuration: {error}"))?;
    let layout_dir = config.options.layout_dir.or_else(|| {
        opts.config_dir
            .clone()
            .or_else(zellij_utils::home::find_default_config_dir)
            .map(|directory| directory.join("layouts"))
    });
    let (source, raw, swap) = match selected {
        LayoutInfo::BuiltIn(name) => Layout::stringified_from_default_assets(Path::new(&name)),
        LayoutInfo::File(name, _) => {
            Layout::stringified_from_dir(Path::new(&name), layout_dir.as_ref())
        },
        LayoutInfo::Stringified(raw) => Ok(("selected layout".to_owned(), raw, None)),
        LayoutInfo::Url(_) => {
            return Err(
                "remote project layouts require explicit local review before admission".to_owned(),
            );
        },
    }
    .map_err(|error| format!("cannot resolve selected layout: {error}"))?;
    Layout::from_kdl(
        &raw,
        Some(source),
        swap.as_ref()
            .map(|(path, raw)| (path.as_str(), raw.as_str())),
        None,
    )
    .map_err(|error| format!("selected layout is invalid: {error}"))?;

    // The owner and resurrection records may retain this path after this helper
    // exits. Keep an exclusively allocated cache directory, including its swap
    // document, instead of handing the owner an already-deleted temporary file.
    let cache = ZELLIJ_CACHE_DIR.join("project-layouts");
    std::fs::create_dir_all(&cache)
        .map_err(|error| format!("cannot create project layout cache: {error}"))?;
    let directory = tempfile::Builder::new()
        .prefix("admitted-")
        .tempdir_in(cache)
        .map_err(|error| format!("cannot retain selected layout: {error}"))?;
    let path = directory.path().join("layout.kdl");
    std::fs::write(&path, raw)
        .map_err(|error| format!("cannot retain selected layout: {error}"))?;
    if let Some((_, raw)) = swap {
        std::fs::write(path.with_extension("swap.kdl"), raw)
            .map_err(|error| format!("cannot retain selected swap layout: {error}"))?;
    }
    let _ = directory.into_path();
    Ok(Some(path))
}

fn owner_admission(
    runtime: &SelectedRuntime,
    root: &Path,
    request: &PrepareProjectCli,
    layout: Option<&Path>,
) -> Result<Admission, String> {
    let script = runtime
        .root
        .join("vibecrafted-core/vibecrafted_core/config/vc-frame/vc-agent-workshop.py");
    if !script.is_file()
        || script.ancestors().any(|ancestor| {
            std::fs::symlink_metadata(ancestor)
                .map(|metadata| metadata.file_type().is_symlink())
                .unwrap_or(true)
        })
    {
        return Err("selected-runtime project preparation API is unavailable".to_owned());
    }
    let runtime_bin = runtime.root.join("bin");
    // Frame adds its contract component to the environment's socket base.
    // Export that base explicitly, including when this process used the native
    // platform default, so the owner cannot select another namespace.
    let socket_base = ZELLIJ_SOCK_DIR
        .parent()
        .ok_or_else(|| "native session namespace has no socket base".to_owned())?;
    let mut path = vec![runtime_bin.clone()];
    if let Some(inherited) = std::env::var_os("PATH") {
        path.extend(std::env::split_paths(&inherited));
    }
    let mut command = Command::new(&runtime.python);
    command
        .arg(script)
        .args(["prepare", "--root"])
        .arg(root)
        .current_dir(root)
        .env("VIBECRAFTED_RUNTIME_ROOT", &runtime.root)
        .env("VIBECRAFTED_RUNTIME_BIN", &runtime_bin)
        .env("VIBECRAFTED_PYTHON", &runtime.python)
        .env("VIBECRAFTED_ROOT", &runtime.root)
        .env(
            "VIBECRAFTED_CORE_DIR",
            runtime.root.join("vibecrafted-core"),
        )
        .env("VIBECRAFTED_VC_FRAME_BIN", &runtime.frame)
        .env("VC_FRAME_SOCKET_DIR", socket_base)
        .env("ZELLIJ_SOCKET_DIR", socket_base)
        .env(
            "PATH",
            std::env::join_paths(path).map_err(|error| error.to_string())?,
        )
        .env_remove("PYTHONPATH")
        .env_remove("PYTHONHOME")
        .stdin(Stdio::null());
    if let Some(session) = &request.requested_session {
        command.args(["--session", session]);
    }
    if let Some(layout) = layout {
        command.arg("--layout").arg(layout);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot invoke selected-runtime project owner: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "selected-runtime project owner refused preparation ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let receipt: OwnerAdmission = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid selected-runtime admission receipt: {error}"))?;
    if receipt.native_session_name != receipt.operator_session {
        return Err("workspace owner returned conflicting native session identities".to_owned());
    }
    let namespace = normalize_socket_namespace(&receipt.socket_dir, &ZELLIJ_SOCK_DIR)?;
    Ok(Admission {
        schema: ADMISSION_SCHEMA.to_owned(),
        session_name: receipt.native_session_name,
        session_incarnation: receipt.session_incarnation,
        socket_dir: namespace.display().to_string(),
        workspace_root: receipt.workspace_root,
        workspace_id: receipt.workspace_id,
        workspace_instance_id: receipt.workspace_instance_id,
        session_id: receipt.session_id,
        build_id: receipt.build_id,
    })
}

fn normalize_socket_namespace(raw: &str, expected: &Path) -> Result<PathBuf, String> {
    let raw = Path::new(raw);
    if !raw.is_absolute() {
        return Err("workspace owner returned a relative native socket namespace".to_owned());
    }
    let expected = expected
        .canonicalize()
        .map_err(|error| format!("native session namespace is unavailable: {error}"))?;
    let contract = expected
        .file_name()
        .ok_or_else(|| "native session namespace has no contract component".to_owned())?;
    let namespace = raw.join(contract).canonicalize().ok();
    if namespace.as_ref() == Some(&expected) {
        return Ok(expected);
    }
    Err("workspace owner returned a different native session namespace".to_owned())
}

fn validate_admission(admission: &Admission, root: &Path, socket_dir: &Path) -> Result<(), String> {
    if admission.schema != ADMISSION_SCHEMA {
        return Err("workspace owner returned an unsupported admission receipt".to_owned());
    }
    for value in [
        &admission.session_name,
        &admission.session_incarnation,
        &admission.socket_dir,
        &admission.workspace_root,
        &admission.workspace_id,
        &admission.workspace_instance_id,
        &admission.session_id,
        &admission.build_id,
    ] {
        if value.trim().is_empty() {
            return Err("workspace owner returned an incomplete admission receipt".to_owned());
        }
    }
    validate_session_name(&admission.session_name)?;
    if admission.session_name == "vc-host" {
        return Err("workspace owner returned the enclosing host instead of a project".to_owned());
    }
    if Path::new(&admission.workspace_root)
        .canonicalize()
        .ok()
        .as_deref()
        != Some(root)
    {
        return Err("workspace owner returned a different project root".to_owned());
    }
    let expected = socket_dir
        .canonicalize()
        .map_err(|error| format!("native session namespace is unavailable: {error}"))?;
    if Path::new(&admission.socket_dir)
        .canonicalize()
        .ok()
        .as_ref()
        != Some(&expected)
    {
        return Err("workspace owner returned a different native session namespace".to_owned());
    }
    Ok(())
}

fn validate_native_destination(
    runtime: &SelectedRuntime,
    admission: &Admission,
) -> Result<(), String> {
    let socket = Path::new(&admission.socket_dir).join(&admission.session_name);
    if probe_socket_ownership(&socket) != SocketOwnership::Live {
        return Err("admitted native session is not live in its claimed namespace".to_owned());
    }
    let output = Command::new(&runtime.frame)
        .args([
            "--session",
            &admission.session_name,
            "action",
            "list-tabs",
            "--json",
        ])
        .env("VC_FRAME_SOCKET_DIR", ZELLIJ_SOCK_DIR.parent().unwrap())
        .env("ZELLIJ_SOCKET_DIR", ZELLIJ_SOCK_DIR.parent().unwrap())
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot inspect admitted native session: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "cannot inspect admitted native session: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let inventory: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid admitted native session inventory: {error}"))?;
    let tabs = inventory
        .as_array()
        .filter(|tabs| !tabs.is_empty())
        .ok_or_else(|| "admitted native session has no initial tabs".to_owned())?;
    if tabs.iter().any(|tab| {
        tab.get("session_incarnation").and_then(Value::as_str)
            != Some(admission.session_incarnation.as_str())
    }) {
        return Err("native session incarnation differs from its admission receipt".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(root: &Path, socket_dir: &Path) -> Admission {
        Admission {
            schema: ADMISSION_SCHEMA.to_owned(),
            session_name: "project-collision-safe".to_owned(),
            session_incarnation: "native-incarnation".to_owned(),
            socket_dir: socket_dir.display().to_string(),
            workspace_root: root.display().to_string(),
            workspace_id: "workspace".to_owned(),
            workspace_instance_id: "instance".to_owned(),
            session_id: "session".to_owned(),
            build_id: "build".to_owned(),
        }
    }

    #[test]
    fn admission_requires_the_selected_root_namespace_and_complete_identity() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("project");
        let socket_dir = fixture.path().join("sockets");
        let other = fixture.path().join("other");
        for directory in [&root, &socket_dir, &other] {
            std::fs::create_dir(directory).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let mut admission = receipt(&root, &socket_dir);
        validate_admission(&admission, &root, &socket_dir).unwrap();
        admission.workspace_instance_id.clear();
        assert!(validate_admission(&admission, &root, &socket_dir).is_err());
        admission.workspace_instance_id = "instance".to_owned();
        admission.workspace_root = other.display().to_string();
        assert!(validate_admission(&admission, &root, &socket_dir).is_err());
        admission.workspace_root = root.display().to_string();
        admission.socket_dir = other.display().to_string();
        assert!(validate_admission(&admission, &root, &socket_dir).is_err());
        admission.socket_dir = socket_dir.display().to_string();
        admission.session_name = "vc-host".to_owned();
        assert!(validate_admission(&admission, &root, &socket_dir).is_err());
    }

    #[test]
    fn selected_layout_remains_loadable_after_preparation_returns() {
        let fixture = tempfile::tempdir().unwrap();
        let config = fixture.path().join("config.kdl");
        std::fs::write(&config, "").unwrap();
        let opts = CliArgs {
            config: Some(config),
            ..Default::default()
        };
        let selected = serde_json::to_string(&LayoutInfo::BuiltIn("compact".to_owned())).unwrap();
        let path = retained_layout(&opts, Some(&selected)).unwrap().unwrap();
        let result = Layout::from_path_without_config(&path);
        let swap = std::fs::read_to_string(path.with_extension("swap.kdl"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert!(result.is_ok());
        assert!(swap.is_ok_and(|raw| !raw.trim().is_empty()));
    }

    #[test]
    fn absolute_selected_file_preserves_source_layout_over_same_named_fresh_config() {
        let fixture = tempfile::tempdir().unwrap();
        let source_dir = fixture.path().join("source-layouts");
        let foreign_dir = fixture.path().join("fresh-config-layouts");
        std::fs::create_dir(&source_dir).unwrap();
        std::fs::create_dir(&foreign_dir).unwrap();
        let source_path = source_dir.join("selected.kdl");
        let source_raw = "layout {\n tab name=\"source-selected\" {\n  pane command=\"/usr/bin/printf\" { args \"source-command\"; }\n }\n}\n";
        let foreign_raw = "layout {\n tab name=\"foreign-selected\" {\n  pane command=\"/usr/bin/printf\" { args \"foreign-command\"; }\n }\n}\n";
        let (_, _, compact_swap) =
            Layout::stringified_from_default_assets(Path::new("compact")).unwrap();
        let source_swap = format!("// source selected swap\n{}", compact_swap.unwrap().1);
        let foreign_swap = source_swap.replace("source selected swap", "foreign selected swap");
        std::fs::write(&source_path, source_raw).unwrap();
        std::fs::write(source_path.with_extension("swap.kdl"), &source_swap).unwrap();
        std::fs::write(foreign_dir.join("selected.kdl"), foreign_raw).unwrap();
        std::fs::write(foreign_dir.join("selected.swap.kdl"), foreign_swap).unwrap();
        let config = fixture.path().join("fresh-config.kdl");
        std::fs::write(
            &config,
            format!(
                "layout_dir {}\n",
                serde_json::to_string(&foreign_dir.to_string_lossy()).unwrap()
            ),
        )
        .unwrap();
        let opts = CliArgs {
            config: Some(config),
            ..Default::default()
        };
        let selected = LayoutInfo::File(
            source_path.to_string_lossy().into_owned(),
            Default::default(),
        );
        let selected = serde_json::to_string(&selected).unwrap();
        let retained = retained_layout(&opts, Some(&selected)).unwrap().unwrap();
        let raw = std::fs::read_to_string(&retained);
        let swap = std::fs::read_to_string(retained.with_extension("swap.kdl"));
        let loaded = Layout::from_path_without_config(&retained);
        std::fs::remove_dir_all(retained.parent().unwrap()).unwrap();
        assert_eq!(raw.unwrap(), source_raw);
        assert_eq!(swap.unwrap(), source_swap);
        let loaded = loaded.unwrap();
        assert_eq!(loaded.tabs[0].0.as_deref(), Some("source-selected"));
        assert!(
            loaded.tabs[0]
                .1
                .extract_run_instructions()
                .iter()
                .any(|run| {
                    matches!(run, Some(zellij_utils::input::layout::Run::Command(command))
                if command.command == Path::new("/usr/bin/printf")
                    && command.args == ["source-command"])
                })
        );
    }

    #[test]
    fn omitted_layout_leaves_the_selected_runtime_default_in_charge() {
        assert!(
            retained_layout(&CliArgs::default(), None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn owner_socket_base_must_resolve_to_this_exact_contract_namespace() {
        let fixture = tempfile::tempdir().unwrap();
        let base = fixture.path().join("base");
        let namespace = base.join("contract_version_1");
        let foreign = fixture.path().join("foreign");
        std::fs::create_dir_all(&namespace).unwrap();
        std::fs::create_dir_all(foreign.join("contract_version_1")).unwrap();
        assert_eq!(
            normalize_socket_namespace(base.to_str().unwrap(), &namespace).unwrap(),
            namespace.canonicalize().unwrap()
        );
        assert!(normalize_socket_namespace(foreign.to_str().unwrap(), &namespace).is_err());
        assert!(normalize_socket_namespace(namespace.to_str().unwrap(), &namespace).is_err());
        assert!(normalize_socket_namespace("relative/base", &namespace).is_err());
    }
}
