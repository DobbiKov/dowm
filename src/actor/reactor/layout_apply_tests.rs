//! `Reactor::flush_layout`: the tiling layout reaches the presenter (or, without one,
//! the app actors) as frame writes, once per change and never as a feedback loop.
//!
//! One 900x600 display `D` unless a test says otherwise; windows start at (10,10,100,100).

use std::sync::Arc;

use objc2_core_foundation::CGRect;

use super::animation::{AnimationSender, Message};
use super::testing::{Harness, W, rect, w, wid};
use super::{Command, Event, Requested};
use crate::actor::app::{Quiet, Request};
use crate::config;
use crate::layout::ops::LayoutArg;
use crate::model::workspace::{Direction, WorkspaceConfig};

const PID: i32 = 100;

fn setup(n: u32) -> Harness {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(9));
    h.screens(&[("D", 1, rect(0., 0., 900., 600.))]);
    if n > 0 {
        let windows: Vec<W> = (1..=n).map(|i| w(i, 10 + i, rect(10., 10., 100., 100.))).collect();
        h.launch(PID, &windows);
    }
    h
}

/// The frame writes sent to the app since the last call, flattened: (window index, frame).
fn writes(h: &mut Harness) -> Vec<(u32, CGRect)> {
    h.app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            Request::SetWindowFrames(frames, ..) => Some(frames),
            _ => None,
        })
        .flatten()
        .map(|(wid, f)| (wid.idx.get(), f))
        .collect()
}

fn raises(h: &mut Harness) -> Vec<u32> {
    h.app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            Request::Raise(windows, ..) => Some(windows),
            _ => None,
        })
        .flatten()
        .map(|w| w.idx.get())
        .collect()
}

fn flush(h: &mut Harness) {
    h.reactor.flush_visibility();
    h.reactor.flush_layout();
}

fn focus(h: &mut Harness, idx: u32) {
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, idx)), Quiet::No));
}

fn sorted(mut v: Vec<(u32, CGRect)>) -> Vec<(u32, CGRect)> {
    v.sort_by_key(|(i, _)| *i);
    v
}

#[test]
fn tiled_windows_are_written_to_their_tile_frames() {
    let mut h = setup(2);
    flush(&mut h);
    assert_eq!(
        sorted(writes(&mut h)),
        vec![(1, rect(0., 0., 450., 600.)), (2, rect(450., 0., 450., 600.))]
    );
}

#[test]
fn an_unchanged_layout_writes_nothing_more() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    flush(&mut h);
    assert!(writes(&mut h).is_empty());
}

#[test]
fn echoes_of_our_writes_do_not_trigger_more_writes() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    for (idx, frame) in [(1, rect(0., 0., 450., 600.)), (2, rect(450., 0., 450., 600.))] {
        h.send(Event::WindowFrameChanged(wid(PID, idx), frame, None, Requested(true), None));
        flush(&mut h);
    }
    assert!(writes(&mut h).is_empty());
}

#[test]
fn a_window_that_cannot_reach_its_target_is_not_fought_over() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    // The app clamps window 1 to its minimum size and reports that as the answer.
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(0., 0., 500., 600.), None, Requested(true), None));
    flush(&mut h);
    assert!(writes(&mut h).is_empty());
}

#[test]
fn a_tiling_command_rewrites_only_the_windows_that_moved() {
    let mut h = setup(3);
    flush(&mut h);
    writes(&mut h);
    focus(&mut h, 3);
    h.tile(Command::Move(Direction::Left)).unwrap();
    flush(&mut h);
    let moved = writes(&mut h);
    assert_eq!(moved.len(), 2, "windows 2 and 3 swap places: {moved:?}");
    assert!(moved.iter().all(|(i, _)| *i != 1));
}

#[test]
fn a_new_window_retiles_the_workspace() {
    let mut h = setup(1);
    flush(&mut h);
    writes(&mut h);
    h.ax_created(PID, w(2, 12, rect(10., 10., 100., 100.)));
    flush(&mut h);
    assert_eq!(
        sorted(writes(&mut h)),
        vec![(1, rect(0., 0., 450., 600.)), (2, rect(450., 0., 450., 600.))]
    );
}

