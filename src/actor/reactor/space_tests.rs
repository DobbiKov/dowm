//! Windows macOS moves behind dowm's back (display unplug) and windows in another
//! native Space.

use super::testing::{Harness, rect, w, wid};
use super::{Command, Event, Requested};
use crate::actor::app::Request;
use crate::geometry::Point;
use crate::model::visibility::VisibilityAction;
use crate::model::workspace::WorkspaceConfig;
use crate::sys::window_server::WindowServerId;

const PID: i32 = 100;

fn two_screens() -> Harness {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.screens_named(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.), Some("Left")),
        ("RIGHT", 2, rect(1000., 0., 1000., 1000.), Some("Right")),
    ]);
    h
}

fn hides(h: &mut Harness) -> Vec<crate::actor::app::WindowId> {
    h.reactor
        .take_visibility_actions()
        .into_iter()
        .filter_map(|a| match a {
            VisibilityAction::Hide { window, .. } => Some(window),
            _ => None,
        })
        .collect()
}

/// A window at (100,100) on LEFT, hidden by switching LEFT to workspace 3 and carrying
/// out the park (so the write is armed).
fn parked_window() -> Harness {
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    // The app accepts the write: reports the window at its parked position.
    let at = h.reactor.visibility().parked_at(wid(PID, 1)).unwrap();
    let txid = Some(
        super::animation::PresenterHost::transaction_manager(&h.reactor)
            .get_last_sent_txid(WindowServerId::new(11)),
    );
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(at.x, at.y, 400., 300.), txid, Requested(true), None));
    h
}

fn macos_moves(h: &mut Harness, idx: u32, frame: objc2_core_foundation::CGRect) {
    h.send(Event::WindowFrameChanged(wid(PID, idx), frame, None, Requested(false), None));
}

fn restore_targets(h: &Harness) -> Vec<Point> {
    h.reactor.parked_registry().restore_set().iter().map(|(_, to)| *to).collect()
}

#[test]
fn a_parked_window_macos_moves_on_screen_is_parked_again_and_keeps_its_restore_spot() {
    let mut h = parked_window();
    let before = restore_targets(&h);
    assert_eq!(before, vec![Point::new(100., 100.)]);
    macos_moves(&mut h, 1, rect(0., 44., 400., 300.));
    assert_eq!(hides(&mut h), vec![wid(PID, 1)], "hidden again");
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    assert_eq!(restore_targets(&h), before, "restores to where the user had it, not where macOS dropped it");
}

#[test]
fn a_parked_window_that_stays_parked_is_left_alone() {
    let mut h = parked_window();
    let parked = h.reactor.visibility().parked_at(wid(PID, 1)).unwrap();
    macos_moves(&mut h, 1, rect(parked.x, parked.y, 400., 300.));
    assert!(hides(&mut h).is_empty());
}

#[test]
fn a_window_that_keeps_coming_back_is_given_up_on_after_three_tries() {
    let mut h = parked_window();
    let mut parks = 0;
    for _ in 0..6 {
        macos_moves(&mut h, 1, rect(0., 44., 400., 300.));
        parks += hides(&mut h).len();
        // What `flush_visibility` does after carrying out a hide.
        h.reactor.visibility.arm(wid(PID, 1), None, std::time::Instant::now());
        // ...and the app accepts it before macOS moves the window again.
        if let Some(at) = h.reactor.visibility().parked_at(wid(PID, 1)) {
            h.send(Event::WindowFrameChanged(wid(PID, 1), rect(at.x, at.y, 400., 300.), None, Requested(true), None));
        }
    }
    assert_eq!(parks, 3);
}

#[test]
fn removing_a_display_re_parks_against_the_remaining_one_and_keeps_restore_spots() {
    let mut h = parked_window();
    let before = restore_targets(&h);
    h.screens_named(&[("LEFT", 1, rect(0., 0., 1000., 1000.), Some("Left"))]);
    h.reactor.flush_visibility();
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)), "still hidden");
    assert_eq!(restore_targets(&h), before);
}

fn foreign(h: &mut Harness, sys_ids: &[u32]) {
    h.send(Event::ForeignWindows(sys_ids.iter().map(|&i| WindowServerId::new(i)).collect()));
}

#[test]
fn a_window_in_another_native_space_is_neither_hidden_nor_shown() {
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    foreign(&mut h, &[11]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert!(hides(&mut h).is_empty(), "cannot park what AX cannot reach");
    foreign(&mut h, &[]);
    assert_eq!(hides(&mut h), vec![wid(PID, 1)], "back in the Space: hidden as usual");
}

fn raises(h: &mut Harness) -> usize {
    h.app_requests(PID).into_iter().filter(|r| matches!(r, Request::Raise(..))).count()
}

#[test]
fn an_explicit_command_raises_a_foreign_window_even_though_macos_will_switch_spaces() {
    // The user asked for this window; switching to its Space is the only way to get it.
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(1100., 100., 400., 300.))]); // workspace 2, on RIGHT
    raises(&mut h);
    foreign(&mut h, &[11]);
    h.cmd(Command::Workspace("2".into())).unwrap();
    assert_eq!(raises(&mut h), 1);
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn an_automatic_raise_never_touches_a_foreign_window() {
    // Layout-driven and pointer-driven raises must not make macOS animate a Space switch.
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    raises(&mut h);
    foreign(&mut h, &[11]);
    h.reactor.request_raise(wid(PID, 1));
    assert_eq!(raises(&mut h), 0);
    foreign(&mut h, &[]);
    h.reactor.request_raise(wid(PID, 1));
    assert_eq!(raises(&mut h), 1, "no longer foreign: raised as usual");
}

