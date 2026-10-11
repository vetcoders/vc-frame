//! `repair layouts` — the session's chrome contract against the resolved one.
//!
//! Three layers are compared and never collapsed:
//!
//! - **artifact**: the layouts the session's engine embeds vs the ones the
//!   intended binary embeds (byte digests of `setup --dump-layout`);
//! - **contract**: the chrome roles of the resolved layout — the host layout
//!   for a host session, otherwise the user's own `default_layout` when it is a
//!   personal file, else the built-in product layout;
//! - **state**: the session's own serialization (`action dump-layout`) and its
//!   live panes (`action list-panes`).
//!
//! Only role-defining keys (`session_canvas`, `session_canvas_kind`, `rail`,
//! `frame_host`) and missing/duplicated roles are drift. Every other value a
//! session's chrome carries is personal configuration: kept, and carried into
//! the repaired template.
//!
//! The treatment is a template-only adoption: the corrected `session_layer`
//! and new-tab template are published for future tabs through the existing
//! adoption transaction (generation-guarded, replay-safe), and no existing
//! tab, pane, process or geometry is touched — verified by comparing the
//! pane inventory before and after. Tab-level drift (a tab missing chrome, a
//! second Launchpad) is described and refused: curing it would reflow or
//! close user panes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zellij_utils::asset_integrity::sha256_hex;
use zellij_utils::input::actions::{Action, TemplateAdoption, TemplateAdoptionScope};
use zellij_utils::input::layout::{
    CHROME_ROLE_KEYS, Layout, Run, RunPluginOrAlias, SessionChromeRole, TiledPaneLayout,
    session_chrome_role_of,
};

use super::inventory::{
    ENGINE_QUERY_TIMEOUT, Endpoint, EndpointClass, EngineIdentity, Intended, run_engine,
};
use super::live::{LiveSession, TemplateStatus};
use super::treatment::{Scope, Step, StepState, Surface, Treatment};
use super::{Finding, Section, Severity};

/// The capability an engine needs for this treatment.
pub const TEMPLATE_ONLY_ADOPTION: &str = "template-only adoption: publish the session template for \
     future tabs without overriding existing ones (this engine predates it; the session gets it \
     only through an engine handover or a new session)";

/// The capability a running chrome role needs to take a corrected
/// configuration. `session_canvas` chrome is a singleton: the first instance
/// owns the runtime and loads ITS configuration; every later instance —
/// including a new tab built from a corrected template — only projects that
/// runtime. A reload keeps the original configuration.
pub const CHROME_AUTHORITY_RECONFIGURATION: &str = "chrome-authority-reconfiguration: reload a \
     running session_canvas authority with a corrected configuration while keeping its plugin id, \
     role and projections (reload keeps the original configuration; a template only reaches \
     future projections)";

/// The built-in layouts whose bytes are compared between engine and intended.
const BUILT_IN: [(&str, &[u8]); 3] = [
    ("default", zellij_utils::setup::DEFAULT_LAYOUT),
    ("vibecrafted", zellij_utils::setup::VIBECRAFTED_LAYOUT),
    (
        "vibecrafted-host",
        zellij_utils::setup::VIBECRAFTED_HOST_LAYOUT,
    ),
];

/// One chrome plugin of a layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromePane {
    pub role: SessionChromeRole,
    pub location: String,
    pub configuration: BTreeMap<String, String>,
}

/// Every plugin of a tree that claims a chrome role, plus the ones whose
/// role cannot be resolved (named, never silently ordinary).
pub fn chrome_of(tree: &TiledPaneLayout) -> (Vec<ChromePane>, Vec<String>) {
    let mut chrome = vec![];
    let mut unresolved = vec![];
    walk(tree, &mut |node| {
        if let Some((location, configuration)) = plugin_of(node) {
            match session_chrome_role_of(&location, &configuration) {
                Ok(Some(role)) => chrome.push(ChromePane {
                    role,
                    location,
                    configuration,
                }),
                Ok(None) => {},
                Err(error) => unresolved.push(error),
            }
        }
    });
    (chrome, unresolved)
}

