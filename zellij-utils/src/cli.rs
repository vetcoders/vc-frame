use crate::data::{Direction, InputMode, Resize, UnblockCondition};
use crate::setup::Setup;
use crate::{
    consts::{VC_FRAME_CONFIG_DIR_ENV, VC_FRAME_CONFIG_FILE_ENV},
    input::{layout::PluginUserConfiguration, options::Options},
};
use clap::{
    Arg, ArgEnum, ArgMatches, Args, Command as ClapCommand, Error, ErrorKind, Parser, Subcommand,
};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::net::IpAddr;
use std::path::PathBuf;
use url::Url;

fn validate_session(name: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        use crate::consts::ZELLIJ_SOCK_MAX_LENGTH;

        let mut socket_path = crate::consts::ZELLIJ_SOCK_DIR.clone();
        socket_path.push(name);

        if socket_path.as_os_str().len() >= ZELLIJ_SOCK_MAX_LENGTH {
            // socket path must be less than 108 bytes
            let available_length = ZELLIJ_SOCK_MAX_LENGTH
                .saturating_sub(socket_path.as_os_str().len())
                .saturating_sub(1);

            return Err(format!(
                "session name must be less than {} characters",
                available_length
            ));
        };
    };

    Ok(name.to_owned())
}

#[derive(Parser, Default, Debug, Clone, Serialize, Deserialize)]
#[clap(
    version = crate::build_info::HUMAN_VERSION,
    name = "vc-frame",
    about = "vc-frame ⚒ (vibecrafted runtime)"
)]
pub struct CliArgs {
    /// Maximum panes on screen, caution: opening more panes will close old ones
    #[clap(long, value_parser)]
    pub max_panes: Option<usize>,

    /// Change where vc-frame looks for plugins
    #[clap(long, value_parser, overrides_with = "data_dir")]
    pub data_dir: Option<PathBuf>,

    /// Run server listening at the specified socket path
    #[clap(long, value_parser, hide = true, overrides_with = "server")]
    pub server: Option<PathBuf>,

    /// Specify name of a new session
    #[clap(long, short, overrides_with = "session", value_parser = validate_session)]
    pub session: Option<String>,

    /// Name of a predefined layout inside the layout directory or the path to a layout file
    /// if inside a session (or using the --session flag) will be added to the session as a new tab
    /// or tabs, otherwise will start a new session
    #[clap(short, long, value_parser, overrides_with = "layout")]
    pub layout: Option<PathBuf>,

    /// Raw KDL layout string to use directly (instead of a file path)
    /// if inside a session (or using the --session flag) will be added to the session as a new tab
    /// or tabs, otherwise will start a new session
    #[clap(long, value_parser, conflicts_with_all = &["layout", "new-session-with-layout"])]
    pub layout_string: Option<String>,

    /// Name of a predefined layout inside the layout directory or the path to a layout file
    /// Will always start a new session, even if inside an existing session
    #[clap(short, long, value_parser, overrides_with = "new_session_with_layout")]
    pub new_session_with_layout: Option<PathBuf>,

    /// Change where vc-frame looks for the configuration file
    #[clap(short, long, overrides_with = "config", env = VC_FRAME_CONFIG_FILE_ENV, value_parser)]
    pub config: Option<PathBuf>,

    /// Change where vc-frame looks for the configuration directory
    #[clap(long, overrides_with = "config_dir", env = VC_FRAME_CONFIG_DIR_ENV, value_parser)]
    pub config_dir: Option<PathBuf>,

    #[clap(subcommand)]
    pub command: Option<Command>,

    /// Specify emitting additional debug information
    #[clap(short, long, value_parser)]
    pub debug: bool,

    /// Print the embedded build provenance as JSON and exit
    #[clap(long, value_parser)]
    pub build_info: bool,
}

impl CliArgs {
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    pub fn parse_from<I, T>(itr: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        Self::try_parse_from(itr).unwrap_or_else(|e| e.exit())
    }

    pub fn try_parse_from<I, T>(itr: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<OsString>,
    {
        let args: Vec<OsString> = itr.into_iter().map(Into::into).collect();
        if args.iter().any(|arg| arg == "subscribe") {
            return Self::try_parse_subscribe_from(args);
        }
        if args.iter().any(|arg| arg == "web") {
            return Self::try_parse_web_from(args);
        }

        <Self as Parser>::try_parse_from(args)
    }

    fn try_parse_subscribe_from(args: Vec<OsString>) -> Result<Self, clap::Error> {
        let mut cli = CliArgs::default();
        let mut args = args.into_iter();
        let _program_name = args.next();
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            let arg = Self::os_to_string(arg, "argument")?;
            if arg == "subscribe" {
                cli.command = Some(Command::Subscribe(Self::parse_subscribe_cli(
                    args.collect(),
                )?));
                return Ok(cli);
            }

            match arg.as_str() {
                "--session" | "-s" => {
                    let session = Self::next_string_value(&mut args, "--session")?;
                    cli.session = Some(validate_session(&session).map_err(|err| {
                        Error::raw(
                            ErrorKind::ValueValidation,
                            format!("Invalid session: {err}"),
                        )
                    })?);
                },
                "--config" | "-c" => {
                    cli.config = Some(PathBuf::from(Self::next_os_value(&mut args, "--config")?));
                },
                "--config-dir" => {
                    cli.config_dir = Some(PathBuf::from(Self::next_os_value(
                        &mut args,
                        "--config-dir",
                    )?));
                },
                "--debug" | "-d" => {
                    cli.debug = true;
                },
                _ if arg.starts_with("--session=") => {
                    let session = arg.trim_start_matches("--session=");
                    cli.session = Some(validate_session(session).map_err(|err| {
                        Error::raw(
                            ErrorKind::ValueValidation,
                            format!("Invalid session: {err}"),
                        )
                    })?);
                },
                _ if arg.starts_with("--config=") => {
                    cli.config = Some(PathBuf::from(arg.trim_start_matches("--config=")));
                },
                _ if arg.starts_with("--config-dir=") => {
                    cli.config_dir = Some(PathBuf::from(arg.trim_start_matches("--config-dir=")));
                },
                _ => {
                    return Err(Error::raw(
                        ErrorKind::UnknownArgument,
                        format!("Unexpected argument before subscribe: {arg}"),
                    ));
                },
            }
        }

        Err(Error::raw(
            ErrorKind::MissingSubcommand,
            "Expected subscribe subcommand",
        ))
    }

    fn try_parse_web_from(args: Vec<OsString>) -> Result<Self, clap::Error> {
        let mut cli = CliArgs::default();
        let mut args = args.into_iter();
        let _program_name = args.next();
        let mut args = args.peekable();

        while let Some(arg) = args.next() {
            let arg = Self::os_to_string(arg, "argument")?;
            if arg == "web" {
                cli.command = Some(Command::Web(Self::parse_web_cli(args.collect())?));
                return Ok(cli);
            }

            match arg.as_str() {
                "--session" | "-s" => {
                    let session = Self::next_string_value(&mut args, "--session")?;
                    cli.session = Some(validate_session(&session).map_err(|err| {
                        Error::raw(
                            ErrorKind::ValueValidation,
                            format!("Invalid session: {err}"),
                        )
                    })?);
                },
                "--config" | "-c" => {
                    cli.config = Some(PathBuf::from(Self::next_os_value(&mut args, "--config")?));
                },
                "--config-dir" => {
                    cli.config_dir = Some(PathBuf::from(Self::next_os_value(
                        &mut args,
                        "--config-dir",
                    )?));
                },
                "--debug" | "-d" => {
                    cli.debug = true;
                },
                _ if arg.starts_with("--session=") => {
                    let session = arg.trim_start_matches("--session=");
                    cli.session = Some(validate_session(session).map_err(|err| {
                        Error::raw(
                            ErrorKind::ValueValidation,
                            format!("Invalid session: {err}"),
                        )
                    })?);
                },
                _ if arg.starts_with("--config=") => {
                    cli.config = Some(PathBuf::from(arg.trim_start_matches("--config=")));
                },
                _ if arg.starts_with("--config-dir=") => {
                    cli.config_dir = Some(PathBuf::from(arg.trim_start_matches("--config-dir=")));
                },
                _ => {
                    return Err(Error::raw(
                        ErrorKind::UnknownArgument,
                        format!("Unexpected argument before web: {arg}"),
                    ));
                },
            }
        }

        Err(Error::raw(
            ErrorKind::MissingSubcommand,
            "Expected web subcommand",
        ))
    }

