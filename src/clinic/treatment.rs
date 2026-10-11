//! The one shape every runtime repair answers in.
//!
//! `servers`, `plugins`, `sockets` and `layouts` are four named treatments of
//! the same clinic, not four systems: each diagnoses into [`Finding`]s, plans
//! [`Step`]s, and ends in one [`Verdict`] with one exit code, rendered by the
//! same report and the same JSON document.
//!
//! A step is never `Verified` because a file changed or a request was sent —
//! only after the effect was observed again from the outside.

use std::io::Write;

use super::{Finding, Severity};

/// The surface a repair treats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Servers,
    Plugins,
    Sockets,
    Layouts,
}

impl Surface {
    pub fn id(&self) -> &'static str {
        match self {
            Surface::Servers => "servers",
            Surface::Plugins => "plugins",
            Surface::Sockets => "sockets",
            Surface::Layouts => "layouts",
        }
    }
}

/// Where one step stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    /// Decided, not executed (dry run, or not reached).
    Planned,
    /// Executed and its effect observed again from outside.
    Verified,
    /// Not executed: a precondition or a protection said no. The reason is
    /// the product answer, not an error.
    Refused,
    /// Executed or attempted and did not end as intended.
    Failed,
}

impl StepState {
    pub fn id(&self) -> &'static str {
        match self {
            StepState::Planned => "planned",
            StepState::Verified => "verified",
            StepState::Refused => "refused",
            StepState::Failed => "failed",
        }
    }
}

/// One decided action on one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub target: String,
    pub action: String,
    pub state: StepState,
    /// Why it was refused or failed, or what verified it.
    pub evidence: Vec<String>,
    /// The exact capability whose absence made this a refusal.
    pub missing_capability: Option<String>,
}

impl Step {
    pub fn planned(target: impl Into<String>, action: impl Into<String>) -> Self {
        Step {
            target: target.into(),
            action: action.into(),
            state: StepState::Planned,
            evidence: vec![],
            missing_capability: None,
        }
    }

    pub fn refused(
        target: impl Into<String>,
        action: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Step {
            state: StepState::Refused,
            evidence: vec![reason.into()],
            ..Step::planned(target, action)
        }
    }

    pub fn missing(mut self, capability: impl Into<String>) -> Self {
        self.missing_capability = Some(capability.into());
        self
    }

    pub fn with_evidence(mut self, line: impl Into<String>) -> Self {
        self.evidence.push(line.into());
        self
    }

    pub fn verified(&mut self, line: impl Into<String>) {
        self.state = StepState::Verified;
        self.evidence.push(line.into());
    }

    pub fn failed(&mut self, line: impl Into<String>) {
        self.state = StepState::Failed;
        self.evidence.push(line.into());
    }

    /// A protection held at apply time: nothing was changed.
    pub fn refuse(&mut self, line: impl Into<String>) {
        self.state = StepState::Refused;
        self.evidence.push(line.into());
    }
}

/// The single answer a repair run ends in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing drifted; nothing was planned.
    Healthy,
    /// Dry run: every planned step is executable.
    WouldRepair,
    /// Every step executed and verified.
    Repaired,
    /// Some steps verified, others refused or failed.
    Partial,
    /// Nothing could be treated safely; nothing was changed.
    Refused,
    /// A step was attempted and did not verify; nothing else succeeded.
    Failed,
    /// Several candidates and no selector: the run names them and stops.
    NeedsSelection,
}

impl Verdict {
    pub fn id(&self) -> &'static str {
        match self {
            Verdict::Healthy => "healthy",
            Verdict::WouldRepair => "would-repair",
            Verdict::Repaired => "repaired",
            Verdict::Partial => "partial",
            Verdict::Refused => "refused",
            Verdict::Failed => "failed",
            Verdict::NeedsSelection => "needs-selection",
        }
    }

    /// 0 healthy / repaired / dry run that would fully repair; 1 partial;
    /// 2 refused, failed, or a selection is required.
    pub fn exit_code(&self) -> i32 {
        match self {
            Verdict::Healthy | Verdict::WouldRepair | Verdict::Repaired => 0,
            Verdict::Partial => 1,
            Verdict::Refused | Verdict::Failed | Verdict::NeedsSelection => 2,
        }
    }

    /// Derive the verdict from the steps alone — the report can never claim
    /// more than its steps prove.
    pub fn of(steps: &[Step], dry_run: bool) -> Verdict {
        if steps.is_empty() {
            return Verdict::Healthy;
        }
        let count = |state: StepState| steps.iter().filter(|s| s.state == state).count();
        let (planned, verified, refused, failed) = (
            count(StepState::Planned),
            count(StepState::Verified),
            count(StepState::Refused),
            count(StepState::Failed),
        );
        let blocked = refused + failed;
        if dry_run {
            return match (planned, blocked) {
                (0, _) => Verdict::Refused,
                (_, 0) => Verdict::WouldRepair,
                _ => Verdict::Partial,
            };
        }
        match (verified, refused, failed, planned) {
            (v, 0, 0, 0) if v > 0 => Verdict::Repaired,
            (0, _, 0, _) => Verdict::Refused,
            (0, _, _, _) => Verdict::Failed,
            _ => Verdict::Partial,
        }
    }
}

