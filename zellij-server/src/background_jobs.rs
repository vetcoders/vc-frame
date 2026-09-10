// Parts of these three imports are consumed only under web_server_capability;
// builds without the feature would otherwise flag them.
#[cfg_attr(not(feature = "web_server_capability"), allow(unused_imports))]
use zellij_utils::consts::{
    VERSION, ZELLIJ_SESSION_INFO_CACHE_DIR, ZELLIJ_SOCK_DIR, session_info_cache_file_name,
    session_info_folder_for_session,
};
#[cfg_attr(not(feature = "web_server_capability"), allow(unused_imports))]
use zellij_utils::data::{Event, HttpVerb, LayoutInfo, SessionInfo, WebServerStatus};
use zellij_utils::errors::{BackgroundJobContext, ContextType, prelude::*};
use zellij_utils::input::layout::RunPlugin;
#[cfg_attr(not(feature = "web_server_capability"), allow(unused_imports))]
use zellij_utils::shared::parse_base_url;

#[cfg(feature = "web_server_capability")]
use zellij_utils::web_server_commands::{
    InstructionForWebServer, WebServerResponse, discover_webserver_sockets,
    query_webserver_with_response,
};

use isahc::AsyncReadResponseExt;
use isahc::prelude::*;
use isahc::{HttpClient, Request, config::RedirectPolicy};

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::{
    fs::File,
    os::unix::{fs::OpenOptionsExt, io::AsRawFd},
};
use zellij_utils::consts::is_ipc_socket;

use crate::panes::PaneId;
use crate::plugins::{PluginId, PluginInstruction};
use crate::pty::PtyInstruction;
use crate::screen::ScreenInstruction;
use crate::thread_bus::Bus;
use crate::{ClientId, ServerInstruction};

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum BackgroundJob {
    DisplayPaneError(Vec<PaneId>, String),
    AnimatePluginLoading(u32),                       // u32 - plugin_id
    StopPluginLoadingAnimation(u32),                 // u32 - plugin_id
    ReportSessionInfo(String, SessionInfo, bool),    // String - session name, resurrection intent
    ReportPluginList(BTreeMap<PluginId, RunPlugin>), // String - session name
    ReportLayoutInfo(SessionLayoutSnapshot),
    ReadAllSessionInfosOnMachine,
    RunCommand(
        PluginId,
        ClientId,
        String,
        Vec<String>,
        BTreeMap<String, String>,
        PathBuf,
        BTreeMap<String, String>,
    ), // command, args, env_variables, cwd, context
    WebRequest(
        PluginId,
        ClientId,
        String, // url
        HttpVerb,
        BTreeMap<String, String>, // headers
        Vec<u8>,                  // body
        BTreeMap<String, String>, // context
    ),
    HighlightPanesWithMessage(Vec<PaneId>, String),
    RenderToClients,
    QueryZellijWebServerStatus,
    ClearHelpText {
        client_id: ClientId,
    },
    FlashPaneBell(Vec<PaneId>),
    StopFlashPaneBell(Vec<PaneId>),
    FlashTabBell(usize),     // usize = tab_id
    StopFlashTabBell(usize), // usize = tab_id
    Exit,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct SessionLayoutSnapshot {
    pub session_name: String,
    pub generation: u64,
    pub layout: (String, BTreeMap<String, String>),
}

#[derive(Default)]
struct SessionStatePersistenceCoordinator {
    next_generation: AtomicU64,
    latest_generations: Mutex<HashMap<String, u64>>,
}

impl SessionStatePersistenceCoordinator {
    fn reserve(&self, session_name: &str) -> Result<u64, String> {
        let previous = self
            .next_generation
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(1)
            })
            .map_err(|_| "session persistence generation space is exhausted".to_owned())?;
        let generation = previous + 1;
        let mut latest_generations = self.latest_generations.lock().map_err(|_| {
            format!(
                "session persistence coordinator is poisoned for '{}'",
                session_name
            )
        })?;
        latest_generations
            .entry(session_name.to_owned())
            .and_modify(|latest| *latest = (*latest).max(generation))
            .or_insert(generation);
        Ok(generation)
    }

    fn commit_if_current<F>(
        &self,
        session_name: &str,
        generation: u64,
        persist: F,
    ) -> Result<bool, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let latest_generations = self.latest_generations.lock().map_err(|_| {
            format!(
                "session persistence coordinator is poisoned for '{}'",
                session_name
            )
        })?;
        if latest_generations
            .get(session_name)
            .is_some_and(|latest| generation < *latest)
        {
            return Ok(false);
        }
        persist()?;
        Ok(true)
    }
}

static SESSION_STATE_PERSISTENCE: std::sync::OnceLock<SessionStatePersistenceCoordinator> =
    std::sync::OnceLock::new();

fn session_state_persistence() -> &'static SessionStatePersistenceCoordinator {
    SESSION_STATE_PERSISTENCE.get_or_init(SessionStatePersistenceCoordinator::default)
}

pub fn reserve_session_state_generation(session_name: &str) -> Result<u64, String> {
    session_state_persistence().reserve(session_name)
}

impl From<&BackgroundJob> for BackgroundJobContext {
    fn from(background_job: &BackgroundJob) -> Self {
        match *background_job {
            BackgroundJob::DisplayPaneError(..) => BackgroundJobContext::DisplayPaneError,
            BackgroundJob::AnimatePluginLoading(..) => BackgroundJobContext::AnimatePluginLoading,
            BackgroundJob::StopPluginLoadingAnimation(..) => {
                BackgroundJobContext::StopPluginLoadingAnimation
            },
            BackgroundJob::ReportSessionInfo(..) => BackgroundJobContext::ReportSessionInfo,
            BackgroundJob::ReportLayoutInfo(..) => BackgroundJobContext::ReportLayoutInfo,
            BackgroundJob::ReadAllSessionInfosOnMachine => BackgroundJobContext::ListWebSessions,
            BackgroundJob::RunCommand(..) => BackgroundJobContext::RunCommand,
            BackgroundJob::WebRequest(..) => BackgroundJobContext::WebRequest,
            BackgroundJob::ReportPluginList(..) => BackgroundJobContext::ReportPluginList,
            BackgroundJob::RenderToClients => BackgroundJobContext::ReportPluginList,
            BackgroundJob::HighlightPanesWithMessage(..) => {
                BackgroundJobContext::HighlightPanesWithMessage
            },
            BackgroundJob::QueryZellijWebServerStatus => {
                BackgroundJobContext::QueryZellijWebServerStatus
            },
            BackgroundJob::ClearHelpText { .. } => BackgroundJobContext::ClearHelpText,
            BackgroundJob::FlashPaneBell(..) => BackgroundJobContext::FlashPaneBell,
            BackgroundJob::StopFlashPaneBell(..) => BackgroundJobContext::StopFlashPaneBell,
            BackgroundJob::FlashTabBell(..) => BackgroundJobContext::FlashTabBell,
            BackgroundJob::StopFlashTabBell(..) => BackgroundJobContext::StopFlashTabBell,
            BackgroundJob::Exit => BackgroundJobContext::Exit,
        }
    }
}

static LONG_FLASH_DURATION_MS: u64 = 1000;
static FLASH_DURATION_MS: u64 = 400; // Doherty threshold
static PLUGIN_ANIMATION_OFFSET_DURATION_MD: u64 = 500;
static SESSION_METADATA_WRITE_INTERVAL_MS: u64 = 1000;
static DEFAULT_SERIALIZATION_INTERVAL: u64 = 60000;
static REPAINT_DELAY_MS: u64 = 10;
static HELP_TEXT_DEBOUNCE_DURATION: u64 = 5000;

#[derive(Clone)]
pub struct SessionScanState {
    pub current_session_name: Arc<Mutex<String>>,
    pub current_session_info: Arc<Mutex<SessionInfo>>,
    pub current_session_plugin_list: Arc<Mutex<BTreeMap<PluginId, RunPlugin>>>,
}

static SESSION_SCAN_STATE: std::sync::OnceLock<SessionScanState> = std::sync::OnceLock::new();

pub fn session_scan_state() -> Option<&'static SessionScanState> {
    SESSION_SCAN_STATE.get()
}

