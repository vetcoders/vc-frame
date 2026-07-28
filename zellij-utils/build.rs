//! Build-time provenance embedding for vc-frame.
//!
//! This is the ONE place that resolves the build identity. It emits compile-time
//! environment variables consumed by `src/build_info.rs`, which is the single
//! owner every other surface (CLI `--version`, `--build-info`, `setup --check`)
//! reads from. Git is touched here, at BUILD time, and never at runtime.
//!
//! Resolution order for the commit identity:
//!   1. `VC_FRAME_GIT_SHA` / `VC_FRAME_GIT_DIRTY` from the environment — the
//!      canonical outer layer (Makefile / release workflow) passes immutable
//!      values on platforms that build outside a git checkout.
//!   2. `git` in `CARGO_MANIFEST_DIR`.
//!   3. Nothing. Debug builds record `unknown`; RELEASE builds fail closed,
//!      because a release binary must never claim an identity it does not have.
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const UNKNOWN_SHA: &str = "unknown";
const SOURCE_PROJECT: &str = "vc-frame";

fn main() {
    for var in [
        "VC_FRAME_GIT_SHA",
        "VC_FRAME_GIT_DIRTY",
        "VC_FRAME_BUILD_TIME_UTC",
        "VC_FRAME_SOURCE_ORIGIN_URL",
    ] {
        println!("cargo:rerun-if-env-changed={var}");
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    emit_git_rerun_paths(&manifest_dir);

    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "unknown".to_string());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();

    let (sha, dirty) = resolve_commit(&manifest_dir);

    if sha == UNKNOWN_SHA && profile == "release" {
        panic!(
            "vc-frame release build has no commit provenance.\n\
             A release binary must carry a verifiable identity, so this build fails closed.\n\
             Building outside a git checkout? Pass immutable values from the outer layer:\n\
             \n    VC_FRAME_GIT_SHA=<40-hex-sha> VC_FRAME_GIT_DIRTY=0 cargo build --release\n"
        );
    }

    let short = short_sha(&sha);
    let human_version = if dirty {
        format!("{version}+g{short}.dirty")
    } else {
        format!("{version}+g{short}")
    };

    let build_time = std::env::var("VC_FRAME_BUILD_TIME_UTC").unwrap_or_else(|_| rfc3339_now());
    let source_origin_url = std::env::var("VC_FRAME_SOURCE_ORIGIN_URL")
        .ok()
        .filter(|url| !url.trim().is_empty())
        .or_else(|| git(&manifest_dir, &["remote", "get-url", "origin"]))
        .unwrap_or_default();

    println!("cargo:rustc-env=VC_FRAME_GIT_SHA={sha}");
    println!("cargo:rustc-env=VC_FRAME_GIT_SHA_SHORT={short}");
    println!(
        "cargo:rustc-env=VC_FRAME_GIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=VC_FRAME_BUILD_TIME_UTC={build_time}");
    println!("cargo:rustc-env=VC_FRAME_BUILD_PROFILE={profile}");
    println!("cargo:rustc-env=VC_FRAME_HUMAN_VERSION={human_version}");
    println!("cargo:rustc-env=VC_FRAME_SOURCE_MANIFEST_DIR={manifest_dir}");
    println!("cargo:rustc-env=VC_FRAME_SOURCE_ORIGIN_URL={source_origin_url}");
    println!("cargo:rustc-env=VC_FRAME_SOURCE_PROJECT={SOURCE_PROJECT}");
}

