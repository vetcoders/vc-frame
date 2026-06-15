mod file_list_view;
mod platform;
mod search_view;
mod shared;
mod state;

use platform::Platform;
use shared::{
    refresh_directory, render_current_path, render_instruction_line, render_search_term,
    render_virtual_root_header,
};
use state::State;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zellij_tile::prelude::*;

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        let pane_title = configuration
            .get("pane_title")
            .cloned()
            .unwrap_or_else(|| "Strider".to_owned());
        rename_plugin_pane(get_plugin_ids().plugin_id, pane_title);
        let plugin_ids = get_plugin_ids();
        let initial_cwd_str = plugin_ids.initial_cwd.to_string_lossy().to_string();
        let platform = Platform::detect(&initial_cwd_str);
        self.platform = platform;
        self.initial_cwd = Platform::normalize(&plugin_ids.initial_cwd);
        self.file_list_view.platform = platform;
        self.search_view.platform = platform;
        let show_hidden_files = configuration
            .get("show_hidden_files")
            .map(|v| v == "true")
            .unwrap_or(false);
        self.hide_hidden_files = !show_hidden_files;
        self.close_on_selection = configuration
            .get("close_on_selection")
            .map(|v| v == "true")
            .unwrap_or(false);
        subscribe(&[
            EventType::Key,
            EventType::Mouse,
            EventType::CustomMessage,
            EventType::Timer,
            EventType::FileSystemUpdate,
            EventType::HostFolderChanged,
            EventType::PermissionRequestResult,
        ]);
        self.file_list_view.clear_selected();

        let artifacts_mode = artifacts_mode_enabled(&configuration);
        let configured_cwd = match configuration
            .get("caller_cwd")
            .map(|c| Platform::normalize(&PathBuf::from(c)))
        {
            Some(caller_cwd) => caller_cwd,
            None => self.initial_cwd.clone(),
        };
        self.file_list_view.path = resolve_configured_cwd(configured_cwd, artifacts_mode, |name| {
            std::env::var(name).ok()
        });
        if artifacts_mode {
            self.initial_cwd = self.file_list_view.path.clone();
        }
        if self.initial_cwd != self.file_list_view.path {
            change_host_folder(self.file_list_view.path.clone());
        } else {
            scan_host_folder(&"/host");
        }
    }

    fn update(&mut self, event: Event) -> bool {
        let mut should_render = false;
        match event {
            Event::FileSystemUpdate(paths) => {
                self.update_files(paths);
                should_render = true;
            },
            Event::HostFolderChanged(_new_host_folder) => {
                scan_host_folder(&"/host");
                should_render = true;
            },
            Event::Key(key) => match key.bare_key {
                BareKey::Char(character) if key.has_no_modifiers() => {
                    self.update_search_term(character);
                    should_render = true;
                },
                BareKey::Backspace if key.has_no_modifiers() => {
                    self.handle_backspace();
                    should_render = true;
                },
                BareKey::Esc if key.has_no_modifiers() => {
                    if self.is_in_virtual_root {
                        self.exit_virtual_root();
                    } else if self.is_searching {
                        self.clear_search_term();
                    } else {
                        self.file_list_view.clear_selected();
                    }
                    should_render = true;
                },
                BareKey::Char('c') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    self.clear_search_term_or_descend();
                },
                BareKey::Up if key.has_no_modifiers() => {
                    self.move_selection_up();
                    should_render = true;
                },
                BareKey::Down if key.has_no_modifiers() => {
                    self.move_selection_down();
                    should_render = true;
                },
                BareKey::Right | BareKey::Tab | BareKey::Enter if key.has_no_modifiers() => {
                    self.traverse_dir();
                    should_render = true;
                },
                BareKey::Right if key.has_no_modifiers() => {
                    self.traverse_dir();
                    should_render = true;
                },
                BareKey::Left if key.has_no_modifiers() => {
                    if !self.is_in_virtual_root {
                        self.descend_to_previous_path();
                    }
                    should_render = true;
                },
                BareKey::Char('e') if key.has_modifiers(&[KeyModifier::Ctrl]) => {
                    should_render = true;
                    self.toggle_hidden_files();
                    refresh_directory(&self.file_list_view.path);
                },
                _ => (),
            },
            Event::Mouse(mouse_event) => match mouse_event {
                Mouse::ScrollDown(_) => {
                    self.move_selection_down();
                    should_render = true;
                },
                Mouse::ScrollUp(_) => {
                    self.move_selection_up();
                    should_render = true;
                },
                Mouse::LeftClick(line, _) => {
                    self.handle_left_click(line);
                    should_render = true;
                },
                Mouse::Hover(line, _) if line >= 0 => {
                    self.handle_mouse_hover(line);
                    should_render = true;
                },
                _ => {},
            },
            _ => {
                dbg!("Unknown event {:?}", event);
            },
        };
        should_render
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        if pipe_message.is_private && pipe_message.name == "filepicker" {
            let open_directly = pipe_message
                .args
                .get("open_directly")
                .map(|v| v == "true")
                .unwrap_or(false);
            if open_directly {
                // Standalone mode: selecting a file opens it directly,
                // then the plugin closes itself.
                self.close_on_selection = true;
            } else {
                // Filepicker callback mode: send result back to caller.
                #[allow(unused_variables)]
                // pipe_id is used inside #[cfg(target_family = "wasm")] block
                if let PipeSource::Cli(pipe_id) = &pipe_message.source {
                    #[cfg(target_family = "wasm")]
                    block_cli_pipe_input(pipe_id);
                }
                self.handling_filepick_request_from =
                    Some((pipe_message.source, pipe_message.args));
            }
            true
        } else {
            false
        }
    }

    fn render(&mut self, rows: usize, cols: usize) {
        self.current_rows = Some(rows);
        let rows_for_list = rows.saturating_sub(6);
        if self.is_in_virtual_root {
            render_search_term("");
            render_virtual_root_header(cols);
            self.render_virtual_root(rows_for_list, cols);
        } else {
            render_search_term(&self.search_term);
            render_current_path(
                &self.file_list_view.path,
                self.file_list_view.path_is_dir,
                self.handling_filepick_request_from.is_some(),
                cols,
                self.platform,
            );
            if self.is_searching {
                self.search_view.render(rows_for_list, cols);
            } else {
                self.file_list_view.render(rows_for_list, cols);
            }
        }
        render_instruction_line(rows, cols);
    }
}

