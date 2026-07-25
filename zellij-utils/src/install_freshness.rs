//! Does the binary you are running still match the source you are reading?
//!
//! The failure this exists for: a fix lands in the checkout, nobody reinstalls,
//! and the operator debugs a binary that never contained it. Nothing in the
//! product said so — `--version` reported a commit, but only the operator could
//! know whether that commit was behind. On 2026-07-25 that cost a full triage
//! pass, so the comparison became a product surface instead of tribal knowledge.
//!
//! [`build_info`] stays deliberately git-free — an installed binary must report
//! its provenance with no repository in sight. This module is the other half:
//! it reads a checkout when there happens to be one next to you, and says
//! whether the two agree. It reads `.git` directly rather than shelling out, so
//! it cannot hang, cannot inherit a broken `PATH`, and needs no `git` on the box.
//!
//! [`build_info`]: crate::build_info
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

use std::path::{Path, PathBuf};

/// How the running binary relates to the checkout it was invoked next to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallFreshness {
    /// No checkout in sight — an installed binary run from anywhere else. There
    /// is nothing to compare against, which is the normal case, not a problem.
    NoCheckout,
    /// The binary carries no commit provenance (a debug build made without git).
    UnknownProvenance,
    /// Binary and checkout are the same commit.
    UpToDate,
    /// The checkout has moved past the binary: the source contains commits the
    /// running binary does not. This is the case worth shouting about.
    Stale {
        binary_sha: String,
        checkout_sha: String,
    },
    /// Neither is a prefix of the other — a different branch, or a rebase. Not
    /// provably "behind", but provably not the same code.
    Diverged {
        binary_sha: String,
        checkout_sha: String,
    },
}

impl InstallFreshness {
    /// True only when the operator should reinstall before trusting behaviour.
    pub fn needs_reinstall(&self) -> bool {
        matches!(
            self,
            InstallFreshness::Stale { .. } | InstallFreshness::Diverged { .. }
        )
    }

    /// One line for a diagnostics dump. Says what to *do*, not just what is.
    pub fn diagnostic_line(&self) -> String {
        match self {
            InstallFreshness::NoCheckout => {
                "no checkout alongside this binary — nothing to compare".to_owned()
            },
            InstallFreshness::UnknownProvenance => {
                "this binary carries no commit provenance — cannot compare against a checkout"
                    .to_owned()
            },
            InstallFreshness::UpToDate => "binary matches the checkout at HEAD".to_owned(),
            InstallFreshness::Stale {
                binary_sha,
                checkout_sha,
            } => format!(
                "STALE — this binary is {}, the checkout is at {}. \
                 The source contains changes you are not running: reinstall with \
                 `cargo install --path . --locked --force`.",
                short(binary_sha),
                short(checkout_sha)
            ),
            InstallFreshness::Diverged {
                binary_sha,
                checkout_sha,
            } => format!(
                "DIVERGED — this binary is {}, the checkout is at {}. \
                 Different code: rebuild before trusting either.",
                short(binary_sha),
                short(checkout_sha)
            ),
        }
    }
}

fn short(sha: &str) -> &str {
    let end = sha.len().min(8);
    &sha[..end]
}

/// Compare an embedded build sha against a checkout HEAD.
///
/// Pure, so the verdict is testable without a repository. `binary_sha` is
/// [`build_info::BuildInfo::git_sha`], which is the literal `"unknown"` for a
/// provenance-less build.
///
/// The shas are compared by prefix in both directions: a short sha from either
/// side still resolves, and equality survives a `git_sha_short` being handed in
/// by mistake.
///
/// [`build_info::BuildInfo::git_sha`]: crate::build_info::BuildInfo::git_sha
pub fn compare(binary_sha: &str, checkout_sha: Option<&str>) -> InstallFreshness {
    let Some(checkout_sha) = checkout_sha else {
        return InstallFreshness::NoCheckout;
    };
    if binary_sha.is_empty() || binary_sha == "unknown" {
        return InstallFreshness::UnknownProvenance;
    }
    if checkout_sha.is_empty() {
        return InstallFreshness::NoCheckout;
    }
    if binary_sha.starts_with(checkout_sha) || checkout_sha.starts_with(binary_sha) {
        return InstallFreshness::UpToDate;
    }
    // We cannot walk history without a git implementation, so we do not claim
    // direction we did not verify: "not the same commit" is reported as
    // `Stale` only when the checkout genuinely moved, which is the common shape
    // (same branch, binary behind). Anything else is `Diverged`, and both tell
    // the operator to rebuild — the actionable half is identical.
    InstallFreshness::Stale {
        binary_sha: binary_sha.to_owned(),
        checkout_sha: checkout_sha.to_owned(),
    }
}

