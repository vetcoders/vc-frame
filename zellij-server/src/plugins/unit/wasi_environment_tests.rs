use super::*;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use zellij_utils::data::{PermissionType, PluginCommand};
use zellij_utils::input::layout::{RunPlugin, RunPluginLocation};
use zellij_utils::plugin_api::plugin_command::{
    ProtobufGetSessionEnvironmentVariablesResponse, ProtobufPluginCommand,
};

const CHILD: &str = "VC_FRAME_WASI_TEST_CHILD";
const GUEST: &str = r#"
(module
  (import "wasi_snapshot_preview1" "environ_sizes_get" (func $sizes (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_get" (func $environ (param i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_read" (func $read (param i32 i32 i32 i32) (result i32)))
  (import "zellij" "host_run_plugin_command" (func $command))
  (memory (export "memory") 1)
  (func (export "environment_count") (result i32)
    (if (call $sizes (i32.const 0) (i32.const 4)) (then unreachable))
    (if (call $environ (i32.const 16) (i32.const 4096)) (then unreachable))
    (i32.load (i32.const 0)))
  (func (export "session_environment") (result i32)
    (call $command)
    (i32.store (i32.const 0) (i32.const 4096))
    (i32.store (i32.const 4) (i32.const 8192))
    (if (call $read (i32.const 0) (i32.const 0) (i32.const 1) (i32.const 8)) (then unreachable))
    (i32.load (i32.const 8))))
"#;

// Rust's process environment is shared across test threads. Every production
// factory/guest observation runs in a fresh child with synthetic values only.
fn isolated(test: &str, body: impl FnOnce()) {
    let test = test
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(test);
    if std::env::var(CHILD).as_deref() == Ok(test) {
        body();
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    for name in ["home", "cache", "data", "sockets"] {
        std::fs::create_dir(temp.path().join(name)).unwrap();
    }
    // Test-only re-execution of this harness; the path grants no privilege or identity.
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear()
        .env(CHILD, test)
        .env("TMPDIR", temp.path())
        .env("HOME", temp.path().join("home"))
        .env("XDG_CACHE_HOME", temp.path().join("cache"))
        .env("XDG_DATA_HOME", temp.path().join("data"))
        .env("ZELLIJ_SOCKET_DIR", temp.path().join("sockets"))
        .env("VC_FRAME_SOCKET_DIR", temp.path().join("sockets"))
        .env("VC_FRAME_PUBLIC_SENTINEL", "synthetic-public")
        .env("VC_FRAME_API_TOKEN", "synthetic-secret")
        .output()
        .unwrap();
    assert!(
        child.status.success() && String::from_utf8_lossy(&child.stdout).contains("running 1 test"),
        "isolated guest failed: {}{}",
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
}

fn session_snapshot() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "VC_FRAME_API_TOKEN".into(),
            "synthetic-session-secret".into(),
        ),
        (
            "VIBECRAFTED_RUNTIME_ROOT".into(),
            "/synthetic/runtime".into(),
        ),
        ("VIBECRAFTED_RUNTIME_BIN".into(), "/synthetic/bin".into()),
        ("VIBECRAFTED_ROOT".into(), "/synthetic/product".into()),
        ("VIBECRAFTED_CORE_DIR".into(), "/synthetic/core".into()),
        (
            "VIBECRAFTED_WORKSPACE_ID".into(),
            "synthetic-workspace".into(),
        ),
        ("VIBECRAFTED_HOME".into(), "/synthetic/crafted".into()),
        ("HOME".into(), "/synthetic/home".into()),
    ])
}

fn guest(builtin: bool) -> (Store<PluginEnv>, Instance) {
    guest_module(builtin, GUEST.as_bytes())
}

