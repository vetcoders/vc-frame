mod clinic;
mod commands;
mod run_triage_cli;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use zellij_utils::{
    cli::{CliAction, CliArgs, Command, Sessions},
    consts::{VERSION, create_config_and_cache_folders},
    data::UnblockCondition,
    envs,
    input::config::Config,
    logging::*,
    setup::Setup,
    shared::web_server_base_url_from_config,
};

/// Where the layout of an invocation lands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LayoutRoute {
    /// The invocation owns the session it is about to start, so the layout
    /// travels with the client that creates it.
    OwnSession { layout: Option<PathBuf> },
    /// A live session already owns the screen, so the layout becomes a new
    /// tab in it.
    NewTabIn(String),
}

/// Decide where the layout goes before anything else consumes `opts`.
///
/// An explicit attach/create owns its layout: reading `--layout` first would
/// turn `--layout host attach -b -c target` into a new tab in the inherited
/// Frame session and leave the requested target uncreated. `--new-session-with-layout`
/// names the session it creates just as directly, but it only ever reached the
/// layout field on invocations *without* `attach` — so
/// `--new-session-with-layout x attach -b -c target` dropped the requested file
/// and the built-in default layout took the session instead.
fn layout_route(opts: &CliArgs, inherited_session: Option<String>) -> LayoutRoute {
    if matches!(
        opts.command,
        Some(Command::Sessions(Sessions::Attach { .. }))
    ) {
        // An explicit `--layout` still wins over `--new-session-with-layout`,
        // the precedence the branch order used to give it.
        return LayoutRoute::OwnSession {
            layout: opts
                .layout
                .clone()
                .or_else(|| opts.new_session_with_layout.clone()),
        };
    }
    if opts.layout.is_none() && opts.layout_string.is_none() {
        // `--new-session-with-layout` always starts a new session, even when
        // this invocation inherited one.
        return LayoutRoute::OwnSession {
            layout: opts.new_session_with_layout.clone(),
        };
    }
    match opts.session.clone().or(inherited_session) {
        Some(session_name) => LayoutRoute::NewTabIn(session_name),
        None => LayoutRoute::OwnSession {
            layout: opts.layout.clone(),
        },
    }
}