    fn parse_subscribe_cli(args: Vec<OsString>) -> Result<SubscribeCli, clap::Error> {
        let mut pane_id = vec![];
        let mut scrollback = None;
        let mut format = SubscribeFormat::Raw;
        let mut ansi = false;
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.next() {
            let arg = Self::os_to_string(arg, "subscribe argument")?;
            match arg.as_str() {
                "--pane-id" | "-p" => {
                    pane_id.push(Self::next_string_value(&mut args, "--pane-id")?);
                },
                "--scrollback" => {
                    scrollback = if args
                        .peek()
                        .map(|next| !next.to_string_lossy().starts_with('-'))
                        .unwrap_or(false)
                    {
                        Some(Self::parse_usize(
                            &Self::os_to_string(args.next().unwrap(), "--scrollback")?,
                            "--scrollback",
                        )?)
                    } else {
                        Some(0)
                    };
                },
                "--format" | "-f" => {
                    format = Self::parse_subscribe_format(&Self::next_string_value(
                        &mut args, "--format",
                    )?)?;
                },
                "--ansi" => {
                    ansi = true;
                },
                _ if arg.starts_with("--pane-id=") => {
                    pane_id.push(arg.trim_start_matches("--pane-id=").to_string());
                },
                _ if arg.starts_with("--scrollback=") => {
                    scrollback = Some(Self::parse_usize(
                        arg.trim_start_matches("--scrollback="),
                        "--scrollback",
                    )?);
                },
                _ if arg.starts_with("--format=") => {
                    format = Self::parse_subscribe_format(arg.trim_start_matches("--format="))?;
                },
                _ => {
                    return Err(Error::raw(
                        ErrorKind::UnknownArgument,
                        format!("Unexpected subscribe argument: {arg}"),
                    ));
                },
            }
        }

        if pane_id.is_empty() {
            return Err(Error::raw(
                ErrorKind::MissingRequiredArgument,
                "The following required argument was not provided: pane_id",
            ));
        }

        Ok(SubscribeCli {
            pane_id,
            scrollback,
            format,
            ansi,
        })
    }

    fn parse_web_cli(args: Vec<OsString>) -> Result<WebCli, clap::Error> {
        let mut web = WebCli {
            start: false,
            stop: false,
            status: false,
            timeout: None,
            daemonize: false,
            server_startup_timeout: None,
            create_token: false,
            token_name: None,
            create_read_only_token: false,
            revoke_token: None,
            revoke_all_tokens: false,
            list_tokens: false,
            ip: None,
            port: None,
            cert: None,
            key: None,
        };
        let mut args = args.into_iter().peekable();

        while let Some(arg) = args.next() {
            let arg = Self::os_to_string(arg, "web argument")?;
            match arg.as_str() {
                "--start" => web.start = true,
                "--stop" => web.stop = true,
                "--status" => web.status = true,
                "--timeout" => {
                    web.timeout = Some(Self::parse_u64(
                        &Self::next_string_value(&mut args, "--timeout")?,
                        "--timeout",
                    )?);
                },
                "--daemonize" | "-d" => web.daemonize = true,
                "--server-startup-timeout" => {
                    web.server_startup_timeout = Some(Self::parse_u64(
                        &Self::next_string_value(&mut args, "--server-startup-timeout")?,
                        "--server-startup-timeout",
                    )?);
                },
                "--create-token" => web.create_token = true,
                "--token-name" => {
                    web.token_name = Some(Self::next_string_value(&mut args, "--token-name")?);
                },
                "--create-read-only-token" => web.create_read_only_token = true,
                "--revoke-token" => {
                    web.revoke_token = Some(Self::next_string_value(&mut args, "--revoke-token")?);
                },
                "--revoke-all-tokens" => web.revoke_all_tokens = true,
                "--list-tokens" => web.list_tokens = true,
                "--ip" => {
                    web.ip = Some(Self::parse_ip_addr(
                        &Self::next_string_value(&mut args, "--ip")?,
                        "--ip",
                    )?);
                },
                "--port" => {
                    web.port = Some(Self::parse_u16(
                        &Self::next_string_value(&mut args, "--port")?,
                        "--port",
                    )?);
                },
                "--cert" => {
                    web.cert = Some(PathBuf::from(Self::next_os_value(&mut args, "--cert")?));
                },
                "--key" => {
                    web.key = Some(PathBuf::from(Self::next_os_value(&mut args, "--key")?));
                },
                _ if arg.starts_with("--timeout=") => {
                    web.timeout = Some(Self::parse_u64(
                        arg.trim_start_matches("--timeout="),
                        "--timeout",
                    )?);
                },
                _ if arg.starts_with("--server-startup-timeout=") => {
                    web.server_startup_timeout = Some(Self::parse_u64(
                        arg.trim_start_matches("--server-startup-timeout="),
                        "--server-startup-timeout",
                    )?);
                },
                _ if arg.starts_with("--token-name=") => {
                    web.token_name = Some(arg.trim_start_matches("--token-name=").to_string());
                },
                _ if arg.starts_with("--revoke-token=") => {
                    web.revoke_token = Some(arg.trim_start_matches("--revoke-token=").to_string());
                },
                _ if arg.starts_with("--ip=") => {
                    web.ip = Some(Self::parse_ip_addr(
                        arg.trim_start_matches("--ip="),
                        "--ip",
                    )?);
                },
                _ if arg.starts_with("--port=") => {
                    web.port = Some(Self::parse_u16(
                        arg.trim_start_matches("--port="),
                        "--port",
                    )?);
                },
                _ if arg.starts_with("--cert=") => {
                    web.cert = Some(PathBuf::from(arg.trim_start_matches("--cert=")));
                },
                _ if arg.starts_with("--key=") => {
                    web.key = Some(PathBuf::from(arg.trim_start_matches("--key=")));
                },
                _ => {
                    return Err(Error::raw(
                        ErrorKind::UnknownArgument,
                        format!("Unexpected web argument: {arg}"),
                    ));
                },
            }
        }

        Self::validate_web_cli(&web)?;
        Ok(web)
    }

