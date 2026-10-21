use crate::os_input_output::ClientOsApi;
use crate::spawn_server;

use std::path::{Path, PathBuf};
use zellij_utils::{
    consts::session_layout_cache_file_name,
    data::{ConnectToSession, LayoutInfo, LayoutMetadata, WebSharing},
    envs,
    input::{cli_assets::CliAssets, config::Config, options::Options},
    ipc::{ClientAttributes, ClientToServerMsg},
    sessions::{generate_unique_session_name, resurrection_layout},
};

pub fn build_initial_connection(
    session_name: Option<String>,
    expected_session_incarnation: Option<String>,
    config: &Config,
) -> Result<Option<ConnectToSession>, &'static str> {
    // Refreshing a guarded browser URL must attach to the exact admitted
    // lifetime, including when no matching session is currently discoverable.
    if let Some(expected) = expected_session_incarnation {
        if expected.is_empty() || session_name.as_ref().is_none_or(|name| name.is_empty()) {
            return Err("Guarded browser attachment requires a complete native identity.");
        }
        return Ok(Some(ConnectToSession {
            name: session_name,
            expected_session_incarnation: Some(expected),
            ..Default::default()
        }));
    }
    let should_start_with_welcome_screen = session_name.is_none();
    let default_layout_from_config =
        LayoutInfo::from_config(&config.options.layout_dir, &config.options.default_layout);
    if should_start_with_welcome_screen {
        let Some(initial_session_name) = session_name.clone().or_else(generate_unique_session_name)
        else {
            return Err("Failed to generate unique session name, bailing.");
        };
        Ok(Some(ConnectToSession {
            name: Some(initial_session_name.clone()),
            layout: Some(LayoutInfo::BuiltIn("welcome".to_owned())),
            ..Default::default()
        }))
    } else if let Some(session_name) = session_name {
        Ok(Some(ConnectToSession {
            name: Some(session_name.clone()),
            layout: default_layout_from_config,
            ..Default::default()
        }))
    } else if default_layout_from_config.is_some() {
        Ok(Some(ConnectToSession {
            layout: default_layout_from_config,
            ..Default::default()
        }))
    } else {
        Ok(None)
    }
}

pub fn spawn_new_session(
    session_name: &str,
    mut os_input: Box<dyn ClientOsApi>,
    zellij_ipc_pipe: &Path,
) {
    let debug = false;
    envs::set_session_name(session_name.to_owned());
    os_input.update_session_name(session_name.to_owned());
    spawn_server(zellij_ipc_pipe, debug).unwrap();
}

pub fn create_first_message(
    is_read_only: bool,
    config_file_path: Option<PathBuf>,
    client_attributes: ClientAttributes,
    mut config_opts: Options,
    should_create_session: bool,
    session_name: &str,
    initial_layout: Option<LayoutInfo>,
) -> ClientToServerMsg {
    let resurrection_layout = resurrection_layout(session_name).ok().flatten();

    let layout_info = if resurrection_layout.is_some() {
        Some(LayoutInfo::File(
            session_layout_cache_file_name(session_name)
                .display()
                .to_string(),
            LayoutMetadata::default(),
        ))
    } else {
        initial_layout
    };

    config_opts.web_server = Some(true);
    config_opts.web_sharing = Some(WebSharing::On);

    let is_web_client = true;
    if is_read_only {
        // read only clients attach as watchers
        ClientToServerMsg::AttachWatcherClient {
            terminal_size: client_attributes.size,
            is_web_client,
        }
    } else if should_create_session {
        config_opts.web_server = Some(true);
        config_opts.web_sharing = Some(WebSharing::On);
        let cli_assets = CliAssets {
            config_file_path,
            config_dir: None,
            should_ignore_config: false,
            configuration_options: Some(config_opts),
            layout: layout_info,
            terminal_window_size: client_attributes.size,
            data_dir: None,
            is_debug: false,
            max_panes: None,
            force_run_layout_commands: false,
            cwd: None,
            is_resurrection: resurrection_layout.is_some(),
            client_identity: None,
        };

        ClientToServerMsg::FirstClientConnected {
            cli_assets,
            is_web_client,
        }
    } else {
        let cli_assets = CliAssets {
            config_file_path,
            config_dir: None,
            should_ignore_config: false,
            configuration_options: Some(config_opts),
            layout: None,
            terminal_window_size: client_attributes.size,
            data_dir: None,
            is_debug: false,
            max_panes: None,
            force_run_layout_commands: false,
            cwd: None,
            is_resurrection: false,
            client_identity: None,
        };
        let is_web_client = true;

        ClientToServerMsg::AttachClient {
            cli_assets,
            tab_position_to_focus: None,
            pane_to_focus: None,
            is_web_client,
            expected_session_incarnation: None,
        }
    }
}

pub fn create_ipc_pipe(session_name: &str) -> PathBuf {
    if let Err(e) = zellij_utils::sessions::validate_session_name(session_name) {
        log::error!("Invalid session name: {}", e);
        panic!("Invalid session name: {}", e);
    }
    let zellij_ipc_pipe: PathBuf = {
        let mut sock_dir = zellij_utils::consts::ZELLIJ_SOCK_DIR.clone();
        zellij_utils::consts::ensure_socket_runtime_dirs(&sock_dir).unwrap();
        sock_dir.push(session_name);
        sock_dir
    };
    crate::check_ipc_pipe_length(&zellij_ipc_pipe);
    zellij_ipc_pipe
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guarded_browser_refresh_retains_exact_native_identity_without_creation_layout() {
        let config = Config::default();
        let connection = build_initial_connection(
            Some("already-admitted".to_owned()),
            Some("admitted-incarnation".to_owned()),
            &config,
        )
        .unwrap()
        .unwrap();
        assert_eq!(connection.name.as_deref(), Some("already-admitted"));
        assert_eq!(
            connection.expected_session_incarnation.as_deref(),
            Some("admitted-incarnation")
        );
        assert!(connection.layout.is_none());
    }

    #[test]
    fn incomplete_guarded_browser_identity_cannot_become_welcome_or_default_creation() {
        for (name, incarnation) in [
            (None, "admitted-incarnation"),
            (Some(String::new()), "admitted-incarnation"),
            (Some("project".to_owned()), ""),
        ] {
            assert!(
                build_initial_connection(name, Some(incarnation.to_owned()), &Config::default())
                    .is_err()
            );
        }
        let ordinary =
            build_initial_connection(Some("ordinary".to_owned()), None, &Config::default())
                .unwrap()
                .unwrap();
        assert_eq!(ordinary.name.as_deref(), Some("ordinary"));
        assert!(ordinary.expected_session_incarnation.is_none());
    }
}
