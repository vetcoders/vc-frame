use crate::ClientId;
use crate::ThreadSenders;
use crate::plugins::plugin_map::PluginMap;
use crate::plugins::wasm_bridge::PluginCache;
use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use wasmi::Engine;

pub type WorkFn = Box<
    dyn FnOnce(ThreadSenders, Arc<Mutex<PluginMap>>, Arc<Mutex<Vec<ClientId>>>, PluginCache, Engine)
        + Send
        + 'static,
>;
type PanicFn = Box<dyn FnOnce(String) + Send + 'static>;

/// A dynamic thread pool that pins jobs to specific threads based on plugin_id
/// Starts with 1 thread and expands when threads are busy, shrinks when plugins unload
pub struct PinnedExecutor {
    // Sparse vector - Some(thread) for active threads, None for removed threads
    execution_threads: Arc<Mutex<Vec<Option<ExecutionThread>>>>,

    // Maps plugin_id -> thread_index (permanent assignment)
    plugin_assignments: Arc<Mutex<HashMap<u32, usize>>>,

    // Maps thread_index -> set of plugin_ids assigned to it
    thread_plugins: Arc<Mutex<HashMap<usize, HashSet<u32>>>>,

    // Next thread index to use when spawning (monotonically increasing)
    next_thread_idx: AtomicUsize,

    // Maximum threads allowed
    max_threads: usize,

    // state to send to plugins (to be kept on execution threads)
    senders: ThreadSenders,
    plugin_map: Arc<Mutex<PluginMap>>,
    connected_clients: Arc<Mutex<Vec<ClientId>>>,
    plugin_cache: PluginCache,
    engine: Engine,

    #[cfg(test)]
    rejected_test_enqueues: Arc<Mutex<HashSet<u32>>>,
}

struct ExecutionThread {
    sender: Sender<Job>,
    jobs_in_flight: Arc<AtomicUsize>, // Busy state tracking
}

enum Job {
    Work {
        work: WorkFn,
        on_panic: Option<PanicFn>,
    },
    Shutdown, // Signal to exit the worker loop
}

struct InFlightGuard {
    jobs_in_flight: Arc<AtomicUsize>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.jobs_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "plugin executor job panicked with a non-string payload".to_owned()
    }
}

impl PinnedExecutor {
    /// Creates a new pinned executor with the specified maximum number of threads
    /// Starts with exactly 1 thread
    pub fn new(
        max_threads: usize,
        senders: &ThreadSenders,
        plugin_map: &Arc<Mutex<PluginMap>>,
        connected_clients: &Arc<Mutex<Vec<ClientId>>>,
        plugin_cache: &PluginCache,
        engine: &Engine,
    ) -> Self {
        let max_threads = max_threads.max(1); // At least 1

        let thread_0 = Self::spawn_thread(
            0,
            senders.clone(),
            plugin_map.clone(),
            connected_clients.clone(),
            plugin_cache.clone(),
            engine.clone(),
        );

        PinnedExecutor {
            execution_threads: Arc::new(Mutex::new(vec![Some(thread_0)])),
            plugin_assignments: Arc::new(Mutex::new(HashMap::new())),
            thread_plugins: Arc::new(Mutex::new(HashMap::new())),
            next_thread_idx: AtomicUsize::new(1), // Next will be index 1
            max_threads,
            senders: senders.clone(),
            plugin_map: plugin_map.clone(),
            connected_clients: connected_clients.clone(),
            plugin_cache: plugin_cache.clone(),
            engine: engine.clone(),
            #[cfg(test)]
            rejected_test_enqueues: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn spawn_thread(
        thread_idx: usize,
        senders: ThreadSenders,
        plugin_map: Arc<Mutex<PluginMap>>,
        connected_clients: Arc<Mutex<Vec<ClientId>>>,
        plugin_cache: PluginCache,
        engine: Engine,
    ) -> ExecutionThread {
        let (sender, receiver) = channel::<Job>();
        let jobs_in_flight = Arc::new(AtomicUsize::new(0));
        let jobs_in_flight_clone = jobs_in_flight.clone();

        let thread_handle = thread::Builder::new()
            .name(format!("plugin-exec-{}", thread_idx))
            .spawn({
                move || {
                    let senders = senders;
                    let plugin_map = plugin_map;
                    let connected_clients = connected_clients;
                    let plugin_cache = plugin_cache;
                    let engine = engine;
                    while let Ok(job) = receiver.recv() {
                        match job {
                            Job::Work { work, on_panic } => {
                                let _in_flight = InFlightGuard {
                                    jobs_in_flight: jobs_in_flight_clone.clone(),
                                };
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    work(
                                        senders.clone(),
                                        plugin_map.clone(),
                                        connected_clients.clone(),
                                        plugin_cache.clone(),
                                        engine.clone(),
                                    );
                                }));
                                if let Err(payload) = result {
                                    let message = panic_message(payload);
                                    log::error!(
                                        "Plugin executor thread {thread_idx} caught job panic: {message}"
                                    );
                                    if let Some(on_panic) = on_panic {
                                        let callback_result =
                                            catch_unwind(AssertUnwindSafe(|| on_panic(message)));
                                        if callback_result.is_err() {
                                            log::error!(
                                                "Plugin executor panic callback also panicked on thread {thread_idx}"
                                            );
                                        }
                                    }
                                }
                            },
                            Job::Shutdown => break,
                        }
                    }
                }
            });
        if let Err(e) = thread_handle {
            log::error!("Failed to spawn plugin execution thread: {}", e);
        }

