use crate::{os_input_output::AsyncReader, screen::ScreenInstruction, thread_bus::ThreadSenders};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use tokio::task;
use zellij_utils::{
    errors::{ContextType, get_current_ctx, prelude::*},
    logging::debug_to_file,
};

/// Per-pane budget of bytes sent to Screen but not yet parsed. The Screen
/// channel is unbounded (Screen also sends to itself, so bounding it risks
/// deadlock) — without a budget, a pane streaming faster than the single
/// Screen thread can parse grows the queue by gigabytes and pegs a core for
/// as long as the backlog lasts. When the budget is exceeded the read loop
/// pauses and the PTY's kernel buffer absorbs the burst instead.
const PANE_OUTSTANDING_BYTES_BUDGET: usize = 8 * 1024 * 1024;
const BACKPRESSURE_POLL: Duration = Duration::from_millis(10);
/// Fail-open bound: never wedge a reader on a Screen that stopped draining
/// (e.g. during teardown) — after this long, send anyway.
const MAX_BACKPRESSURE_WAIT: Duration = Duration::from_secs(2);

static OUTSTANDING_PTY_BYTES: OnceLock<Mutex<HashMap<u32, Arc<AtomicUsize>>>> = OnceLock::new();

fn outstanding_registry() -> &'static Mutex<HashMap<u32, Arc<AtomicUsize>>> {
    OUTSTANDING_PTY_BYTES.get_or_init(Default::default)
}

fn outstanding_counter(terminal_id: u32) -> Arc<AtomicUsize> {
    outstanding_registry()
        .lock()
        .unwrap()
        .entry(terminal_id)
        .or_default()
        .clone()
}

/// Called by the Screen thread once a `PtyBytes` chunk has been consumed
/// (parsed or dropped) — releases budget back to the pane's reader.
pub(crate) fn note_pty_bytes_processed(terminal_id: u32, n_bytes: usize) {
    let counter = outstanding_registry()
        .lock()
        .unwrap()
        .get(&terminal_id)
        .cloned();
    if let Some(counter) = counter {
        let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |outstanding| {
            Some(outstanding.saturating_sub(n_bytes))
        });
    }
}

fn remove_outstanding_counter(terminal_id: u32) {
    outstanding_registry().lock().unwrap().remove(&terminal_id);
}

pub(crate) struct TerminalBytes {
    terminal_id: u32,
    senders: ThreadSenders,
    async_reader: Box<dyn AsyncReader>,
    debug: bool,
    activity_flag: Arc<AtomicBool>,
}