fn walk(tree: &TiledPaneLayout, visit: &mut dyn FnMut(&TiledPaneLayout)) {
    visit(tree);
    for child in &tree.children {
        walk(child, visit);
    }
}

fn walk_mut(tree: &mut TiledPaneLayout, visit: &mut dyn FnMut(&mut TiledPaneLayout)) {
    visit(tree);
    for child in &mut tree.children {
        walk_mut(child, visit);
    }
}

fn plugin_of(node: &TiledPaneLayout) -> Option<(String, BTreeMap<String, String>)> {
    match &node.run {
        Some(Run::Plugin(RunPluginOrAlias::RunPlugin(plugin))) => Some((
            plugin.location.to_string(),
            plugin.configuration.inner().clone(),
        )),
        Some(Run::Plugin(RunPluginOrAlias::Alias(alias))) => Some((
            alias.name.clone(),
            alias
                .configuration
                .as_ref()
                .map(|configuration| configuration.inner().clone())
                .unwrap_or_default(),
        )),
        _ => None,
    }
}

/// How a session's chrome relates to its contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChromeDiff {
    pub layer_missing: bool,
    pub missing: Vec<SessionChromeRole>,
    pub duplicated: Vec<SessionChromeRole>,
    pub extra: Vec<SessionChromeRole>,
    /// `(role, key, live, contract)` for role-defining keys.
    pub role_keys: Vec<(SessionChromeRole, String, Option<String>, Option<String>)>,
    /// `(role, key, live, contract)` for everything else: personal.
    pub personal: Vec<(SessionChromeRole, String, Option<String>, Option<String>)>,
    pub unresolved: Vec<String>,
}