fn artifacts_mode_enabled(configuration: &BTreeMap<String, String>) -> bool {
    configuration.get("mode").map(|v| v.as_str()) == Some("artifacts")
        || (configuration.get("file_filter").map(|v| v.as_str()) == Some("*.md")
            && configuration.get("sort_by").map(|v| v.as_str()) == Some("modified_desc"))
}

fn resolve_configured_cwd<F>(candidate: PathBuf, artifacts_mode: bool, env_lookup: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if is_existing_dir(&candidate) {
        return candidate;
    }
    if !artifacts_mode && !contains_vibecrafted_marker(&candidate) {
        return candidate;
    }
    vibecrafted_artifacts_fallback(env_lookup).unwrap_or(candidate)
}

fn contains_vibecrafted_marker(path: &Path) -> bool {
    let path = path.to_string_lossy();
    path.contains("${")
        || path.contains('}')
        || path.contains("VIBECRAFTED_HOME")
        || path.contains(".vibecrafted")
}

fn vibecrafted_artifacts_fallback<F>(env_lookup: F) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    env_lookup("VIBECRAFTED_HOME")
        .map(PathBuf::from)
        .map(|home| home.join("artifacts"))
        .filter(|path| is_existing_dir(path))
        .or_else(|| {
            env_lookup("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".vibecrafted").join("artifacts"))
                .filter(|path| is_existing_dir(path))
        })
}

fn is_existing_dir(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

    fn unique_temp_dir(test_name: &str) -> PathBuf {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "strider-{}-{}-{}",
            test_name,
            std::process::id(),
            id
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn invalid_vibecrafted_cwd_falls_back_to_vibecrafted_home_artifacts() {
        let temp_dir = unique_temp_dir("vibecrafted-home");
        let artifacts_dir = temp_dir.join("artifacts");
        std::fs::create_dir_all(&artifacts_dir).unwrap();

        let resolved = resolve_configured_cwd(
            PathBuf::from("${VIBECRAFTED_HOME:-$HOME/.vibecrafted}/artifacts"),
            false,
            |name| match name {
                "VIBECRAFTED_HOME" => Some(temp_dir.display().to_string()),
                "HOME" => None,
                _ => None,
            },
        );

        assert_eq!(resolved, artifacts_dir);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn plain_invalid_cwd_stays_unchanged_without_vibecrafted_markers() {
        let temp_dir = unique_temp_dir("plain-invalid");
        let fallback_home = temp_dir.join("home");
        let fallback_artifacts = fallback_home.join(".vibecrafted").join("artifacts");
        std::fs::create_dir_all(&fallback_artifacts).unwrap();
        let invalid_plain_path = temp_dir.join("does-not-exist");

        let resolved =
            resolve_configured_cwd(invalid_plain_path.clone(), false, |name| match name {
                "VIBECRAFTED_HOME" => None,
                "HOME" => Some(fallback_home.display().to_string()),
                _ => None,
            });

        assert_eq!(resolved, invalid_plain_path);
        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
