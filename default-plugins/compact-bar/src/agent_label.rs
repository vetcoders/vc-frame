//! Panels projection for an interactive agent pane.
//!
//! Identity is the live argv from `CommandChanged` or one vacant
//! `get_pane_running_command` hydrate. `PaneInfo::terminal_command` is the
//! launch declaration and is not an input. A leftover title is state evidence
//! only. Repo comes from a cwd the host already published. Absent cwd, model,
//! or runtime evidence stays `unknown`.
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Interactive agents only. `vc-frame visit` and other `vc-*` tools are not
/// in this list: a blanket `vc-` prefix would relabel guest visits.
/// Fleet keys from the launcher's interactive providers. `cursor-agent` is the
/// installed binary for the `cursor` key. A `vc-` prefix is not an agent.
const INTERACTIVE_AGENTS: &[(&str, &str)] = &[
    ("grok", "grok"),
    ("codex", "codex"),
    ("claude", "claude"),
    ("agy", "agy"),
    ("junie", "junie"),
    ("kimi", "kimi"),
    ("cursor-agent", "cursor"),
    ("cursor", "cursor"),
    ("copilot", "copilot"),
    ("gemini", "gemini"),
    ("voc", "Voc"),
    ("vc-o", "Voc"),
    ("vibecrafted", "vc"),
    ("rmcp-mux", "mux"),
    ("aicx", "aicx"),
    ("lbrx-stt", "stt"),
    ("mlx", "mlx"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProjection {
    pub state: &'static str,
    pub repo: String,
    pub agent: String,
    pub model: String,
    pub label: String,
}

/// Project live argv tokens when they name an interactive agent.
/// Held and exited panes are the caller's concern: this function does not
/// read `PaneInfo`. Missing or non-agent argv is not a discovery row.
pub fn project_live(argv: &[String], title: &str, cwd: Option<&str>) -> Option<AgentProjection> {
    let agent = agent_from_argv(argv)?.to_owned();
    let repo = cwd
        .map(repo_without_org)
        .unwrap_or_else(|| "unknown".to_owned());
    let model = model_from_argv(argv).unwrap_or_else(|| "unknown".to_owned());
    let state = state_from_title(title);
    let label = discovery_label(state, &repo, &agent, &model);
    Some(AgentProjection {
        state,
        repo,
        agent,
        model,
        label,
    })
}

pub fn discovery_label(state: &str, repo: &str, agent: &str, model: &str) -> String {
    format!("{state} · {repo} · {agent} · {model}")
}

/// Last path slug, or the component before a `YYYY_MMDD` dispatch day.
/// `org/repo` keeps `repo`. The slug is not humanized.
pub fn repo_without_org(raw: &str) -> String {
    let parts: Vec<&str> = raw
        .split(['/', '\\'])
        .filter(|part| !part.is_empty() && *part != "." && *part != "..")
        .collect();
    if parts.is_empty() {
        return "unknown".to_owned();
    }
    let slug = if let Some(index) = parts.iter().position(|part| is_dispatch_day(part)) {
        if index > 0 {
            parts[index - 1]
        } else {
            parts[parts.len() - 1]
        }
    } else {
        parts[parts.len() - 1]
    };
    sanitize_repo(slug)
}

fn is_dispatch_day(part: &str) -> bool {
    let mut pieces = part.split('_');
    match (pieces.next(), pieces.next(), pieces.next()) {
        (Some(year), Some(month_day), None) => {
            year.len() == 4
                && month_day.len() == 4
                && year.chars().all(|c| c.is_ascii_digit())
                && month_day.chars().all(|c| c.is_ascii_digit())
        },
        _ => false,
    }
}

fn sanitize_repo(slug: &str) -> String {
    let cleaned: String = slug
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(*c, '-' | '_' | '.'))
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

/// Long `--model` on any command. Codex's own grammar also admits the short
/// flag from `codex --help` (`-m, --model <MODEL>`). `python -m` and any other
/// command's `-m` are not model flags. A missing or flag-like value stays absent.
pub fn model_from_argv(argv: &[String]) -> Option<String> {
    let codex = agent_from_argv(argv) == Some("codex");
    let module_flag = interpreter_module_flag_index(argv);
    let mut index = 0;
    while index < argv.len() {
        let arg = &argv[index];
        // `--` ends options. A later `--model` is prompt text, not a flag.
        if arg == "--" {
            break;
        }
        if let Some(value) = arg.strip_prefix("--model=") {
            return nonempty_model(value);
        }
        if arg == "--model" {
            return argv.get(index + 1).and_then(|value| nonempty_model(value));
        }
        if codex && module_flag != Some(index) {
            if let Some(value) = arg.strip_prefix("-m=") {
                return nonempty_model(value);
            }
            if arg == "-m" {
                return argv.get(index + 1).and_then(|value| nonempty_model(value));
            }
        }
        index += 1;
    }
    None
}

/// The interpreter's own `-m <module>` slot. It is not Codex's model flag.
fn interpreter_module_flag_index(argv: &[String]) -> Option<usize> {
    let exe = token_basename(argv.first()?);
    let interpreter = matches!(exe.as_str(), "node" | "nodejs" | "bun" | "deno" | "ruby")
        || exe.starts_with("python");
    if interpreter && argv.get(1).is_some_and(|arg| arg == "-m") {
        Some(1)
    } else {
        None
    }
}

fn nonempty_model(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') {
        None
    } else {
        Some(value.to_owned())
    }
}

pub fn agent_from_argv(argv: &[String]) -> Option<&'static str> {
    let first = argv.first()?;
    let exe = token_basename(first);
    let interpreter = matches!(exe.as_str(), "node" | "nodejs" | "bun" | "deno" | "ruby")
        || exe.starts_with("python");
    let identity = if interpreter {
        let slot = if argv.get(1).is_some_and(|arg| arg == "-m") {
            2
        } else {
            1
        };
        argv.get(slot)
            .filter(|arg| !arg.starts_with('-'))
            .map(|arg| token_basename(arg))
            .unwrap_or(exe)
    } else {
        exe
    };
    for (name, label) in INTERACTIVE_AGENTS {
        if token_matches(&identity, name) {
            return Some(*label);
        }
    }
    if interpreter && identity.contains("mlx") {
        return Some("mlx");
    }
    None
}