impl ChromeDiff {
    pub fn is_drift(&self) -> bool {
        self.layer_missing
            || !self.missing.is_empty()
            || !self.duplicated.is_empty()
            || !self.role_keys.is_empty()
            || !self.unresolved.is_empty()
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![];
        if self.layer_missing {
            lines.push("the session has no session_layer: new tabs get no chrome".into());
        }
        let roles = |roles: &[SessionChromeRole]| {
            roles
                .iter()
                .map(|role| role.label())
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !self.missing.is_empty() {
            lines.push(format!("missing role(s): {}", roles(&self.missing)));
        }
        if !self.duplicated.is_empty() {
            lines.push(format!("duplicated role(s): {}", roles(&self.duplicated)));
        }
        if !self.extra.is_empty() {
            lines.push(format!(
                "role(s) beyond the contract (kept): {}",
                roles(&self.extra)
            ));
        }
        let pair = |value: &Option<String>| value.clone().unwrap_or_else(|| "(absent)".into());
        for (role, key, live, contract) in &self.role_keys {
            lines.push(format!(
                "{} {key}: {} where the contract says {}",
                role.label(),
                pair(live),
                pair(contract)
            ));
        }
        for (role, key, live, contract) in &self.personal {
            lines.push(format!(
                "personal (kept): {} {key} = {} (contract {})",
                role.label(),
                pair(live),
                pair(contract)
            ));
        }
        lines.extend(self.unresolved.iter().cloned());
        lines
    }
}

/// Compare a session layer with the contract's. Pure.
pub fn diff_chrome(live: Option<&TiledPaneLayout>, contract: &TiledPaneLayout) -> ChromeDiff {
    let (contract_chrome, _) = chrome_of(contract);
    let Some(live) = live else {
        return ChromeDiff {
            layer_missing: !contract_chrome.is_empty(),
            ..ChromeDiff::default()
        };
    };
    let (live_chrome, unresolved) = chrome_of(live);
    let mut diff = ChromeDiff {
        unresolved,
        ..ChromeDiff::default()
    };
    let mut live_by_role: BTreeMap<SessionChromeRole, Vec<&ChromePane>> = BTreeMap::new();
    for pane in &live_chrome {
        live_by_role.entry(pane.role).or_default().push(pane);
    }
    for (role, panes) in &live_by_role {
        if panes.len() > 1 {
            diff.duplicated.push(*role);
        }
        if !contract_chrome.iter().any(|pane| pane.role == *role) {
            diff.extra.push(*role);
        }
    }
    for wanted in &contract_chrome {
        let Some(found) = live_by_role
            .get(&wanted.role)
            .and_then(|panes| panes.first())
        else {
            diff.missing.push(wanted.role);
            continue;
        };
        let mut keys: Vec<&String> = wanted
            .configuration
            .keys()
            .chain(found.configuration.keys())
            .collect();
        keys.sort();
        keys.dedup();
        for key in keys {
            let (live_value, contract_value) = (
                found.configuration.get(key).cloned(),
                wanted.configuration.get(key).cloned(),
            );
            if live_value == contract_value {
                continue;
            }
            let entry = (wanted.role, key.clone(), live_value, contract_value);
            if CHROME_ROLE_KEYS.contains(&key.as_str()) {
                diff.role_keys.push(entry);
            } else {
                diff.personal.push(entry);
            }
        }
    }
    diff
}

/// The contract template, carrying the session's personal chrome values: for
/// every chrome plugin, each non-role key the live session sets wins over the
/// contract's. Applied to the session layer, the new-tab template and every
/// swap layout, so they stay one coherent chrome.
pub fn corrected_template(contract: &Layout, live_layer: Option<&TiledPaneLayout>) -> Layout {
    let mut corrected = contract.clone();
    corrected.tabs.clear();
    corrected.focused_tab_index = None;
    let personal: BTreeMap<SessionChromeRole, BTreeMap<String, String>> = live_layer
        .map(|layer| chrome_of(layer).0)
        .unwrap_or_default()
        .into_iter()
        .rev() // the first instance of a role wins
        .map(|pane| {
            let values = pane
                .configuration
                .into_iter()
                .filter(|(key, _)| !CHROME_ROLE_KEYS.contains(&key.as_str()))
                .collect();
            (pane.role, values)
        })
        .collect();
    let mut carry = |node: &mut TiledPaneLayout| {
        let Some((location, configuration)) = plugin_of(node) else {
            return;
        };
        let Ok(Some(role)) = session_chrome_role_of(&location, &configuration) else {
            return;
        };
        let Some(values) = personal.get(&role) else {
            return;
        };
        match &mut node.run {
            Some(Run::Plugin(RunPluginOrAlias::RunPlugin(plugin))) => {
                for (key, value) in values {
                    plugin.configuration.insert(key.clone(), value.clone());
                }
            },
            Some(Run::Plugin(RunPluginOrAlias::Alias(alias))) => {
                let configuration = alias.configuration.get_or_insert_with(Default::default);
                for (key, value) in values {
                    configuration.insert(key.clone(), value.clone());
                }
            },
            _ => {},
        }
    };
    if let Some((layer, _)) = corrected.session_layer.as_mut() {
        walk_mut(layer, &mut carry);
    }
    if let Some((template, _)) = corrected.template.as_mut() {
        walk_mut(template, &mut carry);
    }
    for (tree, _) in &mut corrected.swap_layouts {
        walk_mut(tree, &mut carry);
    }
    for (variants, _) in &mut corrected.swap_tiled_layouts {
        for tree in variants.values_mut() {
            walk_mut(tree, &mut carry);
        }
    }
    corrected
}

/// The contract a session is judged against, and where it came from.
pub struct Contract {
    pub source: String,
    pub layout: Layout,
}

/// Host sessions are held to the host layout. Guests are held to the user's
/// own `default_layout` when it is a personal file, else the product layout.
pub fn resolve_contract(is_host: bool, personal: Option<&Path>) -> Result<Contract, String> {
    if !is_host
        && let Some(path) = personal
        && !BUILT_IN.iter().any(|(name, _)| Path::new(name) == path)
    {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("personal layout {}: {error}", path.display()))?;
        let layout = Layout::from_str(&raw, path.display().to_string(), None, None)
            .map_err(|error| format!("personal layout {}: {error}", path.display()))?;
        return Ok(Contract {
            source: format!("personal default_layout {}", path.display()),
            layout,
        });
    }
    let name = if is_host {
        "vibecrafted-host"
    } else {
        "vibecrafted"
    };
    let (path, raw, swap) = Layout::stringified_from_default_assets(Path::new(name))
        .map_err(|error| format!("built-in {name}: {error}"))?;
    let layout = Layout::from_str(
        &raw,
        path,
        swap.as_ref()
            .map(|(path, raw)| (path.as_str(), raw.as_str())),
        None,
    )
    .map_err(|error| format!("built-in {name}: {error}"))?;
    Ok(Contract {
        source: format!("built-in {name} layout"),
        layout,
    })
}

