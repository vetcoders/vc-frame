use crate::data::LayoutInfo;
use crate::input::options::Options;
use crate::pane_size::Size;
use crate::{
    home::{find_default_config_dir, get_theme_dir},
    input::{config::Config, layout::Layout, theme::Themes},
    setup::get_default_themes,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CliAssets {
    pub config_file_path: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
    pub should_ignore_config: bool,
    pub configuration_options: Option<Options>, // merged from everywhere: there are the source of truth
    pub layout: Option<LayoutInfo>,
    pub terminal_window_size: Size,
    pub data_dir: Option<PathBuf>,
    pub is_debug: bool,
    pub max_panes: Option<usize>,
    pub force_run_layout_commands: bool,
    pub cwd: Option<PathBuf>,
    /// Set only by the `ClientInfo::Resurrect` path; never infer this from a
    /// cache path, which a fresh invocation can also name.
    pub is_resurrection: bool,
    /// Frontend identity, stable across native session switches; not a server ClientId.
    #[serde(default)]
    pub client_identity: Option<String>,
}

impl CliAssets {
    pub fn load_config_and_layout(&self) -> (Config, Layout) {
        let config = {
            if self.should_ignore_config {
                Config::from_default_assets().unwrap_or_else(|_| Default::default())
            } else if let Some(ref path) = self.config_file_path {
                let default_config =
                    Config::from_default_assets().unwrap_or_else(|_| Default::default());
                Config::from_path(path, Some(default_config.clone())).unwrap_or(default_config)
            } else {
                Config::from_default_assets().unwrap_or_else(|_| Default::default())
            }
        };

        let (mut layout, mut config_with_merged_layout_opts) = {
            let layout_dir = self
                .configuration_options
                .as_ref()
                .and_then(|e| e.layout_dir.clone())
                .or_else(|| config.options.layout_dir.clone())
                .or_else(|| {
                    self.config_dir
                        .clone()
                        .or_else(find_default_config_dir)
                        .map(|dir| dir.join("layouts"))
                });
            self.layout.as_ref().and_then(|layout_info| {
                Layout::from_layout_info_with_config(&layout_dir, layout_info, Some(config.clone()))
                    .ok()
            })
        }
        .unwrap_or_else(|| (Layout::default_layout_asset(), config));

        if self.is_resurrection {
            // The launch cwd is workspace intent; the checkpoint's foreground
            // process cwd (often HOME) must not override it for owned Home.
            let cwd = self.cwd.clone().or_else(|| std::env::current_dir().ok());
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let cwd = cwd.as_deref().filter(|cwd| Some(*cwd) != home.as_deref());
            layout.normalize_host_home(cwd);
        }

        if self.force_run_layout_commands {
            layout.recursively_add_start_suspended(Some(false));
        }

        config_with_merged_layout_opts.themes = config_with_merged_layout_opts
            .themes
            .merge(get_default_themes());

        let user_theme_dir = self
            .configuration_options
            .as_ref()
            .and_then(|o| o.theme_dir.clone())
            .or_else(|| {
                config_with_merged_layout_opts
                    .options
                    .theme_dir
                    .clone()
                    .or_else(|| {
                        get_theme_dir(self.config_dir.clone().or_else(find_default_config_dir))
                    })
                    .filter(|dir| dir.exists())
            });
        if let Some(themes) = user_theme_dir.and_then(|u| Themes::from_dir(u).ok()) {
            config_with_merged_layout_opts.themes =
                config_with_merged_layout_opts.themes.merge(themes);
        }

        (config_with_merged_layout_opts, layout)
    }
}

#[cfg(test)]
mod host_home_tests {
    use super::*;
    use crate::input::layout::Run;

    fn fixture(host: bool) -> String {
        format!(
            r#"layout {{
            cwd "/old/home"
            tab name="Home" {{
                pane command="/old/releases/gf8debfd6/bin/vc-o" name="Dashboard" {{
                    args "--view" "host-config"
                    start_suspended true
                }}
                pane command="dangerous-user-command" {{ start_suspended true; }}
            }}
            tab name="Workspace" {{
                pane {{ plugin location="frame-host" {{ frame_host {host}; }}; }}
                pane {{ plugin location="session-manager" {{ workspace_surface true; }}; }}
            }}
        }}"#
        )
    }

    #[test]
    fn resurrection_normalizes_only_owned_home_and_keeps_projection_and_user_hold() {
        let assets = CliAssets {
            should_ignore_config: true,
            is_resurrection: true,
            cwd: Some(PathBuf::from("/work/project")),
            layout: Some(LayoutInfo::Stringified(fixture(true))),
            ..Default::default()
        };
        let (_, layout) = assets.load_config_and_layout();
        let runs = layout.tabs[0].1.extract_run_instructions();
        let commands: Vec<_> = runs
            .iter()
            .filter_map(|r| match r {
                Some(Run::Command(command)) => Some(command),
                _ => None,
            })
            .collect();
        assert_eq!(commands[0].command, PathBuf::from("vc-o"));
        assert_eq!(commands[0].args, ["--view", "host-config"]);
        assert_eq!(commands[0].cwd, Some(PathBuf::from("/work/project")));
        assert!(!commands[0].hold_on_start);
        assert!(commands[1].hold_on_start);
        assert_eq!(commands[1].command, PathBuf::from("dangerous-user-command"));
        assert!(layout.tabs[1].1.extract_run_instructions().iter().any(|r| {
            matches!(r, Some(Run::Plugin(p)) if p.effective_plugin_configuration()
                .is_some_and(|c| c.get("workspace_surface").map(String::as_str) == Some("true")))
        }));
    }

    #[test]
    fn fresh_layout_and_non_host_resurrection_keep_explicit_suspension() {
        for (host, resurrection) in [(true, false), (false, true)] {
            let assets = CliAssets {
                should_ignore_config: true,
                is_resurrection: resurrection,
                cwd: Some(PathBuf::from("/work/project")),
                layout: Some(LayoutInfo::Stringified(fixture(host))),
                ..Default::default()
            };
            let (_, layout) = assets.load_config_and_layout();
            assert!(layout.tabs[0].1.extract_run_instructions().iter().any(|r| {
                matches!(r, Some(Run::Command(c)) if c.command.is_absolute() && c.hold_on_start)
            }));
        }
    }
}