/// Which session a repair treats, and how that was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One named session, with the rule that chose it.
    Session { name: String, source: &'static str },
    /// The whole endpoint namespace (sockets with no selector).
    Namespace,
    /// No live session exists; there is nothing to treat.
    NoLiveSession,
}

/// The outcome of resolving `--session` against what is actually live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeResolution {
    Resolved(Scope),
    /// Several candidates and no selector.
    Ambiguous(Vec<String>),
    /// The requested name is not among the candidates.
    NotFound {
        requested: String,
        known: Vec<String>,
    },
}

/// Resolve the scope the way the rest of the CLI does: an explicit
/// `--session` wins, then the global `--session`, then the session this
/// shell lives in, then the only live session. Several live sessions and no
/// selector is never a guess and never "all of them".
pub fn resolve_scope(
    explicit: Option<&str>,
    global: Option<&str>,
    environment: Option<&str>,
    live: &[String],
) -> ScopeResolution {
    let requested = explicit
        .map(|name| (name, "--session"))
        .or_else(|| global.map(|name| (name, "global --session")));
    if let Some((name, source)) = requested {
        return if live.iter().any(|candidate| candidate == name) {
            ScopeResolution::Resolved(Scope::Session {
                name: name.to_owned(),
                source,
            })
        } else {
            ScopeResolution::NotFound {
                requested: name.to_owned(),
                known: live.to_vec(),
            }
        };
    }
    if let Some(name) = environment.filter(|name| live.iter().any(|c| c == name)) {
        return ScopeResolution::Resolved(Scope::Session {
            name: name.to_owned(),
            source: "the session this shell runs in",
        });
    }
    match live {
        [] => ScopeResolution::Resolved(Scope::NoLiveSession),
        [only] => ScopeResolution::Resolved(Scope::Session {
            name: only.clone(),
            source: "the only live session",
        }),
        many => ScopeResolution::Ambiguous(many.to_vec()),
    }
}

/// Everything one repair run decided and did.
#[derive(Debug, Clone)]
pub struct Treatment {
    pub surface: Surface,
    pub scope: Scope,
    pub dry_run: bool,
    pub findings: Vec<Finding>,
    pub steps: Vec<Step>,
    /// Candidates to choose from when the verdict is `NeedsSelection`.
    pub choices: Vec<String>,
    /// Facts worth stating that are neither findings nor steps.
    pub notes: Vec<String>,
}

impl Treatment {
    pub fn new(surface: Surface, scope: Scope, dry_run: bool) -> Self {
        Treatment {
            surface,
            scope,
            dry_run,
            findings: vec![],
            steps: vec![],
            choices: vec![],
            notes: vec![],
        }
    }

    pub fn verdict(&self) -> Verdict {
        if !self.choices.is_empty() {
            return Verdict::NeedsSelection;
        }
        Verdict::of(&self.steps, self.dry_run)
    }

    pub fn render(&self, json: bool, out: &mut impl Write) -> i32 {
        let verdict = self.verdict();
        let text = if json {
            self.render_json(verdict)
        } else {
            self.render_report(verdict)
        };
        let _ = out.write_all(text.as_bytes());
        verdict.exit_code()
    }

    fn scope_line(&self) -> String {
        match &self.scope {
            Scope::Session { name, source } => format!("session {name} ({source})"),
            Scope::Namespace => "every endpoint in the session namespaces".to_owned(),
            Scope::NoLiveSession => "no live session".to_owned(),
        }
    }