fn is_host_layer(layer: Option<&TiledPaneLayout>) -> bool {
    layer.is_some_and(|layer| {
        chrome_of(layer).0.iter().any(|pane| {
            pane.role == SessionChromeRole::SessionManager
                && pane.configuration.get("frame_host").map(String::as_str) == Some("true")
        })
    })
}

/// Tab-level facts from `list-panes`: chrome per tab and product tab names.
pub fn tab_findings(session: &str, panes: &[serde_json::Value]) -> Vec<Finding> {
    let mut chrome: BTreeMap<String, BTreeMap<&'static str, usize>> = BTreeMap::new();
    let mut tab_names: BTreeMap<usize, String> = BTreeMap::new();
    for pane in panes {
        let tab = pane["tab_name"].as_str().unwrap_or("?").to_owned();
        if let Some(id) = pane["tab_id"].as_u64() {
            tab_names.insert(id as usize, tab.clone());
        }
        let counts = chrome.entry(tab).or_default();
        if pane["is_plugin"].as_bool() != Some(true) || pane["is_floating"].as_bool() == Some(true)
        {
            continue;
        }
        let url = pane["plugin_url"].as_str().unwrap_or_default();
        let kind = url.rsplit(':').next().unwrap_or_default();
        for role in ["compact-bar", "session-manager", "status-bar"] {
            if kind == role {
                *counts.entry(role).or_default() += 1;
            }
        }
    }
    let mut findings = vec![];
    for (tab, counts) in &chrome {
        let duplicated: Vec<String> = counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(role, count)| format!("{role} ×{count}"))
            .collect();
        if !duplicated.is_empty() {
            findings.push(
                Finding::new(
                    Section::LayoutContract,
                    Severity::Warn,
                    format!("tab \"{tab}\" of {session} carries duplicated chrome"),
                    "close the duplicate pane by hand if it is not deliberate",
                )
                .with_detail(duplicated),
            );
        }
    }
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    for name in tab_names.values() {
        if matches!(name.as_str(), "Launchpad" | "Agents") {
            *seen.entry(name.as_str()).or_default() += 1;
        }
    }
    for (name, count) in seen {
        if count > 1 {
            findings.push(Finding::new(
                Section::LayoutContract,
                Severity::Warn,
                format!("{session} has {count} \"{name}\" tabs"),
                "close the extra tab by hand — the clinic never closes a tab",
            ));
        }
    }
    findings
}

/// Digest each built-in layout the engine embeds against the intended ones.
fn artifact_findings(session: &str, engine: &EngineIdentity) -> Vec<Finding> {
    let Some(image) = engine.intact_image() else {
        return vec![];
    };
    let mut findings = vec![];
    for (name, intended) in BUILT_IN {
        let intended = sha256_hex(intended);
        match run_engine(image, &["setup", "--dump-layout", name], ENGINE_QUERY_TIMEOUT) {
            Ok(dumped) if sha256_hex(dumped.as_bytes()) == intended => {},
            Ok(dumped) => findings.push(Finding::new(
                Section::LayoutContract,
                Severity::Info,
                format!(
                    "the engine of {session} embeds a different built-in `{name}` layout ({} ≠ intended {})",
                    &sha256_hex(dumped.as_bytes())[..12],
                    &intended[..12]
                ),
                "none — a running session never re-reads built-ins; new sessions on the intended engine get them",
            )),
            Err(error) => findings.push(Finding::new(
                Section::LayoutContract,
                Severity::Info,
                format!("the engine of {session} could not dump `{name}`: {error}"),
                "none",
            )),
        }
    }
    findings
}

