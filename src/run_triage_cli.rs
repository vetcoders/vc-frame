//! The `vc-frame triage-run` executor.
//!
//! [`zellij_utils::run_triage`] owns the ordering — capture, then recreate,
//! then close. This module is only the hands: it turns each step into the CLI
//! actions that already exist (`dump-screen`, `attach --create-background`,
//! `new-tab`, `dump-layout`, `close-tab`) and reports back.
//!
//! Every step shells out to the running executable rather than reaching into
//! the client internals, because a bucket session is a *separate server
//! process* — there is no in-process handle to it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use zellij_utils::run_triage::{
    BucketKind, FinishedRun, RunMeta, TransferReport, TriageIo, control_plane_root,
    transfer_finished_run,
};
use zellij_utils::sessions::session_exists;

/// Drives the transfer through real sessions.
struct CliTriageIo {
    /// The `vc-frame` binary to re-invoke. Resolved once so a triage started by
    /// a dev build keeps talking to that same build.
    executable: PathBuf,
    /// Control-plane root, needed to point the viewer pane at the capture.
    root: PathBuf,
}

impl CliTriageIo {
    fn new(root: PathBuf) -> Result<Self, String> {
        let executable = std::env::current_exe()
            .map_err(|e| format!("cannot resolve the vc-frame executable: {}", e))?;
        Ok(CliTriageIo { executable, root })
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let output = Command::new(&self.executable)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("failed to invoke `vc-frame {}`: {}", args.join(" "), e))?;
        if !output.status.success() {
            return Err(format!(
                "`vc-frame {}` failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The bucket tab: a scrollback viewer on top, a suspended rerun below.
///
/// `start_suspended true` is what makes the rerun a single keypress — the pane
/// holds the original command without running it until the operator says so.
fn bucket_tab_layout(scrollback: &Path, meta: &RunMeta) -> String {
    // Chrome mirrors `default_tab_template` in assets/layouts/vibecrafted.kdl:
    // compact-bar on top, the left Sessions rail, status-bar below. The layout
    // contract ("every tab keeps the left Sessions rail") applies to transferred
    // bucket tabs too — a fullscreen scrollback that hides the rail strands the
    // operator inside the bucket with no way back but the keyboard.
    let mut layout = String::from(
        "layout {\n\
         \x20   pane size=1 borderless=true {\n\
         \x20       plugin location=\"compact-bar\"\n\
         \x20   }\n\
         \x20   pane split_direction=\"vertical\" {\n\
         \x20       pane size=24 borderless=true {\n\
         \x20           plugin location=\"session-manager\" {\n\
         \x20               rail true\n\
         \x20               pane_title \"Sessions\"\n\
         \x20           }\n\
         \x20       }\n\
         \x20       pane {\n",
    );
    layout.push_str(&format!(
        "            pane command=\"less\" name=\"scrollback · exit {}\" {{\n                args \"-R\" \"{}\"\n            }}\n",
        meta.exit_code,
        kdl_escape(&scrollback.to_string_lossy())
    ));
    if let Some((program, args)) = meta.command.split_first() {
        layout.push_str(&format!(
            "            pane command=\"{}\" name=\"rerun\" start_suspended=true {{\n",
            kdl_escape(program)
        ));
        if !args.is_empty() {
            let rendered: Vec<String> = args
                .iter()
                .map(|arg| format!("\"{}\"", kdl_escape(arg)))
                .collect();
            layout.push_str(&format!("                args {}\n", rendered.join(" ")));
        }
        if let Some(cwd) = meta.cwd.as_ref() {
            layout.push_str(&format!(
                "                cwd \"{}\"\n",
                kdl_escape(&cwd.to_string_lossy())
            ));
        }
        layout.push_str("            }\n");
    }
    layout.push_str(
        "        }\n\
         \x20   }\n\
         \x20   pane size=1 borderless=true {\n\
         \x20       plugin location=\"status-bar\"\n\
         \x20   }\n\
         }\n",
    );
    layout
}

fn kdl_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

impl TriageIo for CliTriageIo {
    fn capture_scrollback(
        &mut self,
        session: &str,
        pane_id: Option<&str>,
        dest: &Path,
    ) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }
        let dest = dest.to_string_lossy().into_owned();
        let mut args = vec![
            "-s",
            session,
            "action",
            "dump-screen",
            "--full",
            "--path",
            &dest,
        ];
        if let Some(pane_id) = pane_id {
            args.extend_from_slice(&["--pane-id", pane_id]);
        }
        self.run(&args)?;
        // dump-screen reports success even when the pane is gone; an empty file
        // is not a capture, and closing the origin tab on one would lose the run.
        match std::fs::metadata(&dest) {
            Ok(metadata) if metadata.len() > 0 => Ok(()),
            Ok(_) => Err(format!("scrollback dump at {} is empty", dest)),
            Err(e) => Err(format!("scrollback dump at {} is missing: {}", dest, e)),
        }
    }

    fn write_meta(&mut self, dest: &Path, meta: &RunMeta) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
        }
        let serialized = serde_json::to_string_pretty(meta)
            .map_err(|e| format!("cannot serialize run metadata: {}", e))?;
        // Write-then-rename so a crash mid-write cannot leave a half-parsed
        // meta.json next to a good scrollback.
        let temporary = dest.with_extension("json.tmp");
        let mut file = std::fs::File::create(&temporary)
            .map_err(|e| format!("cannot create {}: {}", temporary.display(), e))?;
        file.write_all(serialized.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|e| format!("cannot write {}: {}", temporary.display(), e))?;
        std::fs::rename(&temporary, dest)
            .map_err(|e| format!("cannot commit {}: {}", dest.display(), e))
    }

