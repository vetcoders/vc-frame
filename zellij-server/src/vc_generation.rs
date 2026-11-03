//! Session generation is pinned to the physical server executable, never PATH
//! or an inherited runtime-root. Installation state is read anew on each tick.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VC_GENERATION_MESSAGE: &str = "vc.generation.v1";

pub struct GenerationFeed {
    running_root: Option<PathBuf>,
    tools_home: Option<PathBuf>,
}

#[derive(Serialize)]
struct GenerationSnapshot {
    schema: &'static str,
    running: String,
    active: Option<String>,
    split: Option<bool>,
}

#[derive(Deserialize)]
struct ActiveRuntime {
    schema: String,
    runtime_root: PathBuf,
}

impl GenerationFeed {
    pub fn capture() -> Self {
        let mut feed = Self::for_executable(
            // Display-only provenance: never executed or used to grant permissions.
            std::env::current_exe() // nosemgrep: rust.lang.security.current-exe.current-exe
                .and_then(|path| path.canonicalize())
                .ok()
                .as_deref(),
        );
        feed.tools_home = std::env::var_os("VIBECRAFTED_TOOLS_HOME").map(PathBuf::from);
        feed
    }

    fn for_executable(executable: Option<&Path>) -> Self {
        // Runtime packs put vc-frame in libexec (or bin). Capture once: even
        // if an old pack is later removed, its running identity stays intact.
        let running_root = executable.and_then(|exe| {
            let bin = exe.parent()?;
            if !matches!(bin.file_name()?.to_str()?, "libexec" | "bin") {
                return None;
            }
            let root = bin.parent()?;
            (root.parent()?.file_name()? == "releases").then(|| root.to_owned())
        });
        Self {
            running_root,
            tools_home: None,
        }
    }

    fn snapshot(&self) -> GenerationSnapshot {
        let active_root = self.running_root.as_deref().and_then(|root| {
            let home = root.parent()?.parent()?;
            read_active_root(home, self.tools_home.as_deref())
        });
        GenerationSnapshot {
            schema: VC_GENERATION_MESSAGE,
            running: self
                .running_root
                .as_deref()
                .and_then(generation_label)
                .unwrap_or_else(|| zellij_utils::build_info::HUMAN_VERSION.to_owned()),
            active: active_root.as_deref().and_then(generation_label),
            split: self
                .running_root
                .as_ref()
                .zip(active_root.as_ref())
                .map(|(running, active)| running != active),
        }
    }

    pub fn payload(&self) -> String {
        serde_json::to_string(&self.snapshot()).expect("generation snapshot is serializable")
    }
}

fn generation_label(root: &Path) -> Option<String> {
    let label = root.file_name()?.to_str()?;
    (!label.is_empty()
        && label.len() <= 96
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".+_-".contains(&b)))
    .then(|| label.to_owned())
}

fn read_active_root(home: &Path, tools_home: Option<&Path>) -> Option<PathBuf> {
    let pointer = home.join("active.json");
    if std::fs::symlink_metadata(&pointer)
        .ok()?
        .file_type()
        .is_symlink()
    {
        return None;
    }
    let active: ActiveRuntime = serde_json::from_slice(&std::fs::read(pointer).ok()?).ok()?;
    if active.schema != "vibecrafted.active-runtime.v1" || !active.runtime_root.is_absolute() {
        return None;
    }
    let root = active.runtime_root.canonicalize().ok()?;
    if root.parent()? != home.join("releases").canonicalize().ok()? || !root.is_dir() {
        return None;
    }
    generation_label(&root)?;
    // Same projection used by runtime_paths.resolve_active_generation. A
    // conflicting/broken pointer is unknown, never a false parity claim.
    let tools = tools_home
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join("tools"));
    let current = tools.join("vibecrafted-current");
    match std::fs::symlink_metadata(&current) {
        Ok(metadata) => {
            if !metadata.file_type().is_symlink() || current.canonicalize().ok()? != root {
                return None;
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(_) => return None,
    }
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(home: &Path, generation: &str) -> PathBuf {
        let root = home.join("releases").join(generation);
        std::fs::create_dir_all(root.join("libexec")).unwrap();
        std::fs::write(
            home.join("active.json"),
            serde_json::json!({
                "schema": "vibecrafted.active-runtime.v1", "runtime_root": root,
            })
            .to_string(),
        )
        .unwrap();
        root
    }

    #[test]
    fn installation_rotation_preserves_server_identity_and_raises_split() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let old = install(&home, "4.3.1+gf8debfd6");
        let feed = GenerationFeed::for_executable(Some(&old.join("libexec/vc-frame")));
        assert_eq!(feed.snapshot().split, Some(false));
        install(&home, "4.3.1+g7a69d24d");
        let snapshot = feed.snapshot();
        assert_eq!(snapshot.running, "4.3.1+gf8debfd6");
        assert_eq!(snapshot.active.as_deref(), Some("4.3.1+g7a69d24d"));
        assert_eq!(snapshot.split, Some(true));
        std::fs::remove_dir_all(old).unwrap();
        assert_eq!(feed.snapshot().running, "4.3.1+gf8debfd6");
    }

    #[test]
    fn missing_corrupt_or_escaping_installation_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let old = install(&home, "4.3.1+gf8debfd6");
        let feed = GenerationFeed::for_executable(Some(&old.join("libexec/vc-frame")));
        for body in [
            "bad json".to_owned(),
            serde_json::json!({
                "schema": "vibecrafted.active-runtime.v1", "runtime_root": home,
            })
            .to_string(),
        ] {
            std::fs::write(home.join("active.json"), body).unwrap();
            assert_eq!(feed.snapshot().split, None);
            assert_eq!(feed.snapshot().active, None);
        }
        std::fs::remove_file(home.join("active.json")).unwrap();
        assert_eq!(feed.snapshot().split, None);
    }

    #[test]
    fn standalone_build_does_not_borrow_an_installed_generation() {
        let feed = GenerationFeed::for_executable(Some(Path::new("/work/target/debug/vc-frame")));
        assert_eq!(
            feed.snapshot().running,
            zellij_utils::build_info::HUMAN_VERSION
        );
        assert_eq!(feed.snapshot().split, None);
    }

    #[cfg(unix)]
    #[test]
    fn conflicting_projection_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let old = install(&home, "4.3.1+gf8debfd6");
        install(&home, "4.3.1+g7a69d24d");
        std::fs::create_dir(home.join("tools")).unwrap();
        std::os::unix::fs::symlink(&old, home.join("tools/vibecrafted-current")).unwrap();
        assert_eq!(read_active_root(&home, None), None);
    }
}