    fn validate_web_cli(web: &WebCli) -> Result<(), clap::Error> {
        if web.timeout.is_some() && !web.status {
            return Err(Error::raw(
                ErrorKind::MissingRequiredArgument,
                "--timeout requires --status",
            ));
        }
        if web.status && web.start {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--status conflicts with --start",
            ));
        }
        if web.status && web.stop {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--status conflicts with --stop",
            ));
        }
        if web.stop
            && (web.start
                || web.timeout.is_some()
                || web.daemonize
                || web.server_startup_timeout.is_some()
                || web.create_token
                || web.token_name.is_some()
                || web.create_read_only_token
                || web.revoke_token.is_some()
                || web.revoke_all_tokens
                || web.list_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--stop conflicts with other web options",
            ));
        }
        if web.daemonize
            && (web.stop
                || web.status
                || web.create_token
                || web.revoke_token.is_some()
                || web.revoke_all_tokens)
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--daemonize conflicts with the selected web option",
            ));
        }
        if web.create_token
            && (web.start
                || web.stop
                || web.status
                || web.timeout.is_some()
                || web.daemonize
                || web.create_read_only_token
                || web.revoke_token.is_some()
                || web.revoke_all_tokens
                || web.list_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--create-token conflicts with the selected web option",
            ));
        }
        if web.create_read_only_token
            && (web.start
                || web.stop
                || web.status
                || web.timeout.is_some()
                || web.daemonize
                || web.create_token
                || web.revoke_token.is_some()
                || web.revoke_all_tokens
                || web.list_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--create-read-only-token conflicts with the selected web option",
            ));
        }
        if web.revoke_token.is_some()
            && (web.start
                || web.stop
                || web.status
                || web.timeout.is_some()
                || web.daemonize
                || web.create_token
                || web.token_name.is_some()
                || web.create_read_only_token
                || web.revoke_all_tokens
                || web.list_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--revoke-token conflicts with other web options",
            ));
        }
        if web.revoke_all_tokens
            && (web.start
                || web.stop
                || web.status
                || web.timeout.is_some()
                || web.daemonize
                || web.create_token
                || web.token_name.is_some()
                || web.create_read_only_token
                || web.revoke_token.is_some()
                || web.list_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--revoke-all-tokens conflicts with other web options",
            ));
        }
        if web.list_tokens
            && (web.start
                || web.stop
                || web.status
                || web.timeout.is_some()
                || web.daemonize
                || web.create_token
                || web.token_name.is_some()
                || web.create_read_only_token
                || web.revoke_token.is_some()
                || web.revoke_all_tokens
                || web.ip.is_some()
                || web.port.is_some()
                || web.cert.is_some()
                || web.key.is_some())
        {
            return Err(Error::raw(
                ErrorKind::ArgumentConflict,
                "--list-tokens conflicts with other web options",
            ));
        }

        Ok(())
    }

    fn parse_subscribe_format(value: &str) -> Result<SubscribeFormat, clap::Error> {
        match value {
            "raw" => Ok(SubscribeFormat::Raw),
            "json" => Ok(SubscribeFormat::Json),
            _ => Err(Error::raw(
                ErrorKind::InvalidValue,
                format!("Invalid value for format: {value}"),
            )),
        }
    }

    fn parse_usize(value: &str, arg_name: &str) -> Result<usize, clap::Error> {
        value.parse::<usize>().map_err(|err| {
            Error::raw(
                ErrorKind::ValueValidation,
                format!("Invalid value for {arg_name}: {err}"),
            )
        })
    }

    fn parse_u64(value: &str, arg_name: &str) -> Result<u64, clap::Error> {
        value.parse::<u64>().map_err(|err| {
            Error::raw(
                ErrorKind::ValueValidation,
                format!("Invalid value for {arg_name}: {err}"),
            )
        })
    }

    fn parse_u16(value: &str, arg_name: &str) -> Result<u16, clap::Error> {
        value.parse::<u16>().map_err(|err| {
            Error::raw(
                ErrorKind::ValueValidation,
                format!("Invalid value for {arg_name}: {err}"),
            )
        })
    }

    fn parse_ip_addr(value: &str, arg_name: &str) -> Result<IpAddr, clap::Error> {
        value.parse::<IpAddr>().map_err(|err| {
            Error::raw(
                ErrorKind::ValueValidation,
                format!("Invalid value for {arg_name}: {err}"),
            )
        })
    }

    fn next_string_value<I>(
        args: &mut std::iter::Peekable<I>,
        arg_name: &str,
    ) -> Result<String, clap::Error>
    where
        I: Iterator<Item = OsString>,
    {
        Self::os_to_string(Self::next_os_value(args, arg_name)?, arg_name)
    }

    fn next_os_value<I>(
        args: &mut std::iter::Peekable<I>,
        arg_name: &str,
    ) -> Result<OsString, clap::Error>
    where
        I: Iterator<Item = OsString>,
    {
        args.next().ok_or_else(|| {
            Error::raw(
                ErrorKind::MissingRequiredArgument,
                format!("Expected value for {arg_name}"),
            )
        })
    }

    fn os_to_string(value: OsString, arg_name: &str) -> Result<String, clap::Error> {
        value.into_string().map_err(|_| {
            Error::raw(
                ErrorKind::InvalidUtf8,
                format!("Invalid UTF-8 in {arg_name}"),
            )
        })
    }

    pub fn is_setup_clean(&self) -> bool {
        if let Some(Command::Setup(setup)) = &self.command
            && setup.clean
        {
            return true;
        }
        false
    }
    pub fn options(&self) -> Option<Options> {
        if let Some(Command::Options(cli_options)) = &self.command {
            return Some(*cli_options.options.clone());
        }
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct CliOptions {
    #[clap(flatten)]
    pub options: Box<Options>,
}

#[derive(Debug, Subcommand, Clone, Serialize, Deserialize)]
pub enum Command {
    /// Change the behaviour of vc-frame
    #[clap(name = "options", value_parser)]
    Options(CliOptions),

    /// Setup vc-frame and check its configuration
    #[clap(name = "setup", value_parser)]
    Setup(Setup),

    /// Run a web server to serve terminal sessions
    #[clap(name = "web", value_parser)]
    Web(WebCli),

    /// Send actions to a specific session
    #[clap(visible_alias = "ac")]
    #[clap(subcommand)]
    Action(Box<CliAction>),

    /// Explore existing vc-frame sessions
    #[clap(flatten)]
    Sessions(Sessions),

    /// Subscribe to pane render updates (viewport and scrollback)
    #[clap(override_usage(
        "vc-frame [--session <OTHER SESSION NAME>] subscribe [OPTIONS] --pane-id..."
    ))]
    Subscribe(SubscribeCli),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeCli {
    /// Pane ID(s) to subscribe to (e.g. terminal_1, plugin_2, or bare number like 1)
    pub pane_id: Vec<String>,

    /// Include scrollback lines in initial delivery.
    /// Bare --scrollback = all scrollback, --scrollback N = last N lines.
    pub scrollback: Option<usize>,

    /// Output format
    pub format: SubscribeFormat,

    /// Preserve ANSI styling in the output
    pub ansi: bool,
}

impl clap::Args for SubscribeCli {
    fn augment_args(cmd: ClapCommand<'_>) -> ClapCommand<'_> {
        cmd.arg(
            Arg::new("pane_id")
                .short('p')
                .long("pane-id")
                .takes_value(true)
                .required(true)
                .multiple_occurrences(true)
                .help(
                    "Pane ID(s) to subscribe to (e.g. terminal_1, plugin_2, or bare number like 1)",
                ),
        )
        .arg(
            Arg::new("subscribe_scrollback")
                .long("scrollback")
                .takes_value(true)
                .help("Include scrollback lines in initial delivery"),
        )
        .arg(
            Arg::new("format")
                .short('f')
                .long("format")
                .takes_value(true)
                .default_value("raw")
                .possible_values(["raw", "json"])
                .help("Output format"),
        )
        .arg(
            Arg::new("ansi")
                .long("ansi")
                .takes_value(false)
                .help("Preserve ANSI styling in the output"),
        )
    }

    fn augment_args_for_update(cmd: ClapCommand<'_>) -> ClapCommand<'_> {
        Self::augment_args(cmd)
    }
}

impl clap::FromArgMatches for SubscribeCli {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        Self::from_arg_matches_mut(&mut matches.clone())
    }

    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, Error> {
        if matches.subcommand_name() == Some("subscribe") {
            let (_, mut subscribe_matches) = matches.remove_subcommand().ok_or_else(|| {
                Error::raw(
                    ErrorKind::MissingSubcommand,
                    "Expected subscribe subcommand matches",
                )
            })?;
            return Self::from_subscribe_matches(&mut subscribe_matches);
        }

        Self::from_subscribe_matches(matches)
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.update_from_arg_matches_mut(&mut matches.clone())
    }

    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), Error> {
        *self = Self::from_arg_matches_mut(matches)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ArgEnum)]
pub enum SubscribeFormat {
    Raw,
    Json,
}

impl SubscribeCli {
    fn from_subscribe_matches(matches: &mut ArgMatches) -> Result<Self, Error> {
        let pane_id = matches
            .remove_many::<String>("pane_id")
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default();
        if pane_id.is_empty() {
            return Err(Error::raw(
                ErrorKind::MissingRequiredArgument,
                "The following required argument was not provided: pane_id",
            ));
        }

        let scrollback = matches
            .remove_one::<String>("subscribe_scrollback")
            .map(|value| {
                value.parse::<usize>().map_err(|err| {
                    Error::raw(
                        ErrorKind::ValueValidation,
                        format!("Invalid value for scrollback: {err}"),
                    )
                })
            })
            .transpose()?;

        let format = match matches
            .remove_one::<String>("format")
            .unwrap_or_else(|| "raw".to_string())
            .as_str()
        {
            "raw" => SubscribeFormat::Raw,
            "json" => SubscribeFormat::Json,
            other => {
                return Err(Error::raw(
                    ErrorKind::ValueValidation,
                    format!("Invalid value for format: {other}"),
                ));
            },
        };

        Ok(Self {
            pane_id,
            scrollback,
            format,
            ansi: matches.is_present("ansi"),
        })
    }