fn guest_module(builtin: bool, wasm: &[u8]) -> (Store<PluginEnv>, Instance) {
    let temp = tempfile::tempdir().unwrap();
    let stdin_pipe = Arc::new(Mutex::new(VecDeque::new()));
    let stdout_pipe = Arc::new(Mutex::new(VecDeque::new()));
    let wasi_ctx = PluginLoader::create_wasi_ctx(WasiCtxParams {
        host_dir: temp.path(),
        data_dir: temp.path(),
        cache_dir: temp.path(),
        tmp_dir: temp.path(),
        plugin_url: "synthetic-environment-probe",
        plugin_id: 1,
        stdin_pipe: stdin_pipe.clone(),
        stdout_pipe: stdout_pipe.clone(),
    })
    .unwrap();
    let plugin = PluginConfig::from_run_plugin(&RunPlugin {
        location: if builtin {
            RunPlugin::from_url("vc-frame:session-manager")
                .unwrap()
                .location
        } else {
            RunPluginLocation::File(temp.path().join("environment-probe.wasm"))
        },
        ..Default::default()
    })
    .unwrap();
    let env = PluginEnv {
        plugin_id: 1,
        client_id: 1,
        plugin,
        permissions: Arc::new(Mutex::new(None)),
        senders: ThreadSenders::default(),
        wasi_ctx,
        plugin_own_data_dir: temp.path().to_owned(),
        plugin_own_cache_dir: temp.path().to_owned(),
        tab_index: None,
        path_to_default_shell: PathBuf::from("/synthetic/shell"),
        default_shell: None,
        plugin_cwd: temp.path().to_owned(),
        session_env_vars: session_snapshot(),
        input_pipes_to_unblock: Default::default(),
        input_pipes_to_block: Default::default(),
        layout_dir: None,
        default_mode: Default::default(),
        subscriptions: Default::default(),
        keybinds: Default::default(),
        intercepting_key_presses: false,
        stdin_pipe,
        stdout_pipe,
        store_limits: create_optimized_store_limits(),
    };
    let engine = Engine::default();
    let module = Module::new(&engine, wasm).unwrap();
    let mut linker = Linker::new(&engine);
    wasmi_wasi::add_to_linker(&mut linker, |env: &mut PluginEnv| &mut env.wasi_ctx).unwrap();
    zellij_exports(&mut linker);
    let mut store = Store::new(&engine, env);
    let instance = linker.instantiate_and_start(&mut store, &module).unwrap();
    (store, instance)
}

fn session_environment(
    store: &mut Store<PluginEnv>,
    instance: Instance,
) -> BTreeMap<String, String> {
    let command: ProtobufPluginCommand = PluginCommand::GetSessionEnvironmentVariables
        .try_into()
        .unwrap();
    store
        .data()
        .stdout_pipe
        .lock()
        .unwrap()
        .extend(serde_json::to_vec(&command.encode_to_vec()).unwrap());
    let function = instance
        .get_typed_func::<(), i32>(&*store, "session_environment")
        .unwrap();
    let size = function.call(&mut *store, ()).unwrap() as usize;
    if size == 0 {
        return BTreeMap::new();
    }
    let mut response = vec![0; size];
    instance
        .get_memory(&*store, "memory")
        .unwrap()
        .read(&*store, 4096, &mut response)
        .unwrap();
    let protobuf: Vec<u8> = serde_json::from_slice(&response).unwrap();
    ProtobufGetSessionEnvironmentVariablesResponse::decode(protobuf.as_slice())
        .unwrap()
        .env_vars
        .into_iter()
        .map(|entry| (entry.name, entry.value))
        .collect()
}

#[test]
fn wasi_guest_cannot_read_host_environment() {
    isolated(
        concat!(module_path!(), "::wasi_guest_cannot_read_host_environment"),
        || {
            assert!(std::env::var_os("VC_FRAME_PUBLIC_SENTINEL").is_some());
            assert!(std::env::var_os("VC_FRAME_API_TOKEN").is_some());
            let (mut store, instance) = guest(false);
            let count = instance
                .get_typed_func::<(), i32>(&store, "environment_count")
                .unwrap()
                .call(&mut store, ())
                .unwrap();
            assert_eq!(count, 0, "WASI bypassed the session-environment permission");
        },
    );
}

#[test]
fn session_environment_api_denied_by_default_and_unrelated_permission() {
    isolated(
        concat!(
            module_path!(),
            "::session_environment_api_denied_by_default_and_unrelated_permission"
        ),
        || {
            let (mut store, instance) = guest(false);
            assert!(session_environment(&mut store, instance).is_empty());
            *store.data().permissions.lock().unwrap() =
                Some(HashSet::from([PermissionType::ReadApplicationState]));
            assert!(session_environment(&mut store, instance).is_empty());
        },
    );
}

#[test]
fn session_environment_api_grant_returns_the_session_snapshot() {
    isolated(
        concat!(
            module_path!(),
            "::session_environment_api_grant_returns_the_session_snapshot"
        ),
        || {
            let (mut store, instance) = guest(false);
            *store.data().permissions.lock().unwrap() = Some(HashSet::from([
                PermissionType::ReadSessionEnvironmentVariables,
            ]));
            assert_eq!(
                session_environment(&mut store, instance),
                session_snapshot()
            );
        },
    );
}

#[test]
fn builtins_receive_product_routing_and_artifact_context_without_wasi_env() {
    isolated(
        concat!(
            module_path!(),
            "::builtins_receive_product_routing_and_artifact_context_without_wasi_env"
        ),
        || {
            let (mut store, instance) = guest(true);
            assert_eq!(
                session_environment(&mut store, instance),
                session_snapshot()
            );
        },
    );
}

