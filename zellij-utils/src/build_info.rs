//! The single owner of the vc-frame build identity.
//!
//! Every provenance surface reads from here: clap's `--version`, the
//! `--build-info` JSON, and the `setup --check` diagnostics dump. Version truth
//! is not split across crates — `consts::VERSION` remains the bare semver, and
//! anything that wants the *build* identity comes through `build_info()`.
//!
//! All values are embedded at compile time by `build.rs`. Nothing here shells
//! out to git, reads `.git`, or touches the filesystem: an installed binary
//! reports its provenance with the repository nowhere in sight.
//!
//! 𝚅𝚒𝚋𝚎𝚌𝚛𝚊𝚏𝚝𝚎𝚍. with AI Agents by Vetcoders (c)2024-2026 LibraxisAI

/// Human-facing version string: `<semver>+g<sha8>[.dirty]`.
///
/// A `const` (not a lazy value) so clap can use it directly in its derive
/// attribute — the version surface is resolved at compile time, like the rest.
pub const HUMAN_VERSION: &str = env!("VC_FRAME_HUMAN_VERSION");

/// Immutable build identity, resolved once at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildInfo {
    /// Cargo package semver, e.g. `0.45.4`.
    pub version: &'static str,
    /// Full 40-hex commit sha, or `unknown` for a provenance-less debug build.
    pub git_sha: &'static str,
    /// First 8 characters of `git_sha`.
    pub git_sha_short: &'static str,
    /// Whether tracked files differed from HEAD at build time.
    pub git_dirty: bool,
    /// RFC3339 UTC timestamp of the build.
    pub build_time_utc: &'static str,
    /// Cargo profile the binary was built with, e.g. `release`.
    pub profile: &'static str,
    /// `CARGO_MANIFEST_DIR` of `zellij-utils` when this binary was built.
    pub source_manifest_dir: &'static str,
    /// Build checkout's `origin` URL, or empty when it was unavailable.
    pub source_origin_url: &'static str,
    /// Product repository name used to verify checkout identity.
    pub source_project: &'static str,
}

const EMBEDDED: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    git_sha: env!("VC_FRAME_GIT_SHA"),
    git_sha_short: env!("VC_FRAME_GIT_SHA_SHORT"),
    git_dirty: matches!(env!("VC_FRAME_GIT_DIRTY").as_bytes(), b"1"),
    build_time_utc: env!("VC_FRAME_BUILD_TIME_UTC"),
    profile: env!("VC_FRAME_BUILD_PROFILE"),
    source_manifest_dir: env!("VC_FRAME_SOURCE_MANIFEST_DIR"),
    source_origin_url: env!("VC_FRAME_SOURCE_ORIGIN_URL"),
    source_project: env!("VC_FRAME_SOURCE_PROJECT"),
};

/// The build identity of this binary.
pub fn build_info() -> &'static BuildInfo {
    &EMBEDDED
}

impl BuildInfo {
    /// `<semver>+g<sha8>[.dirty]` — what `vc-frame --version` prints.
    pub fn human_version(&self) -> String {
        let mut out = format!("{}+g{}", self.version, self.git_sha_short);
        if self.git_dirty {
            out.push_str(".dirty");
        }
        out
    }

    /// Machine-readable provenance. Hand-rolled rather than derived so the
    /// contract stays readable and this module needs no serde bound.
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                "{{\n",
                "  \"product\": \"vc-frame\",\n",
                "  \"version\": \"{}\",\n",
                "  \"human_version\": \"{}\",\n",
                "  \"git_sha\": \"{}\",\n",
                "  \"git_sha_short\": \"{}\",\n",
                "  \"git_dirty\": {},\n",
                "  \"build_time_utc\": \"{}\",\n",
                "  \"profile\": \"{}\"\n",
                "}}"
            ),
            self.version,
            self.human_version(),
            self.git_sha,
            self.git_sha_short,
            self.git_dirty,
            self.build_time_utc,
            self.profile,
        )
    }

    /// Single-line provenance for diagnostics dumps.
    pub fn diagnostic_line(&self) -> String {
        format!(
            "{} (sha {}, built {} UTC, profile {})",
            self.human_version(),
            self.git_sha,
            self.build_time_utc,
            self.profile
        )
    }

    #[cfg(test)]
    fn for_test(version: &'static str, git_sha: &'static str, git_dirty: bool) -> Self {
        Self {
            version,
            git_sha,
            git_sha_short: &git_sha[..8],
            git_dirty,
            build_time_utc: "2026-07-20T17:00:00Z",
            profile: "release",
            source_manifest_dir: "/src/vc-frame/zellij-utils",
            source_origin_url: "https://github.com/vetcoders/vc-frame",
            source_project: "vc-frame",
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::build_info::{BuildInfo, build_info};

    #[test]
    fn human_version_is_semver_plus_g_sha8() {
        let info = BuildInfo::for_test("0.45.4", "bcd9e175b5267fb0f0bdcbd12d657072db351999", false);
        assert_eq!(info.human_version(), "0.45.4+gbcd9e175");
    }

    #[test]
    fn human_version_marks_dirty_trees() {
        let info = BuildInfo::for_test("0.45.4", "bcd9e175b5267fb0f0bdcbd12d657072db351999", true);
        assert_eq!(info.human_version(), "0.45.4+gbcd9e175.dirty");
    }

    #[test]
    fn json_carries_full_and_short_sha_dirty_time_and_profile() {
        let json = build_info().to_json();
        for key in [
            "\"product\"",
            "\"version\"",
            "\"git_sha\"",
            "\"git_sha_short\"",
            "\"git_dirty\"",
            "\"build_time_utc\"",
            "\"profile\"",
        ] {
            assert!(json.contains(key), "build-info JSON missing {key}: {json}");
        }
    }

    #[test]
    fn embedded_identity_needs_no_git_at_runtime() {
        let info = build_info();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert!(!info.build_time_utc.is_empty());
        assert!(!info.profile.is_empty());
        assert!(!info.source_manifest_dir.is_empty());
        assert_eq!(info.source_project, "vc-frame");
    }

    #[test]
    fn clap_version_const_agrees_with_the_owner() {
        assert_eq!(super::HUMAN_VERSION, build_info().human_version());
    }

    #[test]
    fn embedded_sha_is_a_real_commit_or_explicitly_unknown() {
        let info = build_info();
        assert!(
            info.git_sha == "unknown"
                || (info.git_sha.len() == 40
                    && info.git_sha.chars().all(|c| c.is_ascii_hexdigit())),
            "git_sha must be a 40-hex commit or the explicit `unknown`: {}",
            info.git_sha
        );
        assert_eq!(
            info.git_sha_short,
            &info.git_sha[..info.git_sha_short.len()]
        );
    }
}