        ExecutionThread {
            sender,
            jobs_in_flight,
        }
    }

    /// Register a plugin and assign it to a thread
    /// Called from wasm_bridge when loading a plugin
    pub fn register_plugin(&self, plugin_id: u32) -> usize {
        let mut assignments = self.plugin_assignments.lock().unwrap();

        // If already assigned (shouldn't happen, but defensive)
        if let Some(&thread_idx) = assignments.get(&plugin_id) {
            return thread_idx;
        }

        let mut thread_plugins = self.thread_plugins.lock().unwrap();
        let threads = self.execution_threads.lock().unwrap();

        // Find a non-busy thread with assigned plugins (prefer reusing threads)
        let mut best_thread: Option<(usize, usize)> = None; // (index, load)

        for (idx, thread_opt) in threads.iter().enumerate() {
            if let Some(thread) = thread_opt {
                let is_busy = thread.jobs_in_flight.load(Ordering::SeqCst) > 0;
                if !is_busy {
                    let load = thread_plugins.get(&idx).map(|s| s.len()).unwrap_or(0);
                    if best_thread.is_none() || best_thread.map(|b| load < b.1).unwrap_or(false) {
                        best_thread = Some((idx, load));
                    }
                }
            }
        }

        let thread_idx = if let Some((idx, _)) = best_thread {
            // Found a non-busy thread
            idx
        } else {
            // All threads are busy - need to expand
            if threads.len() < self.max_threads {
                // Spawn a new thread
                let new_idx = self.next_thread_idx.fetch_add(1, Ordering::SeqCst);
                drop(threads); // Release lock before spawning
                self.add_thread(new_idx);
                new_idx
            } else {
                // At max capacity, assign to least-loaded thread
                threads
                    .iter()
                    .enumerate()
                    .filter_map(|(idx, t)| t.as_ref().map(|_| idx))
                    .min_by_key(|&idx| thread_plugins.get(&idx).map(|s| s.len()).unwrap_or(0))
                    .unwrap_or_else(|| {
                        log::error!("Failed to find free thread to run the plugin!");
                        0 // this is a misconfiguration, but we don't want to crash the app
                        // if it happens
                    })
            }
        };

        // Update mappings
        assignments.insert(plugin_id, thread_idx);
        thread_plugins
            .entry(thread_idx)
            .or_default()
            .insert(plugin_id);

        thread_idx
    }

    fn add_thread(&self, thread_idx: usize) {
        let mut threads = self.execution_threads.lock().unwrap();
        let new_thread = Self::spawn_thread(
            thread_idx,
            self.senders.clone(),
            self.plugin_map.clone(),
            self.connected_clients.clone(),
            self.plugin_cache.clone(),
            self.engine.clone(),
        );

        // Extend vector if needed
        while threads.len() <= thread_idx {
            threads.push(None);
        }
        threads[thread_idx] = Some(new_thread);
    }

    /// Execute job pinned to plugin's assigned thread
    pub fn execute_for_plugin<F>(&self, plugin_id: u32, f: F)
    where
        F: FnOnce(
                ThreadSenders,
                Arc<Mutex<PluginMap>>,
                Arc<Mutex<Vec<ClientId>>>,
                PluginCache,
                Engine,
            ) + Send
            + 'static,
    {
        if let Err(error) = self.try_execute_for_plugin_job(plugin_id, Box::new(f), None) {
            log::error!("{error}");
        }
    }

    #[cfg(test)]
    pub fn try_execute_for_plugin<F>(&self, plugin_id: u32, f: F) -> std::result::Result<(), String>
    where
        F: FnOnce(
                ThreadSenders,
                Arc<Mutex<PluginMap>>,
                Arc<Mutex<Vec<ClientId>>>,
                PluginCache,
                Engine,
            ) + Send
            + 'static,
    {
        self.try_execute_for_plugin_job(plugin_id, Box::new(f), None)
    }

    fn try_execute_for_plugin_job(
        &self,
        plugin_id: u32,
        work: WorkFn,
        on_panic: Option<PanicFn>,
    ) -> std::result::Result<(), String> {
        let thread_idx = {
            let assignments = self.plugin_assignments.lock().unwrap();
            assignments.get(&plugin_id).copied()
        };
        let Some(thread_idx) = thread_idx else {
            return Err(format!(
                "failed to find executor assignment for plugin {plugin_id}"
            ));
        };

        // Get thread and mark as busy
        let threads = self.execution_threads.lock().unwrap();
        let thread = threads.get(thread_idx).and_then(Option::as_ref);
        let Some(thread) = thread else {
            return Err(format!(
                "failed to find executor thread {thread_idx} for plugin {plugin_id}"
            ));
        };

        // Increment busy counter BEFORE sending work
        thread.jobs_in_flight.fetch_add(1, Ordering::SeqCst);

        // Send work
        let job = Job::Work { work, on_panic };
        #[cfg(test)]
        if self
            .rejected_test_enqueues
            .lock()
            .unwrap()
            .remove(&plugin_id)
        {
            thread.jobs_in_flight.fetch_sub(1, Ordering::SeqCst);
            drop(job);
            return Err(format!(
                "plugin executor rejected injected test work for plugin {plugin_id}"
            ));
        }
        if let Err(send_error) = thread.sender.send(job) {
            thread.jobs_in_flight.fetch_sub(1, Ordering::SeqCst);
            drop(send_error.0);
            return Err(format!(
                "plugin executor thread {thread_idx} rejected work for plugin {plugin_id}"
            ));
        }
        Ok(())
    }

    /// Load a plugin: register it and execute the load work on its assigned thread
    /// This combines registration + execution for plugin loading
    pub fn execute_plugin_load<F>(&self, plugin_id: u32, f: F)
    where
        F: FnOnce(
                ThreadSenders,
                Arc<Mutex<PluginMap>>,
                Arc<Mutex<Vec<ClientId>>>,
                PluginCache,
                Engine,
            ) + Send
            + 'static,
    {
        if let Err(error) = self.try_execute_plugin_load_job(plugin_id, Box::new(f), None) {
            log::error!("{error}");
        }
    }

    pub fn try_execute_plugin_load<F, P>(
        &self,
        plugin_id: u32,
        f: F,
        on_panic: P,
    ) -> std::result::Result<(), String>
    where
        F: FnOnce(
                ThreadSenders,
                Arc<Mutex<PluginMap>>,
                Arc<Mutex<Vec<ClientId>>>,
                PluginCache,
                Engine,
            ) + Send
            + 'static,
        P: FnOnce(String) + Send + 'static,
    {
        self.try_execute_plugin_load_job(plugin_id, Box::new(f), Some(Box::new(on_panic)))
    }

    fn try_execute_plugin_load_job(
        &self,
        plugin_id: u32,
        work: WorkFn,
        on_panic: Option<PanicFn>,
    ) -> std::result::Result<(), String> {
        self.register_plugin(plugin_id);
        if let Err(error) = self.try_execute_for_plugin_job(plugin_id, work, on_panic) {
            self.unregister_plugin(plugin_id);
            return Err(error);
        }
        Ok(())
    }

    /// Queue unload work while preserving the executor assignment as retry debt
    /// when enqueue or execution fails.
    ///
    /// Callers that need a terminal proof must use
    /// `try_execute_plugin_unload_with_completion` and wait for its receipt.
    #[cfg(test)]
    pub fn execute_plugin_unload(
        self: &Arc<Self>,
        plugin_id: u32,
        f: impl FnOnce(
            ThreadSenders,
            Arc<Mutex<PluginMap>>,
            Arc<Mutex<Vec<ClientId>>>,
            PluginCache,
            Engine,
        ) + Send
        + 'static,
    ) {
        if let Err(error) = self.try_execute_plugin_unload_with_completion(plugin_id, f) {
            log::error!("{error}");
        }
    }

    /// Queue unload work and return a receipt that resolves only after cleanup and
    /// executor unregistration have both completed.
    ///
    /// Enqueue failures and cleanup panics are returned to the caller. A failed
    /// cleanup deliberately keeps the executor assignment so an idempotent retry
    /// can run on the same pinned thread.
    pub fn try_execute_plugin_unload_with_completion(
        self: &Arc<Self>,
        plugin_id: u32,
        f: impl FnOnce(
            ThreadSenders,
            Arc<Mutex<PluginMap>>,
            Arc<Mutex<Vec<ClientId>>>,
            PluginCache,
            Engine,
        ) + Send
        + 'static,
    ) -> std::result::Result<Receiver<std::result::Result<(), String>>, String> {
        let (completion_tx, completion_rx) = channel();
        let panic_completion_tx = completion_tx.clone();
        let executor = self.clone();
        let work: WorkFn = Box::new(
            move |senders, plugin_map, connected_clients, plugin_cache, engine| {
                f(senders, plugin_map, connected_clients, plugin_cache, engine);
                executor.unregister_plugin(plugin_id);
                let _ = completion_tx.send(Ok(()));
            },
        );
        let on_panic: PanicFn = Box::new(move |message| {
            let _ = panic_completion_tx.send(Err(format!(
                "plugin {plugin_id} unload cleanup panicked: {message}"
            )));
        });
        self.try_execute_for_plugin_job(plugin_id, work, Some(on_panic))?;
        Ok(completion_rx)
    }

    /// Unregister a plugin and potentially shrink the pool
    /// Called from wasm_bridge after plugin cleanup is complete
    pub fn unregister_plugin(&self, plugin_id: u32) {
        let mut assignments = self.plugin_assignments.lock().unwrap();
        let mut thread_plugins = self.thread_plugins.lock().unwrap();

        if let Some(thread_idx) = assignments.remove(&plugin_id)
            && let Some(plugins) = thread_plugins.get_mut(&thread_idx)
        {
            plugins.remove(&plugin_id);
        }

        drop(assignments);
        drop(thread_plugins);

        // Try to shrink the pool
        self.try_shrink_pool();
    }

    fn try_shrink_pool(&self) {
        let mut threads = self.execution_threads.lock().unwrap();
        let thread_plugins = self.thread_plugins.lock().unwrap();

        // Find threads with no assigned plugins (except thread 0, always keep it)
        let threads_to_remove: Vec<usize> = threads
            .iter()
            .enumerate()
            .skip(1) // Never remove thread 0
            .filter_map(|(idx, thread_opt)| {
                if thread_opt.is_some() {
                    let has_plugins = thread_plugins
                        .get(&idx)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false);
                    if !has_plugins { Some(idx) } else { None }
                } else {
                    None
                }
            })
            .collect();

        // Shutdown and remove idle threads
        for idx in threads_to_remove {
            if let Some(thread) = threads[idx].take() {
                let _ = thread.sender.send(Job::Shutdown);
            }
        }
    }

    #[cfg(test)]
    pub fn thread_count(&self) -> usize {
        self.execution_threads
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.is_some())
            .count()
    }

    #[cfg(test)]
    pub(super) fn jobs_in_flight_for_plugin(&self, plugin_id: u32) -> Option<usize> {
        let thread_idx = self
            .plugin_assignments
            .lock()
            .unwrap()
            .get(&plugin_id)
            .copied()?;
        self.execution_threads
            .lock()
            .unwrap()
            .get(thread_idx)
            .and_then(Option::as_ref)
            .map(|thread| thread.jobs_in_flight.load(Ordering::SeqCst))
    }

    #[cfg(test)]
    fn disconnect_thread_for_plugin(&self, plugin_id: u32) {
        let thread_idx = self
            .plugin_assignments
            .lock()
            .unwrap()
            .get(&plugin_id)
            .copied()
            .expect("plugin must be assigned before disconnecting its test thread");
        let (sender, receiver) = channel();
        drop(receiver);
        let mut threads = self.execution_threads.lock().unwrap();
        threads[thread_idx]
            .as_mut()
            .expect("assigned test thread must exist")
            .sender = sender;
    }

    #[cfg(test)]
    pub(super) fn reject_next_enqueue_for_plugin(&self, plugin_id: u32) {
        self.rejected_test_enqueues
            .lock()
            .unwrap()
            .insert(plugin_id);
    }

    pub(super) fn has_assignment(&self, plugin_id: u32) -> bool {
        self.plugin_assignments
            .lock()
            .unwrap()
            .contains_key(&plugin_id)
    }
}

