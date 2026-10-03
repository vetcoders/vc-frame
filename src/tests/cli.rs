use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

use clap::CommandFactory;
use zellij_utils::cli::{CliArgs, Command};

use crate::{LayoutRoute, layout_route};

/// Clap's parser is stack-hungry in debug builds, so every parse in this file
/// runs on a thread with room for it.
fn parse(argv: &[&str]) -> CliArgs {
    let argv: Vec<String> = argv.iter().map(|arg| arg.to_string()).collect();
    std::thread::Builder::new()
        .name("cli-parser".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || CliArgs::try_parse_from(argv).expect("the CLI must parse this invocation"))
        .expect("failed to spawn CLI parser")
        .join()
        .expect("CLI parser panicked")
}

/// Two layouts that cannot be confused with each other or with the built-in
/// default: a green run has to name the file it was given.
const PROBE_SOLO: &str = "/tmp/vc-frame-probe-solo.kdl";
const PROBE_PAIR: &str = "/tmp/vc-frame-probe-pair.kdl";

fn own_session_layout(route: &LayoutRoute) -> Option<&PathBuf> {
    match route {
        LayoutRoute::OwnSession { layout } => layout.as_ref(),
        LayoutRoute::NewTabIn(session_name) => panic!(
            "this invocation creates its own session; it must not open a tab in {session_name}"
        ),
    }
}

#[test]
fn verify_cli() {
    std::thread::Builder::new()
        .name("verify-cli-command".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| CliArgs::command().debug_assert())
        .expect("failed to spawn CLI verifier")
        .join()
        .expect("CLI verifier panicked");
}

#[test]
fn web_cli_status_alone_works() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status"]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert!(web.timeout.is_none());
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn web_cli_status_with_timeout_works() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status", "--timeout", "5"]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert_eq!(web.timeout, Some(5));
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn web_cli_timeout_with_status_works() {
    // Test with --timeout before --status (order shouldn't matter)
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--timeout", "10", "--status"]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert_eq!(web.timeout, Some(10));
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn web_cli_timeout_without_status_fails() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--timeout", "5"]);
    assert!(args.is_err());
}

#[test]
fn web_cli_status_with_start_fails() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status", "--start"]);
    assert!(args.is_err());
}

#[test]
fn web_cli_status_with_stop_fails() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status", "--stop"]);
    assert!(args.is_err());
}

#[test]
fn web_cli_status_with_ip_works() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status", "--ip", "127.0.0.1"]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert_eq!(web.ip, Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))));
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn web_cli_status_with_port_works() {
    let args = CliArgs::try_parse_from(["vc-frame", "web", "--status", "--port", "9000"]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert_eq!(web.port, Some(9000));
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn project_workspace_cli_targets_host_session() {
    std::thread::Builder::new()
        .name("project-workspace-cli".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let args = CliArgs::try_parse_from([
                "vc-frame",
                "--session",
                "frame-host",
                "project-workspace",
                "workspace-b",
            ]);
            assert!(args.is_ok(), "{args:?}");
            let args = args.unwrap();
            assert_eq!(args.session.as_deref(), Some("frame-host"));
            assert!(matches!(
                args.command,
                Some(Command::Sessions(
                    zellij_utils::cli::Sessions::ProjectWorkspace {
                        session_name,
                        tab: None,
                    }
                )) if session_name == "workspace-b"
            ));
        })
        .expect("failed to spawn CLI parser")
        .join()
        .expect("CLI parser panicked");
}

#[test]
fn guest_workspace_flag_parses_on_attach() {
    std::thread::Builder::new()
        .name("guest-workspace-cli".to_string())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            let args = CliArgs::try_parse_from([
                "vc-frame",
                "--layout",
                "vibecrafted",
                "--guest-workspace",
                "attach",
                "-b",
                "-c",
                "workspace-b",
            ]);
            assert!(args.is_ok(), "{args:?}");
            let args = args.unwrap();
            assert!(args.guest_workspace);
            assert_eq!(
                args.layout.as_ref().and_then(|path| path.to_str()),
                Some("vibecrafted")
            );
        })
        .expect("failed to spawn CLI parser")
        .join()
        .expect("CLI parser panicked");
}

