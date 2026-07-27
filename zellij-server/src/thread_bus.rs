//! Definitions and helpers for sending and receiving messages between threads.

use crate::{
    ServerInstruction, background_jobs::BackgroundJob, os_input_output::ServerOsApi,
    plugins::PluginInstruction, pty::PtyInstruction, pty_writer::PtyWriteInstruction,
    screen::ScreenInstruction,
};
use zellij_utils::errors::prelude::*;
use zellij_utils::{channels, channels::SenderWithContext, errors::ErrorContext};

pub struct RecoverableSendError<T> {
    instruction: Box<T>,
    error: anyhow::Error,
}

impl<T> RecoverableSendError<T> {
    fn new(instruction: T, error: anyhow::Error) -> Self {
        Self {
            instruction: Box::new(instruction),
            error,
        }
    }

    pub fn into_parts(self) -> (T, anyhow::Error) {
        (*self.instruction, self.error)
    }
}

/// A container for senders to the different threads in zellij on the server side
#[derive(Default, Clone)]
pub struct ThreadSenders {
    pub to_screen: Option<SenderWithContext<ScreenInstruction>>,
    pub to_pty: Option<SenderWithContext<PtyInstruction>>,
    pub to_plugin: Option<SenderWithContext<PluginInstruction>>,
    pub to_server: Option<SenderWithContext<ServerInstruction>>,
    pub to_pty_writer: Option<SenderWithContext<PtyWriteInstruction>>,
    pub to_background_jobs: Option<SenderWithContext<BackgroundJob>>,
    // this is a convenience for the unit tests
    // it's not advisable to set it to true in production code
    pub should_silently_fail: bool,
}

impl ThreadSenders {
    pub fn send_to_screen(&self, instruction: ScreenInstruction) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_screen {
                let _ = sender.send(instruction);
            }
            Ok(())
        } else {
            self.to_screen
                .as_ref()
                .context("failed to get screen sender")?
                .send(instruction)
                .to_anyhow()
                .context("failed to send message to screen")
        }
    }

    /// Send a layout-critical instruction to Screen without discarding the
    /// instruction when the receiver has gone away.
    ///
    /// `NotificationEnd` is intentionally not meaningfully cloneable: the
    /// original instruction owns the only completion channel.  The ordinary
    /// helpers erase `SendError<T>` into `anyhow::Error`, which is appropriate
    /// for best-effort traffic but would turn a failed layout handoff into a
    /// false-success completion.  Callers of this helper can recover the
    /// original instruction, mark its completion as failed, and run the
    /// transaction cleanup path.
    pub fn send_to_screen_recover(
        &self,
        instruction: ScreenInstruction,
    ) -> std::result::Result<(), RecoverableSendError<ScreenInstruction>> {
        let Some(sender) = self.to_screen.as_ref() else {
            return Err(RecoverableSendError::new(
                instruction,
                anyhow!("failed to get screen sender"),
            ));
        };
        sender.send(instruction).map_err(|error| {
            let (instruction, _) = error.0;
            RecoverableSendError::new(
                instruction,
                anyhow!("failed to send layout-critical message to screen"),
            )
        })
    }

    pub fn send_to_pty(&self, instruction: PtyInstruction) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_pty {
                let _ = sender.send(instruction);
            }
            Ok(())
        } else {
            self.to_pty
                .as_ref()
                .context("failed to get pty sender")?
                .send(instruction)
                .to_anyhow()
                .context("failed to send message to pty")
        }
    }

    /// The PTY counterpart to [`Self::send_to_screen_recover`].
    ///
    /// This path deliberately ignores `should_silently_fail`: a missing
    /// layout-transaction handoff is never safe to report as success, even in
    /// a test bus configured to tolerate unrelated background traffic.
    pub fn send_to_pty_recover(
        &self,
        instruction: PtyInstruction,
    ) -> std::result::Result<(), RecoverableSendError<PtyInstruction>> {
        let Some(sender) = self.to_pty.as_ref() else {
            return Err(RecoverableSendError::new(
                instruction,
                anyhow!("failed to get pty sender"),
            ));
        };
        sender.send(instruction).map_err(|error| {
            let (instruction, _) = error.0;
            RecoverableSendError::new(
                instruction,
                anyhow!("failed to send layout-critical message to pty"),
            )
        })
    }

    pub fn send_to_plugin(&self, instruction: PluginInstruction) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_plugin {
                let _ = sender.send(instruction);
            }
            Ok(())
        } else {
            self.to_plugin
                .as_ref()
                .context("failed to get plugin sender")?
                .send(instruction)
                .to_anyhow()
                .context("failed to send message to plugin")
        }
    }

    /// The Plugin counterpart to [`Self::send_to_screen_recover`].
    ///
    /// Screen uses this for the first transaction handoff, while it still owns
    /// the pending-tab rollback state and the sole action completion channel.
    /// Recovering the instruction lets Screen reject that exact transaction
    /// instead of losing both owners in an erased channel error.
    pub fn send_to_plugin_recover(
        &self,
        instruction: PluginInstruction,
    ) -> std::result::Result<(), RecoverableSendError<PluginInstruction>> {
        let Some(sender) = self.to_plugin.as_ref() else {
            return Err(RecoverableSendError::new(
                instruction,
                anyhow!("failed to get plugin sender"),
            ));
        };
        sender.send(instruction).map_err(|error| {
            let (instruction, _) = error.0;
            RecoverableSendError::new(
                instruction,
                anyhow!("failed to send layout-critical message to plugin"),
            )
        })
    }

    pub fn send_to_server(&self, instruction: ServerInstruction) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_server {
                let _ = sender.send(instruction);
            }
            Ok(())
        } else {
            self.to_server
                .as_ref()
                .context("failed to get server sender")?
                .send(instruction)
                .to_anyhow()
                .context("failed to send message to server")
        }
    }
    pub fn send_to_pty_writer(&self, instruction: PtyWriteInstruction) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_pty_writer {
                let _ = sender.send(instruction);
            }
            Ok(())
        } else {
            self.to_pty_writer
                .as_ref()
                .context("failed to get pty writer sender")?
                .send(instruction)
                .to_anyhow()
                .context("failed to send message to pty writer")
        }
    }
    pub fn send_to_background_jobs(&self, background_job: BackgroundJob) -> Result<()> {
        if self.should_silently_fail {
            if let Some(sender) = &self.to_background_jobs {
                let _ = sender.send(background_job);
            }
            Ok(())
        } else {
            self.to_background_jobs
                .as_ref()
                .context("failed to get background jobs sender")?
                .send(background_job)
                .to_anyhow()
                .context("failed to send message to background jobs")
        }
    }

    #[allow(unused)]
    pub fn silently_fail_on_send(mut self) -> Self {
        // this is mostly used for the tests, see struct
        self.should_silently_fail = true;
        self
    }
    #[allow(unused)]
    pub fn replace_to_pty_writer(
        &mut self,
        new_pty_writer: SenderWithContext<PtyWriteInstruction>,
    ) {
        // this is mostly used for the tests, see struct
        self.to_pty_writer.replace(new_pty_writer);
    }
    #[allow(unused)]
    pub fn replace_to_pty(&mut self, new_pty: SenderWithContext<PtyInstruction>) {
        // this is mostly used for the tests, see struct
        self.to_pty.replace(new_pty);
    }

    #[allow(unused)]
    pub fn replace_to_plugin(&mut self, new_to_plugin: SenderWithContext<PluginInstruction>) {
        // this is mostly used for the tests, see struct
        self.to_plugin.replace(new_to_plugin);
    }
}