fn token_basename(token: &str) -> String {
    token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .trim_start_matches('-')
        .to_lowercase()
}

fn token_matches(token: &str, name: &str) -> bool {
    token == name
        || token
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with('-') || rest.starts_with('.'))
}

/// Title evidence only. A provider status leads the line: a braille spinner,
/// a bare status word, or `{agent} {status}` with nothing after that phrase.
/// A manual name that only contains idle/working/? stays `unknown`. The word
/// `running`, a bare command, or launch acceptance is `unknown`.
pub fn state_from_title(title: &str) -> &'static str {
    let (spinner, rest) = split_leading_spinner(title);
    let head = rest.split('|').next().unwrap_or(rest).trim();
    if let Some(state) = status_head(head) {
        return state;
    }
    if spinner { "working" } else { "unknown" }
}

fn split_leading_spinner(title: &str) -> (bool, &str) {
    let mut chars = title.chars();
    match (chars.next(), chars.next()) {
        (Some(first), Some(' ')) if ('\u{2801}'..='\u{28ff}').contains(&first) => {
            (true, chars.as_str())
        },
        _ => (false, title),
    }
}

fn title_tokens(title: &str) -> Vec<String> {
    title
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| matches!(c, '|' | ':' | ',' | '.')))
        .filter(|token| !token.is_empty())
        .map(|token| token.to_lowercase())
        .collect()
}

/// The status leads. A later copy of the same word inside a manual name does not.
fn status_head(head: &str) -> Option<&'static str> {
    let tokens = title_tokens(head);
    if tokens.is_empty() {
        return None;
    }
    if tokens.first().is_some_and(|token| token == "awaiting")
        && tokens.get(1).is_some_and(|token| token == "question")
    {
        return Some("!");
    }
    if let Some(state) = bare_status_token(&tokens[0]) {
        return Some(state);
    }
    if tokens.len() >= 2 && is_agent_token(&tokens[0]) {
        return exact_agent_status(&tokens[1..]);
    }
    None
}

fn bare_status_token(token: &str) -> Option<&'static str> {
    match token {
        "!" | "?" | "awaiting" => Some("!"),
        "idle" => Some("idle"),
        "working" => Some("working"),
        _ => None,
    }
}

/// `codex idle` is one status phrase. `Junie idle working?` is a manual name.
fn exact_agent_status(tokens: &[String]) -> Option<&'static str> {
    match tokens {
        [one] => bare_status_token(one),
        [first, second] if first == "awaiting" && second == "question" => Some("!"),
        _ => None,
    }
}