#[test]
fn a_destroyed_window_lets_the_others_grow() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    h.cgs_destroyed(12);
    flush(&mut h);
    assert_eq!(writes(&mut h), vec![(1, rect(0., 0., 900., 600.))]);
}

#[test]
fn a_config_reload_with_gaps_moves_the_windows() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    let text = "[gaps]\ninner.horizontal = 10\nouter.left = 20\nouter.right = 20\nouter.top = 30\nouter.bottom = 40\n\n[mode.main.binding]\nalt-h = 'focus left'\n";
    h.send(Event::ConfigReloaded(Arc::new(config::parse(text).unwrap())));
    flush(&mut h);
    assert_eq!(
        sorted(writes(&mut h)),
        vec![(1, rect(20., 30., 425., 530.)), (2, rect(455., 30., 425., 530.))]
    );
}

#[test]
fn a_display_change_retiles_for_the_new_size() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);
    h.screens(&[("D", 1, rect(0., 0., 1000., 600.))]);
    flush(&mut h);
    assert_eq!(
        sorted(writes(&mut h)),
        vec![(1, rect(0., 0., 500., 600.)), (2, rect(500., 0., 500., 600.))]
    );
}

#[test]
fn hidden_workspaces_are_not_laid_out_and_return_to_their_tiles() {
    let mut h = setup(2);
    flush(&mut h);
    writes(&mut h);

    h.cmd(Command::Workspace("2".into())).unwrap();
    flush(&mut h);
    // Writes now are parking only: never to a tile frame.
    let parked = writes(&mut h);
    assert!(!parked.is_empty());
    assert!(
        parked.iter().all(|(_, f)| f.origin.x < 0. || f.origin.x >= 900. || f.origin.y >= 600.),
        "{parked:?}"
    );

    h.cmd(Command::Workspace("1".into())).unwrap();
    flush(&mut h);
    let back = writes(&mut h);
    assert!(!back.is_empty());
    assert!(
        back.iter().all(|(_, f)| *f == rect(0., 0., 450., 600.) || *f == rect(450., 0., 450., 600.)),
        "{back:?}"
    );
}

#[test]
fn parked_windows_are_not_written_by_the_layout() {
    let mut h = setup(2);
    flush(&mut h);
    h.cmd(Command::Workspace("2".into())).unwrap();
    flush(&mut h);
    writes(&mut h);
    h.reactor.flush_layout();
    assert!(writes(&mut h).is_empty());
}

#[test]
fn the_layout_goes_to_the_presenter_as_one_animation_per_display() {
    let mut h = setup(0);
    let (tx, rx) = AnimationSender::channel();
    h.reactor.set_presenter(tx);
    h.launch(PID, &[w(1, 11, rect(10., 10., 100., 100.)), w(2, 12, rect(10., 10., 100., 100.))]);
    flush(&mut h);

    let messages: Vec<Message> = std::iter::from_fn(|| rx.commands.try_recv().ok()).collect();
    let transitions: Vec<_> = messages.iter().filter(|m| matches!(m, Message::Replace(_))).collect();
    assert_eq!(transitions.len(), 1, "{messages:?}");
    assert!(format!("{:?}", transitions[0]).contains("display: DisplayId(1)"));
    assert!(writes(&mut h).is_empty(), "the presenter, not the app, receives the frames");
}

#[test]
fn an_accordion_raises_its_front_window_when_focus_is_elsewhere() {
    let mut h = setup(3);
    flush(&mut h);
    h.app_requests(PID);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Accordion])).unwrap();
    h.app_requests(PID);
    // The app's focus is elsewhere (another app), so the tree's front window is raised.
    h.reactor.focused_window = None;
    flush(&mut h);
    assert_eq!(raises(&mut h), vec![2]);

    flush(&mut h);
    assert!(raises(&mut h).is_empty(), "raise state is unchanged, so no second raise");
}

#[test]
fn nothing_is_applied_during_shutdown() {
    let mut h = setup(2);
    h.reactor.parked_registry().begin_shutdown();
    flush(&mut h);
    assert!(writes(&mut h).is_empty());
}