    fn render_report(&self, verdict: Verdict) -> String {
        let mut out = format!("── vc-frame repair {} ──\n", self.surface.id());
        out.push_str(&format!("scope: {}\n", self.scope_line()));
        if self.dry_run {
            out.push_str("dry run — nothing will be changed\n");
        }
        for note in &self.notes {
            out.push_str(&format!("  {note}\n"));
        }
        if !self.findings.is_empty() {
            out.push_str("diagnosis:\n");
            for finding in &self.findings {
                out.push_str(&format!(
                    "  {:<9}{}\n",
                    finding.severity.label(),
                    finding.title
                ));
                for line in &finding.detail {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
        if !self.choices.is_empty() {
            out.push_str("several candidates — choose one with --session:\n");
            for choice in &self.choices {
                out.push_str(&format!("  {choice}\n"));
            }
        }
        if !self.steps.is_empty() {
            out.push_str("plan:\n");
            for (index, step) in self.steps.iter().enumerate() {
                out.push_str(&format!(
                    "  {}. [{}] {} — {}\n",
                    index + 1,
                    step.state.id(),
                    step.target,
                    step.action
                ));
                if let Some(capability) = &step.missing_capability {
                    out.push_str(&format!("      missing capability: {capability}\n"));
                }
                for line in &step.evidence {
                    out.push_str(&format!("      {line}\n"));
                }
            }
        }
        out.push_str(&format!(
            "outcome: {} (exit {})\n",
            verdict.id(),
            verdict.exit_code()
        ));
        out
    }

    fn render_json(&self, verdict: Verdict) -> String {
        let scope = match &self.scope {
            Scope::Session { name, source } => {
                serde_json::json!({"kind": "session", "session": name, "source": source})
            },
            Scope::Namespace => serde_json::json!({"kind": "namespace"}),
            Scope::NoLiveSession => serde_json::json!({"kind": "no-live-session"}),
        };
        let findings: Vec<serde_json::Value> = self
            .findings
            .iter()
            .map(|finding| {
                serde_json::json!({
                    "id": finding.section.id(),
                    "severity": finding.severity.id(),
                    "title": finding.title,
                    "detail": finding.detail,
                    "remedy": finding.remedy,
                })
            })
            .collect();
        let steps: Vec<serde_json::Value> = self
            .steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "target": step.target,
                    "action": step.action,
                    "state": step.state.id(),
                    "evidence": step.evidence,
                    "missing_capability": step.missing_capability,
                })
            })
            .collect();
        let document = serde_json::json!({
            "version": 1,
            "surface": self.surface.id(),
            "scope": scope,
            "dry_run": self.dry_run,
            "verdict": verdict.id(),
            "exit_code": verdict.exit_code(),
            "notes": self.notes,
            "choices": self.choices,
            "findings": findings,
            "steps": steps,
        });
        format!("{document:#}\n")
    }
}

/// A finding is a drift only from WARN up; INFO is context.
pub fn is_drift(finding: &Finding) -> bool {
    finding.severity >= Severity::Warn
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(state: StepState) -> Step {
        Step {
            state,
            ..Step::planned("t", "a")
        }
    }

    #[test]
    fn the_verdict_never_claims_more_than_its_steps() {
        use StepState::*;
        assert_eq!(Verdict::of(&[], false), Verdict::Healthy);
        assert_eq!(Verdict::of(&[step(Verified)], false), Verdict::Repaired);
        assert_eq!(
            Verdict::of(&[step(Verified), step(Refused)], false),
            Verdict::Partial
        );
        assert_eq!(Verdict::of(&[step(Refused)], false), Verdict::Refused);
        assert_eq!(
            Verdict::of(&[step(Failed), step(Refused)], false),
            Verdict::Failed
        );
        // A step that was planned but never reached is not a repair.
        assert_eq!(
            Verdict::of(&[step(Verified), step(Planned)], false),
            Verdict::Partial
        );
        assert_eq!(Verdict::of(&[step(Planned)], true), Verdict::WouldRepair);
        assert_eq!(
            Verdict::of(&[step(Planned), step(Refused)], true),
            Verdict::Partial
        );
        assert_eq!(Verdict::of(&[step(Refused)], true), Verdict::Refused);
    }

    #[test]
    fn scope_prefers_the_explicit_name_and_never_picks_among_many() {
        let live = vec!["alpha".to_owned(), "beta".to_owned()];
        assert_eq!(
            resolve_scope(Some("beta"), Some("alpha"), Some("alpha"), &live),
            ScopeResolution::Resolved(Scope::Session {
                name: "beta".into(),
                source: "--session"
            })
        );
        assert_eq!(
            resolve_scope(None, None, Some("alpha"), &live),
            ScopeResolution::Resolved(Scope::Session {
                name: "alpha".into(),
                source: "the session this shell runs in"
            })
        );
        assert_eq!(
            resolve_scope(None, None, Some("gone"), &live),
            ScopeResolution::Ambiguous(live.clone())
        );
        assert_eq!(
            resolve_scope(Some("gamma"), None, None, &live),
            ScopeResolution::NotFound {
                requested: "gamma".into(),
                known: live.clone()
            }
        );
        assert_eq!(
            resolve_scope(None, None, None, &live[..1]),
            ScopeResolution::Resolved(Scope::Session {
                name: "alpha".into(),
                source: "the only live session"
            })
        );
        assert_eq!(
            resolve_scope(None, None, None, &[]),
            ScopeResolution::Resolved(Scope::NoLiveSession)
        );
    }

    #[test]
    fn the_json_document_names_verdict_steps_and_missing_capabilities() {
        let mut treatment = Treatment::new(
            Surface::Servers,
            Scope::Session {
                name: "s".into(),
                source: "--session",
            },
            false,
        );
        treatment.steps.push(
            Step::refused(
                "session s",
                "hand the session to the installed engine",
                "no handover",
            )
            .missing("engine-handover"),
        );
        let mut out = Vec::new();
        let exit = treatment.render(true, &mut out);
        assert_eq!(exit, 2);
        let document: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(document["verdict"], "refused");
        assert_eq!(document["surface"], "servers");
        assert_eq!(document["scope"]["session"], "s");
        assert_eq!(document["steps"][0]["state"], "refused");
        assert_eq!(
            document["steps"][0]["missing_capability"],
            "engine-handover"
        );
    }
}
