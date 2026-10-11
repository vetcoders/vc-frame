//! `repair plugins` — the plugin generation a session runs vs the intended one.
//!
//! A server instantiates a builtin plugin only from its own embedded asset
//! map, so a session's plugin generation is a property of its engine. The
//! running generation is therefore read from the engine's own image
//! (`setup --dump-plugins` into a private directory, every byte hashed), and
//! compared with the intended artifact's receipt — and that receipt counts as
//! "intended" only as far as its admission goes (a runtime pack pinning the
//! binary, or plainly "this binary" for an unpacked build).
//!
//! A drifted builtin cannot be repaired in place: reloading re-reads the
//! same embedded bytes, and loading the intended bytes from a file would be a
//! second plugin instance outside the role/singleton the session layer owns.
//! Missing or foreign bytes are refused by name; nothing falls back.

use std::collections::BTreeMap;
use std::path::Path;

use zellij_utils::asset_integrity::sha256_hex;

use super::inventory::{
    Admission, ENGINE_QUERY_TIMEOUT, Endpoint, EndpointClass, EngineIdentity, Intended, run_engine,
};
use super::live::LiveSession;
use super::servers::ENGINE_HANDOVER;
use super::treatment::{Scope, Step, Surface, Treatment};
use super::{Finding, Section, Severity};

/// One plugin instance in a live session, as `list-panes` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub url: String,
    pub tabs: Vec<String>,
}

/// Where an instance's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// `zellij:<name>` / `vc-frame:<name>`: the engine's embedded `<name>.wasm`.
    Builtin(String),
    /// `file:<path>`: bytes on disk.
    File(String),
    /// Anything else (remote URL, unresolved alias).
    Other,
}

pub fn source_of(url: &str) -> Source {
    if let Some(name) = url
        .strip_prefix("zellij:")
        .or_else(|| url.strip_prefix("vc-frame:"))
    {
        return Source::Builtin(format!("{name}.wasm"));
    }
    if let Some(path) = url.strip_prefix("file:") {
        return Source::File(path.to_owned());
    }
    Source::Other
}