/// Tell Cargo which pieces of Git metadata can change the embedded identity.
///
/// Watching `.git/HEAD` alone is insufficient for a normal checkout: it usually
/// contains the stable text `ref: refs/heads/<branch>`, while only the resolved
/// ref file moves when a commit lands. A packed ref needs both `packed-refs` and
/// the nearest existing ref directory watched, because `git update-ref` can
/// materialize a previously absent loose ref. Linked worktrees are handled via
/// their `.git` pointer and `commondir`.
fn emit_git_rerun_paths(manifest_dir: &str) {
    for path in git_rerun_paths(Path::new(manifest_dir)) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_rerun_paths(manifest_dir: &Path) -> BTreeSet<PathBuf> {
    let mut paths = BTreeSet::new();
    let Some(repo_root) = manifest_dir.parent() else {
        return paths;
    };
    let git_marker = repo_root.join(".git");
    let Some((git_dir, common_dir)) = resolve_git_dirs(&git_marker) else {
        return paths;
    };

    if git_marker.is_file() {
        paths.insert(git_marker);
    }
    insert_if_exists(&mut paths, git_dir.join("HEAD"));
    insert_if_exists(&mut paths, git_dir.join("index"));
    insert_if_exists(&mut paths, git_dir.join("config.worktree"));
    insert_if_exists(&mut paths, common_dir.join("config"));
    insert_if_exists(&mut paths, common_dir.join("packed-refs"));

    if let Some(reference) = symbolic_head_ref(&git_dir) {
        let storage_dir = if is_per_worktree_ref(&reference) {
            &git_dir
        } else {
            &common_dir
        };
        let loose_ref = storage_dir.join(reference);
        if loose_ref.exists() {
            paths.insert(loose_ref);
        } else if let Some(parent) = nearest_existing_parent(&loose_ref, storage_dir) {
            // Cargo cannot fingerprint a file that does not exist yet. Tracking
            // its closest existing directory catches packed -> loose ref moves.
            paths.insert(parent);
        }
    }

    paths
}

fn resolve_git_dirs(git_marker: &Path) -> Option<(PathBuf, PathBuf)> {
    let git_dir = if git_marker.is_dir() {
        git_marker.to_path_buf()
    } else {
        let pointer = fs::read_to_string(git_marker).ok()?;
        let raw_git_dir = pointer.trim().strip_prefix("gitdir:")?.trim();
        let candidate = Path::new(raw_git_dir);
        let candidate = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            git_marker.parent()?.join(candidate)
        };
        candidate.canonicalize().unwrap_or(candidate)
    };

    let common_dir = fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let candidate = PathBuf::from(raw);
            let candidate = if candidate.is_absolute() {
                candidate
            } else {
                git_dir.join(candidate)
            };
            candidate.canonicalize().unwrap_or(candidate)
        })
        .unwrap_or_else(|| git_dir.clone());

    Some((git_dir, common_dir))
}

fn symbolic_head_ref(git_dir: &Path) -> Option<PathBuf> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let reference = PathBuf::from(head.trim().strip_prefix("ref:")?.trim());
    if reference
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Some(reference)
    } else {
        None
    }
}

fn is_per_worktree_ref(reference: &Path) -> bool {
    reference.starts_with("refs/bisect")
        || reference.starts_with("refs/worktree")
        || reference.starts_with("refs/rewritten")
}

fn insert_if_exists(paths: &mut BTreeSet<PathBuf>, path: PathBuf) {
    if path.exists() {
        paths.insert(path);
    }
}

fn nearest_existing_parent(path: &Path, boundary: &Path) -> Option<PathBuf> {
    let mut candidate = path.parent();
    while let Some(parent) = candidate {
        if !parent.starts_with(boundary) {
            return None;
        }
        if parent.is_dir() {
            return Some(parent.to_path_buf());
        }
        if parent == boundary {
            return None;
        }
        candidate = parent.parent();
    }
    None
}

/// `(sha, dirty)` — environment override first, then git, then unknown.
fn resolve_commit(manifest_dir: &str) -> (String, bool) {
    if let Ok(sha) = std::env::var("VC_FRAME_GIT_SHA") {
        let sha = sha.trim().to_string();
        if !sha.is_empty() {
            let dirty = matches!(
                std::env::var("VC_FRAME_GIT_DIRTY")
                    .unwrap_or_default()
                    .trim(),
                "1" | "true" | "yes"
            );
            return (sha, dirty);
        }
    }

    let Some(sha) = git(manifest_dir, &["rev-parse", "HEAD"]) else {
        return (UNKNOWN_SHA.to_string(), false);
    };
    // `--porcelain` is empty exactly when tracked state matches HEAD.
    let dirty = git(
        manifest_dir,
        &["status", "--porcelain", "--untracked-files=no"],
    )
    .map(|out| !out.is_empty())
    .unwrap_or(false);
    (sha, dirty)
}