fn is_agent_token(token: &str) -> bool {
    INTERACTIVE_AGENTS
        .iter()
        .any(|(name, label)| token == *name || token.eq_ignore_ascii_case(label))
}

/// Repo and agent stay intact while the model is shortened, then dropped.
/// After the model is gone, the agent shrinks before the repo.
pub fn fit_discovery_label(
    state: &str,
    repo: &str,
    agent: &str,
    model: &str,
    cols: usize,
) -> String {
    if cols == 0 {
        return String::new();
    }
    let full = discovery_label(state, repo, agent, model);
    if full.width() <= cols {
        return full;
    }
    let without_model = format!("{state} · {repo} · {agent}");
    if without_model.width() <= cols {
        let budget = cols.saturating_sub(without_model.width() + " · ".width());
        if budget >= 2 {
            let fitted = ellipsize(model, budget);
            if !fitted.is_empty() && fitted != "…" {
                return format!("{without_model} · {fitted}");
            }
        }
        return without_model;
    }
    let head = format!("{state} · {repo}");
    if head.width() < cols {
        let budget = cols.saturating_sub(head.width() + " · ".width());
        if budget >= 1 {
            let fitted = ellipsize(agent, budget);
            if !fitted.is_empty() {
                return format!("{head} · {fitted}");
            }
        }
    }
    ellipsize(&head, cols)
}

fn ellipsize(text: &str, cols: usize) -> String {
    if cols == 0 || text.is_empty() {
        return String::new();
    }
    if text.width() <= cols {
        return text.to_owned();
    }
    if cols == 1 {
        return "…".to_owned();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let next = ch.width().unwrap_or(0);
        if width + next > cols - 1 {
            break;
        }
        out.push(ch);
        width += next;
    }
    out.push('…');
    out
}