/// A container for a receiver, OS input and the senders to a given thread
#[derive(Default)]
pub(crate) struct Bus<T> {
    receivers: Vec<channels::Receiver<(T, ErrorContext)>>,
    pub senders: ThreadSenders,
    pub os_input: Option<Box<dyn ServerOsApi>>,
}

impl<T> Bus<T> {
    pub fn new(
        receivers: Vec<channels::Receiver<(T, ErrorContext)>>,
        senders: ThreadSenders,
        os_input: Option<Box<dyn ServerOsApi>>,
    ) -> Self {
        Bus {
            receivers,
            senders,
            os_input,
        }
    }
    #[allow(unused)]
    pub fn should_silently_fail(mut self) -> Self {
        // this is mostly used for the tests
        self.senders.should_silently_fail = true;
        self
    }
    #[allow(unused)]
    pub fn empty() -> Self {
        // this is mostly used for the tests
        Bus {
            receivers: vec![],
            senders: ThreadSenders {
                to_screen: None,
                to_pty: None,
                to_plugin: None,
                to_server: None,
                to_pty_writer: None,
                to_background_jobs: None,
                should_silently_fail: true,
            },
            os_input: None,
        }
    }

    pub fn recv(&self) -> Result<(T, ErrorContext), channels::RecvError> {
        let mut selector = channels::Select::new();
        self.receivers.iter().for_each(|r| {
            selector.recv(r);
        });
        let oper = selector.select();
        let idx = oper.index();
        oper.recv(&self.receivers[idx])
    }

    pub fn recv_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<(T, ErrorContext), channels::RecvTimeoutError> {
        let mut selector = channels::Select::new();
        self.receivers.iter().for_each(|r| {
            selector.recv(r);
        });
        match selector.select_timeout(timeout) {
            Ok(oper) => {
                let idx = oper.index();
                oper.recv(&self.receivers[idx])
                    .map_err(|_| channels::RecvTimeoutError::Disconnected)
            },
            Err(_) => Err(channels::RecvTimeoutError::Timeout),
        }
    }
}