    pub fn scrollback_lines(&self) -> Option<usize> {
        self.scrollback
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebCli {
    /// Start the server (default unless other arguments are specified)
    pub start: bool,

    /// Stop the server
    pub stop: bool,

    /// Get the server status
    pub status: bool,

    /// Timeout in seconds for the status check (default: 30)
    pub timeout: Option<u64>,

    /// Run the server in the background
    pub daemonize: bool,
    /// Timeout in seconds waiting for the server to start (default: 10).
    /// Only used on Windows where the daemonized server is polled via TCP.
    /// On Unix, startup signaling uses pipes and this option is ignored.
    pub server_startup_timeout: Option<u64>,
    /// Create a login token for the web interface, will only be displayed once and cannot later be
    /// retrieved. Returns the token name and the token.
    pub create_token: bool,
    /// Optional name for the token
    pub token_name: Option<String>,
    /// Create a read-only login token (can only attach to existing sessions as watcher)
    pub create_read_only_token: bool,
    /// Revoke a login token by its name
    pub revoke_token: Option<String>,
    /// Revoke all login tokens
    pub revoke_all_tokens: bool,
    /// List token names and their creation dates (cannot show actual tokens)
    pub list_tokens: bool,
    /// The ip address to listen on locally for connections (defaults to 127.0.0.1)
    pub ip: Option<IpAddr>,
    /// The port to listen on locally for connections (defaults to 8082)
    pub port: Option<u16>,
    /// The path to the SSL certificate (required if not listening on 127.0.0.1)
    pub cert: Option<PathBuf>,
    /// The path to the SSL key (required if not listening on 127.0.0.1)
    pub key: Option<PathBuf>,
}

impl clap::Args for WebCli {
    fn augment_args(cmd: ClapCommand<'_>) -> ClapCommand<'_> {
        cmd.arg(
            Arg::new("start")
                .long("start")
                .takes_value(false)
                .display_order(1)
                .help("Start the server (default unless other arguments are specified)"),
        )
        .arg(
            Arg::new("stop")
                .long("stop")
                .takes_value(false)
                .conflicts_with_all(&[
                    "start",
                    "status",
                    "timeout",
                    "daemonize",
                    "server-startup-timeout",
                    "create-token",
                    "token-name",
                    "create-read-only-token",
                    "revoke-token",
                    "revoke-all-tokens",
                    "list-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(2)
                .help("Stop the server"),
        )
        .arg(
            Arg::new("status")
                .long("status")
                .takes_value(false)
                .conflicts_with("start")
                .display_order(3)
                .help("Get the server status"),
        )
        .arg(
            Arg::new("timeout")
                .long("timeout")
                .takes_value(true)
                .requires("status")
                .display_order(4)
                .help("Timeout in seconds for the status check (default: 30)"),
        )
        .arg(
            Arg::new("daemonize")
                .short('d')
                .long("daemonize")
                .takes_value(false)
                .conflicts_with_all(&[
                    "stop",
                    "status",
                    "create-token",
                    "revoke-token",
                    "revoke-all-tokens",
                ])
                .display_order(5)
                .help("Run the server in the background"),
        )
        .arg(
            Arg::new("server-startup-timeout")
                .long("server-startup-timeout")
                .takes_value(true)
                .display_order(6)
                .help("Timeout in seconds waiting for the server to start (default: 10)"),
        )
        .arg(
            Arg::new("create-token")
                .long("create-token")
                .takes_value(false)
                .conflicts_with_all(&[
                    "start",
                    "stop",
                    "status",
                    "timeout",
                    "daemonize",
                    "revoke-token",
                    "revoke-all-tokens",
                    "list-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(7)
                .help("Create a login token for the web interface"),
        )
        .arg(
            Arg::new("token-name")
                .long("token-name")
                .takes_value(true)
                .value_name("TOKEN_NAME")
                .display_order(8)
                .help("Optional name for the token"),
        )
        .arg(
            Arg::new("create-read-only-token")
                .long("create-read-only-token")
                .takes_value(false)
                .conflicts_with_all(&[
                    "start",
                    "stop",
                    "status",
                    "timeout",
                    "daemonize",
                    "revoke-token",
                    "revoke-all-tokens",
                    "list-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(9)
                .help("Create a read-only login token"),
        )
        .arg(
            Arg::new("revoke-token")
                .long("revoke-token")
                .takes_value(true)
                .value_name("TOKEN NAME")
                .conflicts_with_all(&[
                    "start",
                    "stop",
                    "status",
                    "timeout",
                    "daemonize",
                    "create-token",
                    "token-name",
                    "create-read-only-token",
                    "revoke-all-tokens",
                    "list-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(10)
                .help("Revoke a login token by its name"),
        )
        .arg(
            Arg::new("revoke-all-tokens")
                .long("revoke-all-tokens")
                .takes_value(false)
                .conflicts_with_all(&[
                    "start",
                    "stop",
                    "status",
                    "timeout",
                    "daemonize",
                    "create-token",
                    "token-name",
                    "create-read-only-token",
                    "revoke-token",
                    "list-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(11)
                .help("Revoke all login tokens"),
        )
        .arg(
            Arg::new("list-tokens")
                .long("list-tokens")
                .takes_value(false)
                .conflicts_with_all(&[
                    "start",
                    "stop",
                    "status",
                    "timeout",
                    "daemonize",
                    "create-token",
                    "token-name",
                    "create-read-only-token",
                    "revoke-token",
                    "revoke-all-tokens",
                    "ip",
                    "port",
                    "cert",
                    "key",
                ])
                .display_order(12)
                .help("List token names and their creation dates"),
        )
        .arg(
            Arg::new("ip")
                .long("ip")
                .takes_value(true)
                .conflicts_with_all(&["stop", "create-token", "revoke-token", "revoke-all-tokens"])
                .display_order(13)
                .help("The ip address to listen on locally for connections"),
        )
        .arg(
            Arg::new("port")
                .long("port")
                .takes_value(true)
                .conflicts_with_all(&["stop", "create-token", "revoke-token", "revoke-all-tokens"])
                .display_order(14)
                .help("The port to listen on locally for connections"),
        )
        .arg(
            Arg::new("cert")
                .long("cert")
                .takes_value(true)
                .conflicts_with_all(&[
                    "stop",
                    "status",
                    "create-token",
                    "revoke-token",
                    "revoke-all-tokens",
                ])
                .display_order(15)
                .help("The path to the SSL certificate"),
        )
        .arg(
            Arg::new("key")
                .long("key")
                .takes_value(true)
                .conflicts_with_all(&[
                    "stop",
                    "status",
                    "create-token",
                    "revoke-token",
                    "revoke-all-tokens",
                ])
                .display_order(16)
                .help("The path to the SSL key"),
        )
    }

    fn augment_args_for_update(cmd: ClapCommand<'_>) -> ClapCommand<'_> {
        Self::augment_args(cmd)
    }
}

impl clap::FromArgMatches for WebCli {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        Self::from_arg_matches_mut(&mut matches.clone())
    }

    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, Error> {
        if matches.subcommand_name() == Some("web") {
            let (_, mut web_matches) = matches.remove_subcommand().ok_or_else(|| {
                Error::raw(
                    ErrorKind::MissingSubcommand,
                    "Expected web subcommand matches",
                )
            })?;
            return Self::from_web_matches(&mut web_matches);
        }

        Self::from_web_matches(matches)
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.update_from_arg_matches_mut(&mut matches.clone())
    }

    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), Error> {
        *self = Self::from_arg_matches_mut(matches)?;
        Ok(())
    }
}

impl WebCli {
    fn from_web_matches(matches: &mut ArgMatches) -> Result<Self, Error> {
        let timeout = matches
            .remove_one::<String>("timeout")
            .map(|value| CliArgs::parse_u64(&value, "--timeout"))
            .transpose()?;
        let server_startup_timeout = matches
            .remove_one::<String>("server-startup-timeout")
            .map(|value| CliArgs::parse_u64(&value, "--server-startup-timeout"))
            .transpose()?;
        let ip = matches
            .remove_one::<String>("ip")
            .map(|value| CliArgs::parse_ip_addr(&value, "--ip"))
            .transpose()?;
        let port = matches
            .remove_one::<String>("port")
            .map(|value| CliArgs::parse_u16(&value, "--port"))
            .transpose()?;

        let web = Self {
            start: matches.is_present("start"),
            stop: matches.is_present("stop"),
            status: matches.is_present("status"),
            timeout,
            daemonize: matches.is_present("daemonize"),
            server_startup_timeout,
            create_token: matches.is_present("create-token"),
            token_name: matches.remove_one::<String>("token-name"),
            create_read_only_token: matches.is_present("create-read-only-token"),
            revoke_token: matches.remove_one::<String>("revoke-token"),
            revoke_all_tokens: matches.is_present("revoke-all-tokens"),
            list_tokens: matches.is_present("list-tokens"),
            ip,
            port,
            cert: matches.remove_one::<String>("cert").map(PathBuf::from),
            key: matches.remove_one::<String>("key").map(PathBuf::from),
        };
        CliArgs::validate_web_cli(&web)?;
        Ok(web)
    }

    pub fn get_start(&self) -> bool {
        self.start
            || !(self.stop
                || self.status
                || self.create_token
                || self.create_read_only_token
                || self.revoke_token.is_some()
                || self.revoke_all_tokens
                || self.list_tokens)
    }
}

#[derive(Debug, Subcommand, Clone, Serialize, Deserialize)]
pub enum SessionCommand {
    /// Change the behaviour of vc-frame
    #[clap(name = "options")]
    Options(Options),
}

#[derive(Debug, Subcommand, Clone, Serialize, Deserialize)]
pub enum Sessions {
    /// List active sessions
    #[clap(visible_alias = "ls")]
    ListSessions {
        /// Do not add colors and formatting to the list (useful for parsing)
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        no_formatting: bool,

        /// Print just the session name
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        short: bool,

        /// List the sessions in reverse order (default is ascending order)
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        reverse: bool,
    },
    /// List existing plugin aliases
    #[clap(visible_alias = "la")]
    ListAliases,
    /// Attach to a session
    #[clap(visible_alias = "a")]
    Attach {
        /// Name of the session to attach to.
        #[clap(value_parser)]
        session_name: Option<String>,

        /// Create a session if one does not exist.
        #[clap(short, long, value_parser)]
        create: bool,

        /// Create a detached session in the background if one does not exist
        #[clap(short('b'), long, value_parser)]
        create_background: bool,

        /// Number of the session index in the active sessions ordered creation date.
        #[clap(long, value_parser)]
        index: Option<usize>,

        /// Change the behaviour of vc-frame
        #[clap(subcommand, name = "options")]
        options: Option<Box<SessionCommand>>,

        /// If resurrecting a dead session, immediately run all its commands on startup
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        force_run_commands: bool,

        /// Authentication token for remote sessions
        #[clap(short('t'), long, value_parser)]
        token: Option<String>,

        /// Save session for automatic re-authentication (4 weeks)
        #[clap(short('r'), long, value_parser)]
        remember: bool,

        /// Delete saved session before connecting
        #[clap(long, value_parser)]
        forget: bool,

        /// Path to a custom CA certificate (PEM format) for verifying the remote server
        #[clap(long, value_name = "FILE", value_parser)]
        ca_cert: Option<PathBuf>,

        /// Skip TLS certificate validation (DANGEROUS — development only)
        #[clap(long, value_parser)]
        insecure: bool,
    },

    /// Watch a session (read-only)
    #[clap(visible_alias = "w")]
    Watch {
        /// Name of the session to watch
        #[clap(value_parser)]
        session_name: Option<String>,
    },

    /// Kill a specific session
    #[clap(visible_alias = "k")]
    KillSession {
        /// Name of target session
        #[clap(value_parser)]
        target_session: Option<String>,
    },

    /// Delete a specific session
    #[clap(visible_alias = "d")]
    DeleteSession {
        /// Name of target session
        #[clap(value_parser)]
        target_session: Option<String>,
        /// Kill the session if it's running before deleting it
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        force: bool,
    },

    /// Kill all sessions
    #[clap(visible_alias = "ka")]
    KillAllSessions {
        /// Automatic yes to prompts
        #[clap(short, long, value_parser)]
        yes: bool,
    },

    /// Delete all sessions
    #[clap(visible_alias = "da")]
    DeleteAllSessions {
        /// Automatic yes to prompts
        #[clap(short, long, value_parser)]
        yes: bool,
        /// Kill the sessions if they're running before deleting them
        #[clap(short, long, value_parser, takes_value(false), default_value("false"))]
        force: bool,
    },

    /// Run a command in a new pane
    /// Returns: Created pane ID (format: terminal_<id>)
    #[clap(visible_alias = "r")]
    Run {
        /// Command to run
        #[clap(last(true), required(true))]
        command: Vec<String>,

        /// Direction to open the new pane in
        #[clap(short, long, value_parser, conflicts_with("floating"))]
        direction: Option<Direction>,

        /// Change the working directory of the new pane
        #[clap(long, value_parser)]
        cwd: Option<PathBuf>,

        /// Open the new pane in floating mode
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        floating: bool,

        /// Open the new pane in place of the current pane, temporarily suspending it
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("floating"),
            conflicts_with("direction")
        )]
        in_place: bool,

        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,

        /// Name of the new pane
        #[clap(short, long, value_parser)]
        name: Option<String>,

        /// Close the pane immediately when its command exits
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        close_on_exit: bool,

        /// Start the command suspended, only running after you first presses ENTER
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        start_suspended: bool,

        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long, requires("floating"))]
        pinned: Option<bool>,
        #[clap(
            long,
            conflicts_with("floating"),
            conflicts_with("direction"),
            value_parser,
            default_value("false"),
            takes_value(false)
        )]
        stacked: bool,
        /// Block until the command has finished and its pane has been closed
        #[clap(long, value_parser, default_value("false"), takes_value(false))]
        blocking: bool,

        /// Block until the command exits successfully (exit status 0) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-failure"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_success: bool,

        /// Block until the command exits with failure (non-zero exit status) OR its pane has been
        /// closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_failure: bool,

        /// Block until the command exits (regardless of exit status) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit-failure")
        )]
        block_until_exit: bool,
        /// if set, will open the pane near the current one rather than following the user's focus
        #[clap(long)]
        near_current_pane: bool,
        /// start this pane without a border (warning: will make it impossible to move with the
        /// mouse)
        #[clap(short, long, value_parser)]
        borderless: Option<bool>,
        /// Target a specific tab by ID
        #[clap(
            long,
            value_parser,
            conflicts_with("near-current-pane"),
            conflicts_with("in-place")
        )]
        tab_id: Option<usize>,
    },
    /// Load a plugin
    /// Returns: Created pane ID (format: plugin_<id>)
    #[clap(visible_alias = "p")]
    Plugin {
        /// Plugin URL, can either start with http(s), file: or zellij:
        #[clap(last(true), required(true))]
        url: String,

        /// Plugin configuration
        #[clap(short, long, value_parser)]
        configuration: Option<PluginUserConfiguration>,

        /// Open the new pane in floating mode
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        floating: bool,

        /// Open the new pane in place of the current pane, temporarily suspending it
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("floating")
        )]
        in_place: bool,

        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,

        /// Skip the memory and HD cache and force recompile of the plugin (good for development)
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        skip_plugin_cache: bool,
        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long, requires("floating"))]
        pinned: Option<bool>,
        /// start this pane without a border (warning: will make it impossible to move with the
        /// mouse)
        #[clap(short, long, value_parser)]
        borderless: Option<bool>,
        /// Target a specific tab by ID
        #[clap(long, value_parser, conflicts_with("in-place"))]
        tab_id: Option<usize>,
    },
    /// Edit file with default $EDITOR / $VISUAL
    /// Returns: Created pane ID (format: terminal_<id>)
    #[clap(visible_alias = "e")]
    Edit {
        file: PathBuf,

        /// Open the file in the specified line number
        #[clap(short, long, value_parser)]
        line_number: Option<usize>,

        /// Direction to open the new pane in
        #[clap(short, long, value_parser, conflicts_with("floating"))]
        direction: Option<Direction>,

        /// Open the new pane in place of the current pane, temporarily suspending it
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("floating"),
            conflicts_with("direction")
        )]
        in_place: bool,

        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,

        /// Open the new pane in floating mode
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        floating: bool,

        /// Change the working directory of the editor
        #[clap(long, value_parser)]
        cwd: Option<PathBuf>,
        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long, requires("floating"))]
        pinned: Option<bool>,
        /// if set, will open the pane near the current one rather than following the user's focus
        #[clap(long)]
        near_current_pane: bool,
        /// start this pane without a border (warning: will make it impossible to move with the
        /// mouse)
        #[clap(short, long, value_parser)]
        borderless: Option<bool>,
        /// Target a specific tab by ID
        #[clap(
            long,
            value_parser,
            conflicts_with("near-current-pane"),
            conflicts_with("in-place")
        )]
        tab_id: Option<usize>,
    },
    ConvertConfig {
        old_config_file: PathBuf,
    },
    ConvertLayout {
        old_layout_file: PathBuf,
    },
    ConvertTheme {
        old_theme_file: PathBuf,
    },
    /// Send data to one or more plugins, launch them if they are not running.
    #[clap(override_usage(
