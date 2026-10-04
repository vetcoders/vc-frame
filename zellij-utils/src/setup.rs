#[cfg(not(target_family = "wasm"))]
use crate::consts::ASSET_MAP;
// Feeds get_default_themes, which is cfg(not(test)) — test builds would
// otherwise flag the import as dead.
#[cfg(not(test))]
use crate::consts::ZELLIJ_DEFAULT_THEMES;
use crate::input::theme::Themes;
use crate::{
    cli::{CliArgs, Command, SessionCommand, Sessions},
    consts::{FEATURES, ZELLIJ_CACHE_DIR},
    data::LayoutInfo,
    errors::prelude::*,
    home::*,
    input::{
        config::{Config, ConfigError},
        layout::Layout,
        options::Options,
    },
};
use clap::{Args, IntoApp};
use clap_complete::Shell;
use log::info;
use serde::{Deserialize, Serialize};
use std::{
    convert::TryFrom,
    fmt::Write as FmtWrite,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process,
};

const CONFIG_NAME: &str = "config.kdl";
static ARROW_SEPARATOR: &str = "";

#[cfg(not(test))]
pub fn get_default_themes() -> Themes {
    let mut themes = Themes::default();
    for file in ZELLIJ_DEFAULT_THEMES.files() {
        if let Some(content) = file.contents_utf8() {
            let sourced_from_external_file = true;
            if let Ok(theme) = Themes::from_string(content, sourced_from_external_file) {
                themes = themes.merge(theme)
            }
        }
    }
    themes
}

#[cfg(test)]
pub fn get_default_themes() -> Themes {
    Themes::default()
}

pub fn dump_asset(asset: &[u8]) -> std::io::Result<()> {
    std::io::stdout().write_all(asset)?;
    Ok(())
}

pub const DEFAULT_CONFIG: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/config/default.kdl"
));

pub const DEFAULT_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/default.kdl"
));

pub const DEFAULT_SWAP_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/default.swap.kdl"
));

pub const STRIDER_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/strider.kdl"
));

pub const STRIDER_SWAP_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/strider.swap.kdl"
));

pub const NO_STATUS_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/disable-status-bar.kdl"
));

pub const COMPACT_BAR_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/compact.kdl"
));

pub const COMPACT_BAR_SWAP_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/compact.swap.kdl"
));

pub const CLASSIC_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/classic.kdl"
));

pub const CLASSIC_SWAP_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/classic.swap.kdl"
));

pub const WELCOME_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/welcome.kdl"
));

pub const VC_DASHBOARD_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vc-dashboard.kdl"
));

pub const VIBECRAFTED_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vibecrafted.kdl"
));

pub const VIBECRAFTED_HOST_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vibecrafted-host.kdl"
));

/// Name of the host chrome baked into the binary. A host session loads this
/// asset directly. It is not a file under the layout directory.
const EMBEDDED_HOST_LAYOUT: &str = "vibecrafted-host";

// Retired name remains an alias to the ordinary complete project session.
pub const VIBECRAFTED_GUEST_LAYOUT: &[u8] = VIBECRAFTED_LAYOUT;

pub const VC_WORKFLOW_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vc-workflow.kdl"
));

pub const VC_MARBLES_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vc-marbles.kdl"
));

pub const VC_RESEARCH_LAYOUT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/layouts/vc-research.kdl"
));

pub const FISH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.fish"
));

pub const BASH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.bash"
));

pub const ZSH_EXTRA_COMPLETION: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/completions/comp.zsh"
));

pub const BASH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.bash"
));

pub const FISH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.fish"
));

pub const ZSH_AUTO_START_SCRIPT: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/",
    "assets/shell/auto-start.zsh"
));

pub fn add_layout_ext(s: &str) -> String {
    match s {
        c if s.ends_with(".kdl") => c.to_owned(),
        _ => {
            let mut s = s.to_owned();
            s.push_str(".kdl");
            s
        },
    }
}

pub fn dump_default_config() -> std::io::Result<()> {
    dump_asset(DEFAULT_CONFIG)
}

pub fn dump_specified_layout(layout: &str) -> std::io::Result<()> {
    let layout = if layout == "disable-status" {
        "disable-status-bar"
    } else {
        layout
    };
    if let Ok((_layout_name, stringified_layout, _swap_layouts)) =
        Layout::stringified_from_default_assets(Path::new(layout))
    {
        return std::io::stdout().write_all(stringified_layout.as_bytes());
    }

    info!("Dump {layout} layout");
    let custom = add_layout_ext(layout);
    let home = default_layout_dir();
    let path = home.map(|h| h.join(&custom));
    let layout_exists = path.as_ref().map(|p| p.exists()).unwrap_or_default();

    match (path, layout_exists) {
        (Some(path), true) => {
            let content = fs::read_to_string(path)?;
            std::io::stdout().write_all(content.as_bytes())
        },
        _ => {
            log::error!("No layout named {custom} found");
            Ok(())
        },
    }
}

