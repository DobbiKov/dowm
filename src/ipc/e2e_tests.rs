//! Whole-path tests: a real reactor on its own thread behind the real server and socket,
//! driven the way the `dowm` CLI drives it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use dowm_client::{Client, ClientError};
use dowm_protocol::{Event as IpcEvent, Topic};

use super::{EventBus, ReactorBackend, socket, spawn};
use crate::actor;
use crate::actor::input::Shared;
use crate::actor::reactor::testing::{Harness, rect, w};
use crate::cli::{ClientCommand, execute};
use crate::sys::executor::Executor;

const CONFIG: &str = "[mode.main.binding]\nalt-r = 'mode resize'\n[mode.resize.binding]\nesc = 'mode main'\n";

struct Daemon {
    path: PathBuf,
}

fn start(name: &str) -> Daemon {
    let dir = std::env::temp_dir().join(format!("dowm-e2e-{}-{name}", std::process::id()));
    _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("dowm.sock");

    let mut h = Harness::new();
    h.screens_named(&[
        ("LEFT", 1, rect(0., 0., 1000., 800.), Some("Left")),
        ("RIGHT", 2, rect(1000., 0., 1000., 800.), Some("Right")),
    ]);
    h.launch(100, &[w(1, 11, rect(10., 10., 300., 200.)), w(2, 12, rect(50., 50., 300., 200.)), w(3, 13, rect(90., 90., 300., 200.))]);
    let mut reactor = h.reactor;

    let bus = Arc::new(EventBus::new());
    reactor.set_event_bus(Arc::clone(&bus));
    let (events_tx, events_rx) = actor::channel();
    std::thread::spawn(move || Executor::run(reactor.run(events_rx)));

    let shared = Shared::new(&crate::config::parse(CONFIG).unwrap());
    let backend = Arc::new(ReactorBackend::new(events_tx, shared));
    spawn(socket::bind(&path).unwrap(), backend, bus).unwrap();
    Daemon { path }
}

fn cli(d: &Daemon, args: &[&str]) -> Result<String, ClientError> {
    let command = match args {
        ["list-workspaces"] => ClientCommand::ListWorkspaces { json: false },
        ["list-windows"] => ClientCommand::ListWindows { json: false },
        ["list-monitors"] => ClientCommand::ListMonitors { json: false },
        ["mode"] => ClientCommand::Mode,
        words => ClientCommand::Run(words.iter().map(|s| (*s).to_owned()).collect()),
    };
    let mut out = Vec::new();
    execute(command, &d.path, &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

/// Every command the M2-M4 plan names, in its CLI spelling.
const EVERY_COMMAND: &[&str] = &[
    "workspace 2",
    "workspace-back-and-forth",
    "move-node-to-workspace 3",
    "move-node-to-workspace --focus-follows-window 1",
    "move-workspace-to-monitor next",
    "move-workspace-to-monitor --wrap-around prev",
    "focus-monitor next",
    "focus-monitor --wrap-around left",
    "focus left",
    "focus --boundaries-action wrap-around-the-workspace right",
    "move right",
    "join-with left",
    "split horizontal",
    "split vertical",
    "split opposite",
    "resize width +50",
    "resize height -20",
    "resize smart +10",
    "layout tiles horizontal",
    "layout accordion",
    "layout floating tiling",
    "fullscreen",
    "balance-sizes",
    "flatten-workspace-tree",
    "move-mouse window-lazy-center",
    "mode resize",
    "mode main",
    "reload-config",
];

#[test]
fn the_cli_drives_every_command_over_the_socket() {
    let d = start("every");
    for command in EVERY_COMMAND {
        let words: Vec<&str> = command.split_whitespace().collect();
        match cli(&d, &words) {
            Ok(_) => {}
            // The reactor ran it and refused for a state reason (e.g. nothing to join with);
            // that is still a command that went over the socket and was executed.
            Err(ClientError::Daemon(message)) => assert!(
                !message.contains("unknown command") && !message.contains("needs an argument"),
                "`{command}` was not understood by the daemon: {message}"
            ),
            Err(e) => panic!("`{command}` failed at the transport: {e}"),
        }
    }
}

#[test]
fn a_workspace_command_is_visible_to_the_next_list_query() {
    let d = start("visible");
    cli(&d, &["workspace", "4"]).unwrap();
    let table = cli(&d, &["list-workspaces"]).unwrap();
    let focused: Vec<&str> = table.lines().filter(|l| l.starts_with('*')).collect();
    assert_eq!(focused.len(), 1, "{table}");
    assert!(focused[0].starts_with("* 4\t"), "{table}");

    let windows = cli(&d, &["list-windows"]).unwrap();
    assert_eq!(windows.lines().count(), 3, "{windows}");
    let monitors = cli(&d, &["list-monitors"]).unwrap();
    assert!(monitors.contains("Left") && monitors.contains("Right"), "{monitors}");
}

#[test]
fn the_mode_command_changes_nothing_until_the_effect_runner_applies_it() {
    // `mode` is an effect for the platform shell; here only the query path is checked.
    let d = start("mode");
    assert_eq!(cli(&d, &["mode"]).unwrap(), "main\n");
}

#[test]
fn bad_commands_are_reported_to_the_caller() {
    let d = start("bad");
    assert!(matches!(cli(&d, &["frobnicate"]), Err(ClientError::Daemon(m)) if m.contains("unknown command")));
    assert!(matches!(cli(&d, &["workspace"]), Err(ClientError::Daemon(_))));
    assert!(matches!(cli(&d, &["workspace", "99"]), Err(ClientError::Daemon(_))));
    assert!(matches!(
        cli(&d, &["exec-and-forget", "touch", "/tmp/dowm-should-not-exist"]),
        Err(ClientError::Daemon(m)) if m.contains("config file")
    ));
    assert!(!std::path::Path::new("/tmp/dowm-should-not-exist").exists());
}

#[test]
fn subscribers_see_workspace_changes_caused_by_commands() {
    let d = start("sub");
    let stream = Client::connect(&d.path).unwrap().subscribe(&[Topic::Workspace]).unwrap();
    let path = d.path.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        Client::connect(&path).unwrap().command("workspace 5").unwrap();
    });
    let found = stream
        .map(Result::unwrap)
        .find(|e| matches!(e, IpcEvent::WorkspaceChanged { workspace, .. } if workspace == "5"));
    assert_eq!(found, Some(IpcEvent::WorkspaceChanged { workspace: "5".into(), previous: Some("1".into()) }));
}