r#"
vc-frame pipe [OPTIONS] [--] <PAYLOAD>

* Send data to a specific plugin:

vc-frame pipe --plugin file:/path/to/my/plugin.wasm --name my_pipe_name -- my_arbitrary_data

* To all running plugins (that are listening):

vc-frame pipe --name my_pipe_name -- my_arbitrary_data

* Pipe data into this command's STDIN and get output from the plugin on this command's STDOUT

tail -f /tmp/my-live-logfile | vc-frame pipe --name logs --plugin https://example.com/my-plugin.wasm | wc -l
"#))]
    Pipe {
        /// The name of the pipe
        #[clap(short, long, value_parser, display_order(1))]
        name: Option<String>,
        /// The data to send down this pipe (if blank, will listen to STDIN)
        payload: Option<String>,

        #[clap(short, long, value_parser, display_order(2))]
        /// The args of the pipe
        args: Option<PluginUserConfiguration>, // TODO: we might want to not re-use
        // PluginUserConfiguration
        /// The plugin url (eg. file:/tmp/my-plugin.wasm) to direct this pipe to, if not specified,
        /// will be sent to all plugins, if specified and is not running, the plugin will be launched
        #[clap(short, long, value_parser, display_order(3))]
        plugin: Option<String>,
        /// The plugin configuration (note: the same plugin with different configuration is
        /// considered a different plugin for the purposes of determining the pipe destination)
        #[clap(short('c'), long, value_parser, display_order(4))]
        plugin_configuration: Option<PluginUserConfiguration>,
    },

    /// Transfer a finished run's tab into its status bucket session
    ///
    /// Captures the pane's scrollback and the run metadata to durable storage,
    /// recreates a viewer/rerun tab in "Finalized runs", "Failed runs" or
    /// "Needs attention", and only then closes the origin tab. A PTY cannot
    /// migrate between sessions, so this recreates rather than moves.
    #[clap(name = "triage-run")]
    TriageRun {
        /// Run identifier — names the capture directory and the bucket tab
        #[clap(long, value_parser)]
        run: String,

        /// Exit code of the finished run; picks the bucket when --bucket is absent
        #[clap(long, value_parser)]
        exit_code: i32,

        /// Bucket verdict from the caller: finalized, failed or needs-attention.
        ///
        /// The drawer is a conjunction of signals — exit code, report state, log
        /// volume — and only the caller can see all of them. When given, this
        /// overrides the exit-code derivation. When absent, the exit code decides
        /// and a non-zero exit lands in "Needs attention" rather than claiming a
        /// clean failure it cannot verify.
        #[clap(long, value_parser = crate::run_triage::parse_bucket_verdict)]
        bucket: Option<crate::run_triage::BucketKind>,

        /// Session the run lived in (defaults to the current session)
        #[clap(long, value_parser)]
        origin_session: Option<String>,

        /// Tab to close once the capture is durable (defaults to the run id)
        #[clap(long, value_parser)]
        origin_tab: Option<String>,

        /// Pane to dump, eg. terminal_3. Defaults to the focused pane.
        #[clap(long, value_parser)]
        pane_id: Option<String>,

        /// Working directory recorded for rerun
        #[clap(long, value_parser)]
        cwd: Option<PathBuf>,

        /// Report what would happen without touching any session
        #[clap(long, value_parser, default_value("false"), takes_value(false))]
        dry_run: bool,

        /// The original command line, preserved so the bucket tab can rerun it
        #[clap(last(true), value_parser)]
        command: Vec<String>,
    },
}