pub fn dump_specified_swap_layout(swap_layout: &str) -> std::io::Result<()> {
    match swap_layout {
        "strider" => dump_asset(STRIDER_SWAP_LAYOUT),
        "default" => dump_asset(DEFAULT_SWAP_LAYOUT),
        "compact" => dump_asset(COMPACT_BAR_SWAP_LAYOUT),
        "classic" => dump_asset(CLASSIC_SWAP_LAYOUT),
        not_found => Err(std::io::Error::other(format!(
            "Swap Layout not found for: {}",
            not_found
        ))),
    }
}

#[cfg(not(target_family = "wasm"))]
pub fn dump_builtin_plugins(path: &Path) -> Result<()> {
    for (asset_path, bytes) in ASSET_MAP.iter() {
        let plugin_path = path.join(asset_path);
        plugin_path
            .parent()
            .with_context(|| {
                format!(
                    "failed to acquire parent path of '{}'",
                    plugin_path.display()
                )
            })
            .and_then(|parent_path| {
                std::fs::create_dir_all(parent_path).context("failed to create parent path")
            })
            .with_context(|| {
                format!(
                    "failed to create folder '{}' to dump plugin '{}' to",
                    path.display(),
                    plugin_path.display()
                )
            })?;

        std::fs::write(plugin_path, bytes)
            .with_context(|| format!("failed to dump builtin plugin '{}'", asset_path.display()))?;
    }

    Ok(())
}

#[cfg(target_family = "wasm")]
pub fn dump_builtin_plugins(_path: &PathBuf) -> Result<()> {
    Ok(())
}

#[derive(Debug, Default, Clone, Args, Serialize, Deserialize)]
pub struct Setup {
    /// Dump the default configuration file to stdout
    #[clap(long, value_parser)]
    pub dump_config: bool,

    /// Disables loading of configuration file at default location,
    /// loads the defaults that vc-frame ships with
    #[clap(long, value_parser)]
    pub clean: bool,

    /// Checks the configuration of vc-frame and displays
    /// currently used directories
    #[clap(long, value_parser)]
    pub check: bool,

    /// Dump specified layout to stdout
    #[clap(long, value_parser)]
    pub dump_layout: Option<String>,

    /// Dump the specified swap layout file to stdout
    #[clap(long, value_parser)]
    pub dump_swap_layout: Option<String>,

    /// Dump the builtin plugins to DIR or "DATA DIR" if unspecified
    #[clap(
        long,
        value_name = "DIR",
        value_parser,
        exclusive = true,
        min_values = 0,
        max_values = 1
    )]
    pub dump_plugins: Option<Option<PathBuf>>,

    /// Generates completion for the specified shell
    #[clap(long, value_name = "SHELL", value_parser)]
    pub generate_completion: Option<String>,

    /// Generates auto-start script for the specified shell
    #[clap(long, value_name = "SHELL", value_parser)]
    pub generate_auto_start: Option<String>,

    /// Install / refresh the Vibecrafted layouts into the user's vc-frame
    /// layout directory. Resolution order for the
    /// framework root: `--vibecrafted-root` flag → `$VIBECRAFTED_HOME` env →
    /// `which vibecrafted` walk-up. Idempotent.
    #[clap(long, value_parser)]
    pub install_vibecrafted_layouts: bool,

    /// Explicit path to the Vibecrafted framework root (the directory
    /// containing `config/zellij/layouts/`). Used by
    /// `--install-vibecrafted-layouts`; overrides env / PATH lookup.
    #[clap(long, value_name = "PATH", value_parser)]
    pub vibecrafted_root: Option<PathBuf>,
}