/// The pane inventory that must not change while only the template moves.
fn pane_fingerprint(panes: &[serde_json::Value]) -> Vec<String> {
    let mut rows: Vec<String> = panes
        .iter()
        .map(|pane| {
            format!(
                "{}:{} tab{} {} {}x{}@{},{} {} {}",
                if pane["is_plugin"].as_bool() == Some(true) {
                    "plugin"
                } else {
                    "terminal"
                },
                pane["id"],
                pane["tab_id"],
                pane["is_floating"],
                pane["pane_columns"],
                pane["pane_rows"],
                pane["pane_x"],
                pane["pane_y"],
                pane["plugin_url"],
                pane["exited"],
            )
        })
        .collect();
    rows.sort();
    rows
}

/// Existing tabs whose materialized chrome breaks the contract's roles. The
/// session's running chrome authorities were loaded from these tabs, so this
/// is the drift the operator actually sees. Tabs without any chrome are
/// explicit no-layer tabs, not drift.
pub fn tab_chrome_drift(live: &Layout, contract: &TiledPaneLayout) -> Vec<(String, ChromeDiff)> {
    live.tabs
        .iter()
        .filter_map(|(name, tree, _)| {
            if chrome_of(tree).0.is_empty() {
                return None;
            }
            let diff = diff_chrome(Some(tree), contract);
            let role_drift = !diff.role_keys.is_empty()
                || !diff.missing.is_empty()
                || !diff.duplicated.is_empty()
                || !diff.unresolved.is_empty();
            role_drift.then(|| {
                (
                    name.clone().unwrap_or_else(|| "(unnamed)".to_owned()),
                    ChromeDiff {
                        personal: vec![],
                        extra: vec![],
                        ..diff
                    },
                )
            })
        })
        .collect()
}