// web_server_base_url is read only under web_server_capability.
#[cfg_attr(not(feature = "web_server_capability"), allow(unused_variables))]
pub(crate) fn background_jobs_main(
    bus: Bus<BackgroundJob>,
    serialization_interval: Option<u64>,
    disable_session_metadata: bool,
    web_server_base_url: String,
    has_clients: Arc<AtomicBool>,
) -> Result<()> {
    let err_context = || "failed to write to pty".to_string();
    let mut running_jobs: HashMap<BackgroundJob, Instant> = HashMap::new();
    let mut loading_plugins: HashMap<u32, Arc<AtomicBool>> = HashMap::new(); // u32 - plugin_id
    let current_session_name = Arc::new(Mutex::new(String::default()));
    let current_session_info = Arc::new(Mutex::new(SessionInfo::default()));
    let current_session_is_resurrection = Arc::new(AtomicBool::new(false));
    let current_session_plugin_list: Arc<Mutex<BTreeMap<PluginId, RunPlugin>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let current_session_layout: Arc<Mutex<Option<SessionLayoutSnapshot>>> =
        Arc::new(Mutex::new(None));

    let _ = SESSION_SCAN_STATE.set(SessionScanState {
        current_session_name: current_session_name.clone(),
        current_session_info: current_session_info.clone(),
        current_session_plugin_list: current_session_plugin_list.clone(),
    });
    let last_serialization_time = Arc::new(Mutex::new(Instant::now()));
    let serialization_interval = serialization_interval.map(|s| s * 1000); // convert to
    // milliseconds
    let last_render_request: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
    let pending_help_text_clear: Arc<Mutex<HashMap<ClientId, Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut flashing_pane_bells: HashMap<PaneId, Arc<AtomicBool>> = HashMap::new();
    let mut flashing_tab_bells: HashMap<usize, Arc<AtomicBool>> = HashMap::new();

    log::info!("background_jobs_main: building http client");
    let http_client = HttpClient::builder()
        // TODO: timeout?
        .redirect_policy(RedirectPolicy::Follow)
        .build()
        .ok();
    log::info!("background_jobs_main: acquiring tokio runtime");
    // We needn't do anything with the runtime, but it should exist at this point.
    let runtime = crate::global_async_runtime::get_tokio_runtime();

    log::info!("background_jobs_main: bootstrapping session metadata job");
    let _ = bus
        .senders
        .send_to_background_jobs(BackgroundJob::ReadAllSessionInfosOnMachine);

    log::info!("background_jobs_main: entering event loop");
    loop {
        let (event, mut err_ctx) = bus.recv().with_context(err_context)?;
        err_ctx.add_call(ContextType::BackgroundJob((&event).into()));
        let job = event.clone();
        match event {
            BackgroundJob::DisplayPaneError(pane_ids, text) => {
                if job_already_running(job, &mut running_jobs) {
                    continue;
                }
                runtime.spawn({
                    let senders = bus.senders.clone();
                    async move {
                        let _ = senders.send_to_screen(
                            ScreenInstruction::AddRedPaneFrameColorOverride(
                                pane_ids.clone(),
                                Some(text),
                            ),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            LONG_FLASH_DURATION_MS,
                        ))
                        .await;
                        let _ = senders.send_to_screen(
                            ScreenInstruction::ClearPaneFrameColorOverride(pane_ids),
                        );
                    }
                });
            },
            BackgroundJob::AnimatePluginLoading(pid) => {
                let loading_plugin = Arc::new(AtomicBool::new(true));
                if job_already_running(job, &mut running_jobs) {
                    continue;
                }
                runtime.spawn({
                    let senders = bus.senders.clone();
                    let loading_plugin = loading_plugin.clone();
                    async move {
                        while loading_plugin.load(Ordering::SeqCst) {
                            let _ = senders.send_to_screen(
                                ScreenInstruction::ProgressPluginLoadingOffset(pid),
                            );
                            tokio::time::sleep(std::time::Duration::from_millis(
                                PLUGIN_ANIMATION_OFFSET_DURATION_MD,
                            ))
                            .await;
                        }
                    }
                });
                loading_plugins.insert(pid, loading_plugin);
            },
            BackgroundJob::StopPluginLoadingAnimation(pid) => {
                if let Some(loading_plugin) = loading_plugins.remove(&pid) {
                    loading_plugin.store(false, Ordering::SeqCst);
                }
            },
            BackgroundJob::ReportSessionInfo(session_name, session_info, is_resurrection) => {
                *current_session_name.lock().unwrap() = session_name;
                *current_session_info.lock().unwrap() = session_info;
                current_session_is_resurrection.store(is_resurrection, Ordering::SeqCst);
            },
            BackgroundJob::ReportPluginList(plugin_list) => {
                *current_session_plugin_list.lock().unwrap() = plugin_list;
            },
            BackgroundJob::ReportLayoutInfo(session_layout) => {
                let current_name = current_session_name.lock().unwrap().clone();
                let mut cached_layout = current_session_layout.lock().unwrap();
                let is_current_session =
                    current_name.is_empty() || session_layout.session_name == current_name;
                let is_newest_generation = cached_layout
                    .as_ref()
                    .is_none_or(|cached| session_layout.generation >= cached.generation);
                if is_current_session && is_newest_generation {
                    *cached_layout = Some(session_layout);
                }
            },
            BackgroundJob::ReadAllSessionInfosOnMachine => {
                // this job should only be run once and it keeps track of other sessions (as well
                // as this one's) infos (metadata mostly) and sends it to the screen which in turn
                // forwards it to plugins and other places it needs to be
                log::info!(
                    "ReadAllSessionInfosOnMachine received (already running: {})",
                    running_jobs.contains_key(&job)
                );
                if running_jobs.contains_key(&job) {
                    continue;
                }
                running_jobs.insert(job, Instant::now());
                runtime.spawn({
                    let senders = bus.senders.clone();
                    let current_session_info = current_session_info.clone();
                    let current_session_is_resurrection = current_session_is_resurrection.clone();
                    let current_session_name = current_session_name.clone();
                    let current_session_layout = current_session_layout.clone();
                    let current_session_plugin_list = current_session_plugin_list.clone();
                    let last_serialization_time = last_serialization_time.clone();
                    let has_clients = has_clients.clone();
                    let http_client = http_client.clone();
                    async move {
                        let mut live_runs_donor_was_degraded = false;
                        log::info!(
                            "session metadata loop started (disable_session_metadata: {})",
                            disable_session_metadata
                        );
                        loop {
                            let current_session_name =
                                current_session_name.lock().unwrap().to_string();
                            let current_session_info = current_session_info.lock().unwrap().clone();
                            let current_session_is_resurrection =
                                current_session_is_resurrection.load(Ordering::SeqCst);
                            let current_session_layout =
                                current_session_layout.lock().unwrap().clone();
                            if !disable_session_metadata {
                                let (generation, layout) = current_session_layout
                                    .filter(|snapshot| {
                                        snapshot.session_name == current_session_name
                                    })
                                    .map(|snapshot| (snapshot.generation, snapshot.layout))
                                    .unwrap_or_else(|| (0, (String::new(), BTreeMap::new())));
                                match write_session_state_to_disk(
                                    generation,
                                    current_session_name.clone(),
                                    current_session_info.clone(),
                                    layout,
                                    current_session_is_resurrection,
                                ) {
                                    Err(error) => log::error!(
                                        "Failed to durably save session '{}': {}",
                                        current_session_name,
                                        error
                                    ),
                                    Ok(false) => {},
                                    Ok(true) => {
                                        // Send SavedCurrentSession instruction to plugin thread only
                                        // after every cache file reached durable storage.
                                        let timestamp_millis = std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis()
                                            as u64;
                                        let _ = senders.send_to_plugin(
                                            PluginInstruction::UpdateSessionSaveTime(
                                                timestamp_millis,
                                            ),
                                        );
                                    },
                                }
                            }
                            let mut session_infos_on_machine = read_other_live_session_states(
                                &current_session_name,
                                &ZELLIJ_SOCK_DIR,
                                &ZELLIJ_SESSION_INFO_CACHE_DIR,
                            );
                            let current_session_plugin_list =
                                current_session_plugin_list.lock().unwrap().clone();
                            overlay_current_session_info(
                                &mut session_infos_on_machine,
                                &current_session_name,
                                &current_session_info,
                                &current_session_plugin_list,
                            );
                            let resurrectable_sessions = find_resurrectable_sessions(
                                &session_infos_on_machine,
                                &ZELLIJ_SESSION_INFO_CACHE_DIR,
                            );
                            // Vibecrafted Server is the one semantic owner of
                            // run lifecycle. VC Frame reads its configured
                            // public URL and consumes only `active_runs`; local
                            // files, PIDs, sessions, and tabs never substitute
                            // for an unavailable donor.
                            let census = if let Some(http_client) = http_client.as_ref() {
                                match crate::vc_live_runs::fetch_live_runs(http_client).await {
                                    Ok(snapshot) => {
                                        if live_runs_donor_was_degraded {
                                            log::info!("LIVE runs donor recovered");
                                        }
                                        live_runs_donor_was_degraded = false;
                                        Some(snapshot)
                                    },
                                    Err(error) => {
                                        if !live_runs_donor_was_degraded {
                                            log::warn!("LIVE runs donor degraded: {error}");
                                        }
                                        live_runs_donor_was_degraded = true;
                                        None
                                    },
                                }
                            } else {
                                if !live_runs_donor_was_degraded {
                                    log::warn!("LIVE runs donor degraded: HTTP client unavailable");
                                }
                                live_runs_donor_was_degraded = true;
                                None
                            };
                            let live_run_count =
                                census.as_ref().map(|snapshot| snapshot.runs.len());
                            let _ = senders.send_to_screen(ScreenInstruction::UpdateSessionInfos(
                                session_infos_on_machine,
                                resurrectable_sessions,
                                live_run_count,
                            ));
                            let _ = senders.send_to_pty(PtyInstruction::UpdateAndReportCwds);
                            if let Some(payload) = census.and_then(|snapshot| snapshot.payload()) {
                                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                    None,
                                    None,
                                    Event::CustomMessage(
                                        crate::vc_live_runs::VC_LIVE_RUNS_MESSAGE.to_owned(),
                                        payload,
                                    ),
                                )]));
                            }
                            if last_serialization_time
                                .lock()
                                .unwrap()
                                .elapsed()
                                .as_millis()
                                >= serialization_interval
                                    .unwrap_or(DEFAULT_SERIALIZATION_INTERVAL)
                                    .into()
                            {
                                let _ = senders.send_to_screen(
                                    ScreenInstruction::SerializeLayoutForResurrection,
                                );
                                *last_serialization_time.lock().unwrap() = Instant::now();
                            }
                            let sleep_ms = if has_clients.load(Ordering::Relaxed) {
                                SESSION_METADATA_WRITE_INTERVAL_MS
                            } else {
                                SESSION_METADATA_WRITE_INTERVAL_MS * 5 // 5s when detached
                            };
                            tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                        }
                    }
                });
            },
            BackgroundJob::RunCommand(
                plugin_id,
                client_id,
                command,
                args,
                env_variables,
                cwd,
                context,
            ) => {
                runtime.spawn({
                    let senders = bus.senders.clone();
                    async move {
                        let output = tokio::process::Command::new(&command)
                            .args(&args)
                            .envs(env_variables)
                            .current_dir(cwd)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::piped())
                            .stderr(std::process::Stdio::piped())
                            .output()
                            .await;
                        match output {
                            Ok(output) => {
                                let stdout = output.stdout.to_vec();
                                let stderr = output.stderr.to_vec();
                                let exit_code = output.status.code();
                                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(plugin_id),
                                    Some(client_id),
                                    Event::RunCommandResult(exit_code, stdout, stderr, context),
                                )]));
                            },
                            Err(e) => {
                                log::error!("Failed to run command: {}", e);
                                let stdout = vec![];
                                let stderr = format!("{}", e).as_bytes().to_vec();
                                let exit_code = Some(2);
                                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(plugin_id),
                                    Some(client_id),
                                    Event::RunCommandResult(exit_code, stdout, stderr, context),
                                )]));
                            },
                        }
                    }
                });
            },
            BackgroundJob::WebRequest(plugin_id, client_id, url, verb, headers, body, context) => {
                runtime.spawn({
                    let senders = bus.senders.clone();
                    let http_client = http_client.clone();
                    async move {
                        async fn web_request(
                            url: String,
                            verb: HttpVerb,
                            headers: BTreeMap<String, String>,
                            body: Vec<u8>,
                            http_client: HttpClient,
                        ) -> Result<
                            (u16, BTreeMap<String, String>, Vec<u8>), // status_code, headers, body
                            isahc::Error,
                        > {
                            let mut request = match verb {
                                HttpVerb::Get => Request::get(url),
                                HttpVerb::Post => Request::post(url),
                                HttpVerb::Put => Request::put(url),
                                HttpVerb::Delete => Request::delete(url),
                            };
                            for (header, value) in headers {
                                request = request.header(header.as_str(), value);
                            }
                            let mut res = if !body.is_empty() {
                                let req = request.body(body)?;
                                http_client.send_async(req).await?
                            } else {
                                let req = request.body(())?;
                                http_client.send_async(req).await?
                            };

                            let status_code = res.status();
                            let headers: BTreeMap<String, String> = res
                                .headers()
                                .iter()
                                .filter_map(|(name, value)| match value.to_str() {
                                    Ok(value) => Some((name.to_string(), value.to_string())),
                                    Err(e) => {
                                        log::error!(
                                            "Failed to convert header {:?} to string: {:?}",
                                            name,
                                            e
                                        );
                                        None
                                    },
                                })
                                .collect();
                            let body = res.bytes().await?;
                            Ok((status_code.as_u16(), headers, body))
                        }
                        let Some(http_client) = http_client else {
                            log::error!("Cannot perform http request, likely due to a misconfigured http client");
                            return;
                        };

                        match web_request(url, verb, headers, body, http_client).await {
                            Ok((status, headers, body)) => {
                                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(plugin_id),
                                    Some(client_id),
                                    Event::WebRequestResult(status, headers, body, context),
                                )]));
                            },
                            Err(e) => {
                                log::error!("Failed to send web request: {}", e);
                                let error_body = e.to_string().as_bytes().to_vec();
                                let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                    Some(plugin_id),
                                    Some(client_id),
                                    Event::WebRequestResult(
                                        400,
                                        BTreeMap::new(),
                                        error_body,
                                        context,
                                    ),
                                )]));
                            },
                        }
                    }
                });
            },
            BackgroundJob::QueryZellijWebServerStatus => {
                #[cfg(feature = "web_server_capability")]
                {
                    let status = query_webserver_via_ipc(&web_server_base_url)
                        .unwrap_or(WebServerStatus::Offline);
                    runtime.spawn({
                        let senders = bus.senders.clone();
                        let _web_server_base_url = web_server_base_url.clone();
                        async move {
                            let _ = senders.send_to_plugin(PluginInstruction::Update(vec![(
                                None,
                                None,
                                Event::WebServerStatus(status),
                            )]));
                        }
                    });
                }
            },
            BackgroundJob::RenderToClients => {
                // last_render_request being Some() represents a render request that is pending
                // last_render_request is only ever set to Some() if an async task is spawned to
                // send the actual render instruction
                //
                // given this:
                // - if last_render_request is None and we received this job, we should spawn an
                // async task to send the render instruction and log the current task time
                // - if last_render_request is Some(), it means we're currently waiting to render,
                // so we should log the render request and do nothing, once the async task has
                // finished running, it will check to see if the render time was updated while it
                // was running, and if so send this instruction again so the process can start anew
                let (should_run_task, current_time) = {
                    let mut last_render_request = last_render_request.lock().unwrap();
                    let should_run_task = last_render_request.is_none();
                    let current_time = Instant::now();
                    *last_render_request = Some(current_time);
                    (should_run_task, current_time)
                };
                if should_run_task {
                    runtime.spawn({
                        let senders = bus.senders.clone();
                        let last_render_request = last_render_request.clone();
                        let task_start_time = current_time;
                        async move {
                            tokio::time::sleep(std::time::Duration::from_millis(REPAINT_DELAY_MS))
                                .await;
                            let _ = senders.send_to_screen(ScreenInstruction::RenderToClients);
                            {
                                let mut last_render_request = last_render_request.lock().unwrap();
                                if let Some(last_render_request) = *last_render_request
                                    && last_render_request > task_start_time
                                {
                                    // another render request was received while we were
                                    // sleeping, schedule this job again so that we can also
                                    // render that request
                                    let _ = senders
                                        .send_to_background_jobs(BackgroundJob::RenderToClients);
                                }
                                // reset the last_render_request so that the task will be spawned
                                // again once a new request is received
                                *last_render_request = None;
                            }
                        }
                    });
                }
            },
            BackgroundJob::HighlightPanesWithMessage(pane_ids, text) => {
                if job_already_running(job, &mut running_jobs) {
                    continue;
                }
                runtime.spawn({
                    let senders = bus.senders.clone();
                    async move {
                        let _ = senders.send_to_screen(
                            ScreenInstruction::AddHighlightPaneFrameColorOverride(
                                pane_ids.clone(),
                                Some(text),
                            ),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(FLASH_DURATION_MS))
                            .await;
                        let _ = senders.send_to_screen(
                            ScreenInstruction::ClearPaneFrameColorOverride(pane_ids),
                        );
                    }
                });
            },
            BackgroundJob::ClearHelpText { client_id } => {
                let should_spawn = {
                    let mut pending = pending_help_text_clear.lock().unwrap();
                    let current_time = Instant::now();
                    let should_spawn = !pending.contains_key(&client_id);
                    pending.insert(client_id, current_time);
                    should_spawn
                };

                if should_spawn {
                    runtime.spawn({
                        let senders = bus.senders.clone();
                        let pending = pending_help_text_clear.clone();
                        let debounce_duration = Duration::from_millis(HELP_TEXT_DEBOUNCE_DURATION);
                        async move {
                            tokio::time::sleep(debounce_duration).await;
                            loop {
                                let next_sleep_duration = {
                                    let mut pending = pending.lock().unwrap();
                                    match pending.get(&client_id) {
                                        Some(&last_motion_time) => {
                                            let time_since_motion =
                                                Instant::now().duration_since(last_motion_time);
                                            if time_since_motion >= debounce_duration {
                                                pending.remove(&client_id);
                                                None
                                            } else {
                                                let remaining = debounce_duration
                                                    .saturating_sub(time_since_motion);
                                                Some(remaining)
                                            }
                                        },
                                        None => break,
                                    }
                                };

                                match next_sleep_duration {
                                    Some(duration) => {
                                        tokio::time::sleep(duration).await;
                                    },
                                    None => {
                                        let _ = senders.send_to_server(
                                            ServerInstruction::ClearMouseHelpText(client_id),
                                        );
                                        break;
                                    },
                                }
                            }
                        }
                    });
                }
            },
            BackgroundJob::FlashPaneBell(pane_ids) => {
                let is_flashing = Arc::new(AtomicBool::new(true));
                for &pane_id in &pane_ids {
                    flashing_pane_bells.insert(pane_id, is_flashing.clone());
                }
                runtime.spawn({
                    let senders = bus.senders.clone();
                    let pane_ids_clone = pane_ids.clone();
                    let flag = is_flashing.clone();
                    async move {
                        let _ = senders.send_to_screen(
                            ScreenInstruction::AddHighlightPaneFrameColorOverride(
                                pane_ids_clone.clone(),
                                None,
                            ),
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(FLASH_DURATION_MS))
                            .await;
                        if flag.load(Ordering::SeqCst) {
                            let _ = senders.send_to_screen(
                                ScreenInstruction::ClearPaneFrameColorOverride(pane_ids_clone),
                            );
                        }
                    }
                });
            },
            BackgroundJob::StopFlashPaneBell(pane_ids) => {
                for &pane_id in &pane_ids {
                    if let Some(flag) = flashing_pane_bells.remove(&pane_id) {
                        flag.store(false, Ordering::SeqCst);
                    }
                }
                let _ = bus
                    .senders
                    .send_to_screen(ScreenInstruction::ClearPaneFrameColorOverride(pane_ids));
            },
            BackgroundJob::FlashTabBell(tab_id) => {
                let is_flashing = Arc::new(AtomicBool::new(true));
                flashing_tab_bells.insert(tab_id, is_flashing.clone());
                runtime.spawn({
                    let senders = bus.senders.clone();
                    let flag = is_flashing.clone();
                    async move {
                        let _ = senders
                            .send_to_screen(ScreenInstruction::SetTabBellFlash(tab_id, true));
                        tokio::time::sleep(std::time::Duration::from_millis(FLASH_DURATION_MS))
                            .await;
                        if flag.load(Ordering::SeqCst) {
                            let _ = senders
                                .send_to_screen(ScreenInstruction::SetTabBellFlash(tab_id, false));
                        }
                    }
                });
            },
            BackgroundJob::StopFlashTabBell(tab_id) => {
                if let Some(flag) = flashing_tab_bells.remove(&tab_id) {
                    flag.store(false, Ordering::SeqCst);
                }
                let _ = bus
                    .senders
                    .send_to_screen(ScreenInstruction::SetTabBellFlash(tab_id, false));
            },
            BackgroundJob::Exit => {
                for loading_plugin in loading_plugins.values() {
                    loading_plugin.store(false, Ordering::SeqCst);
                }

                let cache_file_name =
                    session_info_cache_file_name(&current_session_name.lock().unwrap().to_owned());
                let _ = std::fs::remove_file(cache_file_name);
                return Ok(());
            },
        }
    }
}

