//! IPC scenarios: queries, commands over the socket, and published events.

use std::sync::{Arc, Mutex, mpsc};

use dowm_protocol::{Event as IpcEvent, Query, Response};

use super::ipc::{IpcRequest, wire_window_id};
use super::testing::{Harness, rect, w, wid};
use super::{Command, Event};
use crate::config::parse_command;
use crate::ipc::EventBus;

const PID: i32 = 100;

fn one_screen() -> Harness {
    let mut h = Harness::new();
    h.screens_named(&[("MAIN", 1, rect(0., 0., 1000., 800.), Some("Built-in"))]);
    h
}

/// The recorded events minus `MonitorsChanged`, which has its own tests.
fn without_monitors(seen: &Arc<Mutex<Vec<IpcEvent>>>) -> Vec<IpcEvent> {
    seen.lock().unwrap().iter().filter(|e| !matches!(e, IpcEvent::MonitorsChanged { .. })).cloned().collect()
}

fn query(h: &mut Harness, query: Query) -> Response {
    let (reply, rx) = mpsc::channel();
    h.send(Event::Ipc(IpcRequest::Query { query, reply }));
    rx.recv().unwrap()
}

fn run(h: &mut Harness, text: &str) -> Result<(), String> {
    let actions = vec![parse_command(text).unwrap()];
    let (reply, rx) = mpsc::channel();
    h.send(Event::Ipc(IpcRequest::Run { actions, reply }));
    rx.recv().unwrap()
}

fn recording_bus(h: &mut Harness) -> Arc<Mutex<Vec<IpcEvent>>> {
    let bus = Arc::new(EventBus::new());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    bus.subscribe(&[], move |e| {
        sink.lock().unwrap().push(e);
        true
    });
    h.reactor.set_event_bus(bus);
    seen
}

#[test]
fn list_workspaces_reports_visibility_focus_and_window_counts() {
    let mut h = one_screen();
    h.launch(PID, &[w(1, 11, rect(10., 10., 300., 200.)), w(2, 12, rect(50., 50., 300., 200.))]);
    let Response::Workspaces { workspaces } = query(&mut h, Query::ListWorkspaces) else { panic!() };
    assert_eq!(workspaces.len(), 9, "the default pool");
    let first = &workspaces[0];
    assert!(first.focused && first.visible);
    assert_eq!(first.monitor.as_deref(), Some("MAIN"));
    assert_eq!(first.windows, 2);
    assert!(!workspaces[1].visible && !workspaces[1].focused && workspaces[1].monitor.is_none());
}

#[test]
fn list_monitors_reports_name_frame_and_workspace() {
    let mut h = one_screen();
    let Response::Monitors { monitors } = query(&mut h, Query::ListMonitors) else { panic!() };
    assert_eq!(monitors.len(), 1);
    assert_eq!(monitors[0].id, "MAIN");
    assert_eq!(monitors[0].name.as_deref(), Some("Built-in"));
    assert!(monitors[0].focused);
    assert_eq!(monitors[0].workspace.as_deref(), Some("1"));
    assert_eq!((monitors[0].frame.width, monitors[0].frame.height), (1000.0, 800.0));
}

#[test]
fn list_windows_reports_each_window_with_its_workspace() {
    let mut h = one_screen();
    h.launch(PID, &[w(1, 11, rect(10., 20., 300., 200.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "3".into(), window: Some(wid(PID, 1)), follow: false })
        .unwrap();
    let Response::Windows { windows } = query(&mut h, Query::ListWindows) else { panic!() };
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0].id, wire_window_id(wid(PID, 1)));
    assert_eq!(windows[0].pid, PID);
    assert_eq!(windows[0].workspace.as_deref(), Some("3"));
    assert_eq!(windows[0].frame.x, 10.0);
    assert_eq!(windows[0].app.as_deref(), Some("com.example.app100"));
}

#[test]
fn commands_over_ipc_run_and_report_success() {
    let mut h = one_screen();
    assert_eq!(run(&mut h, "workspace 4"), Ok(()));
    assert_eq!(h.active("MAIN"), Some(4));
}

#[test]
fn a_failing_command_reports_the_reason() {
    let mut h = one_screen();
    let error = run(&mut h, "workspace 99").unwrap_err();
    assert!(!error.is_empty());
    assert_eq!(h.active("MAIN"), Some(1), "state unchanged");
}

#[test]
fn mode_over_ipc_becomes_an_effect() {
    let mut h = one_screen();
    assert_eq!(run(&mut h, "mode resize"), Ok(()));
    assert!(h.effects().iter().any(|e| matches!(e, super::effects::Effect::SetMode(m) if m == "resize")));
}

#[test]
fn a_workspace_switch_is_published_once() {
    let mut h = one_screen();
    let seen = recording_bus(&mut h);
    h.reactor.publish_ipc_changes();
    seen.lock().unwrap().clear();
    h.cmd(Command::Workspace("2".into())).unwrap();
    h.reactor.publish_ipc_changes();
    h.reactor.publish_ipc_changes();
    assert_eq!(
        without_monitors(&seen),
        vec![IpcEvent::WorkspaceChanged { workspace: "2".into(), previous: Some("1".into()) }]
    );
}

#[test]
fn the_first_publish_announces_the_starting_workspace() {
    let mut h = one_screen();
    let seen = recording_bus(&mut h);
    h.reactor.publish_ipc_changes();
    assert_eq!(
        without_monitors(&seen),
        vec![IpcEvent::WorkspaceChanged { workspace: "1".into(), previous: None }]
    );
}

#[test]
fn a_focus_change_is_published() {
    let mut h = one_screen();
    h.launch(PID, &[w(1, 11, rect(10., 10., 300., 200.))]);
    let seen = recording_bus(&mut h);
    h.reactor.publish_ipc_changes();
    seen.lock().unwrap().clear();
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, 1)), crate::actor::app::Quiet::No));
    h.reactor.publish_ipc_changes();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![IpcEvent::FocusChanged { window: Some(wire_window_id(wid(PID, 1))) }]
    );
}

fn monitor_events(seen: &Arc<Mutex<Vec<IpcEvent>>>) -> Vec<Vec<dowm_protocol::MonitorInfo>> {
    seen.lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e {
            IpcEvent::MonitorsChanged { monitors } => Some(monitors.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn monitors_are_published_once_per_change() {
    let mut h = Harness::with_workspaces(crate::model::workspace::WorkspaceConfig::numbered(4));
    h.screens_named(&[
        ("LEFT", 1, rect(0., 0., 1000., 800.), Some("Left")),
        ("RIGHT", 2, rect(1000., 0., 1000., 800.), Some("Right")),
    ]);
    let seen = recording_bus(&mut h);
    h.reactor.publish_ipc_changes();
    h.reactor.publish_ipc_changes();
    let first = monitor_events(&seen);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].len(), 2);
    seen.lock().unwrap().clear();
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.publish_ipc_changes();
    h.reactor.publish_ipc_changes();
    let changed = monitor_events(&seen);
    assert_eq!(changed.len(), 1);
    assert!(changed[0].iter().any(|m| m.focused && m.workspace.as_deref() == Some("3")));
}