impl Drop for PinnedExecutor {
    fn drop(&mut self) {
        let mut threads = self.execution_threads.lock().unwrap();

        // Send shutdown to all threads
        for thread in threads.iter_mut().flatten() {
            let _ = thread.sender.send(Job::Shutdown);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;
    use std::time::Duration;

    type TestDependencies = (
        ThreadSenders,
        Arc<Mutex<PluginMap>>,
        Arc<Mutex<Vec<ClientId>>>,
        PluginCache,
        Engine,
    );

    // Test fixtures
    fn create_test_dependencies() -> TestDependencies {
        use std::path::PathBuf;
        use wasmi::Module;
        use zellij_utils::channels::{self, SenderWithContext};

        let (send_to_pty, _receive_pty) = channels::bounded(1);
        let (send_to_screen, _receive_screen) = channels::bounded(1);
        let (send_to_plugin, _receive_plugin) = channels::bounded(1);
        let (send_to_server, _receive_server) = channels::bounded(1);
        let (send_to_pty_writer, _receive_pty_writer) = channels::bounded(1);
        let (send_to_background_jobs, _receive_background_jobs) = channels::bounded(1);

        let to_pty = SenderWithContext::new(send_to_pty);
        let to_screen = SenderWithContext::new(send_to_screen);
        let to_plugin = SenderWithContext::new(send_to_plugin);
        let to_server = SenderWithContext::new(send_to_server);
        let to_pty_writer = SenderWithContext::new(send_to_pty_writer);
        let to_background_jobs = SenderWithContext::new(send_to_background_jobs);

        let senders = ThreadSenders {
            to_pty: Some(to_pty),
            to_screen: Some(to_screen),
            to_plugin: Some(to_plugin),
            to_server: Some(to_server),
            to_pty_writer: Some(to_pty_writer),
            to_background_jobs: Some(to_background_jobs),
            should_silently_fail: false,
        };

        let plugin_map = Arc::new(Mutex::new(PluginMap::default()));
        let connected_clients = Arc::new(Mutex::new(vec![]));

        let plugin_cache = Arc::new(Mutex::new(
            std::collections::HashMap::<PathBuf, Module>::new(),
        ));

        let engine = Engine::default();

        (senders, plugin_map, connected_clients, plugin_cache, engine)
    }

    fn create_test_executor(max_threads: usize) -> Arc<PinnedExecutor> {
        let (senders, plugin_map, clients, cache, engine) = create_test_dependencies();
        Arc::new(PinnedExecutor::new(
            max_threads,
            &senders,
            &plugin_map,
            &clients,
            &cache,
            &engine,
        ))
    }

    type SignalingJob = Box<
        dyn FnOnce(
                ThreadSenders,
                Arc<Mutex<PluginMap>>,
                Arc<Mutex<Vec<ClientId>>>,
                PluginCache,
                Engine,
            ) + Send
            + 'static,
    >;

    // Helper to create a job that signals completion via channel
    fn make_signaling_job(tx: Sender<()>) -> SignalingJob {
        Box::new(move |_senders, _plugin_map, _clients, _cache, _engine| {
            tx.send(()).unwrap();
        })
    }

    // Helper to verify thread assignment by capturing thread name in job
    fn get_thread_name_for_plugin(executor: &Arc<PinnedExecutor>, plugin_id: u32) -> String {
        let (tx, rx) = channel();
        executor.execute_for_plugin(plugin_id, move |_s, _p, _c, _ca, _e| {
            let name = thread::current().name().unwrap().to_string();
            tx.send(name).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Thread name should be received")
    }

    #[test]
    fn test_new_creates_one_thread() {
        let executor = create_test_executor(4);
        assert_eq!(executor.thread_count(), 1);
    }

    #[test]
    fn test_new_respects_min_threads() {
        let executor = create_test_executor(0);
        assert_eq!(
            executor.thread_count(),
            1,
            "Executor should enforce minimum of 1 thread"
        );
    }

    #[test]
    fn test_first_plugin_assigned_to_thread_zero() {
        let executor = create_test_executor(4);
        let thread_idx = executor.register_plugin(1);
        assert_eq!(thread_idx, 0);
    }

    #[test]
    fn test_multiple_plugins_share_thread_when_idle() {
        let executor = create_test_executor(4);
        let thread_idx1 = executor.register_plugin(1);
        let thread_idx2 = executor.register_plugin(2);
        assert_eq!(thread_idx1, 0);
        assert_eq!(
            thread_idx2, 0,
            "Second plugin should share thread 0 when idle"
        );
    }

    #[test]
    fn test_new_thread_spawns_when_all_busy() {
        let executor = create_test_executor(3);

        // Register plugin 1, gets thread 0
        let thread_idx1 = executor.register_plugin(1);
        assert_eq!(thread_idx1, 0);

        // Make thread 0 busy with a barrier
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });

        // Give the job a moment to start executing and block
        thread::sleep(Duration::from_millis(50));

        // Register plugin 2 while thread 0 is busy
        let thread_idx2 = executor.register_plugin(2);
        assert_eq!(
            thread_idx2, 1,
            "Plugin 2 should get new thread 1 when thread 0 is busy"
        );

        // Verify thread count
        assert_eq!(executor.thread_count(), 2);

        // Release barrier
        barrier.wait();
    }

    #[test]
    fn test_respects_max_threads_limit() {
        let executor = create_test_executor(2);

        // Register plugin 1, gets thread 0
        executor.register_plugin(1);

        // Make thread 0 busy
        let barrier1 = Arc::new(Barrier::new(2));
        let barrier1_clone = barrier1.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier1_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Register plugin 2, gets thread 1
        executor.register_plugin(2);

        // Make thread 1 busy
        let barrier2 = Arc::new(Barrier::new(2));
        let barrier2_clone = barrier2.clone();
        executor.execute_for_plugin(2, move |_s, _p, _c, _ca, _e| {
            barrier2_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Register plugin 3 when all threads busy
        let thread_idx3 = executor.register_plugin(3);
        assert!(
            thread_idx3 == 0 || thread_idx3 == 1,
            "Plugin 3 should be assigned to existing thread"
        );
        assert_eq!(executor.thread_count(), 2, "Should not exceed max_threads");

        // Release barriers
        barrier1.wait();
        barrier2.wait();
    }

    #[test]
    fn test_duplicate_registration_returns_same_thread() {
        let executor = create_test_executor(4);
        let thread_idx1 = executor.register_plugin(1);
        let thread_idx2 = executor.register_plugin(1);
        assert_eq!(
            thread_idx1, thread_idx2,
            "Duplicate registration should return same thread"
        );
    }

    #[test]
    fn test_load_balancing_prefers_least_loaded() {
        let executor = create_test_executor(3);

        // Register plugins 1, 2, 3 to thread 0 (when idle)
        executor.register_plugin(1);
        executor.register_plugin(2);
        executor.register_plugin(3);

        // Make thread 0 busy
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Register plugin 4 while thread 0 is busy (spawns thread 1)
        let thread_idx4 = executor.register_plugin(4);
        assert_eq!(thread_idx4, 1);

        // Release barrier
        barrier.wait();
        thread::sleep(Duration::from_millis(50));

        // Register plugin 5 when both threads idle
        // Thread 0 has 3 plugins, thread 1 has 1 plugin
        let thread_idx5 = executor.register_plugin(5);
        assert_eq!(
            thread_idx5, 1,
            "Plugin 5 should be assigned to less loaded thread 1"
        );
    }

    #[test]
    fn test_execute_for_plugin_runs_on_correct_thread() {
        let executor = create_test_executor(3);

        // Register plugin 1 to thread 0
        executor.register_plugin(1);

        // Make thread 0 busy to force plugin 2 to thread 1
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Register plugin 2 to thread 1
        executor.register_plugin(2);

        // Release barrier
        barrier.wait();
        thread::sleep(Duration::from_millis(50));

        // Get thread names for both plugins
        let thread_name1 = get_thread_name_for_plugin(&executor, 1);
        let thread_name2 = get_thread_name_for_plugin(&executor, 2);

        assert_eq!(thread_name1, "plugin-exec-0");
        assert_eq!(thread_name2, "plugin-exec-1");
    }

    #[test]
    fn test_execute_for_plugin_unregistered() {
        let executor = create_test_executor(4);
        let (tx, rx) = channel();

        // Execute job for unregistered plugin
        executor.execute_for_plugin(999, make_signaling_job(tx));

        // Try to receive with timeout - should timeout
        let result = rx.recv_timeout(Duration::from_millis(100));
        assert!(
            result.is_err(),
            "Job for unregistered plugin should not execute"
        );
    }

    #[test]
    fn test_job_execution_order_per_thread() {
        let executor = create_test_executor(4);
        executor.register_plugin(1);

        let order = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = channel();

        // Execute 3 jobs for plugin 1
        for i in 1..=3 {
            let order_clone = order.clone();
            let tx_clone = tx.clone();
            executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
                order_clone.lock().unwrap().push(i);
                tx_clone.send(()).unwrap();
            });
        }

        // Wait for all 3 jobs to complete
        for _ in 0..3 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Job should complete");
        }

        assert_eq!(
            *order.lock().unwrap(),
            vec![1, 2, 3],
            "Jobs should execute in order"
        );
    }

    #[test]
    fn test_jobs_complete_successfully() {
        let executor = create_test_executor(4);
        executor.register_plugin(1);

        let (tx, rx) = channel();
        executor.execute_for_plugin(1, make_signaling_job(tx));

        let result = rx.recv_timeout(Duration::from_secs(5));
        assert!(result.is_ok(), "Job should complete successfully");
    }

    #[test]
    fn test_concurrent_jobs_on_different_threads() {
        let executor = create_test_executor(3);

        // Register plugin 1 to thread 0
        executor.register_plugin(1);

        // Make thread 0 busy to force plugin 2 to thread 1
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Register plugin 2 to thread 1
        executor.register_plugin(2);

        // Release barrier
        barrier.wait();
        thread::sleep(Duration::from_millis(50));

        // Execute jobs on both threads concurrently
        let sync_barrier = Arc::new(Barrier::new(3)); // 2 jobs + test thread
        let (tx1, rx1) = channel();
        let (tx2, rx2) = channel();

        let barrier1 = sync_barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier1.wait();
            tx1.send(()).unwrap();
        });

