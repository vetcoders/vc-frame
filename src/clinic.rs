//! The config clinic: `vc-frame doctor` diagnoses, `vc-frame repair` treats.
//!
//! The disease this exists for: a config generated once — with
//! `keybinds clear-defaults=true` and a full dump of the keybinds of that day —
//! freezes the key contract forever. The binary ships new verbs, the config
//! keeps answering with the old ones, and nothing errors. On 2026-07-30 a
//! 262-line frozen block hid `Ctrl q → CloseFocus` (so `Ctrl q` still meant
//! `Quit`, tearing down whole sessions), hid session `x`, and hid the rail
//! navigation inside LOCK — while the status bar honestly rendered `<q> QUIT`,
//! because that genuinely was the binding in force. Nothing in the product
//! could say so, so it became a product surface instead of tribal knowledge.
//!
//! Two roles that never blur:
//!
//! - [`doctor`] reads. It compares the *effective* contract against the one
//!   shipped in the assets and reports. It writes no byte, anywhere.
//! - [`repair`] writes exactly one file — the user config — and only after a
//!   timestamped backup.
//!
//! The diff is semantic, not textual: both sides are run through the real
//! config parser and compared as `InputMode → key → actions` maps, so a
//! reformatted config is not a finding and a renamed action is.
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kdl::{KdlDocument, KdlNode};
use zellij_utils::cli::{CliArgs, RepairCli};
use zellij_utils::data::{BareKey, InputMode, KeyWithModifier};
use zellij_utils::home::{find_default_config_dir, get_layout_dir};
use zellij_utils::input::actions::Action;
use zellij_utils::input::config::{Config, ConfigError, DEFAULT_CONFIG_FILE_NAME};
use zellij_utils::input::keybinds::Keybinds;
use zellij_utils::input::layout::Layout;
use zellij_utils::input::options::Options;

/// The runtime default when `auto_lock_after_seconds` is absent from the config.
///
/// Mirrors `zellij_client::input_handler::auto_lock_if_idle`, where an absent
/// option means five seconds and an explicit `0` means never. A doctor that
/// treated "absent" as "off" would clear exactly the configs most likely to be
/// stranded — the ones written before the option existed.
const AUTO_LOCK_DEFAULT_SECONDS: u64 = 5;

// ---------------------------------------------------------------- findings ---

/// How loudly a finding should be said, and what it costs at the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth knowing, costs nothing.
    Info,
    /// Something is degraded and the operator should act.
    Warn,
    /// A shipped safety property is actively defeated.
    Critical,
    /// The config could not be read at all.
    Error,
}

impl Severity {
    /// The stable machine token. Consumers branch on this.
    pub fn id(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warn => "warn",
            Severity::Critical => "critical",
            Severity::Error => "error",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warn => "WARN",
            Severity::Critical => "CRITICAL",
            Severity::Error => "ERROR",
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Severity::Info => 0,
            Severity::Warn => 1,
            Severity::Critical | Severity::Error => 2,
        }
    }
}

/// The report sections, in the order they are printed.
///
/// The `id` strings are the documented consumer contract (`docs/DOCTOR.md`) —
/// several findings may share one id at different severities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    ConfigShadowing,
    LockStranding,
    InstallFreshness,
    Shell,
    ConfigParse,
}

impl Section {
    /// The stable machine id emitted as `findings[].id`.
    pub fn id(&self) -> &'static str {
        match self {
            Section::ConfigShadowing => "config-shadowing",
            Section::LockStranding => "lock-stranding",
            Section::InstallFreshness => "install-freshness",
            Section::Shell => "shell",
            Section::ConfigParse => "config-parse",
        }
    }

    fn header(&self) -> &'static str {
        match self {
            Section::ConfigShadowing => "CONFIG SHADOWING",
            Section::LockStranding => "LOCK STRANDING",
            Section::InstallFreshness => "INSTALL FRESHNESS",
            Section::Shell => "SHELL",
            Section::ConfigParse => "CONFIG PARSE",
        }
    }

    const ORDER: [Section; 5] = [
        Section::ConfigShadowing,
        Section::LockStranding,
        Section::InstallFreshness,
        Section::Shell,
        Section::ConfigParse,
    ];
}

/// One thing that is true about this install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub section: Section,
    pub severity: Severity,
    pub title: String,
    /// Evidence lines, printed indented under the title.
    pub detail: Vec<String>,
    pub remedy: String,
}

impl Finding {
    fn new(
        section: Section,
        severity: Severity,
        title: impl Into<String>,
        remedy: impl Into<String>,
    ) -> Self {
        Finding {
            section,
            severity,
            title: title.into(),
            detail: vec![],
            remedy: remedy.into(),
        }
    }

    fn with_detail(mut self, detail: Vec<String>) -> Self {
        self.detail = detail;
        self
    }
}

/// Everything `doctor` learned, ready to render as prose or as JSON.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diagnosis {
    pub findings: Vec<Finding>,
    /// Per-section reassurance, printed when the section produced no finding.
    pub ok_notes: Vec<(Section, String)>,
    /// Sections that could not be examined. A section nobody looked at must not
    /// render as `ok` — that is how a diagnosis starts lying by omission.
    pub skipped: Vec<Section>,
}

impl Diagnosis {
    /// The process exit code: the worst severity present.
    pub fn exit_code(&self) -> i32 {
        self.findings
            .iter()
            .map(|f| f.severity.exit_code())
            .max()
            .unwrap_or(0)
    }

    fn ok_note(&self, section: Section) -> &str {
        self.ok_notes
            .iter()
            .find(|(s, _)| *s == section)
            .map(|(_, note)| note.as_str())
            .unwrap_or("nothing to report")
    }

    fn in_section(&self, section: Section) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.section == section)
            .collect()
    }
}

// ------------------------------------------------------------ keybind diff ---

/// One divergence, collapsed across every mode it appears in.
///
/// A frozen dump binds `Ctrl q → Quit` in fifteen modes; reporting that as
/// fifteen findings would bury the one sentence that matters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindGroup {
    pub key: KeyWithModifier,
    pub modes: Vec<InputMode>,
    /// What is in force now. Empty when the bind is absent.
    pub actual: Vec<Action>,
    /// What the assets ship. Empty when the bind exists only in the user config.
    pub contract: Vec<Action>,
}

impl BindGroup {
    /// True when this bind tears down the whole session while the contract's
    /// cannot — the one class that earns CRITICAL.
    ///
    /// Scope is intentionally narrow: only [`Action::Quit`]. Other
    /// session-damaging verbs (`CloseTab`, `CloseFocus`, kill-session, …)
    /// stay WARN until a fuller taxonomy lands. Do not read this as
    /// "every destructive action".
    pub fn is_destructive_divergence(&self) -> bool {
        quits(&self.actual) && !quits(&self.contract)
    }

    fn render_divergent(&self) -> String {
        let tail = if self.is_destructive_divergence() {
            "  ← kills the whole session"
        } else {
            ""
        };
        format!(
            "{} {} → {}  (contract: {}){}",
            self.render_modes(),
            self.key.to_kdl(),
            render_actions(&self.actual),
            render_actions(&self.contract),
            tail
        )
    }

    fn render_missing(&self) -> String {
        format!(
            "{} {} → {}  missing",
            self.render_modes(),
            self.key.to_kdl(),
            render_actions(&self.contract)
        )
    }

    fn render_lost(&self) -> String {
        format!("{} → {}", self.key.to_kdl(), render_actions(&self.actual))
    }

    fn render_modes(&self) -> String {
        match self.modes.len() {
            0 => "?".to_owned(),
            1 => mode_name(self.modes[0]),
            n if n <= 3 => self
                .modes
                .iter()
                .map(|m| mode_name(*m))
                .collect::<Vec<_>>()
                .join("/"),
            n => format!("[{n} modes]"),
        }
    }
}

/// The semantic delta between the shipped contract and what is in force.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindDiff {
    /// The key is bound on both sides, to different actions.
    pub divergent: Vec<BindGroup>,
    /// The contract binds the key; the effective config does not.
    pub missing: Vec<BindGroup>,
    /// The effective config binds a key the contract leaves alone.
    pub extra: Vec<BindGroup>,
}

impl KeybindDiff {
    /// True when the effective contract is the shipped one.
    pub fn is_clean(&self) -> bool {
        self.divergent.is_empty() && self.missing.is_empty() && self.extra.is_empty()
    }

    /// Divergences that defeat a safety property, worst first.
    pub fn destructive(&self) -> Vec<&BindGroup> {
        self.divergent
            .iter()
            .filter(|g| g.is_destructive_divergence())
            .collect()
    }

    /// Everything a `clear-defaults` retirement would throw away: the user's
    /// own version of a diverging bind, plus binds the contract never had.
    pub fn lost_on_retirement(&self) -> Vec<&BindGroup> {
        self.divergent.iter().chain(self.extra.iter()).collect()
    }
}