#[test]
fn the_foreign_flag_is_reported_to_ipc_clients() {
    use dowm_protocol::{Query, Response};
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.)), w(2, 12, rect(150., 150., 400., 300.))]);
    foreign(&mut h, &[12]);
    let Response::Windows { windows } = h.reactor.answer_query(Query::ListWindows) else { panic!() };
    let flags: Vec<bool> = windows.iter().map(|w| w.foreign).collect();
    assert_eq!(flags, vec![false, true]);
}

#[test]
fn the_probe_is_polled_on_every_tick_and_its_answer_applied() {
    let mut h = two_screens();
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    let answer = std::sync::Arc::new(std::sync::Mutex::new(vec![11u32]));
    let shared = std::sync::Arc::clone(&answer);
    h.reactor.set_foreign_probe(move |ids| {
        assert_eq!(ids, [11], "the probe is asked about the managed windows");
        shared.lock().unwrap().clone()
    });
    h.send(Event::VisibilityTick);
    assert!(h.reactor.visibility().is_foreign(wid(PID, 1)));
    answer.lock().unwrap().clear();
    h.send(Event::VisibilityTick);
    assert!(!h.reactor.visibility().is_foreign(wid(PID, 1)));
}

#[test]
fn a_parked_window_whose_workspace_is_shown_while_it_is_foreign_stays_parked_until_it_returns() {
    // The crash seen when a display was plugged back in: the invariant checker (run by
    // `send`/`cmd`) must allow a foreign window to stay parked on a visible workspace.
    let mut h = parked_window();
    foreign(&mut h, &[11]);
    h.cmd(Command::Workspace("1".into())).unwrap();
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)), "cannot be moved from another Space");
    let unhides = |h: &mut Harness| {
        h.reactor.take_visibility_actions().iter().filter(|a| a.is_unhide()).count()
    };
    assert_eq!(unhides(&mut h), 0);
    foreign(&mut h, &[]);
    assert_eq!(unhides(&mut h), 1, "back in this Space: shown at last");
}

fn clock_now(h: &Harness) -> std::time::Instant { h.reactor.clock.0.now() }

fn with_clock(h: &mut Harness) -> std::sync::Arc<crate::model::visibility::ManualClock> {
    let clock = std::sync::Arc::new(crate::model::visibility::ManualClock::new());
    h.reactor.set_clock(clock.clone());
    clock
}

#[test]
fn a_hide_that_never_took_effect_is_noticed_and_retried() {
    // The hide write failed (the app was mid display teardown): no frame event ever says
    // the window left the screen, yet the records still show it on screen.
    let mut h = two_screens();
    let clock = with_clock(&mut h);
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility(); // armed; the window never moves
    // The app still reports it where it was.
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(100., 100., 400., 300.), None, Requested(false), None));
    clock.advance(std::time::Duration::from_millis(1000));
    h.send(Event::VisibilityTick);
    assert!(hides(&mut h).is_empty(), "within the TTL it may still be on its way");
    clock.advance(std::time::Duration::from_millis(1500));
    h.send(Event::VisibilityTick);
    assert_eq!(hides(&mut h), vec![wid(PID, 1)], "overdue and still on screen: hide again");
}

#[test]
fn a_window_sliding_out_is_not_mistaken_for_one_that_failed_to_hide() {
    let mut h = two_screens();
    let clock = with_clock(&mut h);
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();
    // A mid-slide frame from our own write: on screen, requested, well inside the TTL.
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(40., 60., 400., 300.), None, Requested(true), None));
    clock.advance(std::time::Duration::from_millis(300));
    h.send(Event::VisibilityTick);
    assert!(hides(&mut h).is_empty());
}

#[test]
fn a_hide_that_keeps_failing_is_retried_for_far_longer_than_three_tries() {
    // macOS needs several seconds to settle after a display change; giving up after 6s
    // left windows stuck on screen.
    let mut h = two_screens();
    let clock = with_clock(&mut h);
    h.launch(PID, &[w(1, 11, rect(100., 100., 400., 300.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();
    let on_screen = rect(100., 100., 400., 300.);
    h.send(Event::WindowFrameChanged(wid(PID, 1), on_screen, None, Requested(false), None));
    let mut retries = 0;
    for _ in 0..8 {
        clock.advance(std::time::Duration::from_millis(2500));
        h.send(Event::VisibilityTick);
        retries += hides(&mut h).len();
        // What flush does after a hide: arm the new write. The window still does not move.
        h.reactor.visibility.arm(wid(PID, 1), None, clock_now(&h));
        h.send(Event::WindowFrameChanged(wid(PID, 1), on_screen, None, Requested(false), None));
    }
    assert_eq!(retries, 8, "every overdue hide is retried");
}

#[test]
fn a_display_change_gives_up_windows_a_fresh_start() {
    let mut h = parked_window();
    for _ in 0..5 {
        macos_moves(&mut h, 1, rect(0., 44., 400., 300.));
        h.reactor.visibility.arm(wid(PID, 1), None, std::time::Instant::now());
        if let Some(at) = h.reactor.visibility().parked_at(wid(PID, 1)) {
            h.send(Event::WindowFrameChanged(wid(PID, 1), rect(at.x, at.y, 400., 300.), None, Requested(true), None));
        }
    }
    assert!(h.reactor.visibility().is_given_up(wid(PID, 1)), "precondition: given up");
    hides(&mut h);
    h.screens_named(&[("LEFT", 1, rect(0., 0., 1000., 1000.), Some("Left"))]);
    assert!(!h.reactor.visibility().is_given_up(wid(PID, 1)));
}