#[derive(Debug, Subcommand, Clone, Serialize, Deserialize)]
pub enum CliAction {
    /// Write bytes to the terminal.
    Write {
        bytes: Vec<u8>,
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Write characters to the terminal.
    WriteChars {
        chars: String,
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Paste text to the terminal (using bracketed paste mode).
    Paste {
        chars: String,
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Send one or more keys to the terminal (e.g., "Ctrl a", "F1", "Alt Shift b")
    SendKeys {
        /// Keys to send as space-separated strings
        #[clap(value_parser, required = true)]
        keys: Vec<String>,

        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// [increase|decrease] the focused panes area at the [left|down|up|right] border.
    Resize {
        resize: Resize,
        direction: Option<Direction>,
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Change focus to the next pane
    FocusNextPane,
    /// Change focus to the previous pane
    FocusPreviousPane,
    /// Focus a specific pane by its ID
    FocusPaneId {
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3
        pane_id: String,
    },
    /// Move the focused pane in the specified direction. [right|left|up|down]
    MoveFocus {
        direction: Direction,
    },
    /// Move focus to the pane or tab (if on screen edge) in the specified direction
    /// [right|left|up|down]
    MoveFocusOrTab {
        direction: Direction,
    },
    /// Change the location of the focused pane in the specified direction or rotate forwrads
    /// [right|left|up|down]
    MovePane {
        direction: Option<Direction>,
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Rotate the location of the previous pane backwards
    MovePaneBackwards {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Clear all buffers for a focused pane
    Clear {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Dumps the viewport and optionally scrollback of a pane to a file or STDOUT
    DumpScreen {
        /// File path to dump the pane content to. If omitted, prints to STDOUT.
        #[clap(long, value_parser)]
        path: Option<PathBuf>,

        /// Dump the pane with full scrollback
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        full: bool,

        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3). If not specified, dumps the focused pane.
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,

        /// Preserve ANSI styling in the dump output
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        ansi: bool,
    },
    /// Dump current layout to stdout
    DumpLayout,
    /// Save the current session state to disk immediately
    SaveSession,
    /// Open the pane scrollback in your default editor
    EditScrollback {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,

        /// Preserve ANSI styling in the scrollback dump
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        ansi: bool,
    },
    /// Scroll up in the focused pane
    ScrollUp {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll down in focus pane.
    ScrollDown {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll down to bottom in focus pane.
    ScrollToBottom {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll up to top in focus pane.
    ScrollToTop {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll up one page in focus pane.
    PageScrollUp {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll down one page in focus pane.
    PageScrollDown {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll up half page in focus pane.
    HalfPageScrollUp {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Scroll down half page in focus pane.
    HalfPageScrollDown {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Toggle between fullscreen focus pane and normal layout.
    ToggleFullscreen {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Toggle frames around panes in the UI
    TogglePaneFrames,
    /// Toggle between sending text commands to all panes on the current tab and normal mode.
    ToggleActiveSyncTab {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Open a new pane in the specified direction [right|down]
    /// If no direction is specified, will try to use the biggest available space.
    /// Returns: Created pane ID (format: terminal_<id> or plugin_<id>)
    NewPane {
        /// Direction to open the new pane in
        #[clap(short, long, value_parser, conflicts_with("floating"))]
        direction: Option<Direction>,

        #[clap(last(true))]
        command: Vec<String>,

        #[clap(short, long, conflicts_with("command"), conflicts_with("direction"))]
        plugin: Option<String>,

        /// Change the working directory of the new pane
        #[clap(long, value_parser)]
        cwd: Option<PathBuf>,

        /// Open the new pane in floating mode
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        floating: bool,

        /// Open the new pane in place of the current pane, temporarily suspending it
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("floating"),
            conflicts_with("direction")
        )]
        in_place: bool,

        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,

        /// Name of the new pane
        #[clap(short, long, value_parser)]
        name: Option<String>,

        /// Close the pane immediately when its command exits
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("command")
        )]
        close_on_exit: bool,
        /// Start the command suspended, only running it after the you first press ENTER
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("command")
        )]
        start_suspended: bool,
        #[clap(long, value_parser)]
        configuration: Option<PluginUserConfiguration>,
        #[clap(long, value_parser)]
        skip_plugin_cache: bool,
        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long, requires("floating"))]
        pinned: Option<bool>,
        #[clap(
            long,
            conflicts_with("floating"),
            conflicts_with("direction"),
            value_parser,
            default_value("false"),
            takes_value(false)
        )]
        stacked: bool,
        /// Block until the command has finished and its pane has been closed
        #[clap(short, long)]
        blocking: bool,

        /// Block until the command exits successfully (exit status 0) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-failure"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_success: bool,

        /// Block until the command exits with failure (non-zero exit status) OR its pane has been
        /// closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_failure: bool,

        /// Block until the command exits (regardless of exit status) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("blocking"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit-failure")
        )]
        block_until_exit: bool,

        #[clap(skip)]
        unblock_condition: Option<UnblockCondition>,

        /// if set, will open the pane near the current one rather than following the user's focus
        #[clap(long)]
        near_current_pane: bool,
        /// start this pane without a border (warning: will make it impossible to move with the
        /// mouse)
        #[clap(long, value_parser)]
        borderless: Option<bool>,
        /// Target a specific tab by ID
        #[clap(
            long,
            value_parser,
            conflicts_with("near-current-pane"),
            conflicts_with("in-place")
        )]
        tab_id: Option<usize>,
    },
    /// Open the specified file in a new vc-frame pane with your default EDITOR
    /// Returns: Created pane ID (format: terminal_<id>)
    Edit {
        file: PathBuf,

        /// Direction to open the new pane in
        #[clap(short, long, value_parser, conflicts_with("floating"))]
        direction: Option<Direction>,

        /// Open the file in the specified line number
        #[clap(short, long, value_parser)]
        line_number: Option<usize>,

        /// Open the new pane in floating mode
        #[clap(short, long, value_parser, default_value("false"), takes_value(false))]
        floating: bool,

        /// Open the new pane in place of the current pane, temporarily suspending it
        #[clap(
            short,
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            conflicts_with("floating"),
            conflicts_with("direction")
        )]
        in_place: bool,

        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,

        /// Change the working directory of the editor
        #[clap(long, value_parser)]
        cwd: Option<PathBuf>,
        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long, requires("floating"))]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long, requires("floating"))]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long, requires("floating"))]
        pinned: Option<bool>,
        /// if set, will open the pane near the current one rather than following the user's focus
        #[clap(long)]
        near_current_pane: bool,
        /// start this pane without a border (warning: will make it impossible to move with the
        /// mouse)
        #[clap(short, long, value_parser)]
        borderless: Option<bool>,
        /// Target a specific tab by ID
        #[clap(
            long,
            value_parser,
            conflicts_with("near-current-pane"),
            conflicts_with("in-place")
        )]
        tab_id: Option<usize>,
    },
    /// Switch input mode of all connected clients [locked|pane|tab|resize|move|search|session]
    SwitchMode {
        input_mode: InputMode,
    },
    /// Embed focused pane if floating or float focused pane if embedded
    TogglePaneEmbedOrFloating {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Toggle the visibility of all floating panes in the current Tab, open one if none exist
    ToggleFloatingPanes {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Show all floating panes in the specified tab (or active tab if tab_id is not provided).
    ///
    /// Returns exit code 0 if state was changed, 2 if already visible, 1 if tab not found.
    ShowFloatingPanes {
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Hide all floating panes in the specified tab (or active tab if tab_id is not provided).
    ///
    /// Returns exit code 0 if state was changed, 2 if already hidden, 1 if tab not found.
    HideFloatingPanes {
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Check if floating panes are visible in the specified tab (or active tab).
    ///
    /// Prints "true" to stdout and exits 0 if visible.
    /// Prints "false" to stdout and exits 1 if not visible.
    AreFloatingPanesVisible {
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Close the focused pane.
    ClosePane {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Renames the focused pane
    RenamePane {
        name: String,
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Remove a previously set pane name
    UndoRenamePane {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Go to the next tab.
    GoToNextTab,
    /// Go to the previous tab.
    GoToPreviousTab,
    /// Close the current tab.
    CloseTab {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Go to tab with index [index]
    GoToTab {
        index: u32,
    },
    /// Go to tab with name [name]
    ///
    /// Returns: When --create is used and tab is created, outputs the tab ID as a single number
    GoToTabName {
        name: String,
        /// Create a tab if one does not exist.
        #[clap(short, long, value_parser)]
        create: bool,
    },
    /// Renames the focused pane
    RenameTab {
        name: String,
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Remove a previously set tab name
    UndoRenameTab {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Go to tab with stable ID
    GoToTabById {
        id: u64,
    },
    /// Close tab with stable ID
    CloseTabById {
        id: u64,
    },
    /// Rename tab by stable ID
    RenameTabById {
        id: u64,
        name: String,
    },
    /// Create a new tab, optionally with a specified tab layout and name
    ///
    /// Returns: The created tab's ID as a single number on stdout
    NewTab {
        /// Layout to use for the new tab
        #[clap(short, long, value_parser, conflicts_with = "layout-string")]
        layout: Option<PathBuf>,

        /// Raw KDL layout string to use directly (instead of a layout file path)
        #[clap(long, value_parser, conflicts_with = "layout")]
        layout_string: Option<String>,

        /// Default folder to look for layouts
        #[clap(long, value_parser, requires("layout"))]
        layout_dir: Option<PathBuf>,

        /// Name of the new tab
        #[clap(short, long, value_parser)]
        name: Option<String>,

        /// Change the working directory of the new tab
        #[clap(short, long, value_parser)]
        cwd: Option<PathBuf>,

        /// Insert the new tab directly right of the base (first) tab instead of
        /// appending it at the end of the tab bar
        #[clap(long, value_parser, default_value("false"), takes_value(false))]
        after_base: bool,

        /// Optional initial command to run in the new tab
        #[clap(
            value_parser,
            conflicts_with("initial-plugin"),
            multiple_values(true),
            takes_value(true),
            last(true)
        )]
        initial_command: Vec<String>,

        /// Initial plugin to load in the new tab
        #[clap(long, value_parser, conflicts_with("initial-command"))]
        initial_plugin: Option<String>,

        /// Close the pane immediately when its command exits
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("initial-command")
        )]
        close_on_exit: bool,

        /// Start the command suspended, only running it after you first press ENTER
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("initial-command")
        )]
        start_suspended: bool,

        /// Block until the command exits successfully (exit status 0) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("initial-command"),
            conflicts_with("block-until-exit-failure"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_success: bool,

        /// Block until the command exits with failure (non-zero exit status) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("initial-command"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit")
        )]
        block_until_exit_failure: bool,

        /// Block until the command exits (regardless of exit status) OR its pane has been closed
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("initial-command"),
            conflicts_with("block-until-exit-success"),
            conflicts_with("block-until-exit-failure")
        )]
        block_until_exit: bool,
    },
    /// Move the focused tab in the specified direction. [right|left]
    MoveTab {
        direction: Direction,
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    PreviousSwapLayout {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    NextSwapLayout {
        /// Target a specific tab by ID
        #[clap(short, long, value_parser)]
        tab_id: Option<usize>,
    },
    /// Override the layout of the active tab
    OverrideLayout {
        /// Path to the layout file
        #[clap(
            value_parser,
            required_unless_present = "layout-string",
            conflicts_with = "layout-string"
        )]
        layout: Option<PathBuf>,

        /// Raw KDL layout string to use directly (instead of a layout file path)
        #[clap(long, value_parser, conflicts_with = "layout")]
        layout_string: Option<String>,

        /// Default folder to look for layouts
        #[clap(long, value_parser)]
        layout_dir: Option<PathBuf>,

        /// Retain existing terminal panes that do not fit in the layout (default: false)
        #[clap(long, value_parser, takes_value(false), default_value("false"))]
        retain_existing_terminal_panes: bool,

        /// Retain existing plugin panes that do not fit with the layout default: false)
        #[clap(long, value_parser, takes_value(false), default_value("false"))]
        retain_existing_plugin_panes: bool,

        /// Only apply the layout to the active tab (uses just the first layout tab if it has
        /// multiple)
        #[clap(long, value_parser, takes_value(false), default_value("false"))]
        apply_only_to_active_tab: bool,
    },
    /// Query all tab names
    QueryTabNames,
    StartOrReloadPlugin {
        url: String,
        #[clap(short, long, value_parser)]
        configuration: Option<PluginUserConfiguration>,
    },
    /// Returns: Plugin pane ID (format: plugin_<id>) when creating or focusing plugin
    LaunchOrFocusPlugin {
        #[clap(short, long, value_parser)]
        floating: bool,
        #[clap(short, long, value_parser)]
        in_place: bool,
        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,
        #[clap(short, long, value_parser)]
        move_to_focused_tab: bool,
        url: String,
        #[clap(short, long, value_parser)]
        configuration: Option<PluginUserConfiguration>,
        #[clap(short, long, value_parser)]
        skip_plugin_cache: bool,
        /// Target a specific tab by ID
        #[clap(long, value_parser, conflicts_with("in-place"))]
        tab_id: Option<usize>,
    },
    /// Returns: Plugin pane ID (format: plugin_<id>)
    LaunchPlugin {
        #[clap(short, long, value_parser)]
        floating: bool,
        #[clap(short, long, value_parser)]
        in_place: bool,
        /// Close the replaced pane instead of suspending it (only effective with --in-place)
        #[clap(
            long,
            value_parser,
            default_value("false"),
            takes_value(false),
            requires("in-place")
        )]
        close_replaced_pane: bool,
        url: Url,
        #[clap(short, long, value_parser)]
        configuration: Option<PluginUserConfiguration>,
        #[clap(short, long, value_parser)]
        skip_plugin_cache: bool,
        /// Target a specific tab by ID
        #[clap(long, value_parser, conflicts_with("in-place"))]
        tab_id: Option<usize>,
    },
    RenameSession {
        name: String,
    },
    /// Send data to one or more plugins, launch them if they are not running.
    #[clap(override_usage(
