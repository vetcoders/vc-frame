//! Vibecrafted layout installer.
//!
//! Manages symlinks under `~/.config/zellij/layouts/` so that the canonical
//! Vibecrafted layouts shipped in `<vibecrafted-root>/config/zellij/layouts/*.kdl`
//! are picked up by stock `zellij --layout <name>` invocations.
//!
//! The installer is intentionally narrow:
//!
//! 1. It resolves the framework root **dynamically** (CLI override → env →
//!    `which vibecrafted` walk-up) and refuses to install against a path that
//!    does not actually contain a populated `config/zellij/layouts/` tree.
//! 2. The source of truth for *which* layouts ship is the live filesystem
//!    listing of `<root>/config/zellij/layouts/*.kdl` — there is no hardcoded
//!    list in this module. Add a new `.kdl` to the repo, re-run the installer,
//!    the symlink appears.
//! 3. Legacy compatibility redirects come from an optional data file at
//!    `<root>/config/zellij/layouts/aliases.txt` (`old=new`, one per line).
//!    No rebuild required to add a rename redirect.
//! 4. Every run cleans up symlinks under `~/.config/zellij/layouts/` whose
//!    target either no longer exists or has drifted to a stale copy of the
//!    vibecrafted tree. Hand-written files and symlinks pointing at unrelated
//!    frameworks are preserved.
//! 5. Idempotent: re-running produces identical filesystem state and identical
//!    summary output.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use thiserror::Error;

use crate::home::home_config_dir;

const ALIAS_FILENAME: &str = "aliases.txt";

#[derive(Debug, Default)]
pub struct InstallSummary {
    pub vibecrafted_root: PathBuf,
    pub layouts_dir: PathBuf,
    pub target_dir: PathBuf,
    /// Newly-created symlinks (target did not previously exist).
    pub created: Vec<String>,
    /// Symlinks whose target pointer was rewritten to match the current
    /// vibecrafted root.
    pub updated: Vec<String>,
    /// Symlinks already pointing at the correct target; left untouched.
    pub already_correct: Vec<String>,
    /// Broken or stale vibecrafted-side symlinks that were removed prior to
    /// install. Symlinks pointing outside the vibecrafted tree are not
    /// included here — they are preserved.
    pub stale_removed: Vec<String>,
    /// `(old_name, new_name)` aliases written as compatibility redirects.
    pub aliases_installed: Vec<(String, String)>,
    /// Alias entries from the data file whose `new_name` did not exist in the
    /// current vibecrafted tree. Recorded so the operator can see what got
    /// dropped instead of silently re-creating broken symlinks.
    pub aliases_dropped: Vec<(String, String)>,
    /// Non-symlink files in the target directory (hand-written layouts) left
    /// alone. Recorded for the summary only.
    pub preserved_files: Vec<String>,
}

impl InstallSummary {
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Vibecrafted layout install");
        let _ = writeln!(
            out,
            "  vibecrafted root: {}",
            self.vibecrafted_root.display()
        );
        let _ = writeln!(out, "  source layouts:   {}", self.layouts_dir.display());
        let _ = writeln!(out, "  target dir:       {}", self.target_dir.display());
        let _ = writeln!(out);
        let _ = writeln!(out, "  created:          {}", fmt_list(&self.created));
        let _ = writeln!(out, "  re-pointed:       {}", fmt_list(&self.updated));
        let _ = writeln!(
            out,
            "  already correct:  {}",
            fmt_list(&self.already_correct)
        );
        let _ = writeln!(out, "  stale removed:    {}", fmt_list(&self.stale_removed));
        let _ = writeln!(
            out,
            "  aliases:          {}",
            fmt_alias_list(&self.aliases_installed)
        );
        let _ = writeln!(
            out,
            "  aliases dropped:  {}",
            fmt_alias_list(&self.aliases_dropped)
        );
        let _ = writeln!(
            out,
            "  preserved files:  {}",
            fmt_list(&self.preserved_files)
        );
        out
    }
}

fn fmt_list(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items.join(", ")
    }
}

