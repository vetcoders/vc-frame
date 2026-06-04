//! Definitions and helpers for sending and receiving messages between threads.

use crate::errors::{ErrorContext, get_current_ctx};
pub use crossbeam::channel::{
    Receiver, RecvError, RecvTimeoutError, Select, SendError, Sender, TrySendError, bounded,
    unbounded,
};

/// An [MPSC](mpsc) asynchronous channel with added error context.
pub type ChannelWithContext<T> = (Sender<(T, ErrorContext)>, Receiver<(T, ErrorContext)>);

/// Sends messages on an [MPSC](std::sync::mpsc) channel, along with an [`ErrorContext`],
/// synchronously or asynchronously depending on the underlying [`SenderType`].
#[derive(Clone)]
pub struct SenderWithContext<T> {
    sender: Sender<(T, ErrorContext)>,
}

impl<T: Clone> SenderWithContext<T> {
    pub fn new(sender: Sender<(T, ErrorContext)>) -> Self {
        Self { sender }
    }

    /// Sends an event, along with the current [`ErrorContext`], on this
    /// [`SenderWithContext`]'s channel.
    pub fn send(&self, event: T) -> Result<(), SendError<(T, ErrorContext)>> {
        let err_ctx = get_current_ctx();
        self.sender.send((event, err_ctx))
    }
}