fn job_already_running(
    job: BackgroundJob,
    running_jobs: &mut HashMap<BackgroundJob, Instant>,
) -> bool {
    match running_jobs.get_mut(&job) {
        Some(current_running_job_start_time) => {
            if current_running_job_start_time.elapsed()
                > Duration::from_millis(LONG_FLASH_DURATION_MS)
            {
                *current_running_job_start_time = Instant::now();
                false
            } else {
                true
            }
        },
        None => {
            running_jobs.insert(job.clone(), Instant::now());
            false
        },
    }
}

fn file_content_changed(path: &std::path::Path, new_content: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(existing) => existing != new_content,
        Err(_) => true,
    }
}

fn sync_parent_directory(path: &Path) -> Result<(), String> {
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .ok_or_else(|| format!("cache path has no parent: {}", path.display()))?;
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| {
                format!(
                    "cannot sync cache directory {}: {}",
                    parent.display(),
                    error
                )
            })?;
    }
    Ok(())
}

fn write_file_durably(path: &Path, contents: &[u8]) -> Result<(), String> {
    write_cache_file_durably(path, contents, false)
}

fn write_cache_file_durably(path: &Path, contents: &[u8], immutable: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("cache path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| {
        format!(
            "cannot create cache directory {}: {}",
            parent.display(),
            error
        )
    })?;

    let unchanged = if immutable {
        match std::fs::read(path) {
            Ok(existing) if existing == contents => true,
            Ok(_) => {
                return Err(format!(
                    "immutable pane content mismatch: {}",
                    path.display()
                ));
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(format!(
                    "cannot read immutable pane content {}: {}",
                    path.display(),
                    error
                ));
            },
        }
    } else {
        !file_content_changed(path, contents)
    };
    if unchanged {
        std::fs::File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| format!("cannot sync cache file {}: {}", path.display(), error))?;
        return sync_parent_directory(path);
    }

    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        format!(
            "cannot create temporary cache file in {}: {}",
            parent.display(),
            error
        )
    })?;
    temporary.write_all(contents).map_err(|error| {
        format!(
            "cannot write temporary cache file for {}: {}",
            path.display(),
            error
        )
    })?;
    temporary.as_file_mut().sync_all().map_err(|error| {
        format!(
            "cannot sync temporary cache file for {}: {}",
            path.display(),
            error
        )
    })?;
    // Never replace an immutable name, including a file created after our
    // initial read. A concurrent creator causes a retry, not an overwrite.
    let publication = if immutable {
        temporary.persist_noclobber(path)
    } else {
        temporary.persist(path)
    };
    publication.map_err(|error| {
        format!(
            "cannot atomically replace cache file {}: {}",
            path.display(),
            error.error
        )
    })?;
    sync_parent_directory(path)
}