/// Compare two keybind maps as `mode → key → actions`, grouped by key.
///
/// Deterministic: modes and keys are walked in sorted order, so two runs on the
/// same pair of configs produce byte-identical reports.
pub fn diff_keybinds(contract: &Keybinds, effective: &Keybinds) -> KeybindDiff {
    let mut modes: BTreeSet<InputMode> = BTreeSet::new();
    modes.extend(contract.0.keys().copied());
    modes.extend(effective.0.keys().copied());

    // (key, actual signature, contract signature) → the group being built. The
    // signatures are full KDL renderings, so two actions that merely *print*
    // alike are never merged.
    let mut groups: BTreeMap<(KeyWithModifier, String, String), BindGroup> = BTreeMap::new();
    let record =
        |key: &KeyWithModifier,
         mode: InputMode,
         actual: &[Action],
         contract: &[Action],
         groups: &mut BTreeMap<(KeyWithModifier, String, String), BindGroup>| {
            let signature = (
                key.clone(),
                actions_signature(actual),
                actions_signature(contract),
            );
            groups
                .entry(signature)
                .or_insert_with(|| BindGroup {
                    key: key.clone(),
                    modes: vec![],
                    actual: actual.to_vec(),
                    contract: contract.to_vec(),
                })
                .modes
                .push(mode);
        };

    let empty = Vec::new();
    for mode in modes {
        let contract_binds = contract.0.get(&mode);
        let effective_binds = effective.0.get(&mode);
        let mut keys: BTreeSet<&KeyWithModifier> = BTreeSet::new();
        if let Some(binds) = contract_binds {
            keys.extend(binds.keys());
        }
        if let Some(binds) = effective_binds {
            keys.extend(binds.keys());
        }
        for key in keys {
            let in_contract = contract_binds.and_then(|b| b.get(key));
            let in_effect = effective_binds.and_then(|b| b.get(key));
            match (in_contract, in_effect) {
                (Some(expected), Some(actual)) if expected != actual => {
                    record(key, mode, actual, expected, &mut groups);
                },
                (Some(expected), None) => record(key, mode, &empty, expected, &mut groups),
                (None, Some(actual)) => record(key, mode, actual, &empty, &mut groups),
                _ => {},
            }
        }
    }

    let mut diff = KeybindDiff::default();
    for group in groups.into_values() {
        if group.actual.is_empty() {
            diff.missing.push(group);
        } else if group.contract.is_empty() {
            diff.extra.push(group);
        } else {
            diff.divergent.push(group);
        }
    }
    // Destructive divergences first: the sentence that matters leads.
    diff.divergent
        .sort_by_key(|g| !g.is_destructive_divergence());
    diff
}

/// Is the operator dropped into LOCK on a timer, with no way to navigate out?
///
/// Returns the effective timeout when both are true. `Ctrl g` is excluded on
/// purpose: it is the *only* binding upstream LOCK has, so a locked block that
/// knows nothing else is exactly the stranded shape.
pub fn lock_stranding(options: &Options, keybinds: &Keybinds) -> Option<u64> {
    let seconds = options
        .auto_lock_after_seconds
        .unwrap_or(AUTO_LOCK_DEFAULT_SECONDS);
    if seconds == 0 {
        return None;
    }
    let unlock = KeyWithModifier::new(BareKey::Char('g')).with_ctrl_modifier();
    let navigable = keybinds
        .0
        .get(&InputMode::Locked)
        .map(|binds| binds.keys().any(|key| key != &unlock))
        .unwrap_or(false);
    (!navigable).then_some(seconds)
}

// ------------------------------------------------------------ kdl shadowing --

/// What the raw config *says* about shadowing, as opposed to what it results in.
///
/// The effective diff cannot tell a frozen dump from a deliberate override:
/// both just produce a different map. Only the `clear-defaults` attribute can,
/// and it lives in the text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowShape {
    /// `clear-defaults=true` on the `keybinds` node: the whole contract is gone.
    pub clear_defaults: bool,
    /// Mode blocks carrying their own `clear-defaults=true`. Shadows less, and
    /// is the harder form to spot by eye.
    pub cleared_modes: Vec<String>,
    /// How many `bind` nodes the block holds. A dump is recognisable by size.
    pub bind_count: usize,
    /// The file carries the `configuration` plugin's autogeneration marker, so
    /// the block came from a preset — and the plugin is the way back.
    pub autogenerated: bool,
}

/// Read the shadowing shape out of a raw config. `None` when there is no
/// `keybinds` node at all, which is the healthy state.
pub fn inspect_shadowing(raw: &str) -> Option<ShadowShape> {
    let document: KdlDocument = raw.parse().ok()?;
    let keybinds = document.get("keybinds")?;
    let mut shape = ShadowShape {
        clear_defaults: is_truthy(keybinds, "clear-defaults"),
        autogenerated: raw.contains("AUTOGENERATED"),
        ..Default::default()
    };
    if let Some(children) = keybinds.children() {
        for block in children.nodes() {
            if is_truthy(block, "clear-defaults") {
                shape.cleared_modes.push(block.name().value().to_owned());
            }
            shape.bind_count += count_binds(block);
        }
    }
    Some(shape)
}

fn count_binds(block: &KdlNode) -> usize {
    block
        .children()
        .map(|children| {
            children
                .nodes()
                .iter()
                .filter(|node| node.name().value() == "bind")
                .count()
        })
        .unwrap_or(0)
}

fn is_truthy(node: &KdlNode, argument: &str) -> bool {
    node.get(argument)
        .and_then(|entry| entry.value().as_bool())
        .unwrap_or(false)
}

// ---------------------------------------------------------------- rendering --

fn mode_name(mode: InputMode) -> String {
    format!("{mode:?}").to_lowercase()
}

fn quits(actions: &[Action]) -> bool {
    actions.iter().any(|action| matches!(action, Action::Quit))
}

/// A full, unambiguous rendering — used for equality, never shown.
fn actions_signature(actions: &[Action]) -> String {
    actions
        .iter()
        .filter_map(|action| action.to_kdl())
        .map(|node| node.to_string())
        .collect::<Vec<_>>()
        .join("; ")
}