impl TerminalBytes {
    pub fn new(
        terminal_id: u32,
        async_reader: Box<dyn AsyncReader>,
        senders: ThreadSenders,
        debug: bool,
        activity_flag: Arc<AtomicBool>,
    ) -> Self {
        TerminalBytes {
            terminal_id,
            senders,
            debug,
            async_reader,
            activity_flag,
        }
    }
    pub async fn listen(&mut self) -> Result<()> {
        // This function reads bytes from the pty and then sends them as
        // ScreenInstruction::PtyBytes to screen to be parsed there.
        //
        // Backpressure: the Screen channel is unbounded, so instead of relying
        // on send-blocking we account bytes in flight per pane. When more than
        // PANE_OUTSTANDING_BYTES_BUDGET is queued and unparsed, the read loop
        // pauses (bounded by MAX_BACKPRESSURE_WAIT) — the PTY's kernel buffer
        // then throttles the producing process the way a real terminal would.
        let err_context = || "failed to listen for bytes from PTY".to_string();

        let mut err_ctx = get_current_ctx();
        err_ctx.add_call(ContextType::AsyncTask);
        let outstanding = outstanding_counter(self.terminal_id);
        // Terminal ids are reused; a counter leaked by an errored-out
        // predecessor must not throttle this pane from its first byte.
        outstanding.store(0, Ordering::Relaxed);
        let mut buf = [0u8; 65536];
        loop {
            match self.async_reader.read(&mut buf).await {
                Ok(0) => break, // EOF
                Err(err) => {
                    log::error!("{}", err);
                    break;
                },
                Ok(n_bytes) => {
                    self.activity_flag.store(true, Ordering::Relaxed);
                    let bytes = &buf[..n_bytes];
                    if self.debug {
                        let _ = debug_to_file(bytes, self.terminal_id as i32);
                    }
                    let backpressure_started_at = Instant::now();
                    while outstanding.load(Ordering::Relaxed) > PANE_OUTSTANDING_BYTES_BUDGET
                        && backpressure_started_at.elapsed() < MAX_BACKPRESSURE_WAIT
                    {
                        tokio::time::sleep(BACKPRESSURE_POLL).await;
                    }
                    outstanding.fetch_add(n_bytes, Ordering::Relaxed);
                    self.async_send_to_screen(ScreenInstruction::PtyBytes(
                        self.terminal_id,
                        bytes.to_vec(),
                    ))
                    .await
                    .with_context(err_context)?;
                },
            }
        }
        remove_outstanding_counter(self.terminal_id);

        // Ignore any errors that happen here.
        // We only leave the loop above when the pane exits. This can happen in a lot of ways, but
        // the most problematic is when quitting zellij with `Ctrl+q`. That is because the channel
        // for `Screen` will have exited already, so this send *will* fail. This isn't a problem
        // per-se because the application terminates anyway, but it will print a lengthy error
        // message into the log for every pane that was still active when we quit the application.
        // This:
        //
        // 1. Makes the log rather pointless, because even when the application exits "normally",
        //    there will be errors inside and
        // 2. Leaves the impression we have a bug in the code and can't terminate properly
        //
        // FIXME: Ideally we detect whether the application is being quit and only ignore the error
        // in that particular case?
        let _ = self.async_send_to_screen(ScreenInstruction::Render).await;

        Ok(())
    }
    async fn async_send_to_screen(
        &self,
        screen_instruction: ScreenInstruction,
    ) -> Result<Duration> {
        // returns the time it blocked the thread for
        let sent_at = Instant::now();
        let senders = self.senders.clone();
        task::spawn_blocking(move || senders.send_to_screen(screen_instruction))
            .await
            .context("failed to async-send to screen")?
            .context("failed to block on sending message to screen")?;
        Ok(sent_at.elapsed())
    }
}

#[cfg(test)]
mod backpressure_tests {
    use super::*;

    #[test]
    fn processed_bytes_release_budget_and_saturate() {
        let id = 951_001; // distinct id: the registry is shared process-wide
        let counter = outstanding_counter(id);
        counter.store(0, Ordering::Relaxed);
        counter.fetch_add(100, Ordering::Relaxed);
        note_pty_bytes_processed(id, 40);
        assert_eq!(counter.load(Ordering::Relaxed), 60);
        note_pty_bytes_processed(id, 1000);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "release saturates at zero instead of wrapping"
        );
        remove_outstanding_counter(id);
        // After removal the release is a no-op and must not panic or
        // resurrect the entry.
        note_pty_bytes_processed(id, 10);
        assert!(
            !outstanding_registry().lock().unwrap().contains_key(&id),
            "processing after removal must not resurrect the counter"
        );
    }

    #[test]
    fn reused_terminal_id_starts_with_a_clean_budget() {
        let id = 951_002;
        let stale = outstanding_counter(id);
        stale.fetch_add(PANE_OUTSTANDING_BYTES_BUDGET * 2, Ordering::Relaxed);
        // emulate the listen() preamble on a reused terminal id
        let fresh = outstanding_counter(id);
        fresh.store(0, Ordering::Relaxed);
        assert_eq!(
            stale.load(Ordering::Relaxed),
            0,
            "listener reset clears a counter leaked by an errored predecessor"
        );
        remove_outstanding_counter(id);
    }
}