impl Setup {
    /// Entrypoint from main
    /// Merges options from the config file and the command line options
    /// into `[Options]`, the command line options superceeding the layout
    /// file options, superceeding the config file options:
    /// 1. command line options (`vc-frame options`)
    /// 2. layout options
    ///    (`layout.kdl` / `vc-frame --layout`)
    /// 3. config options (`config.kdl`)
    pub fn from_cli_args(
        cli_args: &CliArgs,
    ) -> Result<(Config, Option<LayoutInfo>, Options, Config, Options), ConfigError> {
        // note that this can potentially exit the process
        Setup::handle_setup_commands(cli_args);
        let config = Config::try_from(cli_args)?;
        let cli_config_options: Option<Options> =
            if let Some(Command::Options(cli_options)) = cli_args.command.clone() {
                Some(*cli_options.options)
            } else {
                None
            };

        // the attach CLI command can also have its own Options, we need to merge them if they
        // exist
        let cli_config_options = merge_attach_command_options(cli_config_options, cli_args);

        let mut config_without_layout = config.clone();
        let (layout_info, mut config) =
            Setup::parse_layout_and_override_config(cli_config_options.as_ref(), config, cli_args)?;

        let config_options =
            apply_themes_to_config(&mut config, cli_config_options.clone(), cli_args)?;
        let config_options_without_layout =
            apply_themes_to_config(&mut config_without_layout, cli_config_options, cli_args)?;
        fn apply_themes_to_config(
            config: &mut Config,
            cli_config_options: Option<Options>,
            cli_args: &CliArgs,
        ) -> Result<Options, ConfigError> {
            let config_options = match cli_config_options {
                Some(cli_config_options) => config.options.merge(cli_config_options),
                None => config.options.clone(),
            };

            config.themes = config.themes.merge(get_default_themes());

            let user_theme_dir = config_options.theme_dir.clone().or_else(|| {
                get_theme_dir(cli_args.config_dir.clone().or_else(find_default_config_dir))
                    .filter(|dir| dir.exists())
            });
            if let Some(user_theme_dir) = user_theme_dir {
                config.themes = config.themes.merge(Themes::from_dir(user_theme_dir)?);
            }
            Ok(config_options)
        }

        if let Some(Command::Setup(setup)) = &cli_args.command {
            setup
                .from_cli_with_options(cli_args, &config_options)
                .map_or_else(
                    |e| {
                        eprintln!("{:?}", e);
                        process::exit(1);
                    },
                    |_| {},
                );
        };
        Ok((
            config,
            layout_info,
            config_options,
            config_without_layout,
            config_options_without_layout,
        ))
    }

    /// General setup helpers
    pub fn from_cli(&self) -> Result<()> {
        if self.clean {
            return Ok(());
        }

        if self.dump_config {
            dump_default_config()?;
            std::process::exit(0);
        }

        if let Some(shell) = &self.generate_completion {
            Self::generate_completion(shell);
            std::process::exit(0);
        }

        if let Some(shell) = &self.generate_auto_start {
            Self::generate_auto_start(shell);
            std::process::exit(0);
        }

        if let Some(layout) = &self.dump_layout {
            dump_specified_layout(layout)?;
            std::process::exit(0);
        }

        if let Some(swap_layout) = &self.dump_swap_layout {
            dump_specified_swap_layout(swap_layout)?;
            std::process::exit(0);
        }

        if self.install_vibecrafted_layouts {
            #[cfg(not(target_family = "wasm"))]
            {
                match crate::vibecrafted_install::install(self.vibecrafted_root.clone(), None) {
                    Ok(summary) => {
                        print!("{}", summary.render());
                        std::process::exit(0);
                    },
                    Err(err) => {
                        eprintln!("vibecrafted layout install failed: {err}");
                        std::process::exit(1);
                    },
                }
            }
        }

        Ok(())
    }

    /// Checks the merged configuration
    pub fn from_cli_with_options(&self, opts: &CliArgs, config_options: &Options) -> Result<()> {
        if self.check {
            Setup::check_defaults_config(opts, config_options)?;
            std::process::exit(0);
        }

        if let Some(maybe_path) = &self.dump_plugins {
            let data_dir = &opts.data_dir.clone().unwrap_or_else(get_default_data_dir);
            let dir = match maybe_path {
                Some(path) => path,
                None => data_dir,
            };

            println!("Dumping plugins to '{}'", dir.display());
            dump_builtin_plugins(dir)?;
            std::process::exit(0);
        }

        Ok(())
    }

