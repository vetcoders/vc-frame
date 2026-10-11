//! Reading a live session through the CLI actions it already answers.
//!
//! No new IPC message exists for the clinic: a new variant would require a
//! contract bump, and a bumped contract cannot even see the sessions it is
//! supposed to treat. Everything here is an existing `vc-frame action …`
//! request, built by the same CLI parser and captured instead of printed.

use std::path::PathBuf;
use std::time::Duration;

use zellij_client::cli_client::{CapturedReply, capture_actions};
use zellij_utils::cli::{CliArgs, Command};
use zellij_utils::consts::ZELLIJ_SOCK_DIR;
use zellij_utils::input::actions::Action;

/// How long one live query may take end to end.
pub const QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// One session, addressed by the name the rest of the CLI uses.
#[derive(Debug, Clone)]
pub struct LiveSession {
    pub name: String,
    socket: PathBuf,
}

impl LiveSession {
    pub fn new(name: &str) -> Self {
        LiveSession {
            name: name.to_owned(),
            socket: ZELLIJ_SOCK_DIR.join(name),
        }
    }

    /// Run `vc-frame action <argv…>` against this session and capture it.
    pub fn action(&self, argv: &[&str]) -> Result<Vec<String>, String> {
        let actions = parse_action(argv)?;
        self.send(actions)
    }

    pub fn send(&self, actions: Vec<Action>) -> Result<Vec<String>, String> {
        let CapturedReply {
            exit_code,
            stdout,
            stderr,
        } = capture_actions(&self.socket, actions, QUERY_TIMEOUT)
            .map_err(|error| format!("session {}: {error}", self.name))?;
        if exit_code == 0 {
            Ok(stdout)
        } else {
            Err(format!(
                "session {} answered exit {exit_code}: {}",
                self.name,
                stderr.join(" / ")
            ))
        }
    }

    /// `list-panes --json --all`, parsed.
    pub fn panes(&self) -> Result<Vec<serde_json::Value>, String> {
        let text = self.action(&["list-panes", "--json", "--all"])?.join("\n");
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("list-panes: {error}"))?;
        Ok(match value {
            serde_json::Value::Array(rows) => rows,
            other => other["panes"].as_array().cloned().unwrap_or_default(),
        })
    }

    /// `dump-layout`: the session's own serialization, KDL.
    pub fn dump_layout(&self) -> Result<String, String> {
        Ok(self.action(&["dump-layout"])?.join("\n"))
    }

    /// `override-layout --template-status`: `(generation, last_request_id)`.
    pub fn template_status(&self) -> Result<TemplateStatus, String> {
        let line = self
            .action(&["override-layout", "--template-status"])?
            .join(" ");
        TemplateStatus::parse(&line).ok_or_else(|| format!("unreadable template status: {line}"))
    }
}

/// The template generation the server reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateStatus {
    pub generation: String,
    pub pending: bool,
    pub last_request_id: u64,
}

impl TemplateStatus {
    /// `template_generation=<incarnation>:<n> pending=<bool> last_adoption_request_id=<n>`
    pub fn parse(line: &str) -> Option<Self> {
        let field = |key: &str| {
            line.split_whitespace()
                .find_map(|token| token.strip_prefix(key)?.strip_prefix('='))
        };
        Some(TemplateStatus {
            generation: field("template_generation")?.to_owned(),
            pending: field("pending")?.parse().ok()?,
            last_request_id: field("last_adoption_request_id")?.parse().ok()?,
        })
    }
}

/// Build actions from CLI words with the real parser, so the clinic can never
/// send a request the CLI itself would not.
fn parse_action(argv: &[&str]) -> Result<Vec<Action>, String> {
    let words = ["vc-frame", "action"]
        .into_iter()
        .chain(argv.iter().copied());
    let args = CliArgs::try_parse_from(words).map_err(|error| error.to_string())?;
    let Some(Command::Action(cli_action)) = args.command else {
        return Err(format!("not an action: {}", argv.join(" ")));
    };
    Action::actions_from_cli(
        *cli_action,
        Box::new(|| std::env::current_dir().unwrap_or_default()),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_status_is_read_from_the_server_line() {
        let status = TemplateStatus::parse(
            "template_generation=6f1c:3 pending=false last_adoption_request_id=2",
        )
        .unwrap();
        assert_eq!(status.generation, "6f1c:3");
        assert!(!status.pending);
        assert_eq!(status.last_request_id, 2);
        assert_eq!(TemplateStatus::parse("garbage"), None);
    }

    #[test]
    fn actions_come_from_the_real_cli_parser() {
        assert!(matches!(
            parse_action(&["dump-layout"]).unwrap().as_slice(),
            [Action::DumpLayout]
        ));
        assert!(parse_action(&["no-such-action"]).is_err());
    }
}