/// A compact rendering — `CloseFocus`, `MessagePlugin "session-rail"` — for
/// humans. Never used for equality: two distinct actions can render alike.
fn render_actions(actions: &[Action]) -> String {
    if actions.is_empty() {
        return "(unbound)".to_owned();
    }
    actions
        .iter()
        .map(|action| match action.to_kdl() {
            Some(node) => {
                let name = node.name().value().to_owned();
                match node
                    .entries()
                    .iter()
                    .find_map(|entry| entry.value().as_string())
                {
                    Some(argument) => format!("{name} \"{argument}\""),
                    None => name,
                }
            },
            None => "(unrepresentable)".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Join short items into `width`-bounded lines, ` · ` separated.
fn wrap_joined(items: &[String], width: usize) -> Vec<String> {
    let mut lines: Vec<String> = vec![];
    let mut current = String::new();
    for item in items {
        if !current.is_empty() && current.chars().count() + item.chars().count() + 3 > width {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str(" · ");
        }
        current.push_str(item);
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

// ------------------------------------------------------------------- doctor --

/// Where the user config lives, read-only.
///
/// Deliberately not [`Config::config_file_path`]: that one creates the config
/// directory as a side effect, and `doctor` writes nothing — not even a
/// directory.
fn user_config_path(opts: &CliArgs) -> Option<PathBuf> {
    if let Some(path) = &opts.config {
        return Some(path.clone());
    }
    if let Some(dir) = &opts.config_dir {
        return Some(dir.join(DEFAULT_CONFIG_FILE_NAME));
    }
    Config::default_config_file_path()
}

/// Diagnose the install. Reads; never writes. Returns the exit code.
pub fn doctor(opts: &CliArgs, json: bool) -> i32 {
    let config_path = user_config_path(opts);
    let raw = config_path
        .as_ref()
        .filter(|path| path.exists())
        .and_then(|path| std::fs::read_to_string(path).ok());
    let diagnosis = diagnose(config_path.as_deref(), raw.as_deref());
    let exit_code = diagnosis.exit_code();
    let mut out = std::io::stdout().lock();
    let rendered = if json {
        render_json(&diagnosis, exit_code)
    } else {
        render_report(&diagnosis, exit_code)
    };
    let _ = out.write_all(rendered.as_bytes());
    exit_code
}

/// The whole diagnosis, from a config path and its text. Pure enough to test:
/// the only environment it reads is the install freshness and `$SHELL`.
fn diagnose(config_path: Option<&Path>, raw: Option<&str>) -> Diagnosis {
    let mut diagnosis = Diagnosis::default();

    let contract = match Config::from_default_assets() {
        Ok(config) => config,
        Err(error) => {
            // The assets are compiled in; if they do not parse, nothing below
            // can mean anything.
            diagnosis.findings.push(Finding::new(
                Section::ConfigParse,
                Severity::Error,
                format!("the built-in default config does not parse: {error}"),
                "reinstall vc-frame — this build is corrupt",
            ));
            return diagnosis;
        },
    };

    match (config_path, raw) {
        (Some(path), Some(raw)) => {
            diagnose_config(&mut diagnosis, path, raw, &contract);
        },
        (Some(path), None) => {
            diagnosis.ok_notes.push((
                Section::ConfigShadowing,
                format!(
                    "no config at {} — running the shipped contract",
                    path.display()
                ),
            ));
            diagnosis
                .ok_notes
                .push((Section::ConfigParse, "nothing to parse".to_owned()));
            diagnose_lock(&mut diagnosis, &contract.options, &contract.keybinds);
        },
        _ => {
            diagnosis.ok_notes.push((
                Section::ConfigShadowing,
                "no config directory resolved — running the shipped contract".to_owned(),
            ));
            diagnosis
                .ok_notes
                .push((Section::ConfigParse, "nothing to parse".to_owned()));
            diagnose_lock(&mut diagnosis, &contract.options, &contract.keybinds);
        },
    }

    diagnose_freshness(&mut diagnosis);
    diagnose_shell(&mut diagnosis);
    diagnosis
}

fn diagnose_config(diagnosis: &mut Diagnosis, path: &Path, raw: &str, contract: &Config) {
    let config_only = match Config::from_kdl(raw, Some(contract.clone())) {
        Ok(config) => config,
        Err(error) => {
            diagnosis.findings.push(
                Finding::new(
                    Section::ConfigParse,
                    Severity::Error,
                    format!("{} does not parse", path.display()),
                    "fix the reported KDL error; until then vc-frame runs on defaults",
                )
                .with_detail(parse_error_detail(raw, &error)),
            );
            diagnosis.ok_notes.push((
                Section::ConfigShadowing,
                "not analysed — the config does not parse".to_owned(),
            ));
            diagnosis.skipped.push(Section::ConfigShadowing);
            // A config that does not parse is not in force, so the shipped
            // contract is what the session will actually run.
            diagnose_lock(diagnosis, &contract.options, &contract.keybinds);
            return;
        },
    };
    diagnosis
        .ok_notes
        .push((Section::ConfigParse, "well defined".to_owned()));

    // Startup runs Layout::from_* after config parse and merges layout-side
    // config (including keybinds) into the session. Doctor must use that same
    // session-effective surface — config-only green is a false green.
    let (effective, layout_label) = match apply_session_layout(config_only.clone()) {
        Ok((merged, label)) => (merged, Some(label)),
        Err(error) => {
            diagnosis.findings.push(
                Finding::new(
                    Section::ConfigShadowing,
                    Severity::Warn,
                    "layout not analysed — session keybinds may differ from this report"
                        .to_owned(),
                    "fix default_layout / layout_dir, or remove them to use the built-in default",
                )
                .with_detail(vec![
                    format!("layout resolve error: {error}"),
                    "startup applies the selected layout on top of the config; without a readable layout the clinic cannot claim the effective contract matches the assets".to_owned(),
                ]),
            );
            (config_only, None)
        },
    };

    let shape = inspect_shadowing(raw).unwrap_or_default();
    let diff = diff_keybinds(&contract.keybinds, &effective.keybinds);

    if shape.clear_defaults {
        let mut finding = Finding::new(
            Section::ConfigShadowing,
            if diff.destructive().is_empty() {
                Severity::Warn
            } else {
                Severity::Critical
            },
            format!(
                "keybinds clear-defaults=true ({} binds) shadows the shipped contract",
                shape.bind_count
            ),
            "vc-frame repair key-bindings",
        );
        finding.detail.extend(shadow_evidence(&diff));
        if shape.autogenerated {
            finding.detail.push(
                "this file was autogenerated — the `configuration` plugin (Ctrl+o then c) \
                 offers the same layouts with the fork's corrections folded in"
                    .to_owned(),
            );
        }
        diagnosis.findings.push(finding);
    } else if !shape.cleared_modes.is_empty() {
        let mut finding = Finding::new(
            Section::ConfigShadowing,
            if diff.destructive().is_empty() {
                Severity::Warn
            } else {
                Severity::Critical
            },
            format!(
                "clear-defaults=true on mode block(s): {}",
                shape.cleared_modes.join(", ")
            ),
            "vc-frame repair key-bindings",
        );
        finding.detail.extend(shadow_evidence(&diff));
        diagnosis.findings.push(finding);
    } else if !diff.divergent.is_empty() || !diff.missing.is_empty() {
        for group in diff.destructive() {
            diagnosis.findings.push(
                Finding::new(
                    Section::ConfigShadowing,
                    Severity::Critical,
                    format!(
                        "{} is bound to {} where the contract ships {}",
                        group.key.to_kdl(),
                        render_actions(&group.actual),
                        render_actions(&group.contract)
                    ),
                    "vc-frame repair key-bindings",
                )
                .with_detail(vec![group.render_divergent()]),
            );
        }
        let benign: Vec<&BindGroup> = diff
            .divergent
            .iter()
            .filter(|g| !g.is_destructive_divergence())
            .collect();
        if !benign.is_empty() {
            diagnosis.findings.push(
                Finding::new(
                    Section::ConfigShadowing,
                    Severity::Warn,
                    format!("{} contract bind(s) overridden", benign.len()),
                    "keep them if deliberate; otherwise vc-frame repair key-bindings",
                )
                .with_detail(cap_evidence(
                    benign.iter().map(|g| g.render_divergent()).collect(),
                )),
            );
        }
        if !diff.missing.is_empty() {
            let mut absent: Vec<(bool, String)> = diff
                .missing
                .iter()
                .map(|group| (is_headline(group), group.render_missing()))
                .collect();
            absent.sort_by_key(|(headline, _)| !*headline);
            diagnosis.findings.push(
                Finding::new(
                    Section::ConfigShadowing,
                    Severity::Warn,
                    format!("{} contract bind(s) absent", diff.missing.len()),
                    "vc-frame repair key-bindings, or add the binds by hand",
                )
                .with_detail(cap_evidence(
                    absent.into_iter().map(|(_, line)| line).collect(),
                )),
            );
        }
    } else if diff.is_clean()
        && let Some(label) = layout_label.as_ref()
    {
        diagnosis.ok_notes.push((
            Section::ConfigShadowing,
            format!("session-effective contract (config + layout `{label}`) matches the assets"),
        ));
        // layout_label == None: layout failed; the WARN above already forbids green.
    }

    if !diff.extra.is_empty() {
        diagnosis.findings.push(
            Finding::new(
                Section::ConfigShadowing,
                Severity::Info,
                format!("{} personal bind(s) beyond the contract", diff.extra.len()),
                "none — additions on top of the contract are the healthy shape",
            )
            .with_detail(cap_evidence(
                diff.extra.iter().map(|g| g.render_divergent()).collect(),
            )),
        );
    }

    diagnose_lock(diagnosis, &effective.options, &effective.keybinds);
}

/// Apply the same layout overlay startup uses: `default_layout` (or the
/// built-in `default` asset) merged through [`Layout::from_path_or_default`].
///
/// Returns the merged config and a short layout label for the report.
fn apply_session_layout(config: Config) -> Result<(Config, String), ConfigError> {
    let layout_dir = config
        .options
        .layout_dir
        .clone()
        .or_else(|| get_layout_dir(find_default_config_dir()))
        .map(|dir| dir.canonicalize().unwrap_or(dir));
    let chosen = config.options.default_layout.clone();
    let label = chosen
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "default".to_owned());
    let (_, merged) = Layout::from_path_or_default(chosen.as_ref(), layout_dir, config)?;
    Ok((merged, label))
}

/// How many evidence lines a finding may carry.
///
/// A frozen `clear-defaults` block shadows the *entire* contract — around 170
/// binds. Printing all of them buries the two sentences that matter and makes
/// the report unreadable, which is how a diagnosis stops being read at all. The
/// remedy is one command either way, so the list is evidence, not an inventory.
const EVIDENCE_LIMIT: usize = 16;

/// Is this one of the verbs this fork added — the ones a frozen dump or an
/// upstream-shaped config cannot possibly have?
///
/// LOCK navigation and anything routed to the session rail: `session x`,
/// `session r`, the rail hop inside LOCK. Those are the concrete losses an
/// operator can recognise, so they lead the evidence.
fn is_headline(group: &BindGroup) -> bool {
    headline_priority(group).is_some()
}

/// Lower = more important. Operator-recognisable fork verbs first, then LOCK
/// nav, then other session-rail traffic. Without ranking, Alt product binds
/// crowd out `session x` / `session r` under the evidence cap.
fn headline_priority(group: &BindGroup) -> Option<u8> {
    let sig = actions_signature(&group.contract);
    let key = group.key.to_kdl();
    let session_mode = group.modes.len() == 1 && group.modes[0] == InputMode::Session;
    if session_mode && (key == "x" || key == "r") && sig.contains("session-rail") {
        return Some(0);
    }
    if group.modes.contains(&InputMode::Locked)
        && (key.contains("Ctrl") || key.contains("Alt"))
        && (sig.contains("GoToPreviousTab")
            || sig.contains("GoToNextTab")
            || sig.contains("session-rail")
            || sig.contains("Write"))
    {
        return Some(1);
    }
    if group.modes.contains(&InputMode::Locked) {
        return Some(2);
    }
    if sig.contains("session-rail") {
        return Some(3);
    }
    None
}

/// Truncate to [`EVIDENCE_LIMIT`], saying how much was left out.
fn cap_evidence(mut lines: Vec<String>) -> Vec<String> {
    if lines.len() > EVIDENCE_LIMIT {
        let dropped = lines.len() - EVIDENCE_LIMIT;
        lines.truncate(EVIDENCE_LIMIT);
        lines.push(format!(
            "… and {dropped} more shadowed bind(s) — one command cures all of them"
        ));
    }
    lines
}

/// The evidence lines for a shadowing finding.
///
/// Every dangerous divergence, then every verb this fork added that is now
/// missing (ranked), then a count of the rest. The long tail of a frozen dump
/// *is* the whole upstream contract — a count says that better than a sample
/// of `resize H` and `tmux %` ever could.
fn shadow_evidence(diff: &KeybindDiff) -> Vec<String> {
    let mut detail: Vec<String> = diff
        .destructive()
        .iter()
        .map(|group| group.render_divergent())
        .collect();
    let named = detail.len();

    let mut headlines: Vec<(u8, String)> = Vec::new();
    let mut rest = 0usize;
    for group in diff
        .divergent
        .iter()
        .filter(|group| !group.is_destructive_divergence())
    {
        match headline_priority(group) {
            Some(rank) => headlines.push((rank, group.render_divergent())),
            None => rest += 1,
        }
    }
    for group in &diff.missing {
        match headline_priority(group) {
            Some(rank) => headlines.push((rank, group.render_missing())),
            None => rest += 1,
        }
    }
    headlines.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    detail.extend(headlines.into_iter().map(|(_, line)| line));

    if detail.len() == named && rest == 0 {
        detail.push("no divergence yet — but the block is frozen at today's contract".to_owned());
        return detail;
    }
    // One readable list: keep the first EVIDENCE_LIMIT lines, then a single
    // "… and N more" that folds both headline overflow and the non-headline
    // long tail. Two footers used to fire when the fork's own verbs alone
    // exceeded the cap (Alt product contract added more LOCK headlines).
    if detail.len() > EVIDENCE_LIMIT {
        let dropped = detail.len() - EVIDENCE_LIMIT;
        detail.truncate(EVIDENCE_LIMIT);
        rest += dropped;
    }
    if rest > 0 {
        detail.push(format!(
            "… and {rest} more contract bind(s) shadowed — one command cures all of them"
        ));
    }
    detail
}

/// Say *where* the config broke, not just that it did.
///
/// [`ConfigError`]'s `Display` is a fixed sentence — the position lives in the
/// miette span, which never reaches a plain report. "It does not parse" without
/// a line number is a diagnosis the operator cannot act on.
fn parse_error_detail(raw: &str, error: &ConfigError) -> Vec<String> {
    let (message, offset) = match error {
        ConfigError::KdlError(kdl_error) => (kdl_error.error_message.clone(), kdl_error.offset),
        ConfigError::KdlDeserializationError(kdl_error) => (
            kdl_error
                .help
                .unwrap_or("KDL deserialization error")
                .to_owned(),
            Some(kdl_error.span.offset()),
        ),
        other => (format!("{other}"), None),
    };
    let mut detail = vec![message];
    if let Some(offset) = offset {
        let (line, column) = line_and_column(raw, offset);
        detail.push(format!("at line {line}, column {column}"));
    }
    detail
}

fn line_and_column(raw: &str, offset: usize) -> (usize, usize) {
    // A span offset is a byte index and the config may hold multi-byte text, so
    // walk back to a boundary rather than risk slicing mid-character.
    let mut offset = offset.min(raw.len());
    while offset > 0 && !raw.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &raw[..offset];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map(|last| offset - last)
        .unwrap_or(offset + 1);
    (line, column)
}

fn diagnose_lock(diagnosis: &mut Diagnosis, options: &Options, keybinds: &Keybinds) {
    match lock_stranding(options, keybinds) {
        Some(seconds) => diagnosis.findings.push(
            Finding::new(
                Section::LockStranding,
                Severity::Warn,
                format!("auto_lock_after_seconds {seconds}, and LOCK knows only Ctrl+g"),
                "vc-frame repair key-bindings, or set auto_lock_after_seconds 0",
            )
            .with_detail(vec![format!(
                "the frame will appear to freeze every {seconds}s: no rail hop, \
                 no tab navigation, only Ctrl+g back out"
            )]),
        ),
        None => diagnosis.ok_notes.push((
            Section::LockStranding,
            if options
                .auto_lock_after_seconds
                .unwrap_or(AUTO_LOCK_DEFAULT_SECONDS)
                == 0
            {
                "autolock off".to_owned()
            } else {
                "LOCK keeps its navigation".to_owned()
            },
        )),
    }
}

/// Reuses the single freshness owner — the same comparison `setup --check`
/// prints — rather than re-deriving "is my binary current" a second way.
fn diagnose_freshness(diagnosis: &mut Diagnosis) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let freshness = zellij_utils::install_freshness::current(&cwd);
    if freshness.needs_reinstall() {
        diagnosis.findings.push(
            Finding::new(
                Section::InstallFreshness,
                Severity::Warn,
                "the installed binary does not match the checkout",
                "make install",
            )
            .with_detail(vec![freshness.diagnostic_line()]),
        );
    } else {
        diagnosis
            .ok_notes
            .push((Section::InstallFreshness, freshness.diagnostic_line()));
    }
}

fn diagnose_shell(diagnosis: &mut Diagnosis) {
    // This is the *caller's* environment (the process running `doctor`), not
    // proof of the live server's SHELL. The server has its own env → passwd →
    // /bin/sh chain; do not imply this line audited that process.
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.is_empty() {
        diagnosis.findings.push(
            Finding::new(
                Section::Shell,
                Severity::Info,
                "no SHELL in the caller environment; the server falls back to passwd then /bin/sh",
                "none; informational",
            )
            .with_detail(vec![
                "this line reads the doctor process, not a running server — a host \
                 preset that launches vc-frame directly may still reconstruct the \
                 login shell from passwd before /bin/sh"
                    .to_owned(),
            ]),
        );
    } else {
        diagnosis.ok_notes.push((
            Section::Shell,
            format!("{shell} (caller environment — not the server process)"),
        ));
    }
}

// ------------------------------------------------------------------ reports --

fn render_report(diagnosis: &Diagnosis, exit_code: i32) -> String {
    let mut out = String::from("── vc-frame doctor ──\n");
    for section in Section::ORDER {
        let findings = diagnosis.in_section(section);
        let bracket = format!("[{}]", section.header());
        if findings.is_empty() {
            let status = if diagnosis.skipped.contains(&section) {
                "skipped"
            } else {
                "ok"
            };
            out.push_str(&format!(
                "{bracket:<20}{status:<10}{}\n",
                diagnosis.ok_note(section)
            ));
            continue;
        }
        for (index, finding) in findings.iter().enumerate() {
            let label = if index == 0 {
                bracket.clone()
            } else {
                String::new()
            };
            out.push_str(&format!(
                "{label:<20}{:<10}{}\n",
                finding.severity.label(),
                finding.title
            ));
            for line in &finding.detail {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    let mut remedies: Vec<&str> = vec![];
    for finding in &diagnosis.findings {
        if finding.severity >= Severity::Warn && !remedies.contains(&finding.remedy.as_str()) {
            remedies.push(&finding.remedy);
        }
    }
    for remedy in remedies {
        out.push_str(&format!("    → {remedy}\n"));
    }
    out.push_str(&format!("exit {exit_code}\n"));
    out
}

fn render_json(diagnosis: &Diagnosis, exit_code: i32) -> String {
    let findings: Vec<serde_json::Value> = diagnosis
        .findings
        .iter()
        .map(|finding| {
            serde_json::json!({
                "id": finding.section.id(),
                "severity": finding.severity.id(),
                "title": finding.title,
                "detail": finding.detail.join("; "),
                "remedy": finding.remedy,
            })
        })
        .collect();
    let document = serde_json::json!({
        "version": 1,
        "exit_code": exit_code,
        "findings": findings,
    });
    format!("{document:#}\n")
}

// ------------------------------------------------------------------- repair --

/// What `repair key-bindings` intends to do, decided before a byte is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairPlan {
    /// Human lines describing each edit, in the order they were decided.
    pub steps: Vec<String>,
    /// Binds that genuinely differed from the current defaults and are being
    /// dropped — printed so a deliberate override can be re-applied by hand.
    pub lost: Vec<String>,
    /// The config text after the edits. Equal to the input when nothing to do.
    pub new_text: String,
}

impl RepairPlan {
    /// True when the config is already healthy.
    pub fn is_noop(&self) -> bool {
        self.steps.is_empty()
    }
}

/// Decide the cure for one config text. Pure: no clock, no filesystem.
///
/// `backup_name` only ever appears inside the guard comment, so the plan can be
/// computed for a `--dry-run` without a backup existing.
pub fn plan_key_bindings_repair(
    raw: &str,
    contract: &Keybinds,
    options: &Options,
    today: &str,
    backup_name: &str,
) -> Result<RepairPlan, String> {
    let mut document: KdlDocument = raw
        .parse()
        .map_err(|error| format!("the config does not parse as KDL: {error}"))?;
    let mut plan = RepairPlan {
        steps: vec![],
        lost: vec![],
        new_text: raw.to_owned(),
    };

    let Some(index) = document
        .nodes()
        .iter()
        .position(|node| node.name().value() == "keybinds")
    else {
        return Ok(plan);
    };

    let guard = guard_comment(today, backup_name);

    if is_truthy(&document.nodes()[index], "clear-defaults") {
        let declared = declared_from_node(&document.nodes()[index], options);
        let diff = diff_keybinds(contract, &declared);
        plan.lost
            .extend(diff.lost_on_retirement().iter().map(|g| g.render_lost()));
        plan.steps.push(
            "removed: keybinds block (clear-defaults=true), guard comment left in place".to_owned(),
        );
        retire_node(&mut document, index, &guard);
        plan.new_text = document.to_string();
        return Ok(plan);
    }

    // No blanket clear-defaults: work block by block. A mode block with its own
    // clear-defaults is retired whole for the same reason the outer one is — a
    // partial cure leaves the shadow standing.
    let keybinds = &mut document.nodes_mut()[index];
    let block_names: Vec<String> = keybinds
        .children()
        .map(|children| {
            children
                .nodes()
                .iter()
                .map(|block| block.name().value().to_owned())
                .collect()
        })
        .unwrap_or_default();

    let mut retire_indices: Vec<usize> = vec![];
    for (block_index, block_name) in block_names.iter().enumerate() {
        let Some(children) = keybinds.children() else {
            break;
        };
        let block = &children.nodes()[block_index];
        if is_truthy(block, "clear-defaults") {
            let declared = declared_from_block(block, None, options);
            let diff = diff_keybinds(contract, &declared);
            plan.lost
                .extend(diff.lost_on_retirement().iter().map(|g| g.render_lost()));
            plan.steps.push(format!(
                "removed: `{block_name}` block (clear-defaults=true) — a partial cure keeps shadowing"
            ));
            retire_indices.push(block_index);
            continue;
        }
        let noise = noise_bind_indices(block, contract, options);
        if noise.is_empty() {
            continue;
        }
        plan.steps.push(format!(
            "dropped {} bind(s) from `{block_name}` that only restate the defaults",
            noise.len()
        ));
        let children = keybinds.children_mut().as_mut().expect("children exist");
        let block = &mut children.nodes_mut()[block_index];
        let block_children = block.children_mut().as_mut().expect("binds exist");
        for bind_index in noise.iter().rev() {
            block_children.nodes_mut().remove(*bind_index);
        }
    }

    if let Some(children) = keybinds.children_mut().as_mut() {
        for block_index in retire_indices.iter().rev() {
            children.nodes_mut().remove(*block_index);
        }
        // A block whose every bind was noise is an empty shell; drop it too.
        // Node count, not `KdlDocument::is_empty`: that one measures the
        // rendered length, so the indentation left behind reads as content.
        children.nodes_mut().retain(|block| {
            block
                .children()
                .map(|binds| !binds.nodes().is_empty())
                .unwrap_or(false)
        });
        if children.nodes().is_empty() {
            plan.steps.push(
                "removed: the keybinds block, now empty, guard comment left in place".to_owned(),
            );
            retire_node(&mut document, index, &guard);
        }
    }

    plan.new_text = document.to_string();
    Ok(plan)
}

/// The guard comment left where a retired block used to be.
///
/// It exists so the next reader — human or agent — does not "helpfully" restore
/// a dump, and so the backup that holds the old block is named at the scene.
fn guard_comment(today: &str, backup_name: &str) -> String {
    format!(
        "// keybinds: INTENTIONALLY ABSENT (vc-frame repair key-bindings, {today}).\n\
         // A frozen clear-defaults=true snapshot shadowed every contract shipped\n\
         // in the assets. Add ONLY targeted personal overrides here — never\n\
         // clear-defaults, never a full dump. Old block: {backup_name}\n"
    )
}

/// Remove the node at `index` and leave `guard` standing in its place.
///
/// The node's own leading trivia goes with it, which is the point: the stale
/// "uncomment this to clear defaults" comment above a frozen block should not
/// outlive the block.
fn retire_node(document: &mut KdlDocument, index: usize, guard: &str) {
    document.nodes_mut().remove(index);
    match document.nodes_mut().get_mut(index) {
        Some(next) => {
            let leading = next.leading().unwrap_or_default().to_owned();
            next.set_leading(format!("\n{guard}{leading}"));
        },
        None => {
            let trailing = document.trailing().unwrap_or_default().to_owned();
            document.set_trailing(format!("{trailing}\n{guard}"));
        },
    }
}

/// What a whole `keybinds` node declares *on its own*, with no defaults under it.
///
/// Runs the real parser, so "what does this block mean" is never a second
/// implementation of the semantics. The trivia is stripped because a node
/// stringifies with its leading comments attached, and those would not survive
/// being re-wrapped.
fn declared_from_node(node: &KdlNode, options: &Options) -> Keybinds {
    let mut shell = node.clone();
    shell.set_leading("");
    shell.set_trailing("\n");
    // Force the empty base: the answer must be what this node says, not what it
    // says on top of the defaults we are comparing it against.
    shell.insert("clear-defaults", true);
    parse_declared(&shell.to_string(), options)
}

/// What one block — optionally narrowed to a single `bind` — contributes.
fn declared_from_block(
    block: &KdlNode,
    only_bind: Option<&KdlNode>,
    options: &Options,
) -> Keybinds {
    let text = format!(
        "keybinds clear-defaults=true {{\n{}\n}}\n",
        wrap_block(block, only_bind)
    );
    parse_declared(&text, options)
}

fn parse_declared(keybinds_node: &str, options: &Options) -> Keybinds {
    Keybinds::from_string(keybinds_node.to_owned(), Keybinds::default(), options)
        .map(prune_empty_modes)
        .unwrap_or_default()
}

/// `shared_*` blocks touch every mode, creating empty maps for the ones with
/// nothing bound. Those are not declarations and must not read as ones.
fn prune_empty_modes(mut keybinds: Keybinds) -> Keybinds {
    keybinds.0.retain(|_, binds| !binds.is_empty());
    keybinds
}

/// Wrap one block (optionally with a single bind) as a standalone `keybinds`
/// node, so the real parser can say what it contributes.
///
/// Property entries are dropped and positional ones kept: the mode list of a
/// `shared_except "locked"` is meaning, a `clear-defaults=true` is not — under
/// an empty base it would be a no-op anyway.
fn wrap_block(block: &KdlNode, only_bind: Option<&KdlNode>) -> String {
    let mut shell = KdlNode::new(block.name().value());
    for entry in block.entries() {
        if entry.name().is_none() {
            shell.push(entry.value().clone());
        }
    }
    let mut children = KdlDocument::new();
    match only_bind {
        Some(bind) => children.nodes_mut().push(bind.clone()),
        None => {
            if let Some(existing) = block.children() {
                for node in existing.nodes() {
                    children.nodes_mut().push(node.clone());
                }
            }
        },
    }
    shell.set_children(children);
    shell.to_string()
}

/// Which `bind` nodes in this block say nothing the defaults do not already say.
///
/// Only exact semantic identity counts as noise: every `(mode, key)` the bind
/// contributes must resolve to the very actions the contract has there. Anything
/// less — a different action, a mode the contract leaves alone, a bind we cannot
/// parse — is intent and stays.
fn noise_bind_indices(block: &KdlNode, contract: &Keybinds, options: &Options) -> Vec<usize> {
    let Some(children) = block.children() else {
        return vec![];
    };
    let mut noise = vec![];
    for (index, node) in children.nodes().iter().enumerate() {
        if node.name().value() != "bind" {
            continue;
        }
        let declared = declared_from_block(block, Some(node), options);
        if declared.0.is_empty() {
            continue;
        }
        let identical = declared.0.iter().all(|(mode, binds)| {
            binds.iter().all(|(key, actions)| {
                contract
                    .get_actions_for_key_in_mode(mode, key)
                    .is_some_and(|expected| expected == actions)
            })
        });
        if identical {
            noise.push(index);
        }
    }
    noise
}

/// `config.kdl.bak-<YYYYMMDD-HHMMSS>`, alongside the original.
///
/// UTC, from the one clock reading the whole repair uses, so the name in the
/// guard comment and the file on disk can never disagree.
pub fn backup_path(config_path: &Path, now: SystemTime) -> PathBuf {
    let file_name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(DEFAULT_CONFIG_FILE_NAME);
    let mut backup = config_path.to_path_buf();
    backup.set_file_name(format!("{file_name}.bak-{}", timestamp(now)));
    backup
}

/// `YYYYMMDD-HHMMSS` in UTC, without pulling in a date library.
fn timestamp(now: SystemTime) -> String {
    let stamp = humantime::format_rfc3339_seconds(now).to_string();
    let date: String = stamp.chars().take(10).filter(|c| *c != '-').collect();
    let time: String = stamp
        .chars()
        .skip(11)
        .take(8)
        .filter(|c| *c != ':')
        .collect();
    format!("{date}-{time}")
}

/// `YYYY-MM-DD` in UTC, for the guard comment.
fn today(now: SystemTime) -> String {
    humantime::format_rfc3339_seconds(now)
        .to_string()
        .chars()
        .take(10)
        .collect()
}

/// Treat what `doctor` diagnosed. Writes the user config and nothing else.
pub fn repair(opts: &CliArgs, what: &RepairCli) -> i32 {
    let RepairCli::KeyBindings { dry_run } = what;
    let Some(path) = user_config_path(opts) else {
        println!("no config directory could be resolved — nothing to repair.");
        return 0;
    };
    let mut out = std::io::stdout().lock();
    repair_key_bindings_at(&path, *dry_run, SystemTime::now(), &mut out)
}

/// The IO shell of `repair key-bindings`, with the path, the clock and the sink
/// injected so a test can watch it work in a tempdir.
fn repair_key_bindings_at(
    path: &Path,
    dry_run: bool,
    now: SystemTime,
    out: &mut impl Write,
) -> i32 {
    if !path.exists() {
        let _ = writeln!(
            out,
            "no config file at {} — nothing to repair. vc-frame is already running \
             the shipped contract.",
            path.display()
        );
        return 0;
    }
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            let _ = writeln!(out, "cannot read {}: {error}", path.display());
            return 2;
        },
    };
    let contract = match Config::from_default_assets() {
        Ok(config) => config,
        Err(error) => {
            let _ = writeln!(out, "the built-in default config does not parse: {error}");
            return 2;
        },
    };
    let options = Config::from_kdl(&raw, Some(contract.clone()))
        .map(|config| config.options)
        .unwrap_or_else(|_| contract.options.clone());

    let backup = backup_path(path, now);
    let backup_name = backup
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.kdl.bak")
        .to_owned();

    let plan = match plan_key_bindings_repair(
        &raw,
        &contract.keybinds,
        &options,
        &today(now),
        &backup_name,
    ) {
        Ok(plan) => plan,
        Err(error) => {
            let _ = writeln!(out, "{error}");
            return 2;
        },
    };

    if plan.is_noop() {
        let _ = writeln!(
            out,
            "{} carries no frozen keybinds and no redundant binds — nothing to repair.",
            path.display()
        );
        return 0;
    }

    // Never hand back a config we just broke.
    if let Err(error) = Config::from_kdl(&plan.new_text, Some(contract.clone())) {
        let _ = writeln!(
            out,
            "aborted: the repaired config would not parse ({error}). Nothing was written."
        );
        return 2;
    }

    if dry_run {
        let _ = writeln!(out, "dry run — nothing was written.");
        let _ = writeln!(out, "would back up: {} → {backup_name}", path.display());
    } else {
        if let Err(error) = std::fs::copy(path, &backup) {
            let _ = writeln!(
                out,
                "aborted: could not back up {} to {} ({error}). Nothing was written.",
                path.display(),
                backup.display()
            );
            return 2;
        }
        let _ = writeln!(
            out,
            "backup:  {} → {backup_name}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("config.kdl")
        );
        if let Err(error) = std::fs::write(path, plan.new_text.as_bytes()) {
            let _ = writeln!(
                out,
                "failed to write {}: {error}. The backup at {} holds your config.",
                path.display(),
                backup.display()
            );
            return 2;
        }
    }

    for step in &plan.steps {
        let _ = writeln!(out, "{step}");
    }
    if !plan.lost.is_empty() {
        let _ = writeln!(
            out,
            "kept for you to re-apply as targeted overrides, if you still want them:"
        );
        for line in wrap_joined(&plan.lost, 72) {
            let _ = writeln!(out, "    {line}");
        }
    }
    let _ = writeln!(
        out,
        "restart the session for this to take effect, then re-run: vc-frame doctor"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A shortened but realistic frozen dump: the exact shape that cost a
    /// morning — `clear-defaults=true`, LOCK knowing only `Ctrl g`, and
    /// `Ctrl q → Quit` shadowing the shipped `CloseFocus`.
    const FROZEN_DUMP: &str = r#"// generated once, frozen forever
keybinds clear-defaults=true {
    locked {
        bind "Ctrl g" { SwitchToMode "Normal"; }
    }
    shared_except "locked" {
        bind "Ctrl g" { SwitchToMode "Locked"; }
        bind "Ctrl q" { Quit; }
        bind "Alt n" { NewPane; }
    }
}
theme "vc-frame"
"#;

    /// No `clear-defaults`: two binds that merely restate the shipped defaults
    /// plus one real override.
    const NOISY_OVERLAY: &str = r#"keybinds {
    shared_except "locked" {
        bind "Alt n" { NewPane; }
        bind "Alt f" { ToggleFloatingPanes; }
        bind "Alt y" { Quit; }
    }
}
"#;

    fn contract() -> Config {
        Config::from_default_assets().expect("the shipped assets must parse")
    }

    fn at(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn key(spec: &str) -> KeyWithModifier {
        use std::str::FromStr;
        KeyWithModifier::from_str(spec).expect("test key must parse")
    }

    fn effective(raw: &str) -> Config {
        Config::from_kdl(raw, Some(contract())).expect("fixture must parse")
    }

    // ------------------------------------------------------------ the diff --

    #[test]
    fn a_frozen_dump_diverges_destructively_on_ctrl_q() {
        let diff = diff_keybinds(&contract().keybinds, &effective(FROZEN_DUMP).keybinds);

        let destructive = diff.destructive();
        assert_eq!(destructive.len(), 1, "{:#?}", diff.divergent);
        let ctrl_q = destructive[0];
        assert_eq!(ctrl_q.key, key("Ctrl q"));
        assert!(quits(&ctrl_q.actual));
        assert!(!quits(&ctrl_q.contract));
        let rendered = ctrl_q.render_divergent();
        assert!(rendered.contains("Ctrl q → Quit"), "{rendered}");
        assert!(rendered.contains("contract: CloseFocus"), "{rendered}");
        assert!(rendered.contains("kills the whole session"), "{rendered}");
        // One group, not one per mode.
        assert!(ctrl_q.modes.len() > 5, "{:?}", ctrl_q.modes);
    }

    #[test]
    fn a_frozen_dump_is_missing_the_verbs_added_after_it_froze() {
        let diff = diff_keybinds(&contract().keybinds, &effective(FROZEN_DUMP).keybinds);
        let missing = diff
            .missing
            .iter()
            .map(|group| group.render_missing())
            .collect::<Vec<_>>()
            .join("\n");

        // session "x" — kill the current session and hop to the next.
        assert!(
            missing.contains("session x → MessagePlugin \"session-rail\""),
            "{missing}"
        );
        // The rail navigation inside LOCK.
        assert!(missing.contains("locked Ctrl up"), "{missing}");
        assert!(missing.contains("locked Ctrl down"), "{missing}");
    }

    #[test]
    fn an_untouched_config_produces_no_diff() {
        let diff = diff_keybinds(&contract().keybinds, &contract().keybinds);
        assert!(diff.is_clean(), "{diff:#?}");
    }

    #[test]
    fn a_bind_the_contract_does_not_have_is_extra_not_divergent() {
        let diff = diff_keybinds(&contract().keybinds, &effective(NOISY_OVERLAY).keybinds);
        assert!(diff.missing.is_empty(), "{:#?}", diff.missing);
        assert!(
            diff.extra.iter().any(|group| group.key == key("Alt y")),
            "{:#?}",
            diff.extra
        );
    }

    // -------------------------------------------------------- lock stranding --

    #[test]
    fn a_frozen_locked_block_is_stranded_under_the_shipped_autolock() {
        // The assets ship `auto_lock_after_seconds 5`, so a user config that
        // says nothing inherits it — and a frozen LOCK block that knows only
        // Ctrl+g turns that into a frame which appears to freeze every 5s.
        let config = effective(FROZEN_DUMP);
        assert_eq!(config.options.auto_lock_after_seconds, Some(5));
        assert_eq!(lock_stranding(&config.options, &config.keybinds), Some(5));
    }

    #[test]
    fn an_unset_autolock_reads_as_five_seconds_not_as_off() {
        // Mirrors `auto_lock_if_idle`'s `unwrap_or(5)`. A doctor that read
        // "absent" as "off" would clear exactly the pre-option configs at risk.
        let config = effective(FROZEN_DUMP);
        let mut options = config.options.clone();
        options.auto_lock_after_seconds = None;
        assert_eq!(
            lock_stranding(&options, &config.keybinds),
            Some(AUTO_LOCK_DEFAULT_SECONDS)
        );
    }

    #[test]
    fn the_shipped_contract_is_not_stranded() {
        let config = contract();
        assert_eq!(lock_stranding(&config.options, &config.keybinds), None);
    }

    #[test]
    fn an_explicit_zero_autolock_is_never_stranded() {
        let mut config = effective(FROZEN_DUMP);
        config.options.auto_lock_after_seconds = Some(0);
        assert_eq!(lock_stranding(&config.options, &config.keybinds), None);
    }

    // ------------------------------------------------------------ shadowing --

    #[test]
    fn shadowing_is_read_from_the_text_not_from_the_result() {
        let shape = inspect_shadowing(FROZEN_DUMP).expect("keybinds node present");
        assert!(shape.clear_defaults);
        assert_eq!(shape.bind_count, 4);
        assert!(shape.cleared_modes.is_empty());
        assert!(!shape.autogenerated);

        let overlay = inspect_shadowing(NOISY_OVERLAY).expect("keybinds node present");
        assert!(!overlay.clear_defaults);
        assert_eq!(overlay.bind_count, 3);
    }

    #[test]
    fn a_per_mode_clear_defaults_is_caught_too() {
        let raw = "keybinds {\n    locked clear-defaults=true {\n        bind \"Ctrl g\" { SwitchToMode \"Normal\"; }\n    }\n}\n";
        let shape = inspect_shadowing(raw).expect("keybinds node present");
        assert!(!shape.clear_defaults);
        assert_eq!(shape.cleared_modes, vec!["locked".to_owned()]);
    }

    #[test]
    fn an_autogenerated_file_names_the_plugin_as_the_way_back() {
        let raw = format!("//\n// THIS FILE WAS AUTOGENERATED BY ZELLIJ\n//\n\n{FROZEN_DUMP}");
        assert!(
            inspect_shadowing(&raw)
                .expect("keybinds node")
                .autogenerated
        );

        let diagnosis = diagnose(Some(Path::new("/tmp/config.kdl")), Some(&raw));
        let shadowing = diagnosis.in_section(Section::ConfigShadowing);
        assert_eq!(shadowing.len(), 1, "{shadowing:#?}");
        assert!(
            shadowing[0]
                .detail
                .iter()
                .any(|line| line.contains("`configuration` plugin")),
            "{:#?}",
            shadowing[0].detail
        );
    }

    // -------------------------------------------------------------- diagnose --

    #[test]
    fn a_frozen_dump_exits_two_and_a_clean_config_does_not() {
        let sick = diagnose(Some(Path::new("/tmp/config.kdl")), Some(FROZEN_DUMP));
        assert_eq!(sick.exit_code(), 2, "{sick:#?}");
        let shadowing = sick.in_section(Section::ConfigShadowing);
        assert_eq!(shadowing[0].severity, Severity::Critical);
        assert_eq!(shadowing[0].section.id(), "config-shadowing");
        assert_eq!(shadowing[0].remedy, "vc-frame repair key-bindings");
        assert_eq!(
            sick.in_section(Section::LockStranding)[0].severity,
            Severity::Warn
        );

        // The shipped assets, verbatim, must be a healthy config.
        let healthy = diagnose(
            Some(Path::new("/tmp/config.kdl")),
            Some(std::str::from_utf8(zellij_utils::setup::DEFAULT_CONFIG).unwrap()),
        );
        assert!(
            healthy.in_section(Section::ConfigShadowing).is_empty(),
            "{:#?}",
            healthy.in_section(Section::ConfigShadowing)
        );
        assert!(
            healthy.in_section(Section::LockStranding).is_empty(),
            "{:#?}",
            healthy.in_section(Section::LockStranding)
        );
        let ok = healthy
            .ok_notes
            .iter()
            .find(|(section, _)| *section == Section::ConfigShadowing)
            .map(|(_, note)| note.as_str())
            .unwrap_or("");
        assert!(
            ok.contains("session-effective contract"),
            "green must name the layout-aware path, got {ok:?}"
        );
    }

    /// A layout that injects keybinds is part of session truth. Config-only
    /// analysis would report green; session-effective must catch the Quit.
    #[test]
    fn layout_supplied_keybinds_enter_the_session_effective_diff() {
        let dir = tempfile::tempdir().unwrap();
        let layout_dir = dir.path().join("layouts");
        std::fs::create_dir_all(&layout_dir).unwrap();
        std::fs::write(
            layout_dir.join("evil.kdl"),
            r#"
layout {
    pane
}
keybinds {
    shared_except "locked" {
        bind "Ctrl q" { Quit; }
    }
}
"#,
        )
        .unwrap();

        let raw = format!(
            r#"
default_layout "evil"
layout_dir "{}"
"#,
            layout_dir.display()
        );
        let diagnosis = diagnose(Some(Path::new("/tmp/config.kdl")), Some(&raw));
        let shadowing = diagnosis.in_section(Section::ConfigShadowing);
        assert!(
            shadowing.iter().any(|f| f.severity == Severity::Critical
                && f.title.contains("Ctrl q")
                && f.title.contains("Quit")),
            "layout-injected Quit must be CRITICAL, got {shadowing:#?}"
        );
        assert_eq!(diagnosis.exit_code(), 2, "{diagnosis:#?}");
    }

    #[test]
    fn shell_status_names_the_caller_not_the_server() {
        let diagnosis = diagnose(None, None);
        let from_ok = diagnosis
            .ok_notes
            .iter()
            .find(|(section, _)| *section == Section::Shell)
            .map(|(_, text)| text.as_str());
        let from_finding = diagnosis
            .findings
            .iter()
            .find(|f| f.section == Section::Shell)
            .map(|f| f.title.as_str());
        let shell_text = from_ok.or(from_finding).unwrap_or("");
        assert!(
            shell_text.contains("caller"),
            "shell line must not claim server truth, got {shell_text:?}"
        );
    }

    /// A frozen block shadows the whole contract — ~170 binds. The report has to
    /// stay readable, and the lines that survive the cut have to be the ones an
    /// operator recognises as a loss.
    #[test]
    fn the_evidence_is_capped_and_leads_with_this_forks_own_verbs() {
        let diagnosis = diagnose(Some(Path::new("/tmp/config.kdl")), Some(FROZEN_DUMP));
        let shadowing = diagnosis.in_section(Section::ConfigShadowing);
        let detail = &shadowing[0].detail;

        assert!(detail.len() <= EVIDENCE_LIMIT + 1, "{detail:#?}");
        assert!(
            detail
                .last()
                .unwrap()
                .contains("more contract bind(s) shadowed"),
            "{detail:#?}"
        );
        // The destructive divergence always leads.
        assert!(detail[0].contains("Ctrl q → Quit"), "{detail:#?}");
        // Every verb this fork added and the config now lacks is named.
        let named = detail.join("\n");
        assert!(named.contains("locked Ctrl up"), "{named}");
        assert!(named.contains("locked Ctrl down"), "{named}");
        assert!(
            named.contains("session x → MessagePlugin \"session-rail\""),
            "{named}"
        );
        assert!(
            named.contains("session r → LaunchOrFocusPlugin \"session-rail\""),
            "{named}"
        );
        // The upstream long tail is a count, not a sample.
        assert!(!named.contains("resize H"), "{named}");
        // Deterministic across runs.
        let again = diagnose(Some(Path::new("/tmp/config.kdl")), Some(FROZEN_DUMP));
        assert_eq!(
            &again.in_section(Section::ConfigShadowing)[0].detail,
            detail
        );
    }

    #[test]
    fn a_config_that_does_not_parse_is_an_error_not_a_panic() {
        let diagnosis = diagnose(
            Some(Path::new("/tmp/config.kdl")),
            Some("keybinds { normal { bind \"Ctrl q\" { NoSuchAction; } } }"),
        );
        let parse = diagnosis.in_section(Section::ConfigParse);
        assert_eq!(parse.len(), 1, "{parse:#?}");
        assert_eq!(parse[0].severity, Severity::Error);
        assert_eq!(parse[0].section.id(), "config-parse");
        assert_eq!(diagnosis.exit_code(), 2);
        // Where, not just whether.
        assert!(
            parse[0]
                .detail
                .iter()
                .any(|line| line.starts_with("at line ")),
            "{:#?}",
            parse[0].detail
        );
        // And the section nobody could examine says so instead of saying "ok".
        assert!(diagnosis.skipped.contains(&Section::ConfigShadowing));
        let report = render_report(&diagnosis, diagnosis.exit_code());
        assert!(report.contains("[CONFIG SHADOWING]  skipped"), "{report}");
    }

    #[test]
    fn a_parse_error_position_maps_to_a_line_and_column() {
        assert_eq!(line_and_column("abc", 0), (1, 1));
        assert_eq!(line_and_column("abc", 2), (1, 3));
        assert_eq!(line_and_column("ab\ncd\nef", 3), (2, 1));
        assert_eq!(line_and_column("ab\ncd\nef", 7), (3, 2));
        // An offset past the end must not panic on a multi-byte boundary.
        assert_eq!(line_and_column("ab\n→", 999), (2, 4));
    }

    #[test]
    fn the_json_document_carries_the_documented_contract() {
        let diagnosis = diagnose(Some(Path::new("/tmp/config.kdl")), Some(FROZEN_DUMP));
        let rendered = render_json(&diagnosis, diagnosis.exit_code());
        let parsed: serde_json::Value = serde_json::from_str(&rendered).expect("valid json");

        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["exit_code"], 2);
        let findings = parsed["findings"].as_array().expect("findings array");
        let ids: Vec<&str> = findings.iter().map(|f| f["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&"config-shadowing"), "{ids:?}");
        assert!(ids.contains(&"lock-stranding"), "{ids:?}");
        for finding in findings {
            for field in ["id", "severity", "title", "detail", "remedy"] {
                assert!(finding[field].is_string(), "{field} missing in {finding}");
            }
        }
    }

    #[test]
    fn the_report_aligns_its_columns_and_names_the_exit_code() {
        let diagnosis = diagnose(Some(Path::new("/tmp/config.kdl")), Some(FROZEN_DUMP));
        let report = render_report(&diagnosis, diagnosis.exit_code());
        assert!(report.starts_with("── vc-frame doctor ──\n"), "{report}");
        assert!(
            report.contains("[CONFIG SHADOWING]  CRITICAL  "),
            "{report}"
        );
        assert!(
            report.contains("[LOCK STRANDING]    WARN      "),
            "{report}"
        );
        assert!(
            report.contains("    → vc-frame repair key-bindings\n"),
            "{report}"
        );
        assert!(report.ends_with("exit 2\n"), "{report}");
    }

    // ---------------------------------------------------------------- repair --

    fn plan(raw: &str) -> RepairPlan {
        let contract = contract();
        plan_key_bindings_repair(
            raw,
            &contract.keybinds,
            &contract.options,
            "2026-07-30",
            "config.kdl.bak-20260730-082100",
        )
        .expect("fixture must plan")
    }

    #[test]
    fn a_clear_defaults_block_is_retired_whole_and_guarded() {
        let plan = plan(FROZEN_DUMP);

        let repaired: KdlDocument = plan.new_text.parse().expect("still valid KDL");
        assert!(repaired.get("keybinds").is_none(), "{}", plan.new_text);
        assert!(
            plan.new_text.contains("// keybinds: INTENTIONALLY ABSENT"),
            "{}",
            plan.new_text
        );
        assert!(
            plan.new_text
                .contains("Old block: config.kdl.bak-20260730-082100"),
            "{}",
            plan.new_text
        );
        assert!(
            plan.new_text
                .contains("(vc-frame repair key-bindings, 2026-07-30)"),
            "{}",
            plan.new_text
        );
        // Everything that is not keybinds survives untouched.
        assert!(
            plan.new_text.contains("theme \"vc-frame\""),
            "{}",
            plan.new_text
        );
        // The stale comment that belonged to the retired block goes with it.
        assert!(
            !plan.new_text.contains("generated once, frozen forever"),
            "{}",
            plan.new_text
        );
        // And the result is a config again.
        Config::from_kdl(&plan.new_text, Some(contract())).expect("repaired config must parse");
    }

    #[test]
    fn retirement_names_the_binds_it_throws_away() {
        let plan = plan(FROZEN_DUMP);
        let lost = plan.lost.join(" · ");

        // The user's own version of a diverging bind is a conscious loss.
        assert!(lost.contains("Ctrl q → Quit"), "{lost}");
        // A bind identical to the contract is restored by the removal, not lost.
        assert!(!lost.contains("Alt n"), "{lost}");
    }

    #[test]
    fn without_clear_defaults_only_the_redundant_binds_are_dropped() {
        let plan = plan(NOISY_OVERLAY);

        // Both restatements of the shipped defaults go.
        assert!(!plan.new_text.contains("Alt n"), "{}", plan.new_text);
        assert!(!plan.new_text.contains("Alt f"), "{}", plan.new_text);
        // The real override stays, and so does the block holding it.
        assert!(
            plan.new_text.contains("bind \"Alt y\" { Quit; }"),
            "{}",
            plan.new_text
        );
        assert!(plan.new_text.contains("keybinds"), "{}", plan.new_text);
        assert!(
            !plan.new_text.contains("INTENTIONALLY ABSENT"),
            "{}",
            plan.new_text
        );
        assert_eq!(plan.steps.len(), 1, "{:#?}", plan.steps);
        assert!(
            plan.steps[0].contains("dropped 2 bind(s)"),
            "{:#?}",
            plan.steps
        );
    }

    #[test]
    fn a_block_that_was_all_noise_takes_the_keybinds_node_with_it() {
        let plan = plan(
            "keybinds {\n    shared_except \"locked\" {\n        bind \"Alt n\" { NewPane; }\n    }\n}\n",
        );
        assert!(
            plan.new_text.contains("INTENTIONALLY ABSENT"),
            "{}",
            plan.new_text
        );
        assert!(!plan.new_text.contains("Alt n"), "{}", plan.new_text);
    }

    #[test]
    fn a_per_mode_clear_defaults_block_is_retired_like_the_outer_one() {
        let plan = plan(
            "keybinds {\n    locked clear-defaults=true {\n        bind \"Ctrl g\" { SwitchToMode \"Normal\"; }\n        bind \"Ctrl q\" { Quit; }\n    }\n}\n",
        );
        assert!(
            plan.steps
                .iter()
                .any(|step| step.contains("`locked` block (clear-defaults=true)")),
            "{:#?}",
            plan.steps
        );
        assert!(
            plan.lost.join(" ").contains("Ctrl q → Quit"),
            "{:?}",
            plan.lost
        );
        // It was the only block, so the whole node goes and the guard takes its
        // place. Asserted structurally: the guard comment quotes the phrase
        // `clear-defaults=true` itself, so a text search would always match.
        let repaired: KdlDocument = plan.new_text.parse().expect("still valid KDL");
        assert!(repaired.get("keybinds").is_none(), "{}", plan.new_text);
        assert!(
            plan.new_text.contains("INTENTIONALLY ABSENT"),
            "{}",
            plan.new_text
        );
    }

    #[test]
    fn a_healthy_config_plans_nothing() {
        let plan = plan("theme \"vc-frame\"\n");
        assert!(plan.is_noop(), "{plan:#?}");
        assert_eq!(plan.new_text, "theme \"vc-frame\"\n");
    }

    #[test]
    fn a_config_with_no_keybinds_node_is_left_byte_for_byte() {
        let raw = "// mine\noptions {\n}\nauto_lock_after_seconds 0\n";
        assert_eq!(plan(raw).new_text, raw);
    }

    // ------------------------------------------------------------ the io shell --

    #[test]
    fn the_backup_name_carries_a_sortable_utc_stamp() {
        let backup = backup_path(Path::new("/home/vc/.config/vc-frame/config.kdl"), at(0));
        assert_eq!(
            backup,
            PathBuf::from("/home/vc/.config/vc-frame/config.kdl.bak-19700101-000000")
        );
        assert_eq!(today(at(0)), "1970-01-01");
    }

    #[test]
    fn repair_writes_the_config_after_backing_it_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        std::fs::write(&path, FROZEN_DUMP).unwrap();
        let mut out = Vec::new();

        assert_eq!(repair_key_bindings_at(&path, false, at(0), &mut out), 0);

        let backup = dir.path().join("config.kdl.bak-19700101-000000");
        assert!(backup.exists(), "the backup must exist before the write");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), FROZEN_DUMP);

        let repaired = std::fs::read_to_string(&path).unwrap();
        assert!(repaired.contains("INTENTIONALLY ABSENT"), "{repaired}");
        assert!(!repaired.contains("Quit"), "{repaired}");

        let report = String::from_utf8(out).unwrap();
        assert!(
            report.contains("backup:  config.kdl → config.kdl.bak-19700101-000000"),
            "{report}"
        );
        assert!(report.contains("Ctrl q → Quit"), "{report}");
        assert!(
            report.contains(
                "restart the session for this to take effect, then re-run: vc-frame doctor"
            ),
            "{report}"
        );
    }

    #[test]
    fn a_dry_run_writes_nothing_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        std::fs::write(&path, FROZEN_DUMP).unwrap();
        let mut out = Vec::new();

        assert_eq!(repair_key_bindings_at(&path, true, at(0), &mut out), 0);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), FROZEN_DUMP);
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["config.kdl".to_owned()],
            "no backup on a dry run"
        );

        let report = String::from_utf8(out).unwrap();
        assert!(
            report.contains("dry run — nothing was written."),
            "{report}"
        );
        assert!(report.contains("would back up"), "{report}");
    }

    #[test]
    fn a_missing_config_is_a_clean_exit_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        let mut out = Vec::new();

        assert_eq!(repair_key_bindings_at(&path, false, at(0), &mut out), 0);

        let report = String::from_utf8(out).unwrap();
        assert!(report.contains("nothing to repair"), "{report}");
        assert!(!path.exists(), "repair must not create a config");
    }

    #[test]
    fn a_healthy_config_is_left_alone_by_repair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.kdl");
        std::fs::write(&path, "theme \"vc-frame\"\n").unwrap();
        let mut out = Vec::new();

        assert_eq!(repair_key_bindings_at(&path, false, at(0), &mut out), 0);

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theme \"vc-frame\"\n"
        );
        assert!(
            !dir.path().join("config.kdl.bak-19700101-000000").exists(),
            "a no-op must not leave a backup behind"
        );
        let report = String::from_utf8(out).unwrap();
        assert!(report.contains("nothing to repair"), "{report}");
    }

    #[test]
    fn wrapping_keeps_the_lost_list_readable() {
        let items: Vec<String> = (0..6).map(|i| format!("Alt {i} → NewPane")).collect();
        let lines = wrap_joined(&items, 40);
        assert!(lines.len() > 1, "{lines:?}");
        for line in &lines {
            assert!(line.chars().count() <= 40, "{line}");
        }
        assert_eq!(lines.join(" · "), items.join(" · "));
    }
}
