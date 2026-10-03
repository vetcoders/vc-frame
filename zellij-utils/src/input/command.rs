//! Trigger a command
use crate::data::{Direction, OriginatingPlugin};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum TerminalAction {
    OpenFile(OpenFilePayload),
    RunCommand(RunCommand),
}

impl TerminalAction {
    pub fn change_cwd(&mut self, new_cwd: PathBuf) {
        match self {
            TerminalAction::OpenFile(open_file_payload) => {
                open_file_payload.cwd = Some(new_cwd);
            },
            TerminalAction::RunCommand(run_command) => {
                run_command.cwd = Some(new_cwd);
            },
        }
    }

    /// What a newly requested pane should run.
    ///
    /// An explicit command wins. A request that names only a directory keeps
    /// the session's configured default shell and moves it into that
    /// directory — `--cwd` must not silently turn a shell pane into a command
    /// pane. A request that names neither falls back to the default shell,
    /// whose cwd the PTY then fills from the pane the caller was looking at.
    pub fn for_new_pane(
        command: Option<RunCommandAction>,
        default_shell: Option<TerminalAction>,
    ) -> Option<TerminalAction> {
        match command {
            Some(command) if command.is_cwd_only() => match (default_shell, command.cwd.clone()) {
                (Some(mut default_shell), Some(cwd)) => {
                    default_shell.change_cwd(cwd);
                    Some(default_shell)
                },
                (Some(default_shell), None) => Some(default_shell),
                // Nothing configured to resolve here: the empty command travels
                // on carrying the requested cwd, and the PTY resolves the shell
                // exactly as it does for a pane that named nothing at all.
                (None, _) => Some(TerminalAction::RunCommand(command.into())),
            },
            Some(command) => Some(TerminalAction::RunCommand(command.into())),
            None => default_shell,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenFilePayload {
    pub path: PathBuf,
    pub line_number: Option<usize>,
    pub cwd: Option<PathBuf>,
    pub originating_plugin: Option<OriginatingPlugin>,
}

impl Default for OpenFilePayload {
    fn default() -> Self {
        OpenFilePayload {
            path: PathBuf::new(),
            line_number: None,
            cwd: None,
            originating_plugin: None,
        }
    }
}

impl OpenFilePayload {
    pub fn new(path: PathBuf, line_number: Option<usize>, cwd: Option<PathBuf>) -> Self {
        OpenFilePayload {
            path,
            line_number,
            cwd,
            originating_plugin: None,
        }
    }
    pub fn with_originating_plugin(mut self, originating_plugin: OriginatingPlugin) -> Self {
        self.originating_plugin = Some(originating_plugin);
        self
    }
}

#[derive(Clone, Debug, Deserialize, Default, Serialize, PartialEq, Eq)]
pub struct RunCommand {
    #[serde(alias = "cmd")]
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub hold_on_close: bool,
    #[serde(default)]
    pub hold_on_start: bool,
    #[serde(default)]
    pub originating_plugin: Option<OriginatingPlugin>,
    #[serde(default)]
    pub use_terminal_title: bool,
}

impl std::fmt::Display for RunCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut command: String = self
            .command
            .as_path()
            .as_os_str()
            .to_string_lossy()
            .to_string();
        for arg in &self.args {
            command.push(' ');
            command.push_str(arg);
        }
        write!(f, "{}", command)
    }
}

/// Intermediate representation
#[derive(Clone, Debug, Deserialize, Default, Serialize, PartialEq, Eq)]
pub struct RunCommandAction {
    #[serde(rename = "cmd")]
    pub command: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub direction: Option<Direction>,
    #[serde(default)]
    pub hold_on_close: bool,
    #[serde(default)]
    pub hold_on_start: bool,
    #[serde(default)]
    pub originating_plugin: Option<OriginatingPlugin>,
    #[serde(default)]
    pub use_terminal_title: bool,
}

impl From<RunCommandAction> for RunCommand {
    fn from(action: RunCommandAction) -> Self {
        RunCommand {
            command: action.command,
            args: action.args,
            cwd: action.cwd,
            hold_on_close: action.hold_on_close,
            hold_on_start: action.hold_on_start,
            originating_plugin: action.originating_plugin,
            use_terminal_title: action.use_terminal_title,
        }
    }
}

impl From<RunCommand> for RunCommandAction {
    fn from(run_command: RunCommand) -> Self {
        RunCommandAction {
            command: run_command.command,
            args: run_command.args,
            cwd: run_command.cwd,
            direction: None,
            hold_on_close: run_command.hold_on_close,
            hold_on_start: run_command.hold_on_start,
            originating_plugin: run_command.originating_plugin,
            use_terminal_title: run_command.use_terminal_title,
        }
    }
}

impl RunCommandAction {
    pub fn new(mut command: Vec<String>) -> Self {
        if command.is_empty() {
            Default::default()
        } else {
            RunCommandAction {
                command: PathBuf::from(command.remove(0)),
                args: command,
                ..Default::default()
            }
        }
    }
    pub fn populate_originating_plugin(&mut self, originating_plugin: OriginatingPlugin) {
        self.originating_plugin = Some(originating_plugin);
    }
    /// A pane the caller placed in a directory without naming a command:
    /// `vc-frame action new-pane --cwd <dir>`. The command stays empty on
    /// purpose — resolving the shell belongs to the session, so the request
    /// carries only the directory that shell has to start in.
    pub fn cwd_only(cwd: PathBuf) -> Self {
        RunCommandAction {
            cwd: Some(cwd),
            // nothing worth printing was asked for, so the pane keeps the title
            // a shell pane has
            use_terminal_title: true,
            ..Default::default()
        }
    }
    /// True when this request names a directory but no command.
    pub fn is_cwd_only(&self) -> bool {
        self.command.as_os_str().is_empty()
    }
}

impl RunCommand {
    pub fn new(command: PathBuf) -> Self {
        RunCommand {
            command,
            ..Default::default()
        }
    }
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }
    /// True when this names a directory but no command; the shell is still the
    /// server's to resolve.
    pub fn is_cwd_only(&self) -> bool {
        self.command.as_os_str().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_shell() -> TerminalAction {
        TerminalAction::RunCommand(RunCommand {
            command: PathBuf::from("zsh"),
            use_terminal_title: true,
            ..Default::default()
        })
    }

    fn resolved(action: Option<TerminalAction>) -> RunCommand {
        match action {
            Some(TerminalAction::RunCommand(run_command)) => run_command,
            other => panic!("expected a command to run, got {other:?}"),
        }
    }

    #[test]
    fn a_cwd_only_request_moves_the_default_shell_into_that_directory() {
        let resolved = resolved(TerminalAction::for_new_pane(
            Some(RunCommandAction::cwd_only(PathBuf::from("/tmp/pane-beta"))),
            Some(default_shell()),
        ));
        assert_eq!(
            resolved.cwd,
            Some(PathBuf::from("/tmp/pane-beta")),
            "the requested directory is the whole point of the request"
        );
        assert_eq!(
            resolved.command,
            PathBuf::from("zsh"),
            "the configured shell still resolves the shell"
        );
        assert!(
            resolved.use_terminal_title,
            "a pane that named no command keeps a shell pane's title"
        );
    }

    #[test]
    fn a_cwd_only_request_keeps_its_directory_without_a_configured_shell() {
        // The PTY is the last rung of the ladder: it resolves the empty command
        // the same way it resolves a pane that named nothing.
        let resolved = resolved(TerminalAction::for_new_pane(
            Some(RunCommandAction::cwd_only(PathBuf::from("/tmp/pane-beta"))),
            None,
        ));
        assert_eq!(resolved.cwd, Some(PathBuf::from("/tmp/pane-beta")));
        assert!(resolved.is_cwd_only());
    }

    #[test]
    fn an_explicit_command_is_untouched() {
        let resolved = resolved(TerminalAction::for_new_pane(
            Some(RunCommandAction {
                command: PathBuf::from("htop"),
                cwd: Some(PathBuf::from("/tmp/pane-beta")),
                ..Default::default()
            }),
            Some(default_shell()),
        ));
        assert_eq!(resolved.command, PathBuf::from("htop"));
        assert_eq!(resolved.cwd, Some(PathBuf::from("/tmp/pane-beta")));
    }

    #[test]
    fn a_pane_that_asked_for_nothing_still_gets_the_bare_default_shell() {
        // No cwd here on purpose: the PTY fills it from the pane the caller was
        // looking at, which is what a plain `new-pane` has always done.
        let resolved = resolved(TerminalAction::for_new_pane(None, Some(default_shell())));
        assert_eq!(resolved.command, PathBuf::from("zsh"));
        assert_eq!(resolved.cwd, None);
    }

    #[test]
    fn a_pane_that_asked_for_nothing_without_a_configured_shell_stays_unresolved() {
        assert!(TerminalAction::for_new_pane(None, None).is_none());
    }
}