        let barrier2 = sync_barrier.clone();
        executor.execute_for_plugin(2, move |_s, _p, _c, _ca, _e| {
            barrier2.wait();
            tx2.send(()).unwrap();
        });

        // Release both jobs simultaneously
        sync_barrier.wait();

        // Both should complete
        assert!(rx1.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(rx2.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn test_execute_plugin_load_registers_and_executes() {
        let executor = create_test_executor(4);

        let (tx, rx) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx));

        // Wait for load to complete
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Verify plugin is registered
        let thread_idx = executor.register_plugin(1);
        assert_eq!(thread_idx, 0, "Plugin should already be registered");
    }

    #[test]
    fn panicking_job_releases_busy_count_and_worker_accepts_next_job() {
        let executor = create_test_executor(1);
        let (panic_tx, panic_rx) = channel();
        executor
            .try_execute_plugin_load(
                41,
                |_s, _p, _c, _ca, _e| panic!("intentional executor test panic"),
                move |message| {
                    panic_tx.send(message).unwrap();
                },
            )
            .expect("panicking job must still be accepted");

        let panic_message = panic_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("panic callback must run");
        assert!(panic_message.contains("intentional executor test panic"));
        for _ in 0..100 {
            if executor.jobs_in_flight_for_plugin(41) == Some(0) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(executor.jobs_in_flight_for_plugin(41), Some(0));

        let (next_tx, next_rx) = channel();
        executor
            .try_execute_for_plugin(41, make_signaling_job(next_tx))
            .expect("worker must survive a job panic");
        next_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("next job must run on the surviving worker");
    }

    #[test]
    fn enqueue_failure_is_reported_and_load_registration_is_rolled_back() {
        let executor = create_test_executor(1);
        executor.register_plugin(42);
        executor.disconnect_thread_for_plugin(42);

        let result = executor.try_execute_plugin_load(42, |_s, _p, _c, _ca, _e| {}, |_message| {});

        assert!(
            result
                .as_ref()
                .is_err_and(|message| message.contains("rejected work"))
        );
        assert!(
            !executor
                .plugin_assignments
                .lock()
                .unwrap()
                .contains_key(&42),
            "failed enqueue must not retain a phantom plugin assignment"
        );
        assert!(
            executor
                .thread_plugins
                .lock()
                .unwrap()
                .values()
                .all(|plugins| !plugins.contains(&42)),
            "failed enqueue must not retain thread ownership"
        );
    }

    #[test]
    fn panicking_fire_and_forget_unload_retains_executor_assignment_for_retry() {
        let executor = create_test_executor(1);
        executor.register_plugin(43);

        executor.execute_plugin_unload(43, |_s, _p, _c, _ca, _e| {
            panic!("intentional unload cleanup panic");
        });

        for _ in 0..100 {
            if executor.jobs_in_flight_for_plugin(43) == Some(0) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            executor.has_assignment(43),
            "panic during unload cleanup must retain retry ownership"
        );
        assert!(
            executor
                .thread_plugins
                .lock()
                .unwrap()
                .values()
                .any(|plugins| plugins.contains(&43)),
            "panic during unload cleanup must retain pinned-thread ownership"
        );

        let retry_receipt = executor
            .try_execute_plugin_unload_with_completion(43, |_s, _p, _c, _ca, _e| {})
            .expect("retry unload work must be accepted");
        assert_eq!(
            retry_receipt
                .recv_timeout(Duration::from_secs(5))
                .expect("retry unload must resolve"),
            Ok(())
        );
        assert!(!executor.has_assignment(43));
    }

    #[test]
    fn completion_aware_unload_panic_retains_assignment_for_retry() {
        let executor = create_test_executor(1);
        executor.register_plugin(44);

        let failed_receipt = executor
            .try_execute_plugin_unload_with_completion(44, |_s, _p, _c, _ca, _e| {
                panic!("intentional retryable unload panic");
            })
            .expect("unload work must be accepted");
        let error = failed_receipt
            .recv_timeout(Duration::from_secs(5))
            .expect("panic must resolve the unload receipt")
            .unwrap_err();

        assert!(error.contains("intentional retryable unload panic"));
        assert!(
            executor.has_assignment(44),
            "failed completion-aware unload must retain retry ownership"
        );

        let retry_receipt = executor
            .try_execute_plugin_unload_with_completion(44, |_s, _p, _c, _ca, _e| {})
            .expect("retry unload work must be accepted");
        assert_eq!(
            retry_receipt
                .recv_timeout(Duration::from_secs(5))
                .expect("retry unload must resolve"),
            Ok(())
        );
        assert!(!executor.has_assignment(44));
    }

    #[test]
    fn test_execute_plugin_unload_runs_cleanup_before_unregister() {
        let executor = create_test_executor(4);

        // Load plugin
        let (tx_load, rx_load) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx_load));
        rx_load
            .recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Unload plugin with cleanup
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let (tx_unload, rx_unload) = channel();