/// Plan and (unless dry run) apply the layout treatment for one session.
pub fn treat(
    scope: Scope,
    endpoints: &[Endpoint],
    intended: &Intended,
    personal_layout: Option<PathBuf>,
    dry_run: bool,
) -> Treatment {
    let mut treatment = Treatment::new(Surface::Layouts, scope.clone(), dry_run);
    treatment.notes.push(intended.admission_line());
    let Scope::Session { name, .. } = &scope else {
        return treatment;
    };
    let target = format!("session {name}");
    let Some(endpoint) = endpoints
        .iter()
        .find(|e| &e.name == name && e.is_current_contract() && e.class == EndpointClass::Live)
    else {
        treatment.steps.push(Step::refused(
            target,
            "read the session layout",
            "no live endpoint of this contract answers for the session",
        ));
        return treatment;
    };
    if let Some(engine) = EngineIdentity::of(endpoint) {
        treatment
            .notes
            .push(format!("engine: {}", engine.describe()));
        treatment.findings.extend(artifact_findings(name, &engine));
    }
    let session = LiveSession::new(name);
    let dumped = match session.dump_layout() {
        Ok(dumped) => dumped,
        Err(error) => {
            treatment
                .steps
                .push(Step::refused(target, "read the session layout", error));
            return treatment;
        },
    };
    treatment.notes.push(format!(
        "session layout digest {}",
        &sha256_hex(dumped.as_bytes())[..16]
    ));
    let live = match Layout::from_str(&dumped, format!("session {name}"), None, None) {
        Ok(live) => live,
        Err(error) => {
            treatment.steps.push(Step::refused(
                target,
                "read the session layout",
                format!("the session's own serialization does not parse here: {error}"),
            ));
            return treatment;
        },
    };
    let live_layer = live.session_layer.as_ref().map(|(layer, _)| layer);
    let contract = match resolve_contract(is_host_layer(live_layer), personal_layout.as_deref()) {
        Ok(contract) => contract,
        Err(error) => {
            treatment
                .steps
                .push(Step::refused(target, "resolve the chrome contract", error));
            return treatment;
        },
    };
    treatment
        .notes
        .push(format!("contract: {}", contract.source));
    let Some((contract_layer, _)) = contract.layout.session_layer.as_ref() else {
        treatment
            .notes
            .push("the contract has no session layer — nothing to hold the session to".into());
        return treatment;
    };
    let diff = diff_chrome(live_layer, contract_layer);
    let panes = session.panes().unwrap_or_default();
    treatment.findings.extend(tab_findings(name, &panes));
    for finding in treatment.findings.clone() {
        if finding.severity >= Severity::Warn {
            treatment.steps.push(Step::refused(
                format!("{target} · {}", finding.title),
                "cure tab-level drift",
                "refused: curing it would reflow or close user panes — named for a human",
            ));
        }
    }
    let running_drift = tab_chrome_drift(&live, contract_layer);
    for (tab, tab_diff) in &running_drift {
        treatment.findings.push(
            Finding::new(
                Section::LayoutContract,
                Severity::Warn,
                format!("tab \"{tab}\" of {name} runs chrome configured against the contract"),
                "visible until the session's chrome is re-created (a new session, or an engine handover)",
            )
            .with_detail(tab_diff.lines()),
        );
    }
    if !running_drift.is_empty() {
        treatment.steps.push(
            Step::refused(
                format!("{target} · running chrome"),
                "reconfigure the running chrome roles to the contract",
                format!(
                    "the session's chrome runtimes were loaded from tab(s) {}; a template reaches \
                     only future projections and a reload keeps the original configuration",
                    running_drift
                        .iter()
                        .map(|(tab, _)| format!("\"{tab}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
            .missing(CHROME_AUTHORITY_RECONFIGURATION),
        );
    }
    if !diff.is_drift() {
        if !diff.personal.is_empty() || !diff.extra.is_empty() {
            treatment.findings.push(
                Finding::new(
                    Section::LayoutContract,
                    Severity::Info,
                    format!("{name} carries personal chrome configuration"),
                    "none — personal configuration is not drift",
                )
                .with_detail(diff.lines()),
            );
        }
        return treatment;
    }
    treatment.findings.push(
        Finding::new(
            Section::LayoutContract,
            Severity::Warn,
            format!("{name}'s session template drifted from its contract"),
            format!("vc-frame repair layouts --session {name}"),
        )
        .with_detail(diff.lines()),
    );
    let corrected = corrected_template(&contract.layout, live_layer);
    let status = match session.template_status() {
        Ok(status) => status,
        Err(error) => {
            treatment.steps.push(
                Step::refused(
                    target,
                    "re-publish the session template",
                    format!("the engine does not report a template generation: {error}"),
                )
                .missing(TEMPLATE_ONLY_ADOPTION),
            );
            return treatment;
        },
    };
    let mut step = Step::planned(
        target.clone(),
        "re-publish the corrected session template for future tabs; touch no existing tab",
    )
    .with_evidence(format!(
        "adoption request {} at generation {}",
        status.last_request_id + 1,
        status.generation
    ));
    if status.pending {
        step.refuse("another template adoption is pending — refusing to compete with it");
        treatment.steps.push(step);
        return treatment;
    }
    if !dry_run {
        apply(&session, &status, corrected, contract_layer, &mut step);
    }
    treatment.steps.push(step);
    treatment
}

fn apply(
    session: &LiveSession,
    diagnosed: &TemplateStatus,
    corrected: Layout,
    contract_layer: &TiledPaneLayout,
    step: &mut Step,
) {
    // Preconditions, re-read immediately before the write.
    let status = match session.template_status() {
        Ok(status) => status,
        Err(error) => return step.failed(format!("re-reading the generation failed: {error}")),
    };
    if &status != diagnosed {
        return step.refuse(format!(
            "the session template changed since diagnosis ({} → {}) — re-run",
            diagnosed.generation, status.generation
        ));
    }
    let before = match session.panes() {
        Ok(panes) => pane_fingerprint(&panes),
        Err(error) => return step.failed(format!("snapshotting panes failed: {error}")),
    };
    let request = TemplateAdoption {
        request_id: (status.last_request_id + 1).to_string(),
        expected_generation: status.generation.clone(),
        layout: Box::new(corrected),
        scope: TemplateAdoptionScope::TemplateOnly,
    };
    let payload = match request.encode() {
        Ok(payload) => payload,
        Err(error) => return step.failed(format!("encoding the adoption failed: {error}")),
    };
    let action = Action::OverrideLayout {
        tabs: vec![],
        template_adoption: Some(payload),
        retain_existing_terminal_panes: true,
        retain_existing_plugin_panes: true,
        apply_only_to_active_tab: false,
    };
    match session.send(vec![action]) {
        Ok(lines) => step.evidence.push(format!("engine: {}", lines.join(" "))),
        Err(error) if error.contains("unknown field") && error.contains("scope") => {
            step.refuse(format!(
                "the engine rejected the template-only scope: {error}"
            ));
            step.missing_capability = Some(TEMPLATE_ONLY_ADOPTION.to_owned());
            return;
        },
        Err(error) => return step.failed(format!("the engine refused the adoption: {error}")),
    }
    // Verification, from the outside.
    let after_status = match session.template_status() {
        Ok(status) => status,
        Err(error) => {
            return step.failed(format!(
                "adopted, but the generation is unreadable: {error}"
            ));
        },
    };
    if after_status.generation == status.generation {
        return step.failed("adopted, but the template generation did not advance");
    }
    let redumped = match session.dump_layout() {
        Ok(dumped) => dumped,
        Err(error) => {
            return step.failed(format!(
                "adopted, but the session cannot be re-read: {error}"
            ));
        },
    };
    let relaid = match Layout::from_str(&redumped, "after".into(), None, None) {
        Ok(layout) => layout,
        Err(error) => {
            return step.failed(format!("adopted, but the new dump does not parse: {error}"));
        },
    };
    let remaining = diff_chrome(
        relaid.session_layer.as_ref().map(|(layer, _)| layer),
        contract_layer,
    );
    if remaining.is_drift() {
        return step.failed(format!(
            "adopted generation {}, yet the template still drifts: {}",
            after_status.generation,
            remaining.lines().join("; ")
        ));
    }
    let after = match session.panes() {
        Ok(panes) => pane_fingerprint(&panes),
        Err(error) => return step.failed(format!("adopted, but panes are unreadable: {error}")),
    };
    if after != before {
        return step.failed(format!(
            "adopted, but the pane inventory changed ({} → {} panes) — inspect the session",
            before.len(),
            after.len()
        ));
    }
    step.verified(format!(
        "generation {} → {}; the template matches the contract; {} pane(s) unchanged (ids, tabs, geometry)",
        status.generation,
        after_status.generation,
        after.len()
    ));
    debug_assert_eq!(step.state, StepState::Verified);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(kdl: &str) -> TiledPaneLayout {
        let layout = Layout::from_str(kdl, "test".into(), None, None).unwrap();
        layout.session_layer.unwrap().0
    }

    const CONTRACT: &str = r#"
layout {
    session_layer {
        pane size=1 borderless=true {
            plugin location="compact-bar" {
                session_canvas true
                session_canvas_kind "compact-bar"
                brand_text "Vibecrafted"
            }
        }
        pane split_direction="vertical" {
            pane size=24 borderless=true {
                plugin location="session-manager" {
                    session_canvas true
                    session_canvas_kind "session-manager"
                    rail true
                }
            }
            pane { children; }
        }
        pane size=1 borderless=true {
            plugin location="status-bar" {
                session_canvas true
                session_canvas_kind "status-bar"
            }
        }
    }
}
"#;

    #[test]
    fn the_built_in_contracts_carry_all_three_roles() {
        for host in [false, true] {
            let contract = resolve_contract(host, None).unwrap();
            let (layer, _) = contract.layout.session_layer.as_ref().unwrap();
            let roles: Vec<_> = chrome_of(layer).0.into_iter().map(|p| p.role).collect();
            assert_eq!(
                roles,
                vec![
                    SessionChromeRole::CompactBar,
                    SessionChromeRole::SessionManager,
                    SessionChromeRole::StatusBar
                ],
                "{}",
                contract.source
            );
        }
    }

    #[test]
    fn identical_chrome_is_not_drift() {
        let contract = layer(CONTRACT);
        assert!(!diff_chrome(Some(&contract), &contract).is_drift());
    }

    #[test]
    fn a_missing_role_and_a_lost_role_key_are_drift() {
        let contract = layer(CONTRACT);
        let no_status = layer(&CONTRACT.replace(
            r#"session_canvas_kind "status-bar""#,
            r#"session_canvas_kind "compact-bar""#,
        ));
        let diff = diff_chrome(Some(&no_status), &contract);
        assert!(diff.is_drift());
        assert_eq!(diff.missing, vec![SessionChromeRole::StatusBar]);
        assert_eq!(diff.duplicated, vec![SessionChromeRole::CompactBar]);

        let no_rail = layer(&CONTRACT.replace("rail true", ""));
        let diff = diff_chrome(Some(&no_rail), &contract);
        assert!(diff.is_drift());
        assert_eq!(diff.role_keys.len(), 1);
        assert_eq!(diff.role_keys[0].1, "rail");
        assert!(diff_chrome(None, &contract).layer_missing);
    }

    #[test]
    fn personal_values_are_not_drift_and_survive_the_repair() {
        let contract_layout = Layout::from_str(CONTRACT, "contract".into(), None, None).unwrap();
        let contract = contract_layout.session_layer.as_ref().unwrap().0.clone();
        // Personal brand, plus a lost rail: one drift, one taste.
        let live = layer(
            &CONTRACT
                .replace(r#"brand_text "Vibecrafted""#, r#"brand_text "Mine""#)
                .replace("rail true", ""),
        );
        let diff = diff_chrome(Some(&live), &contract);
        assert!(diff.is_drift());
        assert_eq!(diff.personal.len(), 1);
        assert_eq!(diff.personal[0].1, "brand_text");

        let corrected = corrected_template(&contract_layout, Some(&live));
        let fixed = corrected.session_layer.as_ref().unwrap().0.clone();
        assert!(!diff_chrome(Some(&fixed), &contract).is_drift());
        let brand = chrome_of(&fixed)
            .0
            .into_iter()
            .find(|pane| pane.role == SessionChromeRole::CompactBar)
            .unwrap()
            .configuration["brand_text"]
            .clone();
        assert_eq!(
            brand, "Mine",
            "personal configuration is carried, not reset"
        );
        assert!(corrected.tabs.is_empty(), "a template carries no tabs");
    }

    #[test]
    fn existing_tabs_carry_the_running_chrome_and_bare_tabs_are_not_drift() {
        let contract = layer(CONTRACT);
        let live = Layout::from_str(
            r#"
layout {
    tab name="Work" canvas_state="materialized" {
        pane size=1 borderless=true {
            plugin location="compact-bar" {
                session_canvas true
                session_canvas_kind "compact-bar"
            }
        }
        pane split_direction="vertical" {
            pane size=24 borderless=true {
                plugin location="session-manager" {
                    session_canvas true
                    session_canvas_kind "session-manager"
                }
            }
            pane
        }
        pane size=1 borderless=true {
            plugin location="status-bar" {
                session_canvas true
                session_canvas_kind "status-bar"
            }
        }
    }
    tab name="Bare" canvas_state="materialized" {
        pane
    }
}
"#,
            "live".into(),
            None,
            None,
        )
        .unwrap();
        let drift = tab_chrome_drift(&live, &contract);
        assert_eq!(drift.len(), 1, "{drift:#?}");
        assert_eq!(drift[0].0, "Work");
        assert_eq!(drift[0].1.role_keys[0].1, "rail");
        assert!(
            drift[0].1.personal.is_empty(),
            "taste is not reported as drift"
        );
    }

    #[test]
    fn tab_level_duplicates_and_second_launchpads_are_named() {
        let pane = |id: u32, tab_id: usize, tab: &str, url: Option<&str>| {
            serde_json::json!({
                "id": id, "tab_id": tab_id, "tab_name": tab,
                "is_plugin": url.is_some(), "is_floating": false,
                "plugin_url": url,
            })
        };
        let panes = vec![
            pane(1, 0, "Launchpad", Some("zellij:status-bar")),
            pane(2, 0, "Launchpad", Some("zellij:status-bar")),
            pane(3, 1, "Launchpad", None),
            pane(4, 2, "Agents", None),
        ];
        let findings = tab_findings("s", &panes);
        assert_eq!(findings.len(), 2, "{findings:#?}");
        assert!(findings[0].title.contains("duplicated chrome"));
        assert!(findings[1].title.contains("2 \"Launchpad\" tabs"));
    }
}