r#"
vc-frame action pipe [OPTIONS] [--] <PAYLOAD>

* Send data to a specific plugin:

vc-frame action pipe --plugin file:/path/to/my/plugin.wasm --name my_pipe_name -- my_arbitrary_data

* To all running plugins (that are listening):

vc-frame action pipe --name my_pipe_name -- my_arbitrary_data

* Pipe data into this command's STDIN and get output from the plugin on this command's STDOUT

tail -f /tmp/my-live-logfile | vc-frame action pipe --name logs --plugin https://example.com/my-plugin.wasm | wc -l
"#))]
    Pipe {
        /// The name of the pipe
        #[clap(short, long, value_parser, display_order(1))]
        name: Option<String>,
        /// The data to send down this pipe (if blank, will listen to STDIN)
        payload: Option<String>,

        #[clap(short, long, value_parser, display_order(2))]
        /// The args of the pipe
        args: Option<PluginUserConfiguration>, // TODO: we might want to not re-use
        // PluginUserConfiguration
        /// The plugin url (eg. file:/tmp/my-plugin.wasm) to direct this pipe to, if not specified,
        /// will be sent to all plugins, if specified and is not running, the plugin will be launched
        #[clap(short, long, value_parser, display_order(3))]
        plugin: Option<String>,
        /// The plugin configuration (note: the same plugin with different configuration is
        /// considered a different plugin for the purposes of determining the pipe destination)
        #[clap(short('c'), long, value_parser, display_order(4))]
        plugin_configuration: Option<PluginUserConfiguration>,
        /// Launch a new plugin even if one is already running
        #[clap(
            short('l'),
            long,
            value_parser,
            takes_value(false),
            default_value("false"),
            display_order(5)
        )]
        force_launch_plugin: bool,
        /// If launching a new plugin, skip cache and force-compile the plugin
        #[clap(
            short('s'),
            long,
            value_parser,
            takes_value(false),
            default_value("false"),
            display_order(6)
        )]
        skip_plugin_cache: bool,
        /// If launching a plugin, should it be floating or not, defaults to floating
        #[clap(short('f'), long, value_parser, display_order(7))]
        floating_plugin: Option<bool>,
        /// If launching a plugin, launch it in-place (on top of the current pane)
        #[clap(
            short('i'),
            long,
            value_parser,
            conflicts_with("floating-plugin"),
            display_order(8)
        )]
        in_place_plugin: Option<bool>,
        /// If launching a plugin, specify its working directory
        #[clap(short('w'), long, value_parser, display_order(9))]
        plugin_cwd: Option<PathBuf>,
        /// If launching a plugin, specify its pane title
        #[clap(short('t'), long, value_parser, display_order(10))]
        plugin_title: Option<String>,
    },
    ListClients,
    /// List all panes in the current session
    ///
    /// Returns: Formatted list of panes (table or JSON) to stdout
    ListPanes {
        /// Include tab information (name, position, ID)
        #[clap(short, long, value_parser)]
        tab: bool,

        /// Include running command information
        #[clap(short, long, value_parser)]
        command: bool,

        /// Include pane state (focused, floating, exited, etc.)
        #[clap(short, long, value_parser)]
        state: bool,

        /// Include geometry (position, size)
        #[clap(short, long, value_parser)]
        geometry: bool,

        /// Include all available fields
        #[clap(short, long, value_parser)]
        all: bool,

        /// Output as JSON
        #[clap(short, long, value_parser)]
        json: bool,
    },
    /// List all tabs with their information
    ///
    /// Returns: Tab information in table or JSON format
    ListTabs {
        /// Include state information (active, fullscreen, sync, floating visibility)
        #[clap(short, long, value_parser)]
        state: bool,

        /// Include dimension information (viewport, display area)
        #[clap(short, long, value_parser)]
        dimensions: bool,

        /// Include pane counts
        #[clap(short, long, value_parser)]
        panes: bool,

        /// Include layout information (swap layout name and dirty state)
        #[clap(short, long, value_parser)]
        layout: bool,

        /// Include all available fields
        #[clap(short, long, value_parser)]
        all: bool,

        /// Output as JSON
        #[clap(short, long, value_parser)]
        json: bool,
    },
    /// Get information about the currently active tab
    ///
    /// Returns: Tab name and ID by default, or full info in JSON
    CurrentTabInfo {
        /// Output as JSON with full TabInfo
        #[clap(short, long, value_parser)]
        json: bool,
    },
    TogglePanePinned {
        /// Target a specific pane by ID (eg. terminal_1, plugin_2, or 3)
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
    },
    /// Stack pane ids
    /// Ids are a space separated list of pane ids.
    /// They should either be in the form of `terminal_<int>` (eg. terminal_1), `plugin_<int>` (eg.
    /// plugin_1) or bare integers in which case they'll be considered terminals (eg. 1 is
    /// the equivalent of terminal_1)
    ///
    /// Example: vc-frame action stack-panes -- terminal_1 plugin_2 3
    StackPanes {
        #[clap(last(true), required(true))]
        pane_ids: Vec<String>,
    },
    ChangeFloatingPaneCoordinates {
        /// The pane_id of the floating pane, eg.  terminal_1, plugin_2 or 3 (equivalent to
        /// terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: String,
        /// The x coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long)]
        x: Option<String>,
        /// The y coordinates if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(short, long)]
        y: Option<String>,
        /// The width if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long)]
        width: Option<String>,
        /// The height if the pane is floating as a bare integer (eg. 1) or percent (eg. 10%)
        #[clap(long)]
        height: Option<String>,
        /// Whether to pin a floating pane so that it is always on top
        #[clap(long)]
        pinned: Option<bool>,
        /// change this pane to be with/without a border (warning: will make it impossible to move with the
        /// mouse if without a border)
        #[clap(short, long, value_parser)]
        borderless: Option<bool>,
    },
    TogglePaneBorderless {
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: String,
    },
    SetPaneBorderless {
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3)
        #[clap(short, long, value_parser)]
        pane_id: String,
        /// Whether the pane should be borderless (flag present) or bordered (flag absent)
        #[clap(short, long, value_parser)]
        borderless: bool,
    },
    /// Detach from the current session
    Detach,
    /// Switch the theme to dark (uses configured `theme_dark`).
    SetDarkTheme,
    /// Switch the theme to light (uses configured `theme_light`).
    SetLightTheme,
    /// Toggle between dark and light themes (used configured `theme_dark` and `theme_light`)
    ToggleTheme,
    /// Switch to a different session
    SwitchSession {
        /// Name of the session to switch to
        name: String,
        /// Optional tab position to focus
        #[clap(long)]
        tab_position: Option<usize>,
        /// Optional pane ID to focus (eg. "terminal_1" for terminal pane with id 1, or "plugin_2" for plugin pane with id 2)
        #[clap(long)]
        pane_id: Option<String>,
        /// Layout to apply when switching to the session (relative paths start at layout-dir)
        #[clap(short, long, value_parser, conflicts_with = "layout-string")]
        layout: Option<PathBuf>,
        /// Raw KDL layout string to use directly
        #[clap(long, value_parser, conflicts_with = "layout")]
        layout_string: Option<String>,
        /// Default folder to look for layouts
        #[clap(long, value_parser, requires("layout"))]
        layout_dir: Option<PathBuf>,
        /// Change the working directory when switching
        #[clap(short, long, value_parser)]
        cwd: Option<PathBuf>,
    },
    /// Set the default foreground/background color of a pane
    SetPaneColor {
        /// The pane_id of the pane, eg. terminal_1, plugin_2 or 3 (equivalent to terminal_3).
        /// Defaults to $VC_FRAME_PANE_ID, then $ZELLIJ_PANE_ID, if not provided.
        #[clap(short, long, value_parser)]
        pane_id: Option<String>,
        /// Foreground color (e.g. "#00e000", "rgb:00/e0/00")
        #[clap(long, value_parser)]
        fg: Option<String>,
        /// Background color (e.g. "#001a3a", "rgb:00/1a/3a")
        #[clap(long, value_parser)]
        bg: Option<String>,
        /// Reset pane colors to terminal defaults
        #[clap(long, value_parser, conflicts_with_all(&["fg", "bg"]))]
        reset: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_subscribe(args: &[&str]) -> SubscribeCli {
        let mut full_args = vec!["vc-frame"];
        full_args.extend_from_slice(args);
        let cli = CliArgs::try_parse_from(full_args).unwrap();
        match cli.command {
            Some(Command::Subscribe(s)) => s,
            other => panic!("Expected Subscribe, got {:?}", other),
        }
    }

    #[test]
    fn subscribe_scrollback_bare_flag() {
        let s = parse_subscribe(&["subscribe", "--pane-id", "terminal_1", "--scrollback"]);
        assert_eq!(s.scrollback_lines(), Some(0));
    }

    #[test]
    fn subscribe_scrollback_with_value() {
        let s = parse_subscribe(&[
            "subscribe",
            "--pane-id",
            "terminal_1",
            "--scrollback",
            "100",
        ]);
        assert_eq!(s.scrollback_lines(), Some(100));
    }

    #[test]
    fn subscribe_scrollback_absent() {
        let s = parse_subscribe(&["subscribe", "--pane-id", "terminal_1"]);
        assert_eq!(s.scrollback_lines(), None);
    }

    #[test]
    fn subscribe_format_json() {
        let s = parse_subscribe(&["subscribe", "--pane-id", "terminal_1", "--format", "json"]);
        assert!(matches!(s.format, SubscribeFormat::Json));
    }

    #[test]
    fn subscribe_format_default_raw() {
        let s = parse_subscribe(&["subscribe", "--pane-id", "terminal_1"]);
        assert!(matches!(s.format, SubscribeFormat::Raw));
    }

    #[test]
    fn subscribe_multiple_pane_ids() {
        let s = parse_subscribe(&[
            "subscribe",
            "--pane-id",
            "terminal_1",
            "--pane-id",
            "plugin_2",
        ]);
        assert_eq!(
            s.pane_id,
            vec!["terminal_1".to_string(), "plugin_2".to_string()]
        );
    }

    #[test]
    fn subscribe_requires_pane_id() {
        let result = CliArgs::try_parse_from(["vc-frame", "subscribe"]);
        assert!(result.is_err());
    }
}