fn main() {
    configure_logger();
    envs::normalize_vc_frame_env_aliases();
    create_config_and_cache_folders();
    let opts = CliArgs::parse();

    // Provenance is a pure read of embedded values — answer before any session,
    // config or IPC work so it stays usable on a broken install.
    if opts.build_info {
        println!("{}", zellij_utils::build_info::build_info().to_json());
        std::process::exit(0);
    }

    // The clinic diagnoses and treats the config itself, so it must answer
    // before any client, server or IPC work — a frozen config is exactly the
    // state in which the rest of the startup path is not to be trusted.
    if let Some(Command::Doctor(doctor_cli)) = &opts.command {
        std::process::exit(clinic::doctor(&opts, doctor_cli.json));
    }
    if let Some(Command::Repair(repair_cli)) = &opts.command {
        std::process::exit(clinic::repair(&opts, repair_cli));
    }
    if let Some(Command::Action(cli_action)) = &opts.command
        && let zellij_utils::cli::CliAction::DoctorRoutes { json } = cli_action.as_ref()
    {
        std::process::exit(commands::doctor_routes(opts.session.clone(), *json));
    }

    {
        let config = Config::try_from(&opts).ok();
        if let Some(Command::Action(cli_action)) = opts.command {
            commands::send_action_to_session(*cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Subscribe(subscribe_cli)) = opts.command {
            commands::subscribe_to_session(subscribe_cli, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Run {
            command,
            direction,
            cwd,
            floating,
            in_place,
            close_replaced_pane,
            name,
            close_on_exit,
            start_suspended,
            x,
            y,
            width,
            height,
            pinned,
            stacked,
            blocking,
            block_until_exit_success,
            block_until_exit_failure,
            block_until_exit,
            near_current_pane,
            borderless,
            tab_id,
        })) = opts.command
        {
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            let skip_plugin_cache = false; // N/A for this action

            // Compute the unblock condition
            let unblock_condition = if block_until_exit_success {
                Some(UnblockCondition::OnExitSuccess)
            } else if block_until_exit_failure {
                Some(UnblockCondition::OnExitFailure)
            } else if block_until_exit {
                Some(UnblockCondition::OnAnyExit)
            } else {
                None
            };

            let command_cli_action = CliAction::NewPane {
                command,
                plugin: None,
                direction,
                cwd,
                floating,
                in_place,
                close_replaced_pane,
                name,
                close_on_exit,
                start_suspended,
                configuration: None,
                skip_plugin_cache,
                x,
                y,
                width,
                height,
                pinned,
                stacked,
                blocking,
                block_until_exit_success: false,
                block_until_exit_failure: false,
                block_until_exit: false,
                unblock_condition,
                near_current_pane,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Plugin {
            url,
            floating,
            in_place,
            close_replaced_pane,
            configuration,
            skip_plugin_cache,
            x,
            y,
            width,
            height,
            pinned,
            borderless,
            tab_id,
        })) = opts.command
        {
            let cwd = None;
            let stacked = false;
            let blocking = false;
            let unblock_condition = None;
            let command_cli_action = CliAction::NewPane {
                command: vec![],
                plugin: Some(url),
                direction: None,
                cwd,
                floating,
                in_place,
                close_replaced_pane,
                name: None,
                close_on_exit: false,
                start_suspended: false,
                configuration,
                skip_plugin_cache,
                x,
                y,
                width,
                height,
                pinned,
                stacked,
                blocking,
                block_until_exit_success: false,
                block_until_exit_failure: false,
                block_until_exit: false,
                unblock_condition,
                near_current_pane: false,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::Edit {
            file,
            direction,
            line_number,
            floating,
            in_place,
            close_replaced_pane,
            cwd,
            x,
            y,
            width,
            height,
            pinned,
            near_current_pane,
            borderless,
            tab_id,
        })) = opts.command
        {
            let mut file = file;
            let cwd = cwd.or_else(|| std::env::current_dir().ok());
            if file.is_relative()
                && let Some(cwd) = cwd.as_ref()
            {
                file = cwd.join(file);
            }
            let command_cli_action = CliAction::Edit {
                file,
                direction,
                line_number,
                floating,
                in_place,
                close_replaced_pane,
                cwd,
                x,
                y,
                width,
                height,
                pinned,
                near_current_pane,
                borderless,
                tab_id,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
        if let Some(Command::Sessions(Sessions::TriageRun {
            run,
            exit_code,
            bucket,
            origin_session,
            origin_tab,
            pane_id,
            runtime_transcript,
            cwd,
            dry_run,
            transfer_lock_fd,
            settlement_revision,
            command,
        })) = opts.command
        {
            match run_triage_cli::triage_run(run_triage_cli::TriageRunParams {
                run,
                exit_code,
                bucket_verdict: bucket,
                origin_session,
                origin_tab,
                pane_id,
                runtime_transcript,
                cwd,
                dry_run,
                transfer_lock_fd,
                settlement_revision,
                command,
            }) {
                Ok(report) => {
                    println!(
                        "{} → {} (scrollback: {})",
                        report.run,
                        report.bucket.session_name(),
                        report.scrollback.display()
                    );
                    std::process::exit(0);
                },
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(2);
                },
            }
        }
        if let Some(Command::Sessions(Sessions::Pipe {
            name,
            payload,
            args,
            plugin,
            plugin_configuration,
        })) = opts.command
        {
            let command_cli_action = CliAction::Pipe {
                name,
                payload,
                args,
                plugin,
                plugin_configuration,

                force_launch_plugin: false,
                skip_plugin_cache: false,
                floating_plugin: None,
                in_place_plugin: None,
                plugin_cwd: None,
                plugin_title: None,
            };
            commands::send_action_to_session(command_cli_action, opts.session, config);
            std::process::exit(0);
        }
    }

    if let Some(Command::Sessions(Sessions::ListSessions {
        no_formatting,
        short,
        reverse,
    })) = opts.command
    {
        commands::list_sessions(no_formatting, short, reverse);
    } else if let Some(Command::Sessions(Sessions::ListAliases)) = opts.command {
        commands::list_aliases(opts);
    } else if let Some(Command::Sessions(Sessions::Watch { ref session_name })) = opts.command {
        commands::watch_session(session_name.clone(), opts);
    } else if let Some(Command::Sessions(Sessions::Visit {
        ref session_name,
        tab,
    })) = opts.command
    {
        commands::visit_session(session_name.clone(), tab, opts);
    } else if let Some(Command::Sessions(Sessions::ProjectWorkspace {
        ref session_name,
        tab,
    })) = opts.command
    {
        commands::project_workspace(session_name.clone(), tab, opts);
    } else if let Some(Command::Sessions(Sessions::KillAllSessions { yes })) = opts.command {
        commands::kill_all_sessions(yes);
    } else if let Some(Command::Sessions(Sessions::KillSession {
        ref target_session,
        yes: _,
        force,
    })) = opts.command
    {
        commands::kill_session(target_session, force);
    } else if let Some(Command::Sessions(Sessions::DeleteAllSessions { yes, force })) = opts.command
    {
        commands::delete_all_sessions(yes, force);
    } else if let Some(Command::Sessions(Sessions::DeleteSession {
        ref target_session,
        force,
    })) = opts.command
    {
        commands::delete_session(target_session, force);
    } else if let Some(path) = opts.server {
        commands::start_server(path, opts.debug);
    // Every invocation that carries a layout answers the same question first:
    // does it own the session it names, or is it handing a layout to a session
    // that already exists? `layout_route` is that single answer.
    } else if opts.layout.is_some()
        || opts.layout_string.is_some()
        || opts.new_session_with_layout.is_some()
        || matches!(
            opts.command,
            Some(Command::Sessions(Sessions::Attach { .. }))
        )
    {
        match layout_route(&opts, envs::get_session_name().ok()) {
            LayoutRoute::OwnSession { layout } => {
                let mut opts = opts.clone();
                opts.new_session_with_layout = None;
                opts.layout = layout;
                commands::start_client(opts);
            },
            LayoutRoute::NewTabIn(session_name) => {
                let config = Config::try_from(&opts).ok();
                let options = Setup::from_cli_args(&opts).ok().map(|r| r.2);
                let new_layout_cli_action = CliAction::NewTab {
                    layout: opts.layout.clone(),
                    layout_string: opts.layout_string.clone(),
                    layout_dir: options.as_ref().and_then(|o| o.layout_dir.clone()),
                    name: None,
                    cwd: options.as_ref().and_then(|o| o.default_cwd.clone()),
                    after_base: false,
                    no_focus: false,
                    initial_command: vec![],
                    initial_plugin: None,
                    close_on_exit: Default::default(),
                    start_suspended: Default::default(),
                    block_until_exit_success: false,
                    block_until_exit_failure: false,
                    block_until_exit: false,
                };
                commands::send_action_to_session(new_layout_cli_action, Some(session_name), config);
            },
        }
    } else if let Some(Command::Web(web_opts)) = &opts.command {
        if web_opts.get_start() {
            let daemonize = web_opts.daemonize;
            commands::start_web_server(
                opts.clone(),
                daemonize,
                web_opts.ip,
                web_opts.port,
                web_opts.cert.clone(),
                web_opts.key.clone(),
                web_opts.server_startup_timeout,
            );
        } else if web_opts.stop {
            match commands::stop_web_server() {
                Ok(()) => {
                    println!("Stopped web server.");
                },
                Err(e) => {
                    eprintln!("Failed to stop web server: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.status {
            let mut config_options = commands::get_config_options_from_cli_args(&opts)
                .expect("Can't find config options");
            if let Some(ip) = web_opts.ip {
                config_options.web_server_ip = Some(ip);
            }
            if let Some(port) = web_opts.port {
                config_options.web_server_port = Some(port);
            }
            let web_server_base_url = web_server_base_url_from_config(config_options);
            match commands::web_server_status(&web_server_base_url, web_opts.timeout) {
                Ok(version) => {
                    let version = version.trim();
                    println!(
                        "Web server online with version: {}. Checked: {}",
                        version, web_server_base_url
                    );
                    if version != VERSION {
                        println!();
                        println!(
                            "Note: this version differs from the current vc-frame version: {}.",
                            VERSION
                        );
                        println!("Consider stopping the server with: vc-frame web --stop");
                        println!("And then restarting it with: vc-frame web --start");
                    }
                },
                Err(_e) => {
                    println!("Web server is offline, checked: {}", web_server_base_url);
                },
            }
        } else if web_opts.create_token {
            let read_only = false;
            match commands::create_auth_token(web_opts.token_name.clone(), read_only) {
                Ok(token_and_name) => {
                    println!("Created token successfully");
                    println!();
                    println!("{}", token_and_name);
                },
                Err(e) => {
                    eprintln!("Failed to create token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.create_read_only_token {
            let read_only = true;
            match commands::create_auth_token(web_opts.token_name.clone(), read_only) {
                Ok(token_and_name) => {
                    println!("Created token successfully");
                    println!();
                    println!("{}", token_and_name);
                },
                Err(e) => {
                    eprintln!("Failed to create token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if let Some(token_name_to_revoke) = &web_opts.revoke_token {
            match commands::revoke_auth_token(token_name_to_revoke) {
                Ok(revoked) => {
                    if revoked {
                        println!("Successfully revoked token.");
                    } else {
                        eprintln!("Token by that name does not exist.");
                        std::process::exit(2)
                    }
                },
                Err(e) => {
                    eprintln!("Failed to revoke token: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.revoke_all_tokens {
            match commands::revoke_all_auth_tokens() {
                Ok(_) => {
                    println!("Successfully revoked all auth tokens");
                },
                Err(e) => {
                    eprintln!("Failed to revoke all auth tokens: {}", e);
                    std::process::exit(2)
                },
            }
        } else if web_opts.list_tokens {
            match commands::list_auth_tokens() {
                Ok(token_list) => {
                    for item in token_list {
                        println!("{}", item);
                    }
                },
                Err(e) => {
                    eprintln!("Failed to list tokens: {}", e);
                    std::process::exit(2)
                },
            }
        }
    } else {
        commands::start_client(opts);
    }
}