    pub fn check_defaults_config(opts: &CliArgs, config_options: &Options) -> std::io::Result<()> {
        let data_dir = opts.data_dir.clone().unwrap_or_else(get_default_data_dir);
        let config_dir = opts.config_dir.clone().or_else(find_default_config_dir);
        let plugin_dir = data_dir.join("plugins");
        let layout_dir = config_options
            .layout_dir
            .clone()
            .or_else(|| get_layout_dir(config_dir.clone()));
        let system_data_dir = system_data_dir();
        let config_file = opts
            .config
            .clone()
            .or_else(|| config_dir.clone().map(|p| p.join(CONFIG_NAME)));

        // according to
        // https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda
        let hyperlink_start = "\u{1b}]8;;";
        let hyperlink_mid = "\u{1b}\\";
        let hyperlink_end = "\u{1b}]8;;\u{1b}\\";

        let mut message = String::new();

        // One provenance owner: the same identity `--version` and `--build-info`
        // report, so a diagnostics dump can never disagree with the binary.
        writeln!(
            &mut message,
            "[Version]: {}",
            crate::build_info::build_info().diagnostic_line()
        )
        .unwrap();
        // A fix that is in the source but not in the installed binary looks
        // exactly like a fix that does not work. Say so here rather than let
        // the operator debug a build that never contained it. Host-only: a wasm
        // plugin has no checkout to compare itself against.
        #[cfg(not(target_family = "wasm"))]
        {
            let freshness = crate::install_freshness::current(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            );
            writeln!(
                &mut message,
                "[INSTALL FRESHNESS]: {}",
                freshness.diagnostic_line()
            )
            .unwrap();
        }
        if let Some(config_dir) = config_dir {
            writeln!(&mut message, "[CONFIG DIR]: \"{}\"", config_dir.display()).unwrap();
        } else {
            message.push_str("[CONFIG DIR]: Not Found\n");
            let mut default_config_dirs = default_config_dirs()
                .iter()
                .filter_map(|p| p.clone())
                .collect::<Vec<PathBuf>>();
            default_config_dirs.dedup();
            message.push_str(
                " On your system vc-frame looks in the following config directories by default:\n",
            );
            for dir in default_config_dirs {
                writeln!(&mut message, " \"{}\"", dir.display()).unwrap();
            }
        }
        if let Some(config_file) = config_file {
            writeln!(
                &mut message,
                "[LOOKING FOR CONFIG FILE FROM]: \"{}\"",
                config_file.display()
            )
            .unwrap();
            match Config::from_path(&config_file, None) {
                Ok(_) => message.push_str("[CONFIG FILE]: Well defined.\n"),
                Err(e) => writeln!(
                    &mut message,
                    "[CONFIG ERROR]: {}. \n By default, vc-frame loads default configuration",
                    e
                )
                .unwrap(),
            }
        } else {
            message.push_str("[CONFIG FILE]: Not Found\n");
            writeln!(
                &mut message,
                " By default vc-frame looks for a file called [{}] in the configuration directory",
                CONFIG_NAME
            )
            .unwrap();
        }
        writeln!(&mut message, "[CACHE DIR]: {}", ZELLIJ_CACHE_DIR.display()).unwrap();
        writeln!(&mut message, "[DATA DIR]: \"{}\"", data_dir.display()).unwrap();
        writeln!(&mut message, "[PLUGIN DIR]: \"{}\"", plugin_dir.display()).unwrap();
        if !cfg!(feature = "disable_automatic_asset_installation") {
            writeln!(
                &mut message,
                " Builtin, default plugins will not be loaded from disk."
            )
            .unwrap();
            writeln!(
                &mut message,
                " Create a custom layout if you require this behavior."
            )
            .unwrap();
        }
        if let Some(layout_dir) = layout_dir {
            writeln!(&mut message, "[LAYOUT DIR]: \"{}\"", layout_dir.display()).unwrap();
        } else {
            message.push_str("[LAYOUT DIR]: Not Found\n");
        }
        writeln!(
            &mut message,
            "[SYSTEM DATA DIR]: \"{}\"",
            system_data_dir.display()
        )
        .unwrap();

        writeln!(&mut message, "[ARROW SEPARATOR]: {}", ARROW_SEPARATOR).unwrap();
        message.push_str(" Is the [ARROW_SEPARATOR] displayed correctly?\n");
        message.push_str(" If not you may want to either start vc-frame with a compatible mode: 'vc-frame options --simplified-ui true'\n");
        let mut hyperlink_compat = String::new();
        hyperlink_compat.push_str(hyperlink_start);
        hyperlink_compat.push_str("https://zellij.dev/documentation/compatibility.html#the-status-bar-fonts-dont-render-correctly");
        hyperlink_compat.push_str(hyperlink_mid);
        hyperlink_compat.push_str("upstream compatibility notes");
        hyperlink_compat.push_str(hyperlink_end);
        write!(
            &mut message,
            " Or check the font that is in use:\n {}\n",
            hyperlink_compat
        )
        .unwrap();
        message.push_str("[MOUSE INTERACTION]: \n");
        message.push_str(" Can be temporarily disabled through pressing the [SHIFT] key.\n");
        message.push_str(" If that doesn't fix any issues consider disabling mouse handling in vc-frame: 'vc-frame options --disable-mouse-mode'\n");

        let default_editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .unwrap_or_else(|_| String::from("Not set, checked $EDITOR and $VISUAL"));
        writeln!(&mut message, "[DEFAULT EDITOR]: {}", default_editor).unwrap();
        writeln!(&mut message, "[FEATURES]: {:?}", FEATURES).unwrap();
        let mut hyperlink = String::new();
        hyperlink.push_str(hyperlink_start);
        hyperlink.push_str("https://www.zellij.dev/documentation/");
        hyperlink.push_str(hyperlink_mid);
        hyperlink.push_str("upstream documentation");
        hyperlink.push_str(hyperlink_end);
        writeln!(&mut message, "[DOCUMENTATION]: {}", hyperlink).unwrap();
        //printf '\e]8;;http://example.com\e\\This is a link\e]8;;\e\\\n'

        std::io::stdout().write_all(message.as_bytes())?;

        Ok(())
    }
    fn generate_completion(shell: &str) {
        let shell: Shell = match shell.to_lowercase().parse() {
            Ok(shell) => shell,
            _ => {
                eprintln!("Unsupported shell: {}", shell);
                std::process::exit(1);
            },
        };
        let mut out = std::io::stdout();
        clap_complete::generate(shell, &mut CliArgs::command(), "vc-frame", &mut out);
        // add shell dependent extra completion
        match shell {
            Shell::Bash => {
                let _ = out.write_all(BASH_EXTRA_COMPLETION);
            },
            Shell::Elvish => {},
            Shell::Fish => {
                let _ = out.write_all(FISH_EXTRA_COMPLETION);
            },
            Shell::PowerShell => {},
            Shell::Zsh => {
                let _ = out.write_all(ZSH_EXTRA_COMPLETION);
            },
            _ => {},
        };
    }

