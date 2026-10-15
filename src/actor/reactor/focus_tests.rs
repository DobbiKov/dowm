//! Keyboard/real focus follows dowm's idea of the focused display and workspace.

use super::effects::Effect;
use super::testing::{Harness, rect, w, wid};
use super::{Command, Event};
use crate::actor::app::{self, Quiet};

const PID: i32 = 100;

/// LEFT (ws 1) and RIGHT (ws 2); window 1 on LEFT, window 2 on RIGHT.
fn setup() -> Harness {
    let mut h = Harness::new();
    h.screens_named(&[
        ("LEFT", 1, rect(0., 0., 1000., 800.), Some("Left")),
        ("RIGHT", 2, rect(1000., 0., 1000., 800.), Some("Right")),
    ]);
    h.launch(PID, &[w(1, 11, rect(100., 100., 300., 200.)), w(2, 12, rect(1100., 100., 300., 200.))]);
    h
}

fn main_window_changed(h: &mut Harness, idx: u32) {
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, idx)), Quiet::No));
}

fn raised(h: &mut Harness) -> Vec<crate::actor::app::WindowId> {
    h.app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            app::Request::Raise(windows, ..) => Some(windows),
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn the_focused_display_follows_the_focused_window() {
    let mut h = setup();
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    main_window_changed(&mut h, 2);
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    main_window_changed(&mut h, 1);
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
}

#[test]
fn a_window_on_a_hidden_workspace_does_not_move_focus_to_a_display() {
    let mut h = setup();
    h.cmd(Command::MoveNodeToWorkspace { workspace: "5".into(), window: Some(wid(PID, 2)), follow: false })
        .unwrap();
    main_window_changed(&mut h, 2);
    assert_eq!(h.focused().as_deref(), Some("LEFT"), "workspace 5 is not on any display");
}

#[test]
fn switching_to_a_workspace_on_the_other_screen_focuses_its_window() {
    let mut h = setup();
    raised(&mut h);
    h.cmd(Command::Workspace("2".into())).unwrap();
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
    assert_eq!(raised(&mut h), vec![wid(PID, 2)]);
}

#[test]
fn switching_to_an_empty_workspace_moves_the_pointer_there_and_clears_focus() {
    let mut h = setup();
    main_window_changed(&mut h, 1);
    raised(&mut h);
    h.effects();
    h.cmd(Command::Workspace("7".into())).unwrap();
    assert_eq!(h.reactor.focused_window(), None, "nothing on workspace 7 to focus");
    assert!(raised(&mut h).is_empty());
    let moves: Vec<_> = h
        .effects()
        .into_iter()
        .filter_map(|e| match e {
            Effect::MoveMouse { rect, lazy } => Some((rect, lazy)),
            _ => None,
        })
        .collect();
    assert_eq!(moves.len(), 1);
    assert!(moves[0].1, "lazy: leave the pointer if it is already there");
    assert_eq!(moves[0].0, rect(0., 0., 1000., 800.));
}

#[test]
fn back_and_forth_and_focus_monitor_also_move_real_focus() {
    let mut h = setup();
    h.cmd(Command::Workspace("2".into())).unwrap();
    raised(&mut h);
    h.cmd(Command::WorkspaceBackAndForth).unwrap();
    assert_eq!(raised(&mut h), vec![wid(PID, 1)]);
    h.cmd(Command::FocusMonitor(crate::model::workspace::MonitorTarget::Next { wrap: true })).unwrap();
    assert_eq!(raised(&mut h), vec![wid(PID, 2)]);
}

#[test]
fn a_failed_switch_does_not_touch_focus() {
    let mut h = setup();
    raised(&mut h);
    assert!(h.cmd(Command::Workspace("99".into())).is_err());
    assert!(raised(&mut h).is_empty());
}