#[test]
fn web_cli_status_with_ip_and_port_works() {
    let args = CliArgs::try_parse_from([
        "vc-frame", "web", "--status", "--ip", "0.0.0.0", "--port", "9000",
    ]);
    assert!(args.is_ok());
    if let Ok(CliArgs {
        command: Some(Command::Web(web)),
        ..
    }) = args
    {
        assert!(web.status);
        assert_eq!(web.ip, Some(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))));
        assert_eq!(web.port, Some(9000));
    } else {
        panic!("Expected Web command");
    }
}

#[test]
fn new_session_with_layout_reaches_an_attaching_client() {
    // `--new-session-with-layout x attach -b -c target` used to be swallowed by
    // the attach arm, so the requested file never reached the layout field and
    // the built-in default took the session.
    let opts = parse(&[
        "vc-frame",
        "--new-session-with-layout",
        PROBE_SOLO,
        "attach",
        "-b",
        "-c",
        "probe-target",
    ]);
    let route = layout_route(&opts, None);
    assert_eq!(
        own_session_layout(&route),
        Some(&PathBuf::from(PROBE_SOLO)),
        "the attaching client must start with the layout it was handed"
    );
}

#[test]
fn attach_carries_the_named_layout_not_merely_some_layout() {
    // Two distinguishable files: a route that returned a constant, or fell back
    // to the default layout, would pass one of these and fail the other.
    for layout in [PROBE_SOLO, PROBE_PAIR] {
        let opts = parse(&[
            "vc-frame",
            "--new-session-with-layout",
            layout,
            "attach",
            "-b",
            "-c",
            "probe-target",
        ]);
        let route = layout_route(&opts, None);
        assert_eq!(
            own_session_layout(&route),
            Some(&PathBuf::from(layout)),
            "attach must carry {layout}"
        );
    }
}

#[test]
fn attach_in_an_inherited_session_creates_the_target_instead_of_a_tab() {
    // The inherited client marker is exactly the case the attach-first branch
    // was written for: the layout belongs to the target being created, never to
    // the Frame session this process happens to be running inside.
    let opts = parse(&[
        "vc-frame",
        "--new-session-with-layout",
        PROBE_SOLO,
        "attach",
        "-b",
        "-c",
        "probe-target",
    ]);
    let route = layout_route(&opts, Some("frame-host".to_string()));
    assert_eq!(
        route,
        LayoutRoute::OwnSession {
            layout: Some(PathBuf::from(PROBE_SOLO)),
        },
        "an explicit attach must not become a tab in the inherited session"
    );
}

#[test]
fn guest_workspace_attach_keeps_the_named_layout() {
    let opts = parse(&[
        "vc-frame",
        "--new-session-with-layout",
        PROBE_PAIR,
        "--guest-workspace",
        "attach",
        "-b",
        "-c",
        "probe-guest",
    ]);
    assert!(opts.guest_workspace);
    let route = layout_route(&opts, Some("frame-host".to_string()));
    assert_eq!(
        own_session_layout(&route),
        Some(&PathBuf::from(PROBE_PAIR)),
        "a guest workspace strips chrome from the requested layout, it does not replace it"
    );
}

#[test]
fn explicit_layout_still_outranks_new_session_with_layout_on_attach() {
    let opts = parse(&[
        "vc-frame",
        "--layout",
        PROBE_SOLO,
        "--new-session-with-layout",
        PROBE_PAIR,
        "attach",
        "-b",
        "-c",
        "probe-target",
    ]);
    let route = layout_route(&opts, None);
    assert_eq!(
        own_session_layout(&route),
        Some(&PathBuf::from(PROBE_SOLO)),
        "the branch order gave `--layout` precedence; keep it"
    );
}

#[test]
fn a_bare_layout_still_becomes_a_tab_in_the_session_it_was_handed_to() {
    let opts = parse(&["vc-frame", "--layout", PROBE_SOLO]);
    assert_eq!(
        layout_route(&opts, Some("frame-host".to_string())),
        LayoutRoute::NewTabIn("frame-host".to_string())
    );
}

#[test]
fn new_session_with_layout_never_becomes_a_tab_in_the_inherited_session() {
    let opts = parse(&["vc-frame", "--new-session-with-layout", PROBE_SOLO]);
    assert_eq!(
        layout_route(&opts, Some("frame-host".to_string())),
        LayoutRoute::OwnSession {
            layout: Some(PathBuf::from(PROBE_SOLO)),
        },
        "the flag always starts a new session, even from inside one"
    );
}
