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
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by VetCoders (c)2024-2026 LibraxisAI

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
    // Re-resolve provenance when the checked-out commit or the index moves.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    for git_path in [".git/HEAD", ".git/index", ".git/config"] {
        let path = std::path::Path::new(&manifest_dir)
            .join("..")
            .join(git_path);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }

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