        executor.execute_plugin_unload(1, move |_s, _p, _c, _ca, _e| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            tx_unload.send(()).unwrap();
        });

        // Wait for unload to complete
        rx_unload
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        // Verify cleanup ran
        assert_eq!(counter.load(Ordering::SeqCst), 1, "Cleanup should have run");

        // Give unregister a moment to complete
        thread::sleep(Duration::from_millis(100));

        // Plugin should be unregistered - registering again should assign new thread
        let thread_idx = executor.register_plugin(1);
        assert_eq!(thread_idx, 0, "Plugin should be re-registered to thread 0");
    }

    #[test]
    fn test_unload_sequence_is_correct() {
        let executor = create_test_executor(4);

        // Load plugin
        let (tx_load, rx_load) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx_load));
        rx_load
            .recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Unload with sequence tracking
        let sequence = Arc::new(Mutex::new(Vec::new()));
        let sequence_clone = sequence.clone();
        let (tx_unload, rx_unload) = channel();

        executor.execute_plugin_unload(1, move |_s, _p, _c, _ca, _e| {
            sequence_clone.lock().unwrap().push("cleanup");
            tx_unload.send(()).unwrap();
        });

        // Wait for unload to complete
        rx_unload
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");
        sequence.lock().unwrap().push("after");

        assert_eq!(*sequence.lock().unwrap(), vec!["cleanup", "after"]);
    }

    #[test]
    fn test_shrink_removes_idle_threads() {
        let executor = create_test_executor(4);

        // Load plugin 1 to thread 0
        let (tx1, rx1) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx1));
        rx1.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Make thread 0 busy to force plugin 2 to thread 1
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        // Load plugin 2 to thread 1
        let (tx2, rx2) = channel();
        executor.execute_plugin_load(2, make_signaling_job(tx2));
        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Release barrier
        barrier.wait();
        thread::sleep(Duration::from_millis(50));

        let thread_count_before = executor.thread_count();
        assert!(thread_count_before >= 2, "Should have at least 2 threads");

        // Unload plugin 2
        let (tx_unload2, rx_unload2) = channel();
        executor.execute_plugin_unload(2, make_signaling_job(tx_unload2));
        rx_unload2
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        // Give shrinking a moment to complete
        thread::sleep(Duration::from_millis(100));

        // Thread count should decrease after unloading
        let thread_count_after = executor.thread_count();
        assert!(
            thread_count_after < thread_count_before,
            "Idle threads should be removed"
        );
        assert!(thread_count_after >= 1, "Thread 0 should remain");
    }

    #[test]
    fn test_thread_zero_never_removed() {
        let executor = create_test_executor(4);

        // Load a plugin and then unload it
        let (tx_load, rx_load) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx_load));
        rx_load
            .recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        let (tx_unload, rx_unload) = channel();
        executor.execute_plugin_unload(1, make_signaling_job(tx_unload));
        rx_unload
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        // Give shrinking a moment
        thread::sleep(Duration::from_millis(100));

        // Thread 0 should remain
        assert!(
            executor.thread_count() >= 1,
            "Thread 0 should never be removed"
        );
    }

    #[test]
    fn test_active_threads_not_removed() {
        let executor = create_test_executor(4);

        // Load plugin 1 to thread 0
        let (tx1, rx1) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx1));
        rx1.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Force plugin 2 to thread 1 by making thread 0 busy
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        let (tx2, rx2) = channel();
        executor.execute_plugin_load(2, make_signaling_job(tx2));
        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        barrier.wait();
        thread::sleep(Duration::from_millis(100));

        let thread_count_with_both = executor.thread_count();
        assert!(
            thread_count_with_both >= 2,
            "Should have at least 2 threads with 2 plugins"
        );

        // Unload plugin 2
        let (tx_unload, rx_unload) = channel();
        executor.execute_plugin_unload(2, make_signaling_job(tx_unload));
        rx_unload
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        thread::sleep(Duration::from_millis(100));

        // Plugin 1's thread should still work (verify active threads not affected)
        let (tx_test, rx_test) = channel();
        executor.execute_for_plugin(1, make_signaling_job(tx_test));
        assert!(
            rx_test.recv_timeout(Duration::from_secs(5)).is_ok(),
            "Plugin 1's thread should still work"
        );

        // Thread count should decrease after unloading
        let thread_count_after = executor.thread_count();
        assert!(
            thread_count_after < thread_count_with_both,
            "Idle thread should be removed"
        );
        assert!(thread_count_after >= 1, "Active threads should remain");
    }

    #[test]
    fn test_shrink_does_not_affect_remaining_threads() {
        let executor = create_test_executor(4);

        // Load plugin 1 to thread 0
        let (tx1, rx1) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx1));
        rx1.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Force plugin 2 to thread 1
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        let (tx2, rx2) = channel();
        executor.execute_plugin_load(2, make_signaling_job(tx2));
        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        barrier.wait();
        thread::sleep(Duration::from_millis(50));

        // Unload plugin 2 (shrinks pool)
        let (tx_unload, rx_unload) = channel();
        executor.execute_plugin_unload(2, make_signaling_job(tx_unload));
        rx_unload
            .recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        thread::sleep(Duration::from_millis(100));

        // Execute job for plugin 1
        let (tx_test, rx_test) = channel();
        executor.execute_for_plugin(1, make_signaling_job(tx_test));

        assert!(
            rx_test.recv_timeout(Duration::from_secs(5)).is_ok(),
            "Plugin 1's thread should still work"
        );
    }

    #[test]
    fn test_drop_cleans_up_gracefully() {
        let executor = create_test_executor(4);

        // Load multiple plugins on different threads
        let (tx1, rx1) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx1));
        rx1.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });
        thread::sleep(Duration::from_millis(50));

        let (tx2, rx2) = channel();
        executor.execute_plugin_load(2, make_signaling_job(tx2));
        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        barrier.wait();

        // Drop executor
        drop(executor);

        // Test completes without panic
    }

    #[test]
    fn test_drop_with_jobs_in_flight() {
        let executor = create_test_executor(4);
        executor.register_plugin(1);

        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();
        executor.execute_for_plugin(1, move |_s, _p, _c, _ca, _e| {
            barrier_clone.wait();
        });

        // Drop executor while job is blocked
        drop(executor);

        // Release barrier (job may or may not complete, but shouldn't panic)
        barrier.wait();

        // Test completes without panic
    }

    #[test]
    fn test_concurrent_plugin_registrations() {
        let executor = create_test_executor(4);

        let handles: Vec<_> = (1..=10)
            .map(|i| {
                let exec = executor.clone();
                thread::spawn(move || exec.register_plugin(i))
            })
            .collect();

        for handle in handles {
            let thread_idx = handle.join().expect("Thread should not panic");
            assert!(thread_idx < 4, "Thread index should be valid");
        }
    }

    #[test]
    fn test_unregister_nonexistent_plugin() {
        let executor = create_test_executor(4);

        // Unregister non-existent plugin
        executor.unregister_plugin(999);

        // Executor should still work
        let (tx, rx) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx));
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn test_max_threads_one() {
        let executor = create_test_executor(1);

        // Register multiple plugins
        let thread_idx1 = executor.register_plugin(1);
        let thread_idx2 = executor.register_plugin(2);
        let thread_idx3 = executor.register_plugin(3);

        assert_eq!(thread_idx1, 0);
        assert_eq!(thread_idx2, 0);
        assert_eq!(thread_idx3, 0);
        assert_eq!(executor.thread_count(), 1);
    }

    #[test]
    fn test_rapid_load_unload_cycles() {
        let executor = create_test_executor(4);

        // Load plugin 1
        let (tx1, rx1) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx1));
        rx1.recv_timeout(Duration::from_secs(5))
            .expect("Load should complete");

        // Unload plugin 1
        let (tx2, rx2) = channel();
        executor.execute_plugin_unload(1, make_signaling_job(tx2));
        rx2.recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        thread::sleep(Duration::from_millis(100));

        // Load plugin 1 again
        let (tx3, rx3) = channel();
        executor.execute_plugin_load(1, make_signaling_job(tx3));
        rx3.recv_timeout(Duration::from_secs(5))
            .expect("Second load should complete");

        // Execute job for plugin 1
        let (tx4, rx4) = channel();
        executor.execute_for_plugin(1, make_signaling_job(tx4));
        assert!(
            rx4.recv_timeout(Duration::from_secs(5)).is_ok(),
            "Executor should handle cycles correctly"
        );
    }

    #[test]
    fn test_many_plugins_limited_threads() {
        let executor = create_test_executor(4);
        let (tx, rx) = channel();

        // Load 20 plugins
        for i in 1..=20 {
            let tx_clone = tx.clone();
            executor.execute_plugin_load(i, make_signaling_job(tx_clone));
        }

        // Collect 20 completion signals
        for _ in 1..=20 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Load should complete");
        }

        assert!(
            executor.thread_count() <= 4,
            "Should not exceed max_threads"
        );
    }

    #[test]
    fn test_full_lifecycle() {
        let executor = create_test_executor(4);
        let (tx, rx) = channel();

        // Load 5 plugins
        for i in 1..=5 {
            let tx_clone = tx.clone();
            executor.execute_plugin_load(i, make_signaling_job(tx_clone));
        }

        // Wait for all loads
        for _ in 0..5 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Load should complete");
        }

        // Execute 2 jobs per plugin (10 total)
        for i in 1..=5 {
            for _ in 0..2 {
                let tx_clone = tx.clone();
                executor.execute_for_plugin(i, make_signaling_job(tx_clone));
            }
        }

        // Wait for all jobs
        for _ in 0..10 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Job should complete");
        }

        let thread_count_before = executor.thread_count();

        // Unload 3 plugins
        for i in 1..=3 {
            let tx_clone = tx.clone();
            executor.execute_plugin_unload(i, make_signaling_job(tx_clone));
        }

        // Wait for unloads
        for _ in 0..3 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Unload should complete");
        }

        thread::sleep(Duration::from_millis(100));

        // Thread count should decrease or stay the same
        let thread_count_after = executor.thread_count();
        assert!(
            thread_count_after <= thread_count_before,
            "Thread count should decrease after unloads"
        );

        // Execute jobs for remaining plugins
        for i in 4..=5 {
            let tx_clone = tx.clone();
            executor.execute_for_plugin(i, make_signaling_job(tx_clone));
        }

        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Job should complete");
        }

        // Drop executor
        drop(executor);
    }

    #[test]
    fn test_realistic_plugin_churn() {
        let executor = create_test_executor(4);
        let (tx, rx) = channel();

        // Load plugins 1, 2, 3
        for i in 1..=3 {
            let tx_clone = tx.clone();
            executor.execute_plugin_load(i, make_signaling_job(tx_clone));
        }
        for _ in 0..3 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Load should complete");
        }

        // Execute jobs for each
        for i in 1..=3 {
            let tx_clone = tx.clone();
            executor.execute_for_plugin(i, make_signaling_job(tx_clone));
        }
        for _ in 0..3 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Job should complete");
        }

        // Unload plugin 2
        let tx_clone = tx.clone();
        executor.execute_plugin_unload(2, make_signaling_job(tx_clone));
        rx.recv_timeout(Duration::from_secs(5))
            .expect("Unload should complete");

        thread::sleep(Duration::from_millis(100));

        // Load plugins 4, 5
        for i in 4..=5 {
            let tx_clone = tx.clone();
            executor.execute_plugin_load(i, make_signaling_job(tx_clone));
        }
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Load should complete");
        }

        // Execute jobs for plugins 1, 3, 4, 5
        for i in &[1, 3, 4, 5] {
            let tx_clone = tx.clone();
            executor.execute_for_plugin(*i, make_signaling_job(tx_clone));
        }
        for _ in 0..4 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Job should complete");
        }

        // Unload plugins 1, 3
        for i in &[1, 3] {
            let tx_clone = tx.clone();
            executor.execute_plugin_unload(*i, make_signaling_job(tx_clone));
        }
        for _ in 0..2 {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("Unload should complete");
        }

        thread::sleep(Duration::from_millis(100));

        // Verify thread count reflects active plugins (4 and 5)
        assert!(
            executor.thread_count() >= 1,
            "Should have at least thread 0"
        );

        drop(executor);
    }
}