    fn ensure_bucket_session(&mut self, session: &str) -> Result<(), String> {
        match session_exists(session) {
            Ok(true) => Ok(()),
            Ok(false) => {
                self.run(&["attach", "--create-background", session])?;
                Ok(())
            },
            Err(e) => Err(format!("cannot check for session '{}': {:?}", session, e)),
        }
    }

    fn open_bucket_tab(&mut self, session: &str, tab: &str, meta: &RunMeta) -> Result<(), String> {
        let scrollback = zellij_utils::run_triage::scrollback_path(&self.root, &meta.run);
        let layout = bucket_tab_layout(&scrollback, meta);
        self.run(&[
            "-s",
            session,
            "action",
            "new-tab",
            "--name",
            tab,
            "--layout-string",
            &layout,
        ])?;
        Ok(())
    }

    fn bucket_tab_exists(&mut self, session: &str, tab: &str) -> Result<bool, String> {
        let layout = self.run(&["-s", session, "action", "dump-layout"])?;
        Ok(layout.contains(tab))
    }

    fn close_origin_tab(&mut self, session: &str, tab: &str) -> Result<(), String> {
        // No --create: if the tab is already gone there is nothing to close, and
        // creating one just to close it would be absurd.
        self.run(&["-s", session, "action", "go-to-tab-name", tab])?;
        self.run(&["-s", session, "action", "close-tab"])?;
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn triage_run(
    run: String,
    exit_code: i32,
    bucket_verdict: Option<BucketKind>,
    origin_session: Option<String>,
    origin_tab: Option<String>,
    pane_id: Option<String>,
    cwd: Option<PathBuf>,
    dry_run: bool,
    command: Vec<String>,
) -> Result<TransferReport, String> {
    let origin_session = origin_session
        .or_else(|| zellij_utils::envs::get_session_name().ok())
        .ok_or_else(|| {
            "no origin session: pass --origin-session or run from inside a session".to_owned()
        })?;
    let origin_tab = origin_tab.unwrap_or_else(|| run.clone());
    let cwd = cwd.or_else(|| std::env::current_dir().ok());

    let finished = FinishedRun {
        run,
        exit_code,
        origin_session,
        origin_tab,
        pane_id,
        command,
        cwd,
        bucket_verdict,
    };
    let root = control_plane_root()
        .ok_or_else(|| "cannot resolve the control plane root (set VIBECRAFTED_HOME)".to_owned())?;
    let captured_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    if dry_run {
        let bucket = finished.bucket();
        println!(
            "would transfer '{}' (exit {}) from {}/{} into '{}'",
            finished.run,
            finished.exit_code,
            finished.origin_session,
            finished.origin_tab,
            bucket.session_name()
        );
        return Ok(TransferReport {
            run: finished.run.clone(),
            bucket,
            scrollback: zellij_utils::run_triage::scrollback_path(&root, &finished.run),
            meta: zellij_utils::run_triage::meta_path(&root, &finished.run),
            origin_tab_closed: false,
        });
    }

    let mut io = CliTriageIo::new(root.clone())?;
    transfer_finished_run(&mut io, &finished, &root, captured_at).map_err(|error| {
        format!(
            "triage failed at {:?}: {} (origin tab preserved: {}, capture durable: {})",
            error.step, error.message, error.origin_tab_preserved, error.capture_is_durable
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(command: Vec<&str>) -> RunMeta {
        RunMeta {
            run: "impl-260720-120000-01000".to_owned(),
            exit_code: 1,
            bucket: BucketKind::NeedsAttention,
            origin_session: "Operator".to_owned(),
            origin_tab: "impl-260720-120000-01000".to_owned(),
            command: command.into_iter().map(str::to_owned).collect(),
            cwd: Some(PathBuf::from("/repo")),
            captured_at: 0,
        }
    }

    #[test]
    fn bucket_tab_shows_the_scrollback_and_a_suspended_rerun() {
        let layout = bucket_tab_layout(
            Path::new("/cp/finished_runs/impl-260720-120000-01000/scrollback.txt"),
            &meta(vec!["claude", "--resume"]),
        );

        assert!(layout.contains("/cp/finished_runs/impl-260720-120000-01000/scrollback.txt"));
        assert!(layout.contains("command=\"claude\""));
        assert!(layout.contains("args \"--resume\""));
        // one-keypress rerun, not an automatic one
        assert!(layout.contains("start_suspended=true"));
        assert!(layout.contains("cwd \"/repo\""));
    }

    #[test]
    fn a_run_without_a_command_still_gets_a_viewer() {
        let layout = bucket_tab_layout(Path::new("/cp/s.txt"), &meta(vec![]));
        assert!(layout.contains("/cp/s.txt"));
        assert!(!layout.contains("start_suspended"));
    }

    #[test]
    fn quotes_and_backslashes_cannot_break_out_of_the_layout() {
        let layout = bucket_tab_layout(
            Path::new("/cp/s.txt"),
            &meta(vec!["sh", "-c", "echo \"hi\""]),
        );
        assert!(layout.contains(r#"args "-c" "echo \"hi\"""#));
    }
}
