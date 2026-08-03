use std::io::prelude::*;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};

/// Cold clipboard helpers (notably hosted-Windows `powershell.exe`) can take
/// several seconds just to start. One second was enough to kill the writer
/// before it ever flushed, which made `CopyPaneScrollback` look like a missing
/// file instead of a slow child. Twenty seconds still bounds hang risk without
/// losing large scrollbacks on cold hosted runners.
const COPY_COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub struct CopyCommand {
    command: String,
    args: Vec<String>,
}

impl CopyCommand {
    pub fn new(command: String) -> Self {
        let mut command_with_args = command.split(' ').map(String::from);

        Self {
            command: command_with_args.next().expect("missing command"),
            args: command_with_args.collect(),
        }
    }
    pub fn set(&self, value: String) -> Result<()> {
        let mut process = Command::new(self.command.clone())
            .args(self.args.clone())
            .stdin(Stdio::piped())
            .spawn()
            .with_context(|| format!("couldn't spawn {}", self.command))?;
        // Close stdin after the write so the child sees EOF (powershell
        // `ReadToEnd`, `cat > file`, …) and can finish promptly.
        {
            let mut stdin = process.stdin.take().context("could not get stdin")?;
            stdin
                .write_all(value.as_bytes())
                .with_context(|| format!("couldn't write to {} stdin", self.command))?;
        }

        std::thread::spawn(move || {
            let start = std::time::Instant::now();

            loop {
                match process.try_wait() {
                    Ok(Some(_)) => {
                        return; // Process finished normally
                    },
                    Ok(None) => {
                        if start.elapsed() > COPY_COMMAND_TIMEOUT {
                            let _ = process.kill();
                            log::error!(
                                "Copy operation timed out after {} seconds",
                                COPY_COMMAND_TIMEOUT.as_secs()
                            );
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    },
                    Err(e) => {
                        log::error!("Clipboard failure: {}", e);
                        return;
                    },
                }
            }
        });

        Ok(())
    }
}