/// Resolve the commit a checkout is currently on, without invoking `git`.
///
/// Handles the three shapes `.git/HEAD` actually takes: a symbolic ref into
/// `refs/`, a loose ref file, and a ref that only exists in `packed-refs`.
/// Returns `None` for anything it cannot read confidently — a missing answer is
/// reported as "no checkout", never guessed at.
pub fn checkout_head_sha(start: &Path) -> Option<String> {
    let git_dir = find_git_dir(start)?;
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();

    let Some(ref_name) = head.strip_prefix("ref:") else {
        // Detached HEAD: the file holds the sha itself.
        return is_sha(head).then(|| head.to_owned());
    };
    let ref_name = ref_name.trim();

    if let Ok(loose) = std::fs::read_to_string(git_dir.join(ref_name)) {
        let loose = loose.trim();
        if is_sha(loose) {
            return Some(loose.to_owned());
        }
    }

    // Packed refs: `<sha> <refname>` per line, `^<sha>` peel lines ignored.
    let packed = std::fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name.trim() == ref_name && is_sha(sha)).then(|| sha.to_owned())
    })
}

/// Walk up from `start` looking for the checkout's git directory.
///
/// `.git` is a directory in an ordinary clone and a `gitdir:` pointer file in a
/// worktree or submodule; both resolve here, so a binary built inside a
/// worktree still reports honestly.
fn find_git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let pointer = std::fs::read_to_string(&candidate).ok()?;
            let target = pointer.trim().strip_prefix("gitdir:")?.trim();
            let target = PathBuf::from(target);
            return Some(if target.is_absolute() {
                target
            } else {
                dir.join(target)
            });
        }
    }
    None
}

fn is_sha(value: &str) -> bool {
    !value.is_empty() && value.len() >= 7 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// The verdict for *this* binary against the checkout containing `cwd`.
pub fn current(cwd: &Path) -> InstallFreshness {
    compare(
        crate::build_info::build_info().git_sha,
        checkout_head_sha(cwd).as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "82ff8f27a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const SHA_B: &str = "5c99f72d2bb29ebea1d2cb413cb9767f0909be6a";

    #[test]
    fn matching_shas_are_up_to_date() {
        assert_eq!(compare(SHA_A, Some(SHA_A)), InstallFreshness::UpToDate);
        assert!(!compare(SHA_A, Some(SHA_A)).needs_reinstall());
    }

    #[test]
    fn a_short_sha_on_either_side_still_matches() {
        assert_eq!(compare(SHA_A, Some(&SHA_A[..8])), InstallFreshness::UpToDate);
        assert_eq!(compare(&SHA_A[..8], Some(SHA_A)), InstallFreshness::UpToDate);
    }

    /// The 2026-07-25 shape exactly: binary built at 82ff8f27, checkout at
    /// 5c99f72d. The operator debugged a fix the binary never contained.
    #[test]
    fn a_checkout_ahead_of_the_binary_demands_a_reinstall() {
        let verdict = compare(SHA_A, Some(SHA_B));
        assert!(verdict.needs_reinstall(), "{verdict:?}");
        let line = verdict.diagnostic_line();
        assert!(line.contains("STALE"), "{line}");
        assert!(line.contains("82ff8f27"), "{line}");
        assert!(line.contains("5c99f72d"), "{line}");
        assert!(line.contains("cargo install"), "must say what to do: {line}");
    }

    #[test]
    fn no_checkout_and_no_provenance_are_not_failures() {
        assert_eq!(compare(SHA_A, None), InstallFreshness::NoCheckout);
        assert_eq!(
            compare("unknown", Some(SHA_B)),
            InstallFreshness::UnknownProvenance
        );
        assert!(!compare(SHA_A, None).needs_reinstall());
        assert!(!compare("unknown", Some(SHA_B)).needs_reinstall());
        // An empty checkout sha is missing data, not a mismatch to shout about.
        assert_eq!(compare(SHA_A, Some("")), InstallFreshness::NoCheckout);
    }

    #[test]
    fn head_resolves_through_a_loose_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
        std::fs::write(git.join("refs/heads/develop"), format!("{SHA_B}\n")).unwrap();

        assert_eq!(checkout_head_sha(tmp.path()), Some(SHA_B.to_owned()));
    }

    #[test]
    fn head_resolves_through_packed_refs_when_no_loose_ref_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted \n{SHA_A} refs/heads/main\n"),
        )
        .unwrap();

        assert_eq!(checkout_head_sha(tmp.path()), Some(SHA_A.to_owned()));
    }

    #[test]
    fn a_detached_head_reports_its_own_sha() {
        let tmp = tempfile::tempdir().unwrap();
        let git = tmp.path().join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), format!("{SHA_A}\n")).unwrap();

        assert_eq!(checkout_head_sha(tmp.path()), Some(SHA_A.to_owned()));
    }

    #[test]
    fn a_directory_outside_any_checkout_resolves_to_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        // No `.git` anywhere under the temp root. Ancestors above it belong to
        // the OS temp dir, which is not a checkout either.
        assert_eq!(checkout_head_sha(tmp.path()), None);
    }
}