fn builtin(
    name: &str,
    configuration: BTreeMap<String, String>,
    senders: ThreadSenders,
    snapshot: BTreeMap<String, String>,
) -> (Store<PluginEnv>, Instance, Arc<AtomicUsize>) {
    let key = PathBuf::from("plugins").join(format!("{name}.wasm"));
    let wasm = &zellij_utils::consts::ASSET_MAP[&key];
    let (mut store, instance) = guest_module(true, wasm);
    let run = RunPlugin::from_url(&format!("vc-frame:{name}"))
        .unwrap()
        .with_configuration(configuration);
    store.data_mut().plugin = PluginConfig::from_run_plugin(&run).unwrap();
    store.data_mut().senders = senders;
    store.data_mut().session_env_vars = snapshot;
    let reads = Arc::new(AtomicUsize::new(0));
    store
        .data()
        .wasi_ctx
        .set_stdout(Box::new(WritePipe::new(SnapshotReadCounter {
            output: store.data().stdout_pipe.clone(),
            reads: reads.clone(),
        })));
    if let Ok(initialize) = instance.get_typed_func::<(), ()>(&store, "_initialize") {
        initialize.call(&mut store, ()).unwrap();
    }
    instance
        .get_typed_func::<(), ()>(&store, "_start")
        .unwrap()
        .call(&mut store, ())
        .unwrap();
    let configuration: ProtobufPluginConfiguration = run.configuration.try_into().unwrap();
    wasi_write_object(store.data(), &configuration.encode_to_vec()).unwrap();
    instance
        .get_typed_func::<(), ()>(&store, "load")
        .unwrap()
        .call(&mut store, ())
        .unwrap();
    store.data().stdout_pipe.lock().unwrap().clear();
    (store, instance, reads)
}

// Observe the real guest's serialized commands while preserving the production
// output pipe and host dispatcher. No runtime counters or registry are added.
struct SnapshotReadCounter {
    output: Arc<Mutex<VecDeque<u8>>>,
    reads: Arc<AtomicUsize>,
}