fn git(dir: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn short_sha(sha: &str) -> String {
    if sha == UNKNOWN_SHA {
        return UNKNOWN_SHA.to_string();
    }
    sha.chars().take(8).collect()
}

/// RFC3339 UTC without pulling a date crate into the build graph.
fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01 -> (y, m, d).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::process::{Command, Output};
    use std::thread;
    use std::time::Duration;

    struct TempRepo {
        _temp_dir: tempfile::TempDir,
        root: PathBuf,
        crate_dir: PathBuf,
    }

    impl TempRepo {
        fn new(name: &str, pack_refs: bool) -> Self {
            let temp_dir = tempfile::Builder::new()
                .prefix(&format!("vc-frame-build-provenance-{name}-"))
                .tempdir()
                .expect("create isolated provenance fixture");
            let root = temp_dir.path().join("repo");
            let crate_dir = root.join("zellij-utils");
            fs::create_dir_all(crate_dir.join("src")).expect("create fixture crate");
            fs::write(
                crate_dir.join("Cargo.toml"),
                "[package]\nname = \"provenance-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
            )
            .expect("write fixture manifest");
            fs::write(crate_dir.join("build.rs"), include_str!("build.rs"))
                .expect("write fixture build script");
            fs::write(
                crate_dir.join("src/main.rs"),
                "fn main() { println!(\"{}\", env!(\"VC_FRAME_GIT_SHA\")); }\n",
            )
            .expect("write fixture binary");

            run(&root, "git", ["init", "--quiet"]);
            run(
                &root,
                "git",
                ["config", "user.email", "provenance-test@localhost"],
            );
            run(&root, "git", ["config", "user.name", "Provenance Test"]);
            run(&root, "git", ["add", "zellij-utils"]);
            run(&root, "git", ["commit", "--quiet", "-m", "initial"]);
            if pack_refs {
                run(&root, "git", ["pack-refs", "--all", "--prune"]);
            }

            Self {
                _temp_dir: temp_dir,
                root,
                crate_dir,
            }
        }

        fn head(&self) -> String {
            run(&self.root, "git", ["rev-parse", "HEAD"])
        }

        fn symbolic_ref(&self) -> String {
            run(&self.root, "git", ["symbolic-ref", "HEAD"])
        }

        fn run_binary(&self) -> String {
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let mut command = Command::new(cargo);
            command
                .current_dir(&self.crate_dir)
                .args(["run", "--quiet"])
                .env("CARGO_TERM_COLOR", "never");
            for name in [
                "VC_FRAME_GIT_SHA",
                "VC_FRAME_GIT_DIRTY",
                "VC_FRAME_BUILD_TIME_UTC",
                "VC_FRAME_SOURCE_ORIGIN_URL",
            ] {
                command.env_remove(name);
            }
            successful_output(command).trim().to_string()
        }

        fn advance_head_without_touching_sources(&self) -> String {
            let old_head = self.head();
            let tree = run(&self.root, "git", ["rev-parse", "HEAD^{tree}"]);
            let new_head = run(
                &self.root,
                "git",
                [
                    "commit-tree",
                    tree.as_str(),
                    "-p",
                    old_head.as_str(),
                    "-m",
                    "advance provenance only",
                ],
            );
            run(
                &self.root,
                "git",
                [
                    "update-ref",
                    self.symbolic_ref().as_str(),
                    new_head.as_str(),
                    old_head.as_str(),
                ],
            );
            new_head
        }

        fn tracked_source_snapshot(&self) -> Vec<Vec<u8>> {
            [
                self.crate_dir.join("Cargo.toml"),
                self.crate_dir.join("build.rs"),
                self.crate_dir.join("src/main.rs"),
                self.root.join(".git/index"),
                self.root.join(".git/HEAD"),
            ]
            .iter()
            .map(|path| fs::read(path).expect("read fixture source snapshot"))
            .collect()
        }
    }

    fn run<I, S>(dir: &Path, program: &str, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(program);
        command.current_dir(dir).args(args);
        successful_output(command).trim().to_string()
    }

    fn successful_output(mut command: Command) -> String {
        let rendered = format!("{command:?}");
        let Output {
            status,
            stdout,
            stderr,
        } = command.output().expect("spawn fixture command");
        assert!(
            status.success(),
            "command failed: {rendered}\nstatus: {status}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        String::from_utf8(stdout).expect("fixture command stdout must be UTF-8")
    }

    fn assert_head_advance_is_rebuilt(pack_refs: bool) {
        let repo = TempRepo::new(
            if pack_refs { "packed-ref" } else { "loose-ref" },
            pack_refs,
        );
        let reference = repo.symbolic_ref();
        let loose_ref = repo.root.join(".git").join(&reference);
        assert_eq!(
            loose_ref.exists(),
            !pack_refs,
            "fixture must start with the requested ref representation"
        );

        let first_head = repo.head();
        assert_eq!(repo.run_binary(), first_head);
        let before = repo.tracked_source_snapshot();

        // Keep this test valid on filesystems whose mtimes have one-second
        // resolution: only the Git ref may move after Cargo's first snapshot.
        thread::sleep(Duration::from_millis(1_100));
        let second_head = repo.advance_head_without_touching_sources();

        assert_ne!(second_head, first_head);
        assert_eq!(repo.tracked_source_snapshot(), before);
        assert_eq!(
            run(
                &repo.root,
                "git",
                ["status", "--porcelain", "--untracked-files=no"]
            ),
            ""
        );
        assert_eq!(
            repo.run_binary(),
            second_head,
            "Cargo reused stale build-script provenance after HEAD advanced"
        );
    }

    #[test]
    fn rebuilds_when_loose_symbolic_head_ref_advances_without_source_touch() {
        assert_head_advance_is_rebuilt(false);
    }

    #[test]
    fn rebuilds_when_packed_symbolic_head_ref_becomes_loose_without_source_touch() {
        assert_head_advance_is_rebuilt(true);
    }
}