pub fn write_session_state_to_disk(
    generation: u64,
    current_session_name: String,
    current_session_info: SessionInfo,
    current_session_layout: (String, BTreeMap<String, String>),
    is_resurrection: bool,
) -> Result<bool, String> {
    write_session_state_to_disk_with_resurrection(
        session_state_persistence(),
        &session_info_folder_for_session(&current_session_name),
        generation,
        current_session_name,
        current_session_info,
        current_session_layout,
        is_resurrection,
    )
}

struct SessionStateWrite<'a> {
    persistence: &'a SessionStatePersistenceCoordinator,
    session_info_folder: &'a Path,
    generation: u64,
    current_session_name: String,
    current_session_info: SessionInfo,
    current_session_layout: (String, BTreeMap<String, String>),
    is_resurrection: bool,
}

fn durable_session_state_writer(
    path: &Path,
    contents: &[u8],
    immutable: bool,
) -> Result<(), String> {
    if immutable {
        write_cache_file_durably(path, contents, true)
    } else {
        write_file_durably(path, contents)
    }
}

// Keep the real writer injectable so publication regressions use a private
// temporary directory and coordinator, never a live session or Founder cache.
#[cfg(test)]
fn write_session_state_to_disk_in(
    persistence: &SessionStatePersistenceCoordinator,
    session_info_folder: &Path,
    generation: u64,
    current_session_name: String,
    current_session_info: SessionInfo,
    current_session_layout: (String, BTreeMap<String, String>),
) -> Result<bool, String> {
    write_session_state_to_disk_with_writer(
        SessionStateWrite {
            persistence,
            session_info_folder,
            generation,
            current_session_name,
            current_session_info,
            current_session_layout,
            is_resurrection: false,
        },
        durable_session_state_writer,
    )
}

fn write_session_state_to_disk_with_writer<F>(
    state: SessionStateWrite<'_>,
    mut write: F,
) -> Result<bool, String>
where
    F: FnMut(&Path, &[u8], bool) -> Result<(), String>,
{
    let SessionStateWrite {
        persistence,
        session_info_folder,
        generation,
        current_session_name,
        current_session_info,
        current_session_layout,
        is_resurrection,
    } = state;
    let mut current_session_info = current_session_info;
    persistence.commit_if_current(&current_session_name, generation, || {
        std::fs::create_dir_all(session_info_folder).map_err(|error| {
            format!(
                "cannot create session cache directory {}: {}",
                session_info_folder.display(),
                error
            )
        })?;

        let metadata_cache_file_name = session_info_folder.join("session-metadata.kdl");
        let previous = fs::read_to_string(&metadata_cache_file_name)
            .ok()
            .and_then(|raw| SessionInfo::from_string(&raw, &current_session_name).ok());
        // A periodic write retains only this exact server incarnation. A new
        // same-name server gets a new slot unless the client supplied the
        // explicit resurrection intent carried through CliAssets.
        current_session_info.rail_order = previous
            .as_ref()
            .filter(|info| {
                info.rail_order > 0
                    && (info.session_incarnation == current_session_info.session_incarnation
                        || is_resurrection)
            })
            .map(|info| info.rail_order)
            .unwrap_or(reserve_rail_order(session_info_folder)?);
        let (current_session_layout, layout_files_to_write) = current_session_layout;
        let new_metadata = current_session_info.to_string();
        write(&metadata_cache_file_name, new_metadata.as_bytes(), false)?;

        if !current_session_layout.is_empty() {
            for (external_file_name, external_file_contents) in layout_files_to_write {
                let expected_name = format!(
                    "pane_contents_sha256_{}",
                    zellij_utils::asset_integrity::sha256_hex(external_file_contents.as_bytes())
                );
                if external_file_name != expected_name {
                    return Err(format!(
                        "invalid immutable pane content name: {}",
                        external_file_name
                    ));
                }
                let external_file_path = session_info_folder.join(&external_file_name);
                write(&external_file_path, external_file_contents.as_bytes(), true)?;
            }
            // The layout is the resurrection commit point. Publish it only after
            // every referenced external pane-content file is durable.
            let layout_cache_file_name = session_info_folder.join("session-layout.kdl");
            write(
                &layout_cache_file_name,
                current_session_layout.as_bytes(),
                false,
            )?;
        }
        Ok(())
    })
}

fn write_session_state_to_disk_with_resurrection(
    persistence: &SessionStatePersistenceCoordinator,
    session_info_folder: &Path,
    generation: u64,
    current_session_name: String,
    current_session_info: SessionInfo,
    current_session_layout: (String, BTreeMap<String, String>),
    is_resurrection: bool,
) -> Result<bool, String> {
    write_session_state_to_disk_with_writer(
        SessionStateWrite {
            persistence,
            session_info_folder,
            generation,
            current_session_name,
            current_session_info,
            current_session_layout,
            is_resurrection,
        },
        durable_session_state_writer,
    )
}