impl std::io::Write for SnapshotReadCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut output = self.output.lock().unwrap();
        output.extend(bytes);
        if bytes.contains(&b'\n')
            && let Ok(encoded) = serde_json::from_slice::<Vec<u8>>(output.make_contiguous())
            && let Ok(protobuf) = ProtobufPluginCommand::decode(encoded.as_slice())
            && matches!(
                PluginCommand::try_from(protobuf),
                Ok(PluginCommand::GetSessionEnvironmentVariables)
            )
        {
            self.reads.fetch_add(1, Ordering::Relaxed);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn builtin_event(
    store: &mut Store<PluginEnv>,
    instance: Instance,
    event: zellij_utils::data::Event,
) {
    let protobuf: zellij_utils::plugin_api::event::ProtobufEvent = event.try_into().unwrap();
    wasi_write_object(store.data(), &protobuf.encode_to_vec()).unwrap();
    instance
        .get_typed_func::<(), i32>(&*store, "update")
        .unwrap()
        .call(&mut *store, ())
        .unwrap();
    store.data().stdout_pipe.lock().unwrap().clear();
}

#[test]
fn real_strider_first_load_uses_authorized_artifact_fallback() {
    isolated(
        concat!(
            module_path!(),
            "::real_strider_first_load_uses_authorized_artifact_fallback"
        ),
        || {
            use crate::plugins::PluginInstruction;
            use zellij_utils::channels::{self, SenderWithContext};
            for artifacts_mode in [false, true] {
                for (snapshot, expected) in [
                    (session_snapshot(), "/synthetic/crafted/artifacts"),
                    (
                        BTreeMap::from([("HOME".into(), "/synthetic/home".into())]),
                        "/synthetic/home/.vibecrafted/artifacts",
                    ),
                ] {
                    let (sender, receiver) = channels::unbounded();
                    let mut configuration = BTreeMap::from([(
                        "caller_cwd".into(),
                        "${VIBECRAFTED_HOME}/artifacts".into(),
                    )]);
                    if artifacts_mode {
                        configuration.insert("mode".into(), "artifacts".into());
                    }
                    let (mut store, instance, reads) = builtin(
                        "strider",
                        configuration,
                        ThreadSenders {
                            to_plugin: Some(SenderWithContext::new(sender)),
                            should_silently_fail: true,
                            ..Default::default()
                        },
                        snapshot,
                    );
                    assert_eq!(
                        reads.load(Ordering::Relaxed),
                        1,
                        "one authorized snapshot per load"
                    );
                    if !artifacts_mode {
                        let paths: Vec<_> = receiver
                            .try_iter()
                            .filter_map(|(instruction, _)| match instruction {
                                PluginInstruction::ChangePluginHostDir(path, 1, 1) => Some(path),
                                _ => None,
                            })
                            .collect();
                        assert_eq!(paths, vec![PathBuf::from(expected)]);
                    }
                    instance
                        .get_typed_func::<(i32, i32), ()>(&store, "render")
                        .unwrap()
                        .call(&mut store, (20, 200))
                        .unwrap();
                    let output: Vec<_> =
                        store.data().stdout_pipe.lock().unwrap().drain(..).collect();
                    // ztext serializes displayed text as comma-separated UTF-8 bytes.
                    let displayed = expected
                        .bytes()
                        .map(|byte| byte.to_string())
                        .collect::<Vec<_>>()
                        .join(",");
                    assert!(String::from_utf8_lossy(&output).contains(&displayed));
                    assert_eq!(
                        reads.load(Ordering::Relaxed),
                        1,
                        "render must reuse the loaded projection"
                    );
                }
            }
        },
    );
}

#[test]
fn real_session_manager_first_project_uses_snapshot_product_context() {
    isolated(
        concat!(
            module_path!(),
            "::real_session_manager_first_project_uses_snapshot_product_context"
        ),
        || {
            use crate::{background_jobs::BackgroundJob, plugins::PluginInstruction};
            use zellij_utils::channels::{self, SenderWithContext};
            use zellij_utils::data::{Event, Mouse, PermissionStatus, PipeMessage, PipeSource};
            for key in [
                "VIBECRAFTED_RUNTIME_ROOT",
                "VIBECRAFTED_RUNTIME_BIN",
                "VIBECRAFTED_ROOT",
                "VIBECRAFTED_CORE_DIR",
                "VIBECRAFTED_WORKSPACE_ID",
            ] {
                let (plugin_sender, plugin_receiver) = channels::unbounded();
                let (job_sender, job_receiver) = channels::unbounded();
                let (mut store, instance, reads) = builtin(
                    "session-manager",
                    BTreeMap::from([("rail".into(), "true".into())]),
                    ThreadSenders {
                        to_plugin: Some(SenderWithContext::new(plugin_sender)),
                        to_background_jobs: Some(SenderWithContext::new(job_sender)),
                        should_silently_fail: true,
                        ..Default::default()
                    },
                    BTreeMap::from([(key.into(), String::new())]),
                );
                assert_eq!(
                    reads.load(Ordering::Relaxed),
                    1,
                    "one authorized snapshot per load"
                );
                instance
                    .get_typed_func::<(i32, i32), ()>(&store, "render")
                    .unwrap()
                    .call(&mut store, (20, 20))
                    .unwrap();
                store.data().stdout_pipe.lock().unwrap().clear();
                builtin_event(&mut store, instance, Event::Mouse(Mouse::LeftClick(1, 0)));
                let request = plugin_receiver
                    .try_iter()
                    .find_map(|(instruction, _)| match instruction {
                        PluginInstruction::MessageFromPlugin { message, .. }
                            if message.message_name == "filepicker" =>
                        {
                            message.message_args.get("request_id").cloned()
                        },
                        _ => None,
                    })
                    .expect("first rail click must open the picker");
                let message = PipeMessage::new(
                    PipeSource::Plugin(2),
                    "filepicker_result",
                    &Some("/synthetic/project".into()),
                    &Some(BTreeMap::from([("request_id".into(), request)])),
                    false,
                );
                let message: zellij_utils::plugin_api::pipe_message::ProtobufPipeMessage =
                    message.try_into().unwrap();
                wasi_write_object(store.data(), &message.encode_to_vec()).unwrap();
                instance
                    .get_typed_func::<(), i32>(&store, "pipe")
                    .unwrap()
                    .call(&mut store, ())
                    .unwrap();
                store.data().stdout_pipe.lock().unwrap().clear();
                builtin_event(
                    &mut store,
                    instance,
                    Event::PermissionRequestResult(PermissionStatus::Granted),
                );
                assert!(job_receiver.try_iter().any(|(instruction, _)| {
                    matches!(instruction, BackgroundJob::RunCommand(1, 1, _, args, _, _, _)
                        if args.starts_with(&["prepare-project".into(), "--root".into(), "/synthetic/project".into()]))
                }), "product context {key} must select the existing project owner");
                assert_eq!(
                    reads.load(Ordering::Relaxed),
                    1,
                    "render and project actions must reuse the loaded projection"
                );
            }
        },
    );
}