    fn generate_auto_start(shell: &str) {
        let shell: Shell = match shell.to_lowercase().parse() {
            Ok(shell) => shell,
            _ => {
                eprintln!("Unsupported shell: {}", shell);
                std::process::exit(1);
            },
        };

        let mut out = std::io::stdout();
        match shell {
            Shell::Bash => {
                let _ = out.write_all(BASH_AUTO_START_SCRIPT);
            },
            Shell::Fish => {
                let _ = out.write_all(FISH_AUTO_START_SCRIPT);
            },
            Shell::Zsh => {
                let _ = out.write_all(ZSH_AUTO_START_SCRIPT);
            },
            _ => {},
        }
    }
    /// `vc-start` creates the host with `attach --create-background <name>` and
    /// no layout. That invocation owns the embedded host contract: config
    /// `default_layout`, `layouts/host.kdl`, and a generated
    /// `layouts/vibecrafted-host.kdl` are not consulted. Guest and tool creates
    /// still pass `--layout`, `--new-session-with-layout`, or `--guest-workspace`.
    fn bare_embedded_host_create(cli_args: &CliArgs) -> bool {
        if cli_args.guest_workspace
            || cli_args.layout.is_some()
            || cli_args.layout_string.is_some()
            || cli_args.new_session_with_layout.is_some()
        {
            return false;
        }
        matches!(
            &cli_args.command,
            Some(Command::Sessions(Sessions::Attach {
                create,
                create_background,
                ..
            })) if *create || *create_background
        )
    }
    fn parse_layout_and_override_config(
        cli_config_options: Option<&Options>,
        config: Config,
        cli_args: &CliArgs,
    ) -> Result<(Option<LayoutInfo>, Config), ConfigError> {
        if Self::bare_embedded_host_create(cli_args) {
            let layout_info = Some(LayoutInfo::BuiltIn(EMBEDDED_HOST_LAYOUT.to_owned()));
            return Layout::from_default_assets(Path::new(EMBEDDED_HOST_LAYOUT), None, config)
                .map(|(_layout, config)| (layout_info, config));
        }
        // find the layout folder relative to which we'll look for our layout
        let layout_dir = cli_config_options
            .as_ref()
            .and_then(|cli_options| cli_options.layout_dir.clone())
            .or_else(|| config.options.layout_dir.clone())
            .or_else(|| get_layout_dir(cli_args.config_dir.clone()))
            .or_else(|| get_layout_dir(find_default_config_dir()))
            // Try to get an absolute path, else let the resolution code figure this out.
            .map(|d| d.canonicalize().unwrap_or(d));
        // the chosen layout can either be a path relative to the layout_dir or a name of one
        // of our assets, this distinction is made when parsing the layout - TODO: ideally, this
        // logic should not be split up and all the decisions should happen here
        let (layout_info, chosen_layout) = if let Some(ref layout_string) = cli_args.layout_string {
            (Some(LayoutInfo::Stringified(layout_string.clone())), None)
        } else if let Some(chosen_layout) = cli_args.layout.clone() {
            let layout_info = LayoutInfo::from_cli(
                &layout_dir,
                &Some(chosen_layout.clone()),
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            );
            (layout_info, Some(chosen_layout))
        } else {
            let chosen_layout = cli_config_options
                .as_ref()
                .and_then(|cli_options| cli_options.default_layout.clone())
                .or_else(|| config.options.default_layout.clone());
            let layout_info = LayoutInfo::from_config(&layout_dir, &chosen_layout);
            (layout_info, chosen_layout)
        };
        match layout_info {
            Some(LayoutInfo::Url(ref layout_url)) => {
                Layout::from_url(layout_url, config).map(|(_layout, config)| (layout_info, config))
            },
            Some(LayoutInfo::Stringified(ref raw_layout)) => {
                Layout::from_stringified_layout(raw_layout, config)
                    .map(|(_layout, config)| (layout_info, config))
            },
            _ => Layout::from_path_or_default(chosen_layout.as_ref(), layout_dir.clone(), config)
                .map(|(_layout, config)| (layout_info, config)),
        }
    }
    fn handle_setup_commands(cli_args: &CliArgs) {
        if let Some(Command::Setup(setup)) = &cli_args.command {
            setup.from_cli().map_or_else(
                |e| {
                    eprintln!("{:?}", e);
                    process::exit(1);
                },
                |_| {},
            );
        };
    }
}

