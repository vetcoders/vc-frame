//! `repair sockets` — classify endpoints; unlink only a proven-dead orphan.
//!
//! The only mutation in this module is `remove_orphaned_socket`, which
//! re-proves the diagnosed `(dev, ino)`, the free lease and the refused
//! connection under the same lifetime lease a starting server must take.
//! Live, foreign, rebinding, ambiguous and owner-mismatched endpoints are
//! described and left exactly as they are.

use zellij_utils::sessions::{OrphanRemoval, probe_endpoint, remove_orphaned_socket};

use super::inventory::{Endpoint, EndpointClass, PROBE_TIMEOUT};
use super::treatment::{Scope, Step, Surface, Treatment};
use super::{Finding, Section, Severity};

/// Diagnose every endpoint as a doctor finding (read-only).
pub fn findings(endpoints: &[Endpoint]) -> (Vec<Finding>, Option<String>) {
    let mut findings = vec![];
    let mut live = 0usize;
    for endpoint in endpoints {
        let finding = match &endpoint.class {
            EndpointClass::Live => {
                live += 1;
                continue;
            },
            EndpointClass::Orphan => Finding::new(
                Section::SocketEndpoints,
                Severity::Warn,
                format!("{} is an orphaned endpoint", endpoint.label()),
                format!("vc-frame repair sockets --session {}", endpoint.name),
            ),
            EndpointClass::Rebinding | EndpointClass::Ambiguous(_) => Finding::new(
                Section::SocketEndpoints,
                Severity::Info,
                format!(
                    "{} is {} — protected",
                    endpoint.label(),
                    endpoint.class.id()
                ),
                "none — the clinic never treats an endpoint it cannot classify",
            ),
            EndpointClass::ForeignOwner(_) | EndpointClass::OwnerMismatch(_) => Finding::new(
                Section::SocketEndpoints,
                Severity::Warn,
                format!(
                    "{} has an unexpected owner ({})",
                    endpoint.label(),
                    endpoint.class.id()
                ),
                "inspect the listener by hand — the clinic never touches it",
            ),
            EndpointClass::Foreign(_) => continue,
        };
        findings.push(finding.with_detail(vec![endpoint.class.reason(), identity_line(endpoint)]));
    }
    let note = (findings.is_empty()).then(|| format!("{live} live endpoint(s), no orphan"));
    (findings, note)
}

fn identity_line(endpoint: &Endpoint) -> String {
    let (dev, ino) = endpoint.probe.identity;
    let peer = endpoint
        .peer
        .as_ref()
        .map(|peer| format!(" · listener pid {}", peer.pid))
        .unwrap_or_default();
    format!(
        "dev {dev} ino {ino} · lease {:?}{peer}",
        endpoint.probe.lease
    )
}

/// Plan and (unless dry run) apply the treatment.
pub fn treat(
    selector: Option<(&str, &'static str)>,
    endpoints: Vec<Endpoint>,
    dry_run: bool,
) -> Treatment {
    let (scope, selected): (Scope, Vec<Endpoint>) = match selector {
        Some((name, source)) => {
            let selected: Vec<Endpoint> = endpoints
                .iter()
                .filter(|endpoint| endpoint.name == name)
                .cloned()
                .collect();
            let scope = Scope::Session {
                name: name.to_owned(),
                source,
            };
            if selected.is_empty() {
                // Idempotence: once an orphan is gone, the same command is a
                // clean no-op, not an error.
                let mut treatment = Treatment::new(Surface::Sockets, scope, dry_run);
                treatment.notes.push(format!(
                    "no endpoint named {name} in any session namespace — nothing to treat"
                ));
                return treatment;
            }
            (scope, selected)
        },
        None => {
            let orphans: Vec<Endpoint> = endpoints
                .iter()
                .filter(|endpoint| endpoint.class == EndpointClass::Orphan)
                .cloned()
                .collect();
            match orphans.as_slice() {
                [] => (Scope::Namespace, vec![]),
                [only] => (
                    Scope::Session {
                        name: only.name.clone(),
                        source: "the only orphaned endpoint",
                    },
                    orphans.clone(),
                ),
                many => {
                    let mut treatment = Treatment::new(Surface::Sockets, Scope::Namespace, dry_run);
                    treatment.choices = many.iter().map(Endpoint::label).collect();
                    treatment.findings = findings(&endpoints).0;
                    return treatment;
                },
            }
        },
    };
    let mut treatment = Treatment::new(Surface::Sockets, scope, dry_run);
    if selected.is_empty() {
        let (findings, note) = findings(&endpoints);
        treatment.findings = findings;
        treatment.notes.extend(note);
        return treatment;
    }
    treatment.findings = findings(&selected).0;
    for endpoint in &selected {
        match &endpoint.class {
            EndpointClass::Orphan => {
                let (dev, ino) = endpoint.probe.identity;
                let mut step = Step::planned(
                    endpoint.label(),
                    format!("unlink the orphaned endpoint (dev {dev}, ino {ino})"),
                );
                if !dry_run {
                    apply(endpoint, &mut step);
                }
                treatment.steps.push(step);
            },
            EndpointClass::Live => treatment
                .notes
                .push(format!("{} is live — nothing to treat", endpoint.label())),
            other => treatment.steps.push(Step::refused(
                endpoint.label(),
                "treat the endpoint",
                format!("protected ({}): {}", other.id(), other.reason()),
            )),
        }
    }
    treatment
}

fn apply(endpoint: &Endpoint, step: &mut Step) {
    let path = &endpoint.probe.path;
    match remove_orphaned_socket(path, endpoint.probe.identity) {
        Ok(OrphanRemoval::Removed) => verify_gone(endpoint, step, "unlinked"),
        Ok(OrphanRemoval::Vanished) => verify_gone(endpoint, step, "already gone at apply time"),
        Ok(OrphanRemoval::Replaced) => {
            step.refuse("the name was rebound since diagnosis — the new endpoint is left alone");
        },
        Ok(OrphanRemoval::LeaseHeld) => {
            step.refuse("a server took the name's lease since diagnosis — left alone");
        },
        Ok(OrphanRemoval::Live) => {
            step.refuse("something accepts on the endpoint again — left alone");
        },
        Ok(OrphanRemoval::Undetermined(why)) => {
            step.refuse(format!("ownership undetermined at apply time: {why}"));
        },
        Err(error) => step.failed(format!("unlink failed: {error}")),
    }
}

/// The step verifies only when the diagnosed inode is observably gone.
fn verify_gone(endpoint: &Endpoint, step: &mut Step, how: &str) {
    match probe_endpoint(&endpoint.probe.path, PROBE_TIMEOUT) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            step.verified(format!("{how}; the pathname no longer exists"));
        },
        Ok(probe) if probe.identity != endpoint.probe.identity => {
            step.verified(format!(
                "{how}; the name is now held by a different endpoint (dev {} ino {}), left alone",
                probe.identity.0, probe.identity.1
            ));
        },
        Ok(_) => step.failed(format!(
            "{how}, yet the same inode is still at the pathname"
        )),
        Err(error) => step.failed(format!("{how}, but re-probing failed: {error}")),
    }
}