fn reserve_rail_order(session_info_folder: &Path) -> Result<u64, String> {
    let root = session_info_folder.parent().ok_or_else(|| {
        format!(
            "session cache folder has no allocator root: {}",
            session_info_folder.display()
        )
    })?;
    fs::create_dir_all(root).map_err(|error| {
        format!(
            "cannot create rail allocator root {}: {error}",
            root.display()
        )
    })?;
    let lock = root.join(".rail-order.lock");
    let _lock_file = acquire_rail_order_lock(&lock)?;
    (|| {
        let high_water = root.join(".rail-order.high-water");
        let persisted = fs::read_to_string(&high_water)
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_default();
        let observed = fs::read_dir(root)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                fs::read_to_string(entry.ok()?.path().join("session-metadata.kdl")).ok()
            })
            .filter_map(|raw| SessionInfo::from_string(&raw, "").ok())
            .map(|info| info.rail_order)
            .max()
            .unwrap_or_default();
        let order = persisted
            .max(observed)
            .checked_add(1)
            .ok_or_else(|| "rail order space is exhausted".to_owned())?;
        write_file_durably(&high_water, order.to_string().as_bytes())?;
        Ok(order)
    })()
}

#[cfg(unix)]
fn acquire_rail_order_lock(lock: &Path) -> Result<File, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(lock)
        .map_err(|error| {
            format!(
                "cannot open rail allocator lock {}: {error}",
                lock.display()
            )
        })?;
    if !file
        .metadata()
        .map_err(|error| {
            format!(
                "cannot inspect rail allocator lock {}: {error}",
                lock.display()
            )
        })?
        .is_file()
    {
        return Err(format!(
            "rail allocator lock is not a regular file: {}",
            lock.display()
        ));
    }
    // SAFETY: `file` owns the descriptor for the entire allocator critical section.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(format!(
            "cannot acquire rail allocator lock {}: {}",
            lock.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_rail_order_lock(lock: &Path) -> Result<std::fs::File, String> {
    Err(format!(
        "rail allocator lock requires advisory file locks on this platform: {}",
        lock.display()
    ))
}

pub fn scan_session_list(
    current_session_name: &str,
    available_layouts: &[LayoutInfo],
    current_session_plugin_list: &BTreeMap<PluginId, RunPlugin>,
    sock_dir: &Path,
    session_info_cache_dir: &Path,
) -> (BTreeMap<String, SessionInfo>, BTreeMap<String, Duration>) {
    let mut session_infos_on_machine =
        read_other_live_session_states(current_session_name, sock_dir, session_info_cache_dir);
    for (name, info) in session_infos_on_machine.iter_mut() {
        if name == current_session_name {
            info.populate_plugin_list(current_session_plugin_list.clone());
            info.available_layouts = available_layouts.to_vec();
        }
    }
    let resurrectable_sessions =
        find_resurrectable_sessions(&session_infos_on_machine, session_info_cache_dir);
    (session_infos_on_machine, resurrectable_sessions)
}

pub(crate) fn overlay_current_session_info(
    session_infos: &mut BTreeMap<String, SessionInfo>,
    current_session_name: &str,
    current_session_info: &SessionInfo,
    current_session_plugin_list: &BTreeMap<PluginId, RunPlugin>,
) {
    if current_session_info.name == current_session_name {
        // Preserve the oldest authoritative launch age. A recovered socket
        // has a fresh filesystem timestamp, while the in-process snapshot
        // still knows how long the session has actually lived.
        let scanned_creation_time = session_infos
            .get(current_session_name)
            .map(|session_info| session_info.creation_time)
            .unwrap_or_default();
        let mut live_current_session = current_session_info.clone();
        live_current_session.name = current_session_name.to_string();
        live_current_session.is_current_session = true;
        live_current_session.creation_time =
            scanned_creation_time.max(current_session_info.creation_time);
        live_current_session.populate_plugin_list(current_session_plugin_list.clone());
        session_infos.insert(current_session_name.to_string(), live_current_session);
    }
}

pub fn scan_session_list_default_dirs(
    current_session_name: &str,
    available_layouts: &[LayoutInfo],
    current_session_plugin_list: &BTreeMap<PluginId, RunPlugin>,
) -> (BTreeMap<String, SessionInfo>, BTreeMap<String, Duration>) {
    scan_session_list(
        current_session_name,
        available_layouts,
        current_session_plugin_list,
        &ZELLIJ_SOCK_DIR,
        &ZELLIJ_SESSION_INFO_CACHE_DIR,
    )
}

fn read_other_live_session_states(
    current_session_name: &str,
    sock_dir: &Path,
    session_info_cache_dir: &Path,
) -> BTreeMap<String, SessionInfo> {
    let mut other_session_names: Vec<(String, Duration)> = vec![];
    let mut session_infos_on_machine = BTreeMap::new();
    // we do this so that the session infos will be actual and we're
    // reasonably sure their session is running
    if let Ok(files) = fs::read_dir(sock_dir) {
        files.for_each(|file| {
            if let Ok(file) = file
                && let Ok(file_name) = file.file_name().into_string()
                && is_ipc_socket(&file.file_type().unwrap())
            {
                let creation_time = std::fs::metadata(file.path())
                    .ok()
                    .and_then(|f| f.created().ok().or_else(|| f.modified().ok()))
                    .and_then(|d| d.elapsed().ok())
                    .unwrap_or_default();
                other_session_names.push((file_name, creation_time));
            }
        });
    }

    for (session_name, creation_time) in other_session_names {
        let session_cache_file_name = session_info_cache_dir
            .join(&session_name)
            .join("session-metadata.kdl");
        let mut session_info = fs::read_to_string(&session_cache_file_name)
            .ok()
            .and_then(|raw_session_info| {
                SessionInfo::from_string(&raw_session_info, current_session_name).ok()
            })
            .unwrap_or_else(|| SessionInfo::new(session_name.clone()));
        session_info.creation_time = creation_time;
        session_info.is_current_session = session_name == current_session_name;
        session_infos_on_machine.insert(session_name, session_info);
    }
    session_infos_on_machine
}

fn find_resurrectable_sessions(
    session_infos_on_machine: &BTreeMap<String, SessionInfo>,
    session_info_cache_dir: &Path,
) -> BTreeMap<String, Duration> {
    match fs::read_dir(session_info_cache_dir) {
        Ok(files_in_session_info_folder) => {
            let files_that_are_folders = files_in_session_info_folder
                .filter_map(|f| f.ok().map(|f| f.path()))
                .filter(|f| f.is_dir());
            files_that_are_folders
                .filter_map(|folder_name| {
                    let session_name = folder_name.file_name()?.to_str()?.to_owned();
                    if session_infos_on_machine.contains_key(&session_name) {
                        // this is not a dead session...
                        return None;
                    }
                    let layout_file_name = folder_name.join("session-layout.kdl");
                    let ctime = match std::fs::metadata(&layout_file_name)
                        .and_then(|metadata| metadata.created())
                    {
                        Ok(created) => Some(created),
                        Err(e) => {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                return None; // no layout file, cannot resurrect session, let's not
                            // list it
                            } else {
                                log::error!(
                                    "Failed to read created stamp of resurrection file: {:?}",
                                    e
                                );
                            }
                            None
                        },
                    };
                    let elapsed_duration = ctime
                        .and_then(|ctime| ctime.elapsed().ok())
                        .unwrap_or_default();
                    Some((session_name, elapsed_duration))
                })
                .collect()
        },
        Err(e) => {
            log::error!("Failed to read session info cache dir: {:?}", e);
            BTreeMap::new()
        },
    }
}