/// Re-fit a label this module produced. Other row titles are left alone.
pub fn refit_discovery_label(label: &str, cols: usize) -> Option<String> {
    let mut parts = label.split(" · ");
    let state = parts.next()?;
    let repo = parts.next()?;
    let agent = parts.next()?;
    let model = parts.next()?;
    if parts.next().is_some() || !matches!(state, "unknown" | "idle" | "working" | "!") {
        return None;
    }
    Some(fit_discovery_label(state, repo, agent, model, cols))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_owned()).collect()
    }

    #[test]
    fn model_flag_overrides_and_missing_model_stays_unknown() {
        assert_eq!(
            model_from_argv(&argv(&["codex", "--model", "gpt-5.4"])).as_deref(),
            Some("gpt-5.4")
        );
        assert_eq!(
            model_from_argv(&argv(&["codex", "--model=grok-4.7"])).as_deref(),
            Some("grok-4.7")
        );
        assert_eq!(model_from_argv(&argv(&["codex", "exec"])), None);
        assert_eq!(model_from_argv(&argv(&["codex", "--model"])), None);
        assert_eq!(model_from_argv(&argv(&["python3", "-m", "codex"])), None);
        assert_eq!(
            model_from_argv(&argv(&["codex", "-m", "gpt-6.1-sol"])).as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(
            model_from_argv(&argv(&["/opt/homebrew/bin/codex", "-m", "gpt-6.1-sol"])).as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(
            model_from_argv(&argv(&["codex", "-m=gpt-6.1-sol"])).as_deref(),
            Some("gpt-6.1-sol")
        );
        assert_eq!(model_from_argv(&argv(&["codex", "-m"])), None);
        assert_eq!(model_from_argv(&argv(&["codex", "-m", "--flag"])), None);
        assert_eq!(
            model_from_argv(&argv(&["python", "-m", "http.server"])),
            None
        );
        assert_eq!(
            model_from_argv(&argv(&["python3", "-m", "gpt-6.1-sol"])),
            None
        );
        assert_eq!(
            model_from_argv(&argv(&["claude", "-m", "gpt-6.1-sol"])),
            None
        );
        assert_eq!(model_from_argv(&argv(&["make", "-m", "gpt-6.1-sol"])), None);
        assert_eq!(
            model_from_argv(&argv(&["codex", "--", "discuss --model fake"])),
            None
        );
        assert_eq!(
            model_from_argv(&argv(&["codex", "--", "discuss", "--model", "fake"])),
            None
        );
        assert_eq!(
            model_from_argv(&argv(&[
                "codex",
                "--model",
                "real",
                "--",
                "discuss --model fake"
            ]))
            .as_deref(),
            Some("real")
        );
    }

    #[test]
    fn repo_strips_org_and_prefers_the_slug_before_a_dispatch_day() {
        assert_eq!(repo_without_org("Vetcoders/vc-frame"), "vc-frame");
        assert_eq!(
            repo_without_org(
                "/Users/operator/.vibecrafted/worktrees/vetcoders/vc-frame/2026_1013/grok-work"
            ),
            "vc-frame"
        );
        assert_eq!(
            repo_without_org("/Volumes/vc-workspace/vetcoders/vibecrafted-suite/vc-frame"),
            "vc-frame"
        );
        assert_eq!(repo_without_org(""), "unknown");
        assert_eq!(repo_without_org("/"), "unknown");
    }

    #[test]
    fn narrow_width_keeps_repo_and_agent_and_truncates_the_model_first() {
        let model = "grok-4.7-very-long-model-name";
        let wide = fit_discovery_label("idle", "vc-frame", "codex", model, 80);
        assert_eq!(wide, format!("idle · vc-frame · codex · {model}"));
        let medium = fit_discovery_label("idle", "vc-frame", "codex", model, 32);
        assert!(medium.contains("vc-frame"), "{medium}");
        assert!(medium.contains("codex"), "{medium}");
        assert!(!medium.contains(model), "{medium}");
        assert!(medium.find("vc-frame").unwrap() < medium.find("codex").unwrap());
        let tight = fit_discovery_label("idle", "vc-frame", "codex", model, 20);
        assert!(tight.contains("vc-frame"), "{tight}");
        assert!(!tight.contains(model), "{tight}");
        assert!(tight.find("vc-frame").unwrap() < tight.find("cod").unwrap_or(usize::MAX));
    }

    #[test]
    fn state_follows_title_evidence_and_launch_is_unknown() {
        assert_eq!(state_from_title(""), "unknown");
        assert_eq!(state_from_title("running"), "unknown");
        assert_eq!(state_from_title("codex"), "unknown");
        assert_eq!(state_from_title("⠦ compiling the crate"), "working");
        assert_eq!(state_from_title("codex working"), "working");
        assert_eq!(state_from_title("⠦ awaiting question"), "!");
        assert_eq!(state_from_title("codex ?"), "!");
        assert_eq!(state_from_title("!"), "!");
        assert_eq!(state_from_title("codex idle | project"), "idle");
        assert_eq!(state_from_title("⠦ idle now"), "idle");
        assert_eq!(state_from_title("Junie idle working?"), "unknown");
        assert_eq!(state_from_title("my idle notes"), "unknown");
        assert_eq!(state_from_title("notes ?"), "unknown");
        assert_eq!(state_from_title("working?"), "unknown");
    }

    #[test]
    fn nested_vibecrafted_workflow_stays_vc() {
        assert_eq!(
            agent_from_argv(&argv(&["vibecrafted", "workflow", "claude"])),
            Some("vc")
        );
        assert_eq!(
            agent_from_argv(&argv(&["vc-frame", "visit", "guest"])),
            None
        );
        assert_eq!(
            agent_from_argv(&argv(&["node", "/usr/local/bin/claude"])),
            Some("claude")
        );
    }

    #[test]
    fn fleet_providers_project_and_cursor_agent_is_cursor() {
        let cases = [
            ("codex", "codex"),
            ("claude", "claude"),
            ("agy", "agy"),
            ("junie", "junie"),
            ("grok", "grok"),
            ("kimi", "kimi"),
            ("cursor", "cursor"),
            ("cursor-agent", "cursor"),
            ("copilot", "copilot"),
        ];
        for (binary, agent) in cases {
            let projected =
                project_live(&argv(&[binary]), "Shell", Some("/src/Vetcoders/vc-frame"))
                    .unwrap_or_else(|| panic!("{binary} was not discovered"));
            assert_eq!(projected.agent, agent, "{binary}");
            assert_eq!(projected.repo, "vc-frame");
            assert_eq!(projected.model, "unknown");
            assert_eq!(projected.state, "unknown");
            assert_eq!(
                projected.label,
                format!("unknown · vc-frame · {agent} · unknown")
            );
        }
        assert!(
            project_live(
                &argv(&["vc-doctor", "--fix"]),
                "Shell",
                Some("/src/Vetcoders/vc-frame"),
            )
            .is_none(),
            "a vc- utility is not an interactive agent"
        );
    }
}