fn merge_attach_command_options(
    cli_config_options: Option<Options>,
    cli_args: &CliArgs,
) -> Option<Options> {
    if let Some(Command::Sessions(Sessions::Attach { options, .. })) = cli_args.command.clone() {
        match options.clone().as_deref() {
            Some(SessionCommand::Options(options)) => match cli_config_options {
                Some(cli_config_options) => {
                    Some(cli_config_options.merge_from_cli(options.to_owned()))
                },
                None => Some(options.to_owned()),
            },
            _ => cli_config_options,
        }
    } else {
        cli_config_options
    }
}

#[cfg(test)]
mod setup_test {
    use super::Setup;
    use crate::cli::{CliArgs, CliOptions, Command, Sessions};
    use crate::data::LayoutInfo;
    use crate::input::layout::Layout;
    use crate::input::options::Options;
    use insta::assert_snapshot;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn attach_command(session_name: &str, create: bool, create_background: bool) -> Command {
        Command::Sessions(Sessions::Attach {
            session_name: Some(session_name.to_owned()),
            create,
            create_background,
            index: None,
            options: None,
            force_run_commands: false,
            token: None,
            remember: false,
            forget: false,
            ca_cert: None,
            insecure: false,
        })
    }

    struct IsolatedLayoutDir {
        root: PathBuf,
    }