#[cfg(feature = "web_server_capability")]
fn query_webserver_via_ipc(web_server_base_url: &str) -> Result<WebServerStatus> {
    let expected_addr =
        parse_base_url(web_server_base_url).context("Failed to parse web server base URL")?;

    let sockets = discover_webserver_sockets().context("Failed to discover web server sockets")?;

    if sockets.is_empty() {
        return Ok(WebServerStatus::Offline);
    }

    for socket_path in sockets {
        let path_str = socket_path.to_str().unwrap_or("");

        match query_webserver_with_response(path_str, InstructionForWebServer::QueryVersion, 500) {
            Ok(WebServerResponse::Version(info)) => {
                let matches_expected =
                    info.ip == expected_addr.ip && info.port == expected_addr.port;

                if !matches_expected {
                    continue;
                }

                if info.version == VERSION {
                    return Ok(WebServerStatus::Online(web_server_base_url.to_string()));
                } else {
                    return Ok(WebServerStatus::DifferentVersion(info.version));
                }
            },
            Err(_) => continue,
        }
    }

    Ok(WebServerStatus::Offline)
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::tempdir;
    use zellij_utils::data::{PaneInfo, PaneManifest, SessionInfo};

    fn make_socket(dir: &std::path::Path, name: &str) -> UnixListener {
        UnixListener::bind(dir.join(name)).expect("bind unix socket")
    }

    fn write_metadata(info_dir: &std::path::Path, session: &str, info: &SessionInfo) {
        let folder = info_dir.join(session);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("session-metadata.kdl"), info.to_string()).unwrap();
    }

    fn write_layout(info_dir: &std::path::Path, session: &str) {
        let folder = info_dir.join(session);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("session-layout.kdl"), "layout { }").unwrap();
    }

    #[test]
    fn durable_cache_write_atomically_replaces_existing_contents() {
        let root = tempdir().unwrap();
        let target = root.path().join("session-layout.kdl");
        std::fs::write(&target, "old layout").unwrap();

        write_file_durably(&target, b"new durable layout").unwrap();

        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "new durable layout"
        );
    }

    #[test]
    fn durable_cache_write_reports_failure_without_destroying_the_target() {
        let root = tempdir().unwrap();
        let target = root.path().join("session-layout.kdl");
        std::fs::create_dir(&target).unwrap();
        let marker = target.join("old-cache-marker");
        std::fs::write(&marker, "still here").unwrap();

        let error = write_file_durably(&target, b"replacement").unwrap_err();

        assert!(error.contains("cannot atomically replace cache file"));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "still here");
    }

    fn checkpoint_capture(
        contents: &str,
        incomplete: bool,
    ) -> zellij_utils::session_serialization::GlobalLayoutManifest {
        use zellij_utils::pane_size::{Dimension, PaneGeom};
        use zellij_utils::session_serialization::{
            GlobalLayoutManifest, PaneLayoutManifest, TabLayoutManifest,
        };
        let pane = PaneLayoutManifest {
            geom: PaneGeom {
                rows: Dimension::fixed(10),
                cols: Dimension::fixed(10),
                ..Default::default()
            },
            pane_contents: Some(contents.to_owned()),
            ..Default::default()
        };
        let first = TabLayoutManifest {
            tab_instance_id: "first-id".into(),
            tiled_panes: vec![pane.clone()],
            ..Default::default()
        };
        let mut second = first.clone();
        second.tab_instance_id = "second-id".into();
        if incomplete {
            let mut displaced = pane;
            displaced.geom.x = 20; // missing columns 10..20: undecomposable
            second.tiled_panes.push(displaced);
        }
        GlobalLayoutManifest {
            tabs: vec![("first".to_owned(), first), ("second".to_owned(), second)],
            ..Default::default()
        }
    }

    fn assert_checkpoint_panes(root: &Path, layout: &str, expected_contents: &str) {
        use zellij_utils::input::layout::Layout;
        let parsed = Layout::from_kdl(
            layout,
            Some(root.join("session-layout.kdl").display().to_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(parsed.tabs.len(), 2);
        for (index, (name, tiled, floating)) in parsed.tabs.iter().enumerate() {
            assert_eq!(
                name.as_deref(),
                Some(if index == 0 { "first" } else { "second" })
            );
            assert_eq!(
                tiled.tab_instance_id.as_deref(),
                Some(if index == 0 { "first-id" } else { "second-id" })
            );
            assert!(floating.is_empty());
            assert_eq!(tiled.children.len(), 1);
            let pane = &tiled.children[0];
            assert!(pane.children.is_empty());
            assert_eq!(
                pane.pane_initial_contents.as_deref(),
                Some(expected_contents)
            );
        }
    }

    #[test]
    fn interrupted_snapshot_preserves_previous_content() {
        use crate::pty::serialize_session_layout_for_save;
        let root = tempdir().unwrap();
        let persistence = SessionStatePersistenceCoordinator::default();
        let session = "interrupted-publication";
        let info = SessionInfo::new(session.to_owned());
        let a =
            serialize_session_layout_for_save(checkpoint_capture("A bytes", false), None).unwrap();
        let a_generation = persistence.reserve(session).unwrap();
        assert!(
            write_session_state_to_disk_in(
                &persistence,
                root.path(),
                a_generation,
                session.to_owned(),
                info.clone(),
                a.clone()
            )
            .unwrap()
        );
        assert_checkpoint_panes(root.path(), &a.0, "A bytes");
        assert_eq!(a.1.len(), 1, "two panes, one immutable blob");
        let old_layout = fs::read(root.path().join("session-layout.kdl")).unwrap();
        let old_contents: BTreeMap<_, _> =
            a.1.keys()
                .map(|name| (name.clone(), fs::read(root.path().join(name)).unwrap()))
                .collect();

        let b =
            serialize_session_layout_for_save(checkpoint_capture("B bytes", false), None).unwrap();
        assert_eq!(b.1.len(), 1);
        assert!(b.1.keys().all(|name| !a.1.contains_key(name)));
        let b_generation = persistence.reserve(session).unwrap();
        let mut reached_publication = false;
        let error = write_session_state_to_disk_with_writer(
            SessionStateWrite {
                persistence: &persistence,
                session_info_folder: root.path(),
                generation: b_generation,
                current_session_name: session.to_owned(),
                current_session_info: info.clone(),
                current_session_layout: b.clone(),
                is_resurrection: false,
            },
            |path, bytes, immutable| {
                if path.file_name().unwrap() == "session-layout.kdl" {
                    reached_publication = true;
                    for (name, contents) in &b.1 {
                        assert_eq!(
                            fs::read(root.path().join(name)).unwrap(),
                            contents.as_bytes()
                        );
                    }
                    return Err("injected before layout publication".into());
                }
                write_cache_file_durably(path, bytes, immutable)
            },
        )
        .unwrap_err();
        assert!(
            reached_publication,
            "all B blobs must be durable before injection"
        );
        assert_eq!(error, "injected before layout publication");
        assert_eq!(
            fs::read(root.path().join("session-layout.kdl")).unwrap(),
            old_layout
        );
        for (name, bytes) in &old_contents {
            assert_eq!(fs::read(root.path().join(name)).unwrap(), *bytes);
        }
        assert_checkpoint_panes(root.path(), &a.0, "A bytes");
        let mut stale_write = false;
        assert!(
            !write_session_state_to_disk_with_writer(
                SessionStateWrite {
                    persistence: &persistence,
                    session_info_folder: root.path(),
                    generation: a_generation,
                    current_session_name: session.to_owned(),
                    current_session_info: info.clone(),
                    current_session_layout: a.clone(),
                    is_resurrection: false,
                },
                |_, _, _| {
                    stale_write = true;
                    Ok(())
                }
            )
            .unwrap()
        );
        assert!(
            !stale_write,
            "stale generation must never reach any file writer"
        );

        // Retry the same generation with its already-durable blobs, then repeat
        // unchanged bytes in a new generation. Neither operation rewrites A.
        assert!(
            write_session_state_to_disk_in(
                &persistence,
                root.path(),
                b_generation,
                session.to_owned(),
                info.clone(),
                b.clone()
            )
            .unwrap()
        );
        let repeated_generation = persistence.reserve(session).unwrap();
        assert!(
            write_session_state_to_disk_in(
                &persistence,
                root.path(),
                repeated_generation,
                session.to_owned(),
                info,
                b.clone()
            )
            .unwrap()
        );
        assert_eq!(
            fs::read_to_string(root.path().join("session-layout.kdl")).unwrap(),
            b.0
        );
        assert_checkpoint_panes(root.path(), &b.0, "B bytes");
        for (name, bytes) in &old_contents {
            assert_eq!(fs::read(root.path().join(name)).unwrap(), *bytes);
        }
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter(|entry| entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pane_contents_sha256_"))
                .count(),
            2,
            "only the A and B blobs; no orphan deletion"
        );
    }

    #[test]
    fn corrupt_immutable_content_rejects_publication_without_repair() {
        use crate::pty::serialize_session_layout_for_save;
        let root = tempdir().unwrap();
        let persistence = SessionStatePersistenceCoordinator::default();
        let session = "corrupt-immutable";
        let info = SessionInfo::new(session.into());
        let a = serialize_session_layout_for_save(checkpoint_capture("A", false), None).unwrap();
        assert!(
            write_session_state_to_disk_in(
                &persistence,
                root.path(),
                persistence.reserve(session).unwrap(),
                session.into(),
                info.clone(),
                a.clone()
            )
            .unwrap()
        );
        let b = serialize_session_layout_for_save(checkpoint_capture("B", false), None).unwrap();
        let name = b.1.keys().next().unwrap().clone();
        fs::write(root.path().join(&name), "corrupt bytes").unwrap();
        let error = write_session_state_to_disk_in(
            &persistence,
            root.path(),
            persistence.reserve(session).unwrap(),
            session.into(),
            info,
            b,
        )
        .unwrap_err();
        assert!(error.contains("immutable pane content mismatch"));
        assert_eq!(
            fs::read_to_string(root.path().join(&name)).unwrap(),
            "corrupt bytes"
        );
        assert_eq!(
            fs::read_to_string(root.path().join("session-layout.kdl")).unwrap(),
            a.0
        );
        assert_checkpoint_panes(root.path(), &a.0, "A");
    }

    #[test]
    fn legacy_checkpoint_contents_survive_new_publication() {
        use crate::pty::serialize_session_layout_for_save;
        let root = tempdir().unwrap();
        let legacy = "layout { tab name=\"old\" { pane contents_file=\"initial_contents_1\"; }; }";
        fs::write(root.path().join("session-layout.kdl"), legacy).unwrap();
        fs::write(root.path().join("initial_contents_1"), "legacy bytes").unwrap();
        let read_legacy = || {
            let parsed = zellij_utils::input::layout::Layout::from_kdl(
                legacy,
                Some(root.path().join("session-layout.kdl").display().to_string()),
                None,
                None,
            )
            .unwrap();
            assert_eq!(
                parsed.tabs[0].1.children[0]
                    .pane_initial_contents
                    .as_deref(),
                Some("legacy bytes")
            );
        };
        read_legacy();
        let persistence = SessionStatePersistenceCoordinator::default();
        let next = serialize_session_layout_for_save(checkpoint_capture("new bytes", false), None)
            .unwrap();
        assert!(
            write_session_state_to_disk_in(
                &persistence,
                root.path(),
                persistence.reserve("legacy").unwrap(),
                "legacy".into(),
                SessionInfo::new("legacy".into()),
                next.clone()
            )
            .unwrap()
        );
        read_legacy();
        assert_checkpoint_panes(root.path(), &next.0, "new bytes");
        assert_eq!(
            fs::read_to_string(root.path().join("initial_contents_1")).unwrap(),
            "legacy bytes"
        );
    }

    #[test]
    fn incomplete_snapshot_preserves_previous_checkpoint() {
        use crate::pty::serialize_session_layout_for_save;
        use crate::route::NotificationEnd;

        // Exercise the explicit-save and periodic-capture admission used by
        // PTY, followed by the same writer used by both publication paths.
        for explicit_save in [true, false] {
            let root = tempdir().unwrap();
            let persistence = SessionStatePersistenceCoordinator::default();
            let session = "private-checkpoint-fixture";
            let info = SessionInfo {
                name: session.to_owned(),
                ..Default::default()
            };
            let original_generation = persistence.reserve(session).unwrap();
            let original =
                serialize_session_layout_for_save(checkpoint_capture("original", false), None)
                    .unwrap();
            assert_eq!(original.1.len(), 1, "identical panes share immutable bytes");
            assert!(
                write_session_state_to_disk_in(
                    &persistence,
                    root.path(),
                    original_generation,
                    session.to_owned(),
                    info.clone(),
                    original.clone()
                )
                .unwrap()
            );
            let layout_path = root.path().join("session-layout.kdl");
            let metadata_path = root.path().join("session-metadata.kdl");
            let old_layout = fs::read(&layout_path).unwrap();
            let old_metadata = fs::read(&metadata_path).unwrap();
            let old_contents: BTreeMap<_, _> = original
                .1
                .keys()
                .map(|name| (name.clone(), fs::read(root.path().join(name)).unwrap()))
                .collect();
            for name in old_contents.keys() {
                assert!(
                    original.0.contains(name),
                    "checkpoint must reference its content files"
                );
            }

            let failed_generation = persistence.reserve(session).unwrap();
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            let mut completion = explicit_save.then(|| NotificationEnd::new(tx));
            let mut published = false;
            let failed = serialize_session_layout_for_save(
                checkpoint_capture("must not overwrite old contents", true),
                completion.as_mut(),
            )
            .map_err(str::to_owned)
            .and_then(|layout| {
                published = true;
                write_session_state_to_disk_in(
                    &persistence,
                    root.path(),
                    failed_generation,
                    session.to_owned(),
                    info.clone(),
                    layout,
                )
            });
            assert!(failed.unwrap_err().contains("Incomplete session snapshot"));
            assert!(!published);
            drop(completion);
            if explicit_save {
                let receipt = rx.try_recv().unwrap();
                assert_eq!(receipt.exit_status, Some(1));
                assert!(
                    receipt
                        .error_message
                        .unwrap()
                        .contains("Incomplete session snapshot")
                );
                assert!(receipt.stdout_message.is_none());
            }
            assert_eq!(fs::read(&layout_path).unwrap(), old_layout);
            assert_eq!(fs::read(&metadata_path).unwrap(), old_metadata);
            for (name, bytes) in &old_contents {
                assert_eq!(fs::read(root.path().join(name)).unwrap(), *bytes);
            }

            // A failed newest capture still fences a delayed older periodic
            // publication; it does not grant stale snapshots admission.
            assert!(
                !write_session_state_to_disk_in(
                    &persistence,
                    root.path(),
                    original_generation,
                    session.to_owned(),
                    info.clone(),
                    ("stale layout".into(), BTreeMap::new())
                )
                .unwrap()
            );
            assert_eq!(fs::read(&layout_path).unwrap(), old_layout);

            let retry_generation = persistence.reserve(session).unwrap();
            let retry = serialize_session_layout_for_save(checkpoint_capture("retry", false), None)
                .unwrap();
            // Periodic publication carries only an admitted complete payload.
            let job = BackgroundJob::ReportLayoutInfo(SessionLayoutSnapshot {
                session_name: session.to_owned(),
                generation: retry_generation,
                layout: retry.clone(),
            });
            let BackgroundJob::ReportLayoutInfo(snapshot) = job else {
                unreachable!()
            };
            assert!(
                write_session_state_to_disk_in(
                    &persistence,
                    root.path(),
                    snapshot.generation,
                    snapshot.session_name,
                    info,
                    snapshot.layout
                )
                .unwrap()
            );
            assert_eq!(fs::read_to_string(&layout_path).unwrap(), retry.0);
            for (name, contents) in retry.1 {
                assert_eq!(
                    fs::read_to_string(root.path().join(name)).unwrap(),
                    contents
                );
            }
        }
    }

    #[test]
    fn newer_session_generation_fences_a_delayed_older_snapshot() {
        let persistence = SessionStatePersistenceCoordinator::default();
        let writes = Mutex::new(Vec::new());
        let older = persistence.reserve("drawer").unwrap();
        let newer = persistence.reserve("drawer").unwrap();

        assert!(
            persistence
                .commit_if_current("drawer", newer, || {
                    writes.lock().unwrap().push("new");
                    Ok(())
                })
                .unwrap()
        );
        assert!(
            !persistence
                .commit_if_current("drawer", older, || {
                    writes.lock().unwrap().push("stale");
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!(*writes.lock().unwrap(), vec!["new"]);
    }

    #[test]
    fn reserved_session_generation_fences_older_writes_even_if_it_fails() {
        let persistence = SessionStatePersistenceCoordinator::default();
        let older = persistence.reserve("drawer").unwrap();
        let newer = persistence.reserve("drawer").unwrap();
        let failed = persistence.commit_if_current("drawer", newer, || Err("disk full".to_owned()));
        assert_eq!(failed.unwrap_err(), "disk full");

        let mut older_wrote = false;
        assert!(
            !persistence
                .commit_if_current("drawer", older, || {
                    older_wrote = true;
                    Ok(())
                })
                .unwrap()
        );
        assert!(!older_wrote);
    }

    #[test]
    fn scan_session_list_returns_empty_when_no_peers() {
        let sock_dir = tempdir().unwrap();
        let info_dir = tempdir().unwrap();
        let (live, resurrectable) = scan_session_list(
            "me",
            &[],
            &BTreeMap::new(),
            sock_dir.path(),
            info_dir.path(),
        );
        assert!(live.is_empty());
        assert!(resurrectable.is_empty());
    }

    #[test]
    fn scan_session_list_finds_peer_from_socket_and_metadata() {
        let sock_dir = tempdir().unwrap();
        let info_dir = tempdir().unwrap();
        let peer = "peer-alpha";
        let _listener = make_socket(sock_dir.path(), peer);
        write_metadata(info_dir.path(), peer, &SessionInfo::new(peer.to_string()));

        let (live, resurrectable) = scan_session_list(
            "me",
            &[],
            &BTreeMap::new(),
            sock_dir.path(),
            info_dir.path(),
        );
        assert_eq!(live.len(), 1);
        assert!(live.contains_key(peer));
        assert!(resurrectable.is_empty());
    }

    #[test]
    fn scan_session_list_keeps_live_socket_without_metadata() {
        let sock_dir = tempdir().unwrap();
        let info_dir = tempdir().unwrap();
        let peer = "peer-without-metadata";
        let _listener = make_socket(sock_dir.path(), peer);

        let (live, resurrectable) = scan_session_list(
            "me",
            &[],
            &BTreeMap::new(),
            sock_dir.path(),
            info_dir.path(),
        );
        let peer_info = live.get(peer).expect("live socket should be visible");
        assert_eq!(peer_info.name, peer);
        assert!(!peer_info.is_current_session);
        assert!(resurrectable.is_empty());
    }

    #[test]
    fn live_current_session_truth_replaces_stale_disk_snapshot() {
        let scanned_creation_time = Duration::from_secs(42);
        let mut scanned_current_session = SessionInfo::new("me".to_string());
        scanned_current_session.creation_time = scanned_creation_time;
        let mut scanned_sessions = BTreeMap::from([
            ("me".to_string(), scanned_current_session),
            ("peer".to_string(), SessionInfo::new("peer".to_string())),
        ]);
        let mut panes = HashMap::new();
        panes.insert(
            0,
            vec![PaneInfo {
                title: "agent".to_string(),
                terminal_command: Some("codex".to_string()),
                ..Default::default()
            }],
        );
        let mut current_session = SessionInfo::new("me".to_string());
        current_session.panes = PaneManifest { panes };

        overlay_current_session_info(
            &mut scanned_sessions,
            "me",
            &current_session,
            &BTreeMap::new(),
        );

        let current = scanned_sessions.get("me").unwrap();
        assert_eq!(current.panes, current_session.panes);
        assert!(current.is_current_session);
        assert_eq!(current.creation_time, scanned_creation_time);
        assert_eq!(scanned_sessions.get("peer").unwrap().name, "peer");
    }

    #[test]
    fn live_current_session_truth_survives_missing_discovery_namespace() {
        let mut panes = HashMap::new();
        panes.insert(
            0,
            vec![PaneInfo {
                title: "detached-agent".to_string(),
                terminal_command: Some("codex".to_string()),
                ..Default::default()
            }],
        );
        let mut current_session = SessionInfo::new("me".to_string());
        current_session.panes = PaneManifest { panes };
        current_session.connected_clients = 0;
        let mut scanned_sessions = BTreeMap::new();

        overlay_current_session_info(
            &mut scanned_sessions,
            "me",
            &current_session,
            &BTreeMap::new(),
        );

        let current = scanned_sessions
            .get("me")
            .expect("in-process current session must be represented");
        assert!(current.is_current_session);
        assert_eq!(current.connected_clients, 0);
        assert_eq!(current.panes, current_session.panes);
    }

    #[test]
    fn recovered_socket_does_not_reset_live_session_age() {
        let mut recovered_socket_info = SessionInfo::new("me".to_string());
        recovered_socket_info.creation_time = Duration::from_secs(1);
        let mut scanned_sessions = BTreeMap::from([("me".to_string(), recovered_socket_info)]);
        let mut current_session = SessionInfo::new("me".to_string());
        current_session.creation_time = Duration::from_secs(137);

        overlay_current_session_info(
            &mut scanned_sessions,
            "me",
            &current_session,
            &BTreeMap::new(),
        );

        assert_eq!(
            scanned_sessions.get("me").unwrap().creation_time,
            Duration::from_secs(137)
        );
    }

    #[test]
    fn uninitialized_current_session_truth_does_not_erase_disk_snapshot() {
        let mut scanned_session = SessionInfo::new("me".to_string());
        scanned_session.connected_clients = 2;
        let mut scanned_sessions = BTreeMap::from([("me".to_string(), scanned_session)]);

        overlay_current_session_info(
            &mut scanned_sessions,
            "me",
            &SessionInfo::default(),
            &BTreeMap::new(),
        );

        assert_eq!(scanned_sessions.get("me").unwrap().connected_clients, 2);
    }

    #[test]
    fn scan_session_list_finds_resurrectable_from_orphan_metadata() {
        let sock_dir = tempdir().unwrap();
        let info_dir = tempdir().unwrap();
        write_layout(info_dir.path(), "dead-beta");

        let (live, resurrectable) = scan_session_list(
            "me",
            &[],
            &BTreeMap::new(),
            sock_dir.path(),
            info_dir.path(),
        );
        assert!(live.is_empty());
        assert_eq!(resurrectable.len(), 1);
        assert!(resurrectable.contains_key("dead-beta"));
    }

    #[test]
    fn scan_session_list_separates_live_from_resurrectable() {
        let sock_dir = tempdir().unwrap();
        let info_dir = tempdir().unwrap();
        for name in ["live-a", "live-b", "live-c"] {
            let _listener = make_socket(sock_dir.path(), name);
            write_metadata(info_dir.path(), name, &SessionInfo::new(name.to_string()));
            std::mem::forget(_listener);
        }
        for name in ["dead-a", "dead-b"] {
            write_layout(info_dir.path(), name);
        }

        let (live, resurrectable) = scan_session_list(
            "me",
            &[],
            &BTreeMap::new(),
            sock_dir.path(),
            info_dir.path(),
        );
        assert_eq!(live.len(), 3);
        assert_eq!(resurrectable.len(), 2);
        for name in ["live-a", "live-b", "live-c"] {
            assert!(!resurrectable.contains_key(name));
        }
    }

    #[test]
    fn writer_scan_and_session_update_keep_slot_across_ticks_and_socket_rebind() {
        let sockets = tempdir().unwrap();
        let metadata = tempdir().unwrap();
        let persistence = SessionStatePersistenceCoordinator::default();
        let session = "clustered";
        let mut first = SessionInfo::new(session.to_owned());
        first.session_incarnation = "incarnation-a".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                first.clone(),
                (String::new(), BTreeMap::new()),
                false,
            )
            .unwrap()
        );
        let listener = make_socket(sockets.path(), session);
        let (first_scan, _) = scan_session_list(
            session,
            &[],
            &BTreeMap::new(),
            sockets.path(),
            metadata.path(),
        );
        assert_eq!(first_scan[session].rail_order, 1);

        // A later publication of the same server lifetime must keep the
        // durable slot even though its elapsed socket age changed.
        first.creation_time = Duration::from_secs(4);
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                first,
                (String::new(), BTreeMap::new()),
                false,
            )
            .unwrap()
        );
        drop(listener);
        fs::remove_file(sockets.path().join(session)).unwrap();
        let _rebound = make_socket(sockets.path(), session);
        let (mut scan_after_rebind, _) = scan_session_list(
            session,
            &[],
            &BTreeMap::new(),
            sockets.path(),
            metadata.path(),
        );
        let current_session_info = scan_after_rebind[session].clone();
        overlay_current_session_info(
            &mut scan_after_rebind,
            session,
            &current_session_info,
            &BTreeMap::new(),
        );
        assert_eq!(scan_after_rebind[session].rail_order, 1);
    }

    #[test]
    fn concurrent_writers_reserve_distinct_monotonic_slots() {
        let root = tempdir().unwrap();
        let lock = root.path().join(".rail-order.lock");
        // A previous process may leave the lock *file* behind; flock state is
        // attached to its closed descriptor, so that file is safely reusable.
        fs::write(&lock, b"retained advisory lock file").unwrap();
        assert_eq!(
            reserve_rail_order(&root.path().join("preexisting")).unwrap(),
            1
        );
        assert!(lock.is_file(), "advisory lock pathname is never unlinked");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut writers = vec![];
        for name in ["writer-a", "writer-b"] {
            let root = root.path().to_owned();
            let barrier = barrier.clone();
            writers.push(std::thread::spawn(move || {
                barrier.wait();
                reserve_rail_order(&root.join(name)).unwrap()
            }));
        }
        let mut slots = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>();
        slots.sort_unstable();
        assert_eq!(slots, vec![2, 3]);
    }

    #[test]
    fn resurrection_reuses_slot_but_fresh_same_name_and_deleted_high_record_do_not() {
        let metadata = tempdir().unwrap();
        let persistence = SessionStatePersistenceCoordinator::default();
        let session = "same-name";
        let mut original = SessionInfo::new(session.to_owned());
        original.session_incarnation = "old".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                original,
                (String::new(), BTreeMap::new()),
                false
            )
            .unwrap()
        );

        let mut resurrected = SessionInfo::new(session.to_owned());
        resurrected.session_incarnation = "resurrected".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                resurrected,
                (String::new(), BTreeMap::new()),
                true
            )
            .unwrap()
        );
        assert_eq!(
            SessionInfo::from_string(
                &fs::read_to_string(metadata.path().join(session).join("session-metadata.kdl"))
                    .unwrap(),
                session
            )
            .unwrap()
            .rail_order,
            1
        );

        let mut fresh = SessionInfo::new(session.to_owned());
        fresh.session_incarnation = "fresh".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                fresh,
                (String::new(), BTreeMap::new()),
                false
            )
            .unwrap()
        );
        assert_eq!(
            SessionInfo::from_string(
                &fs::read_to_string(metadata.path().join(session).join("session-metadata.kdl"))
                    .unwrap(),
                session
            )
            .unwrap()
            .rail_order,
            2
        );

        fs::remove_dir_all(metadata.path().join(session)).unwrap();
        let mut replacement = SessionInfo::new("replacement".to_owned());
        replacement.session_incarnation = "replacement".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join("replacement"),
                persistence.reserve("replacement").unwrap(),
                "replacement".to_owned(),
                replacement,
                (String::new(), BTreeMap::new()),
                false
            )
            .unwrap()
        );
        assert_eq!(
            SessionInfo::from_string(
                &fs::read_to_string(metadata.path().join("replacement/session-metadata.kdl"))
                    .unwrap(),
                "replacement"
            )
            .unwrap()
            .rail_order,
            3
        );
    }

    #[test]
    fn legacy_metadata_without_identity_fields_migrates_to_a_new_durable_slot() {
        let metadata = tempdir().unwrap();
        let sockets = tempdir().unwrap();
        let persistence = SessionStatePersistenceCoordinator::default();
        let session = "legacy";
        let legacy = SessionInfo::new(session.to_owned())
            .to_string()
            .lines()
            .filter(|line| {
                !line.trim_start().starts_with("session_incarnation")
                    && !line.trim_start().starts_with("rail_order")
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            SessionInfo::from_string(&legacy, session)
                .unwrap()
                .rail_order,
            0
        );
        fs::create_dir_all(metadata.path().join(session)).unwrap();
        fs::write(
            metadata.path().join(session).join("session-metadata.kdl"),
            legacy,
        )
        .unwrap();
        let mut migrated = SessionInfo::new(session.to_owned());
        migrated.session_incarnation = "new-incarnation".to_owned();
        assert!(
            write_session_state_to_disk_with_resurrection(
                &persistence,
                &metadata.path().join(session),
                persistence.reserve(session).unwrap(),
                session.to_owned(),
                migrated,
                (String::new(), BTreeMap::new()),
                false
            )
            .unwrap()
        );
        let _listener = make_socket(sockets.path(), session);
        let (live, _) = scan_session_list(
            session,
            &[],
            &BTreeMap::new(),
            sockets.path(),
            metadata.path(),
        );
        assert_eq!(live[session].rail_order, 1);
    }
}