fn fmt_alias_list(items: &[(String, String)]) -> String {
    if items.is_empty() {
        "(none)".to_string()
    } else {
        items
            .iter()
            .map(|(a, b)| format!("{a}->{b}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Error)]
pub enum InstallError {
    #[error(
        "could not resolve vibecrafted root — set VIBECRAFTED_HOME, pass --vibecrafted-root PATH, \
         or ensure `vibecrafted` is on PATH"
    )]
    RootNotFound,
    #[error("vibecrafted root '{0}' does not exist")]
    RootMissing(PathBuf),
    #[error("vibecrafted root '{0}' has no config/zellij/layouts/ directory")]
    LayoutsDirMissing(PathBuf),
    #[error("layouts directory '{0}' contains no .kdl files")]
    NoLayouts(PathBuf),
    #[error("could not determine the user's zellij config directory")]
    NoTargetDir,
    #[error("invalid alias file '{path}' at line {line}: {msg}")]
    AliasFile {
        path: PathBuf,
        line: usize,
        msg: String,
    },
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

/// Resolve the framework root (the directory containing `config/zellij/layouts/`).
///
/// Resolution order:
///   1. `root_override` (CLI flag) if present
///   2. `$VIBECRAFTED_HOME` env var (with `tools/vibecrafted-current` fallback
///      for the user-home convention where `VIBECRAFTED_HOME=$HOME/.vibecrafted`)
///   3. `which vibecrafted` → canonicalize → walk up looking for a framework
///      source root marker (`config/zellij/layouts/` + `skills/`)
pub fn resolve_vibecrafted_root(root_override: Option<PathBuf>) -> Result<PathBuf, InstallError> {
    if let Some(path) = root_override {
        return validate_root(path);
    }

    if let Ok(env_val) = env::var("VIBECRAFTED_HOME") {
        let candidate = PathBuf::from(&env_val);
        if looks_like_framework_root(&candidate) {
            return Ok(canonicalize_or(candidate));
        }
        let installed = candidate.join("tools").join("vibecrafted-current");
        if looks_like_framework_root(&installed) {
            return Ok(canonicalize_or(installed));
        }
        return Err(InstallError::LayoutsDirMissing(candidate));
    }

    if let Some(from_which) = which_vibecrafted_walk_up() {
        return Ok(from_which);
    }

    Err(InstallError::RootNotFound)
}

fn validate_root(path: PathBuf) -> Result<PathBuf, InstallError> {
    if !path.exists() {
        return Err(InstallError::RootMissing(path));
    }
    if !path.join("config").join("zellij").join("layouts").is_dir() {
        return Err(InstallError::LayoutsDirMissing(path));
    }
    Ok(canonicalize_or(path))
}

fn canonicalize_or(path: PathBuf) -> PathBuf {
    dunce::canonicalize(&path).unwrap_or(path)
}

fn looks_like_framework_root(path: &Path) -> bool {
    path.join("config").join("zellij").join("layouts").is_dir()
}

fn which_vibecrafted_walk_up() -> Option<PathBuf> {
    let output = Command::new("which").arg("vibecrafted").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let bin_path = PathBuf::from(raw.trim());
    if bin_path.as_os_str().is_empty() {
        return None;
    }
    let resolved = dunce::canonicalize(&bin_path).ok()?;
    let mut cursor = resolved.parent();
    while let Some(dir) = cursor {
        if looks_like_framework_root(dir) {
            return Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    None
}

fn read_layout_files(layouts_dir: &Path) -> Result<Vec<PathBuf>, InstallError> {
    let mut out = Vec::new();
    for entry in fs::read_dir(layouts_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("kdl") && path.is_file() {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn read_alias_map(path: &Path) -> Result<BTreeMap<String, String>, InstallError> {
    let mut map = BTreeMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let text = fs::read_to_string(path)?;
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (old, new) = line
            .split_once('=')
            .ok_or_else(|| InstallError::AliasFile {
                path: path.to_path_buf(),
                line: idx + 1,
                msg: format!("expected `old=new`, got `{line}`"),
            })?;
        let old = old.trim();
        let new = new.trim();
        if old.is_empty() || new.is_empty() {
            return Err(InstallError::AliasFile {
                path: path.to_path_buf(),
                line: idx + 1,
                msg: "alias old/new name must be non-empty".into(),
            });
        }
        map.insert(old.to_string(), new.to_string());
    }
    Ok(map)
}

fn resolve_user_layouts_dir() -> Result<PathBuf, InstallError> {
    let config = home_config_dir().ok_or(InstallError::NoTargetDir)?;
    Ok(config.join("layouts"))
}

/// Install / refresh vibecrafted layouts under the user's zellij config dir.
///
/// `root_override` is the value of the `--vibecrafted-root` CLI flag, if any.
/// `target_override` lets tests redirect the install dir; production callers
/// pass `None` and the standard `~/.config/zellij/layouts/` is used.
pub fn install(
    root_override: Option<PathBuf>,
    target_override: Option<PathBuf>,
) -> Result<InstallSummary, InstallError> {
    let root = resolve_vibecrafted_root(root_override)?;
    let layouts_dir = root.join("config").join("zellij").join("layouts");
    let layouts = read_layout_files(&layouts_dir)?;
    if layouts.is_empty() {
        return Err(InstallError::NoLayouts(layouts_dir));
    }
    let aliases = read_alias_map(&layouts_dir.join(ALIAS_FILENAME))?;
    let target_dir = match target_override {
        Some(t) => t,
        None => resolve_user_layouts_dir()?,
    };
    fs::create_dir_all(&target_dir)?;

    let mut summary = InstallSummary {
        vibecrafted_root: root.clone(),
        layouts_dir: layouts_dir.clone(),
        target_dir: target_dir.clone(),
        ..Default::default()
    };

    let layout_names: BTreeSet<String> = layouts
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();

    let canonical_layouts_dir = dunce::canonicalize(&layouts_dir).unwrap_or(layouts_dir.clone());

    clean_stale_symlinks(&target_dir, &canonical_layouts_dir, &mut summary)?;

    for layout in &layouts {
        let name = layout.file_name().unwrap().to_string_lossy().into_owned();
        let link = target_dir.join(&name);
        link_or_relink(layout, &link, &name, false, &mut summary)?;
    }

    for (old_name, new_name) in &aliases {
        if old_name == new_name {
            // alias pointing at itself is a no-op; the layout symlink already
            // covers it.
            continue;
        }
        let link = target_dir.join(old_name);
        if !layout_names.contains(new_name) {
            summary
                .aliases_dropped
                .push((old_name.clone(), new_name.clone()));
            if link.is_symlink() {
                let current = fs::read_link(&link).ok();
                if let Some(t) = current {
                    let inside = match dunce::canonicalize(&t) {
                        Ok(c) => c.starts_with(&canonical_layouts_dir),
                        Err(_) => t.starts_with(&canonical_layouts_dir) || !t.exists(),
                    };
                    if inside {
                        fs::remove_file(&link)?;
                    }
                }
            }
            continue;
        }
        let target = layouts_dir.join(new_name);
        link_or_relink(&target, &link, old_name, true, &mut summary)?;
        if !summary.aliases_installed.iter().any(|(o, _)| o == old_name) {
            summary
                .aliases_installed
                .push((old_name.clone(), new_name.clone()));
        }
    }

    record_preserved(&target_dir, &mut summary)?;

    Ok(summary)
}

fn link_or_relink(
    source: &Path,
    link: &Path,
    display_name: &str,
    is_alias: bool,
    summary: &mut InstallSummary,
) -> Result<(), InstallError> {
    let metadata = fs::symlink_metadata(link).ok();
    match metadata {
        Some(meta) if meta.file_type().is_symlink() => {
            let current = fs::read_link(link).unwrap_or_default();
            let current_canon = dunce::canonicalize(&current).unwrap_or_else(|_| current.clone());
            let source_canon = dunce::canonicalize(source).unwrap_or_else(|_| source.to_path_buf());
            if current_canon == source_canon {
                if !is_alias {
                    summary.already_correct.push(display_name.to_string());
                }
                return Ok(());
            }
            fs::remove_file(link)?;
            symlink_to(source, link)?;
            if !is_alias {
                summary.updated.push(display_name.to_string());
            }
        },
        Some(meta) if meta.file_type().is_file() || meta.file_type().is_dir() => {
            // Real file or dir — preserve. Skip install for this name.
            // record_preserved() will tally it.
        },
        _ => {
            symlink_to(source, link)?;
            if !is_alias {
                summary.created.push(display_name.to_string());
            }
        },
    }
    Ok(())
}

#[cfg(unix)]
fn symlink_to(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn symlink_to(source: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(source, link)
}

fn clean_stale_symlinks(
    target_dir: &Path,
    canonical_layouts_dir: &Path,
    summary: &mut InstallSummary,
) -> Result<(), InstallError> {
    for entry in fs::read_dir(target_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let target = match fs::read_link(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };

        let absolute = if target.is_absolute() {
            target.clone()
        } else {
            target_dir.join(&target)
        };

        // A symlink is "ours" (and therefore subject to cleanup) if either:
        //   - it resolves into the current canonical vibecrafted layouts dir, OR
        //   - it is broken AND its raw target path string contains the segment
        //     `/config/zellij/layouts/` (heuristic: it was once one of ours,
        //     pointing at a stale repo location).
        let resolves_inside = dunce::canonicalize(&absolute)
            .map(|c| c.starts_with(canonical_layouts_dir))
            .unwrap_or(false);

        let broken = !absolute.exists();
        let target_str = target.to_string_lossy().replace('\\', "/");
        let path_looks_vibecrafted = target_str.contains("/config/zellij/layouts/");

        let belongs_to_us = resolves_inside || (broken && path_looks_vibecrafted);
        if !belongs_to_us {
            continue;
        }

        if broken {
            let name = path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            fs::remove_file(&path)?;
            summary.stale_removed.push(name);
        }
        // If resolves_inside && alive, we leave it — link_or_relink() will
        // either confirm it as already_correct or repoint it as updated.
    }
    Ok(())
}

fn record_preserved(target_dir: &Path, summary: &mut InstallSummary) -> Result<(), InstallError> {
    for entry in fs::read_dir(target_dir)? {
        let entry = entry?;
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.file_type().is_file()
            && let Some(name) = path.file_name() {
                summary
                    .preserved_files
                    .push(name.to_string_lossy().into_owned());
            }
    }
    summary.preserved_files.sort();
    summary.preserved_files.dedup();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    /// Build a fake vibecrafted root containing the given layout filenames and
    /// an optional aliases.txt body. Returns the root path.
    fn make_root(dir: &Path, layouts: &[&str], aliases: Option<&str>) -> PathBuf {
        let layouts_dir = dir.join("config").join("zellij").join("layouts");
        fs::create_dir_all(&layouts_dir).unwrap();
        fs::create_dir_all(dir.join("skills")).unwrap();
        for name in layouts {
            let mut f = File::create(layouts_dir.join(name)).unwrap();
            writeln!(f, "// layout {name}").unwrap();
        }
        if let Some(body) = aliases {
            fs::write(layouts_dir.join(ALIAS_FILENAME), body).unwrap();
        }
        dir.to_path_buf()
    }

    #[test]
    fn fresh_install_creates_symlinks_from_filesystem_listing() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let root = make_root(
            root_dir.path(),
            &["dashboard.kdl", "marbles.kdl", "operator.kdl"],
            None,
        );

        let summary = install(Some(root), Some(target_dir.path().to_path_buf())).unwrap();

        for name in ["dashboard.kdl", "marbles.kdl", "operator.kdl"] {
            let link = target_dir.path().join(name);
            assert!(link.is_symlink(), "{name} should be a symlink");
            assert!(link.exists(), "{name} target should resolve");
        }
        assert_eq!(summary.created.len(), 3);
        assert_eq!(summary.updated.len(), 0);
        assert!(summary.stale_removed.is_empty());
    }

    #[test]
    fn newly_added_layout_picked_up_without_code_change() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        make_root(root_dir.path(), &["dashboard.kdl"], None);

        let s1 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert_eq!(s1.created.len(), 1);

        // Add a brand-new layout to the repo with no code change anywhere.
        fs::write(
            root_dir
                .path()
                .join("config")
                .join("zellij")
                .join("layouts")
                .join("brand-new.kdl"),
            "// new layout\n",
        )
        .unwrap();

        let s2 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(target_dir.path().join("brand-new.kdl").is_symlink());
        assert_eq!(s2.created, vec!["brand-new.kdl".to_string()]);
        assert!(s2.already_correct.contains(&"dashboard.kdl".to_string()));
    }

    #[test]
    fn repo_move_repoints_all_symlinks() {
        let root_a = tempdir().unwrap();
        let root_b = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        make_root(root_a.path(), &["dashboard.kdl", "workflow.kdl"], None);
        make_root(root_b.path(), &["dashboard.kdl", "workflow.kdl"], None);

        let _ = install(
            Some(root_a.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        let s2 = install(
            Some(root_b.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        let resolved = fs::read_link(target_dir.path().join("dashboard.kdl")).unwrap();
        let resolved_canon = dunce::canonicalize(&resolved).unwrap_or(resolved);
        let root_b_canon = dunce::canonicalize(root_b.path()).unwrap();
        assert!(
            resolved_canon.starts_with(&root_b_canon),
            "{resolved_canon:?}"
        );
        assert_eq!(s2.updated.len(), 2);
        assert!(s2.created.is_empty());
    }

    #[test]
    fn stale_symlinks_are_pruned() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let nonexistent = root_dir
            .path()
            .join("config")
            .join("zellij")
            .join("layouts");
        // Pre-seed a broken symlink whose raw path looks like one of ours
        // ("/config/zellij/layouts/" appears in the target).
        fs::create_dir_all(target_dir.path()).unwrap();
        symlink_to(
            &nonexistent.join("retired.kdl"),
            &target_dir.path().join("retired.kdl"),
        )
        .unwrap();
        // ALSO add a real .kdl so the root validates.
        make_root(root_dir.path(), &["dashboard.kdl"], None);

        let summary = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        assert!(!target_dir.path().join("retired.kdl").exists());
        assert!(!target_dir.path().join("retired.kdl").is_symlink());
        assert!(summary.stale_removed.contains(&"retired.kdl".to_string()));
    }

    #[test]
    fn aliases_install_when_target_exists_and_drop_when_not() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let alias_body = "\
# legacy compatibility map
vc-dashboard.kdl=dashboard.kdl
implement-dual.kdl=workflow.kdl
vibecraft.kdl=operator.kdl
# operator.kdl is missing on purpose so vibecraft.kdl alias should be dropped
";
        make_root(
            root_dir.path(),
            &["dashboard.kdl", "workflow.kdl"],
            Some(alias_body),
        );

        let summary = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        assert!(target_dir.path().join("vc-dashboard.kdl").is_symlink());
        assert!(target_dir.path().join("implement-dual.kdl").is_symlink());
        assert!(!target_dir.path().join("vibecraft.kdl").exists());
        assert!(summary
            .aliases_installed
            .iter()
            .any(|(o, n)| o == "vc-dashboard.kdl" && n == "dashboard.kdl"));
        assert!(summary
            .aliases_dropped
            .iter()
            .any(|(o, n)| o == "vibecraft.kdl" && n == "operator.kdl"));
    }

    #[test]
    fn alias_file_is_data_driven_no_rebuild() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        make_root(root_dir.path(), &["operator.kdl"], None);

        let s1 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(s1.aliases_installed.is_empty());

        // Add an alias entry on disk — no Rust rebuild.
        fs::write(
            root_dir
                .path()
                .join("config")
                .join("zellij")
                .join("layouts")
                .join(ALIAS_FILENAME),
            "vibecrafted.kdl=operator.kdl\n",
        )
        .unwrap();

        let s2 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(target_dir.path().join("vibecrafted.kdl").is_symlink());
        assert_eq!(s2.aliases_installed.len(), 1);
    }

    #[test]
    fn refuses_empty_or_missing_layouts_dir() {
        let target_dir = tempdir().unwrap();
        // Empty dir: no config/zellij/layouts inside.
        let empty = tempdir().unwrap();
        let err = install(
            Some(empty.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::LayoutsDirMissing(_)));

        // Dir present but no .kdl files.
        let no_kdl = tempdir().unwrap();
        fs::create_dir_all(no_kdl.path().join("config").join("zellij").join("layouts")).unwrap();
        let err = install(
            Some(no_kdl.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap_err();
        assert!(matches!(err, InstallError::NoLayouts(_)));
    }

    #[test]
    fn handwritten_layouts_preserved_across_runs() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        make_root(root_dir.path(), &["dashboard.kdl"], None);

        // Real (not-symlink) hand-written layout.
        fs::write(target_dir.path().join("my-custom.kdl"), "// mine\n").unwrap();

        let _ = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(target_dir.path().join("my-custom.kdl").is_file());
        assert!(!target_dir.path().join("my-custom.kdl").is_symlink());

        // Idempotent second run keeps it.
        let summary = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(target_dir.path().join("my-custom.kdl").is_file());
        assert!(summary
            .preserved_files
            .contains(&"my-custom.kdl".to_string()));
    }

    #[test]
    fn symlinks_to_other_frameworks_are_preserved() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let other_fw = tempdir().unwrap();
        let other_layout = other_fw.path().join("their-layout.kdl");
        fs::write(&other_layout, "// not mine\n").unwrap();

        make_root(root_dir.path(), &["dashboard.kdl"], None);
        symlink_to(&other_layout, &target_dir.path().join("their-layout.kdl")).unwrap();

        let _ = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        let preserved = target_dir.path().join("their-layout.kdl");
        assert!(preserved.is_symlink());
        assert_eq!(fs::read_link(&preserved).unwrap(), other_layout);
    }

    #[test]
    fn second_run_is_idempotent() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        make_root(
            root_dir.path(),
            &["dashboard.kdl", "marbles.kdl"],
            Some("vc-dashboard.kdl=dashboard.kdl\n"),
        );

        let s1 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        let s2 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        // s2 should mostly be no-ops.
        assert_eq!(s2.created.len(), 0);
        assert_eq!(s2.updated.len(), 0);
        assert_eq!(s2.stale_removed.len(), 0);
        // The base layouts are reported as already_correct.
        assert_eq!(s2.already_correct.len(), s1.created.len());
    }

    #[test]
    fn renamed_layout_old_disappears_new_appears() {
        let root_dir = tempdir().unwrap();
        let target_dir = tempdir().unwrap();
        let layouts_dir = root_dir
            .path()
            .join("config")
            .join("zellij")
            .join("layouts");
        make_root(root_dir.path(), &["vc-dashboard.kdl"], None);

        let s1 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();
        assert!(target_dir.path().join("vc-dashboard.kdl").is_symlink());
        assert_eq!(s1.created, vec!["vc-dashboard.kdl".to_string()]);

        // Simulate the rename in the repo.
        fs::rename(
            layouts_dir.join("vc-dashboard.kdl"),
            layouts_dir.join("dashboard.kdl"),
        )
        .unwrap();
        fs::write(
            layouts_dir.join(ALIAS_FILENAME),
            "vc-dashboard.kdl=dashboard.kdl\n",
        )
        .unwrap();

        let s2 = install(
            Some(root_dir.path().to_path_buf()),
            Some(target_dir.path().to_path_buf()),
        )
        .unwrap();

        // Old name now resolves through alias to new file.
        assert!(target_dir.path().join("dashboard.kdl").is_symlink());
        let vc_old = target_dir.path().join("vc-dashboard.kdl");
        assert!(vc_old.is_symlink());
        let resolved = fs::read_link(&vc_old).unwrap();
        let resolved_canon = dunce::canonicalize(&resolved).unwrap_or(resolved);
        let canonical_layouts_dir = dunce::canonicalize(&layouts_dir).unwrap();
        assert_eq!(resolved_canon, canonical_layouts_dir.join("dashboard.kdl"));
        assert!(s2
            .aliases_installed
            .iter()
            .any(|(o, _)| o == "vc-dashboard.kdl"));
    }
}