/// Hash every `plugins/*.wasm` an engine image embeds, dumped into a private
/// directory under `$TMPDIR` that is removed when this returns.
pub fn running_generation(image: &Path) -> Result<BTreeMap<String, String>, String> {
    let scratch = tempfile::Builder::new()
        .prefix("vc-frame-clinic-plugins-")
        .tempdir()
        .map_err(|error| format!("private dump directory: {error}"))?;
    let dir = scratch.path().to_string_lossy().to_string();
    run_engine(
        image,
        &["setup", "--dump-plugins", &dir],
        ENGINE_QUERY_TIMEOUT,
    )
    .map_err(|error| format!("dumping the engine's plugins: {error}"))?;
    let plugins = scratch.path().join("plugins");
    let mut generation = BTreeMap::new();
    for entry in std::fs::read_dir(&plugins).map_err(|error| format!("{error}"))? {
        let entry = entry.map_err(|error| format!("{error}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".wasm") {
            let bytes = std::fs::read(entry.path()).map_err(|error| format!("{name}: {error}"))?;
            generation.insert(name, sha256_hex(&bytes));
        }
    }
    if generation.is_empty() {
        return Err("the engine dumped no plugins".into());
    }
    Ok(generation)
}

/// Plugin instances per URL, with the tabs they appear in.
pub fn instances(panes: &[serde_json::Value]) -> Vec<Instance> {
    let mut by_url: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pane in panes {
        if pane["is_plugin"].as_bool() != Some(true) {
            continue;
        }
        let Some(url) = pane["plugin_url"].as_str() else {
            continue;
        };
        let tab = pane["tab_name"].as_str().unwrap_or("?").to_owned();
        let tabs = by_url.entry(url.to_owned()).or_default();
        if !tabs.contains(&tab) {
            tabs.push(tab);
        }
    }
    by_url
        .into_iter()
        .map(|(url, tabs)| Instance { url, tabs })
        .collect()
}

/// The comparison, pure: instantiated plugins against the running and the
/// intended generation. Returns findings and the refused steps they imply.
pub fn compare(
    session: &str,
    instances: &[Instance],
    running: &BTreeMap<String, String>,
    intended: &BTreeMap<String, String>,
    read_file: &dyn Fn(&str) -> Option<Vec<u8>>,
) -> (Vec<Finding>, Vec<Step>) {
    let mut findings = vec![];
    let mut steps = vec![];
    let intended_digests: Vec<&String> = intended.values().collect();
    for instance in instances {
        let target = format!("session {session} · {}", instance.url);
        let tabs = format!("instantiated in tab(s): {}", instance.tabs.join(", "));
        match source_of(&instance.url) {
            Source::Builtin(name) => {
                let short = |digest: Option<&String>| {
                    digest
                        .map(|d| d[..d.len().min(16)].to_owned())
                        .unwrap_or_else(|| "absent".into())
                };
                let (now, meant) = (running.get(&name), intended.get(&name));
                if now.is_some() && now == meant {
                    continue;
                }
                findings.push(
                    Finding::new(
                        Section::PluginGeneration,
                        Severity::Warn,
                        format!("{name} in session {session} is not the intended generation"),
                        format!("vc-frame repair plugins --session {session}"),
                    )
                    .with_detail(vec![
                        format!("running {} · intended {}", short(now), short(meant)),
                        tabs,
                    ]),
                );
                let step = Step::refused(
                    target,
                    format!("re-instantiate {name} from the intended bytes, keeping its role and configuration"),
                    match meant {
                        None => "foreign bytes: the intended artifact has no such plugin — no fallback".to_owned(),
                        Some(_) => "a builtin plugin is instantiated only from its engine's embedded bytes; \
                                    reload would re-read the same generation"
                            .to_owned(),
                    },
                );
                steps.push(match meant {
                    None => step,
                    Some(_) => step.missing(ENGINE_HANDOVER),
                });
            },
            Source::File(path) => {
                let digest = read_file(&path).map(|bytes| sha256_hex(&bytes));
                let (severity, title, refusal) = match &digest {
                    None => (
                        Severity::Critical,
                        format!(
                            "{path} is missing — a new instance in session {session} cannot load"
                        ),
                        format!(
                            "missing bytes: {path} does not exist — no fallback to another version"
                        ),
                    ),
                    Some(digest) if intended_digests.contains(&digest) => continue,
                    Some(digest) => (
                        Severity::Warn,
                        format!("{path} holds foreign bytes for session {session}"),
                        format!(
                            "foreign bytes: sha256 {} is not in the intended receipt — no fallback",
                            &digest[..16]
                        ),
                    ),
                };
                findings.push(
                    Finding::new(
                        Section::PluginGeneration,
                        severity,
                        title,
                        "point the layout at the intended builtin instead of a file path",
                    )
                    .with_detail(vec![tabs]),
                );
                steps.push(Step::refused(
                    target,
                    "load the intended bytes for this plugin",
                    refusal,
                ));
            },
            Source::Other => {},
        }
    }
    (findings, steps)
}

/// Read-only diagnosis for `doctor`: engine generation by build identity
/// (no dump, no scratch file) and every `file:` plugin's bytes.
pub fn findings(endpoint: &Endpoint, intended: &Intended) -> Vec<Finding> {
    let mut findings = vec![];
    let name = &endpoint.name;
    if let Some(engine) = EngineIdentity::of(endpoint) {
        let verdict = super::servers::judge(&engine, intended);
        if verdict.is_drift() {
            findings.push(
                Finding::new(
                    Section::PluginGeneration,
                    Severity::Warn,
                    format!(
                        "session {name} instantiates its engine's plugin generation, not the intended one"
                    ),
                    format!("vc-frame repair plugins --session {name} --dry-run  (per-plugin digests)"),
                )
                .with_detail(vec![verdict.sentence(), engine.describe()]),
            );
        }
    }
    if let Ok(panes) = LiveSession::new(name).panes() {
        let (file_findings, _) = compare(
            name,
            &instances(&panes),
            &intended.plugins,
            &intended.plugins,
            &|path| std::fs::read(path).ok(),
        );
        findings.extend(file_findings);
    }
    findings
}

/// Plan the treatment for one session.
pub fn treat(
    scope: Scope,
    endpoints: &[Endpoint],
    intended: &Intended,
    dry_run: bool,
) -> Treatment {
    let mut treatment = Treatment::new(Surface::Plugins, scope.clone(), dry_run);
    treatment.notes.push(intended.admission_line());
    let Scope::Session { name, .. } = &scope else {
        return treatment;
    };
    let target = format!("session {name}");
    if let Err(problems) = &intended.self_consistent {
        treatment.steps.push(Step::refused(
            target,
            "compare against the intended plugin bytes",
            format!(
                "the intended artifact disagrees with its own receipt: {}",
                problems.join("; ")
            ),
        ));
        return treatment;
    }
    if let Admission::Contradicted(why) = &intended.admission {
        treatment.steps.push(Step::refused(
            target,
            "compare against the intended plugin bytes",
            format!("the intended artifact is contradicted by its pack: {why}"),
        ));
        return treatment;
    }
    let Some(endpoint) = endpoints
        .iter()
        .find(|e| &e.name == name && e.is_current_contract() && e.class == EndpointClass::Live)
    else {
        treatment.steps.push(Step::refused(
            target,
            "read the session's plugins",
            "no live endpoint of this contract answers for the session",
        ));
        return treatment;
    };
    let Some(engine) = EngineIdentity::of(endpoint) else {
        treatment.steps.push(Step::refused(
            target,
            "identify the engine",
            "the listener vanished",
        ));
        return treatment;
    };
    let Some(image) = engine.intact_image() else {
        treatment.steps.push(
            Step::refused(
                target,
                "read the running plugin generation",
                "the engine's image is not on disk any more — its plugin bytes cannot be proven",
            )
            .missing(ENGINE_HANDOVER),
        );
        return treatment;
    };
    let running = match running_generation(image) {
        Ok(running) => running,
        Err(error) => {
            treatment.steps.push(Step::refused(
                target,
                "read the running plugin generation",
                error,
            ));
            return treatment;
        },
    };
    let session = LiveSession::new(name);
    let panes = match session.panes() {
        Ok(panes) => panes,
        Err(error) => {
            treatment.steps.push(Step::refused(
                target,
                "read the session's plugin instances",
                error,
            ));
            return treatment;
        },
    };
    let instances = instances(&panes);
    treatment.notes.push(format!(
        "engine: {} · {} embedded plugin(s) hashed · {} instantiated plugin url(s)",
        engine.describe(),
        running.len(),
        instances.len()
    ));
    for (wasm, digest) in &running {
        let mark = match intended.plugins.get(wasm) {
            Some(meant) if meant == digest => "=",
            Some(_) => "≠",
            None => "+",
        };
        treatment
            .notes
            .push(format!("  {mark} {wasm} {}", &digest[..16]));
    }
    let (findings, steps) = compare(name, &instances, &running, &intended.plugins, &|path| {
        std::fs::read(path).ok()
    });
    treatment.findings = findings;
    treatment.steps = steps;
    treatment
}

#[cfg(test)]
mod tests {
    use super::*;

    fn generation(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, digest)| ((*name).to_owned(), (*digest).to_owned()))
            .collect()
    }

    fn instance(url: &str) -> Instance {
        Instance {
            url: url.into(),
            tabs: vec!["Launchpad".into()],
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn urls_name_their_byte_source() {
        assert_eq!(
            source_of("zellij:compact-bar"),
            Source::Builtin("compact-bar.wasm".into())
        );
        assert_eq!(
            source_of("file:/rt/plugins/x.wasm"),
            Source::File("/rt/plugins/x.wasm".into())
        );
        assert_eq!(source_of("https://example.com/x.wasm"), Source::Other);
    }

    #[test]
    fn the_intended_generation_is_healthy() {
        let both = generation(&[("compact-bar.wasm", A)]);
        let (findings, steps) = compare(
            "s",
            &[instance("zellij:compact-bar")],
            &both,
            &both,
            &|_| None,
        );
        assert!(findings.is_empty() && steps.is_empty());
    }

    #[test]
    fn an_older_generation_is_refused_with_the_missing_capability() {
        let (findings, steps) = compare(
            "s",
            &[instance("zellij:compact-bar")],
            &generation(&[("compact-bar.wasm", B)]),
            &generation(&[("compact-bar.wasm", A)]),
            &|_| None,
        );
        assert_eq!(findings.len(), 1);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].state, super::super::treatment::StepState::Refused);
        assert_eq!(
            steps[0].missing_capability.as_deref(),
            Some(ENGINE_HANDOVER)
        );
    }

    #[test]
    fn missing_and_foreign_file_bytes_are_refused_by_name() {
        let meant = sha256_hex(b"intended bytes");
        let intended = generation(&[("compact-bar.wasm", meant.as_str())]);
        let (findings, steps) = compare(
            "s",
            &[
                instance("file:/gone/compact-bar.wasm"),
                instance("file:/old/compact-bar.wasm"),
                instance("file:/new/compact-bar.wasm"),
            ],
            &intended,
            &intended,
            &|path| match path {
                "/old/compact-bar.wasm" => Some(b"old bytes".to_vec()),
                // Exactly the intended bytes: healthy, wherever they live.
                "/new/compact-bar.wasm" => Some(b"intended bytes".to_vec()),
                _ => None,
            },
        );
        assert_eq!(findings.len(), 2, "{findings:#?}");
        assert_eq!(steps.len(), 2);
        assert!(steps[0].evidence[0].starts_with("missing bytes"));
        assert!(steps[1].evidence[0].starts_with("foreign bytes"));
        // A builtin the intended artifact does not have is foreign too.
        let (_, foreign) = compare(
            "s",
            &[instance("zellij:unknown")],
            &generation(&[("unknown.wasm", B)]),
            &intended,
            &|_| None,
        );
        assert!(foreign[0].evidence[0].starts_with("foreign bytes"));
    }
}