    impl IsolatedLayoutDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "vc-frame-host-contract-{label}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("layouts")).expect("isolated layout dir");
            Self { root }
        }

        fn layouts(&self) -> PathBuf {
            self.root.join("layouts")
        }

        fn write_config(&self, body: &str) {
            fs::write(self.root.join("config.kdl"), body).expect("config.kdl");
        }

        fn cli(&self, command: Option<Command>) -> CliArgs {
            CliArgs {
                config: Some(self.root.join("config.kdl")),
                config_dir: Some(self.root.clone()),
                command,
                ..Default::default()
            }
        }
    }

    impl Drop for IsolatedLayoutDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn assert_embedded_host(layout_info: Option<LayoutInfo>, poison_dir: &Path) {
        let Some(info) = layout_info else {
            panic!("host create must carry the embedded layout");
        };
        match &info {
            LayoutInfo::BuiltIn(name) if name == "vibecrafted-host" => {},
            other => panic!("expected embedded vibecrafted-host, got {other:?}"),
        }
        let (_label, raw, _swap) =
            Layout::stringified_from_default_assets(Path::new("vibecrafted-host"))
                .expect("embedded host asset");
        assert!(raw.contains("frame_host true"));
        assert!(!raw.contains("host_mirror true"));
        assert!(raw.contains("tab name=\"Dashboard\""));
        assert!(!raw.contains("POISON_HOST_CHROME"));
        // The server loads BuiltIn through assets. The poison directory is the
        // layout_dir a disk lookup would have used; invalid KDL there must not
        // be opened.
        let (layout, _) =
            Layout::from_layout_info_with_config(&Some(poison_dir.to_path_buf()), &info, None)
                .expect("built-in host must ignore the layout directory");
        assert!(
            layout.session_layer.is_some(),
            "host keeps the session layer"
        );
        let tab_names: Vec<String> = layout
            .tabs()
            .into_iter()
            .filter_map(|(name, _, _)| name)
            .collect();
        assert_eq!(
            tab_names,
            vec!["Dashboard", "Active runs", "Config", "Doctor", "Projects"]
        );
    }

    #[test]
    fn default_config_with_no_cli_arguments() {
        let cli_args = CliArgs::default();
        let (config, layout_info, options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
        assert_snapshot!(format!("{:#?}", layout_info));
        assert_snapshot!(format!("{:#?}", options));
    }
    #[test]
    fn cli_arguments_override_config_options() {
        let cli_args = CliArgs {
            command: Some(Command::Options(CliOptions {
                options: Box::new(Options {
                    simplified_ui: Some(true),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        };
        let (_config, _layout_info, options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
    }
    #[test]
    fn layout_options_override_config_options() {
        let cli_args = CliArgs {
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-options.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            ..Default::default()
        };
        let (_config, layout_info, options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info doesn't have expected format");
        };
        assert_eq!(
            layout_path,
            format!(
                "{}/src/test-fixtures/layout-with-options.kdl",
                env!("CARGO_MANIFEST_DIR")
            )
        );
    }
    #[test]
    fn cli_arguments_override_layout_options() {
        let cli_args = CliArgs {
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-options.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            command: Some(Command::Options(CliOptions {
                options: Box::new(Options {
                    pane_frames: Some(true),
                    ..Default::default()
                }),
            })),
            ..Default::default()
        };
        let (_config, layout_info, options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", options));
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info doesn't have expected format");
        };
        assert_eq!(
            layout_path,
            format!(
                "{}/src/test-fixtures/layout-with-options.kdl",
                env!("CARGO_MANIFEST_DIR")
            )
        );
    }
    #[test]
    fn layout_env_vars_override_config_env_vars() {
        let cli_args = CliArgs {
            config: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/config-with-env-vars.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-env-vars.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            ..Default::default()
        };
        let (config, _layout_info, _options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_ui_config_overrides_config_ui_config() {
        let cli_args = CliArgs {
            config: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/config-with-ui-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-ui-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            ..Default::default()
        };
        let (config, _layout_info, _options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_themes_override_config_themes() {
        let cli_args = CliArgs {
            config: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/config-with-themes-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-themes-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            ..Default::default()
        };
        let (config, _layout_info, _options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn layout_keybinds_override_config_keybinds() {
        let cli_args = CliArgs {
            config: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/config-with-keybindings-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            layout: Some(PathBuf::from(format!(
                "{}/src/test-fixtures/layout-with-keybindings-config.kdl",
                env!("CARGO_MANIFEST_DIR")
            ))),
            ..Default::default()
        };
        let (config, _layout_info, _options, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        assert_snapshot!(format!("{:#?}", config));
    }
    #[test]
    fn cli_config_dir_overrides_defaults() {
        let config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("test-fixtures")
            .join("config-dirs")
            .join("layout-upside-down");
        let cli_args = CliArgs {
            config_dir: Some(config_dir.clone()),
            ..Default::default()
        };
        let (_, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info has unexpected format");
        };
        let expected = config_dir
            .join("layouts")
            .join("upside-down.kdl")
            .canonicalize()
            .unwrap();
        assert_eq!(layout_path, expected.display().to_string());
    }
    #[test]
    fn cli_config_dir_finds_custom_default() {
        let config_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("test-fixtures")
            .join("config-dirs")
            .join("custom-default-layout");
        let cli_args = CliArgs {
            config_dir: Some(config_dir.clone()),
            ..Default::default()
        };
        let (_, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info has unexpected format");
        };
        let expected = config_dir
            .join("layouts")
            .join("default.kdl")
            .canonicalize()
            .unwrap();
        assert_eq!(layout_path, expected.display().to_string());
    }

    #[test]
    fn cli_with_relative_layout_and_extension() {
        // NOTE: We assume to be in `zellij-utils` root directory. If this doesn't hold, path
        // resolution cannot work (as it actually reads path to ensure they exist).
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(cwd, PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        let cli_args = CliArgs {
            layout: Some(PathBuf::from("assets/layouts/compact.kdl")),
            ..Default::default()
        };
        let (_, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info has unexpected format: {:?}", &layout_info);
        };
        let expected = cwd.join("assets/layouts/compact.kdl");
        assert_eq!(layout_path, expected.display().to_string());
    }

    #[test]
    fn cli_with_relative_layout_and_separator() {
        // NOTE: We assume to be in `zellij-utils` root directory. If this doesn't hold, path
        // resolution cannot work (as it actually reads path to ensure they exist).
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(cwd, PathBuf::from(env!("CARGO_MANIFEST_DIR")));

        let cli_args = CliArgs {
            layout: Some(PathBuf::from("assets/layouts/compact")),
            ..Default::default()
        };
        let (_, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("layout info has unexpected format");
        };
        let expected = cwd.join("assets/layouts/compact");
        assert_eq!(layout_path, expected.display().to_string());
    }

    #[test]
    fn layout_string_cli_argument() {
        let layout_kdl = "layout {\n    pane\n    pane\n}\n".to_string();
        let cli_args = CliArgs {
            layout_string: Some(layout_kdl.clone()),
            ..Default::default()
        };
        let (_, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::Stringified(content)) = layout_info else {
            panic!(
                "layout info should be Stringified variant, got: {:#?}",
                layout_info
            );
        };
        assert_eq!(content, layout_kdl);
    }

    #[test]
    fn bare_host_create_mounts_embedded_contract_and_ignores_config_chrome() {
        let isolated = IsolatedLayoutDir::new("bare-host");
        let layouts = isolated.layouts();
        fs::write(
            layouts.join("host.kdl"),
            "POISON_HOST_CHROME { this is not a layout\n",
        )
        .unwrap();
        fs::write(
            layouts.join("vibecrafted-host.kdl"),
            "POISON_HOST_CHROME { this is not a layout\n",
        )
        .unwrap();
        fs::write(layouts.join("default.kdl"), "layout {\n    pane\n}\n").unwrap();
        isolated.write_config(&format!(
            "default_layout \"host\"\nlayout_dir \"{}\"\n",
            layouts.display()
        ));

        for (create, create_background) in [(true, true), (true, false), (false, true)] {
            let cli_args = isolated.cli(Some(attach_command("vc-host", create, create_background)));
            let (_config, layout_info, _, _, _) = Setup::from_cli_args(&cli_args)
                .unwrap_or_else(|error| panic!("host create must not read poison chrome: {error}"));
            assert_embedded_host(layout_info, &layouts);
        }
    }

    #[test]
    fn explicit_layout_on_host_create_stays_a_guest_or_tool_layout() {
        let layout = PathBuf::from(format!(
            "{}/src/test-fixtures/layout-with-options.kdl",
            env!("CARGO_MANIFEST_DIR")
        ));
        let cli_args = CliArgs {
            layout: Some(layout.clone()),
            command: Some(attach_command("tool-session", true, true)),
            ..Default::default()
        };
        let (_config, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        let Some(LayoutInfo::File(layout_path, _)) = layout_info else {
            panic!("explicit layout must stay a file, got {layout_info:?}");
        };
        assert!(layout_path.ends_with("layout-with-options.kdl"));
    }

    #[test]
    fn guest_workspace_create_does_not_mount_the_host_contract() {
        let isolated = IsolatedLayoutDir::new("guest");
        isolated.write_config("default_layout \"compact\"\n");
        let mut cli_args = isolated.cli(Some(attach_command("workspace-a", true, true)));
        cli_args.guest_workspace = true;
        let (_config, layout_info, _, _, _) = Setup::from_cli_args(&cli_args).unwrap();
        match layout_info {
            Some(LayoutInfo::BuiltIn(name)) => {
                assert_ne!(name, "vibecrafted-host");
                assert_eq!(name, "compact");
            },
            other => panic!("guest create must keep the configured layout, got {other:?}"),
        }
    }

    #[test]
    fn attach_without_create_and_plain_startup_keep_the_operator_layout() {
        let isolated = IsolatedLayoutDir::new("plain");
        isolated.write_config("// no default_layout\n");

        let (_config, layout_info, _, _, _) =
            Setup::from_cli_args(&isolated.cli(Some(attach_command("existing", false, false))))
                .unwrap();
        match layout_info {
            Some(LayoutInfo::BuiltIn(name)) => assert_eq!(name, "vibecrafted"),
            other => panic!("attach without create must stay implicit, got {other:?}"),
        }

        let (_config, layout_info, _, _, _) = Setup::from_cli_args(&isolated.cli(None)).unwrap();
        match layout_info {
            Some(LayoutInfo::BuiltIn(name)) => assert_eq!(name, "vibecrafted"),
            other => panic!("plain startup must stay implicit, got {other:?}"),
        }
    }
}
