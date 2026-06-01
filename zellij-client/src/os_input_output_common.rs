use async_trait::async_trait;

pub(crate) const ENABLE_MOUSE_SUPPORT: &str =
    "\u{1b}[?1000h\u{1b}[?1002h\u{1b}[?1003h\u{1b}[?1015h\u{1b}[?1006h";
pub(crate) const DISABLE_MOUSE_SUPPORT: &str =
    "\u{1b}[?1006l\u{1b}[?1015l\u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalEvent {
    Resize,
    Quit,
}

/// Trait for async signal listening, allowing for testable implementations.
#[async_trait]
pub trait AsyncSignals: Send {
    async fn recv(&mut self) -> Option<SignalEvent>;
}
