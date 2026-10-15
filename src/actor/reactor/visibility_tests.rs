//! Visibility scenarios: a workspace switch becomes ordered park and unpark actions.
//!
//! Driven by synthetic events like `workspace_tests`; `Harness::send` and `Harness::cmd`
//! check the workspace invariants and the parking invariants after every step, so each
//! scenario also proves they hold throughout. `flush_visibility` is called explicitly where
//! a scenario looks at what reached the app actors.

use objc2_core_foundation::CGRect;

use super::animation::Message;
use super::testing::{Harness, rect, w, wid};
use super::{Command, Event, Requested};
use crate::actor::app::{FrameMode, Request};
use crate::geometry::Point;
use std::sync::Arc;
use std::time::Duration;

use super::animation::PresenterHost;
use super::transaction_manager::TransactionId;
use crate::model::visibility::{ManualClock, VisibilityAction, VisibilityConfig, registry_key};
use crate::sys::window_server::WindowServerId;
use crate::model::workspace::{MonitorPattern, WorkspaceConfig};
use crate::actor::reactor::animation::AnimationSender;
use crate::model::display::DisplayUuid;
use crate::sys::screen::DisplayId;

const PID: i32 = 100;

type Screen = (&'static str, u32, CGRect, Option<&'static str>);

fn left(width: f64) -> Screen { ("LEFT", 1, rect(0., 0., width, 1000.), Some("Left")) }

fn right() -> Screen { ("RIGHT", 2, rect(1000., 0., 1000., 1000.), Some("Right")) }

/// Like `harness`, with workspaces 2 and 3 pinned to RIGHT so 3 can be switched there.
fn pinned_harness() -> Harness {
    let config = WorkspaceConfig::numbered(4)
        .with_force(2, vec![MonitorPattern::Sequence(2)])
        .with_force(3, vec![MonitorPattern::Sequence(2)]);
    let mut h = Harness::with_workspaces(config);
    h.screens_named(&[left(1000.), right()]);
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)), "pinned");
    h
}

fn harness() -> Harness {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.screens_named(&[left(1000.), right()]);
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));
    h
}

fn frame() -> CGRect { rect(100., 100., 400., 300.) }

fn actions(h: &mut Harness) -> Vec<VisibilityAction> { h.reactor.take_visibility_actions() }

fn registry_targets(h: &Harness) -> Vec<(i32, Point)> {
    h.reactor.parked_registry().restore_set().iter().map(|(handle, to)| (handle.pid, *to)).collect()
}

/// The app actor reports `idx`'s frame after one of our writes.
fn our_write_landed(h: &mut Harness, idx: u32, frame: CGRect) {
    h.send(Event::WindowFrameChanged(wid(PID, idx), frame, None, Requested(true), None));
}

/// Carry out the planned actions; the txid the write for `sys_id` went out with.
fn flush(h: &mut Harness, sys_id: u32) -> Option<TransactionId> {
    h.reactor.flush_visibility();
    Some(PresenterHost::transaction_manager(&h.reactor).get_last_sent_txid(WindowServerId::new(sys_id)))
}

/// The app actor reports `idx`'s frame for the write that carried `txid`.
fn landed(h: &mut Harness, idx: u32, frame: CGRect, txid: Option<TransactionId>) {
    h.send(Event::WindowFrameChanged(wid(PID, idx), frame, txid, Requested(true), None));
}

fn registry_windows(h: &Harness) -> Vec<Option<u32>> {
    h.reactor.parked_registry().restore_set().iter().map(|(handle, _)| handle.window_id).collect()
}

fn unhidden_then_hidden(actions: &[VisibilityAction]) -> Vec<(bool, u32)> {
    actions.iter().map(|a| (a.is_unhide(), a.window().idx.get())).collect()
}

fn set_requests(h: &mut Harness) -> Vec<(FrameMode, Vec<(u32, CGRect)>)> {
    h.app_requests(PID)
        .into_iter()
        .filter_map(|r| match r {
            Request::SetWindowFrames(frames, _, mode, _) => {
                Some((mode, frames.into_iter().map(|(wid, f)| (wid.idx.get(), f)).collect()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn windows_on_visible_workspaces_are_not_touched() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    assert!(actions(&mut h).is_empty());
    assert_eq!(h.reactor.parked_registry().len(), 0);
}

#[test]
fn switching_workspace_hides_the_old_windows_past_the_left_corner() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);

    h.cmd(Command::Workspace("3".into())).unwrap();

    let acts = actions(&mut h);
    assert_eq!(acts.len(), 1);
    let VisibilityAction::Hide { window, display, corner, from, to } = &acts[0] else {
        panic!("expected a hide, got {acts:?}");
    };
    assert_eq!(*window, wid(PID, 1));
    assert_eq!(display.as_str(), "LEFT");
    assert_eq!(*corner, crate::geometry::Corner::BottomLeft, "nothing is left of LEFT");
    assert_eq!(*from, frame());
    // One pixel of the 400 wide window stays on the display; it hangs below the bottom edge.
    assert_eq!(*to, rect(-399., 999., 400., 300.));
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    // The registry knows where to put it back: its original position.
    assert_eq!(registry_targets(&h), vec![(PID, Point::new(100., 100.))]);
}

#[test]
fn the_right_hand_display_parks_past_its_right_corner() {
    let mut h = pinned_harness();
    h.launch(PID, &[w(1, 11, rect(1200., 100., 400., 300.))]);
    h.cmd(Command::FocusMonitor(crate::model::workspace::MonitorTarget::Direction {
        direction: crate::model::workspace::Direction::Right,
        wrap: false,
    }))
    .unwrap();

    h.cmd(Command::Workspace("3".into())).unwrap();

    let acts = actions(&mut h);
    let [VisibilityAction::Hide { corner, to, .. }] = acts.as_slice() else {
        panic!("expected one hide, got {acts:?}");
    };
    assert_eq!(*corner, crate::geometry::Corner::BottomRight, "LEFT is beside RIGHT's left corner");
    assert_eq!(*to, rect(1999., 999., 400., 300.));
}

#[test]
fn switching_back_unhides_to_the_original_position() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    actions(&mut h);

    h.cmd(Command::Workspace("1".into())).unwrap();

    let acts = actions(&mut h);
    let [VisibilityAction::Unhide { window, display, to, .. }] = acts.as_slice() else {
        panic!("expected one unhide, got {acts:?}");
    };
    assert_eq!(*window, wid(PID, 1));
    assert_eq!(display.as_str(), "LEFT");
    assert_eq!(*to, frame());
    assert!(!h.reactor.visibility().is_parked(wid(PID, 1)));
}

#[test]
fn an_unhidden_window_stays_registered_until_it_is_seen_on_a_display() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    let txid = flush(&mut h, 11);

    // Written, not yet accepted: a crash now must still restore it.
    assert!(h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
    assert_eq!(registry_targets(&h), vec![(PID, Point::new(100., 100.))]);

    // Still parked in the corner, or only a sliver on a display: not confirmed.
    landed(&mut h, 1, rect(-399., 999., 400., 300.), txid);
    landed(&mut h, 1, rect(-390., 100., 400., 300.), txid);
    assert_eq!(h.reactor.parked_registry().len(), 1);

    landed(&mut h, 1, frame(), txid);
    assert!(registry_targets(&h).is_empty());
    assert!(!h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
}

/// M4: the animated path's final frame reaches the reactor as a requested
/// `WindowFrameChanged`. The app actor sends one on `EndWindowAnimation` with the frame it
/// read back and the transaction it last wrote (`app.rs`), so confirming on it empties the
/// registry after an animated unhide.
#[test]
fn an_animated_unhides_final_frame_event_empties_the_registry() {
    let mut h = harness();
    let (tx, _rx) = AnimationSender::channel();
    h.reactor.set_presenter(tx);
    assert!(h.reactor.visibility_config().animate_switches);
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    let txid = flush(&mut h, 11);
    assert_eq!(h.reactor.parked_registry().len(), 1, "animating: not yet confirmed");

    landed(&mut h, 1, frame(), txid); // the end-of-animation event

    assert!(h.reactor.parked_registry().is_empty());
}

#[test]
fn an_event_before_the_write_went_out_confirms_nothing() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap(); // planned, not flushed
    landed(&mut h, 1, frame(), None);
    assert_eq!(h.reactor.parked_registry().len(), 1);
}

#[test]
fn a_stale_on_screen_event_cannot_confirm_a_later_unhide_or_move_a_later_park() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    let first_unhide = flush(&mut h, 11);
    h.cmd(Command::Workspace("3".into())).unwrap();
    let hide = flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    let second_unhide = flush(&mut h, 11);
    assert_ne!(first_unhide, second_unhide);

    // The first unhide's event arrives late, while the second is outstanding.
    landed(&mut h, 1, frame(), first_unhide);
    assert!(h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
    assert_eq!(h.reactor.parked_registry().len(), 1);
    landed(&mut h, 1, frame(), second_unhide);
    assert!(h.reactor.parked_registry().is_empty());

    // The same for a park: an old on-screen event must not become its parked position.
    h.cmd(Command::Workspace("3".into())).unwrap();
    let hide2 = flush(&mut h, 11);
    assert_ne!(hide, hide2);
    landed(&mut h, 1, frame(), second_unhide);
    assert_eq!(h.reactor.visibility().parked_at(wid(PID, 1)), Some(Point::new(-399., 999.)));
}

#[test]
fn minimizing_an_unconfirmed_window_clears_its_registry_entry() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    flush(&mut h, 11);
    assert_eq!(h.reactor.parked_registry().len(), 1);

    h.send(Event::WindowMinimized(wid(PID, 1)));

    assert!(h.reactor.parked_registry().is_empty());
    assert!(!h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
}

#[test]
fn destroying_an_unconfirmed_window_clears_its_registry_entry() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    flush(&mut h, 11);

    h.cgs_destroyed(11);

    assert!(h.reactor.parked_registry().is_empty());
}

#[test]
fn an_unconfirmed_entry_expires_after_the_ttl_if_the_window_looks_shown() {
    let mut h = harness();
    let clock = Arc::new(ManualClock::new());
    h.reactor.set_clock(clock.clone());
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    flush(&mut h, 11);

    clock.advance(Duration::from_millis(1500));
    h.send(Event::VisibilityTick);
    assert_eq!(h.reactor.parked_registry().len(), 1, "not yet");

    clock.advance(Duration::from_millis(1000));
    h.send(Event::VisibilityTick);
    assert!(h.reactor.parked_registry().is_empty(), "no event ever came; the entry cannot linger");
}

#[test]
fn an_expired_entry_for_a_window_that_still_looks_parked_is_kept() {
    let mut h = harness();
    let clock = Arc::new(ManualClock::new());
    h.reactor.set_clock(clock.clone());
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    flush(&mut h, 11);
    h.cmd(Command::Workspace("1".into())).unwrap();
    flush(&mut h, 11);
    // The system reports it still sitting in the corner.
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(-399., 999., 400., 300.), None, Requested(false), None));

    clock.advance(Duration::from_secs(10));
    h.send(Event::VisibilityTick);

    assert_eq!(h.reactor.parked_registry().len(), 1, "a stranded window must stay restorable");
}

#[test]
fn hiding_again_before_the_unhide_was_seen_registers_the_new_original_position() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.cmd(Command::Workspace("1".into())).unwrap();
    // The unhide never landed; the window is still at the park corner. Hide again.
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    assert!(!h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
    assert_eq!(h.reactor.parked_registry().len(), 1);
}

#[test]
fn a_hide_refused_by_shutdown_does_not_lose_the_unconfirmed_entry() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    h.cmd(Command::Workspace("1".into())).unwrap();
    assert!(h.reactor.visibility().is_unconfirmed(wid(PID, 1)));
    // The check in `plan` is only the fast path; the registry's own refusal is what counts.
    h.reactor.parked_registry().begin_shutdown();
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert_eq!(h.reactor.parked_registry().len(), 1, "the mid-unhide entry is still there");
}

#[test]
fn the_registry_handle_carries_the_window_server_id() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert_eq!(registry_windows(&h), vec![Some(11), Some(12)]);
}

#[test]
fn an_app_that_clamps_the_park_move_has_its_real_position_recorded() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    let txid = flush(&mut h, 11);
    let parked_at = |h: &Harness| h.reactor.parked_registry().restore_set()[0].0.parked;
    assert_eq!(parked_at(&h), Point::new(-399., 999.));

    // The user or system moving it is not a clamp of our write.
    h.send(Event::WindowFrameChanged(wid(PID, 1), rect(5., 5., 400., 300.), txid, Requested(false), None));
    assert_eq!(parked_at(&h), Point::new(-399., 999.));

    landed(&mut h, 1, rect(-120., 700., 400., 300.), txid);
    assert_eq!(parked_at(&h), Point::new(-120., 700.));
    assert_eq!(h.reactor.visibility().parked_at(wid(PID, 1)), Some(Point::new(-120., 700.)));
}

#[test]
fn a_parked_window_keeps_its_display_when_its_frame_overlaps_a_neighbour() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert_eq!(h.display_of(wid(PID, 1)), Some(DisplayUuid::new("LEFT")));

    // Our own write reads back a frame lying over RIGHT: the window is still LEFT's.
    our_write_landed(&mut h, 1, rect(1100., 100., 400., 300.));
    assert_eq!(h.display_of(wid(PID, 1)), Some(DisplayUuid::new("LEFT")));
}

#[test]
fn nothing_is_hidden_or_registered_once_shutdown_has_begun() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.reactor.parked_registry().begin_shutdown();

    h.cmd(Command::Workspace("3".into())).unwrap();

    assert!(actions(&mut h).is_empty(), "no hide is planned");
    assert!(!h.reactor.visibility().is_parked(wid(PID, 1)));
    assert!(h.reactor.parked_registry().is_empty(), "the registry is not re-populated");
}

#[test]
fn hides_planned_before_shutdown_are_dropped_at_flush() {
    let mut h = harness();
    h.reactor.set_visibility_config(VisibilityConfig { animate_switches: false });
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert_eq!(h.reactor.pending_visibility_actions().len(), 1);
    h.app_requests(PID);

    h.reactor.parked_registry().begin_shutdown();
    h.reactor.flush_visibility();

    assert!(set_requests(&mut h).is_empty(), "no park write reaches the app");
}

#[test]
fn a_switch_unhides_the_incoming_windows_before_hiding_the_outgoing_ones() {
    let mut h = harness();
    // Window 1 shows on workspace 1. Window 2 is sent to workspace 3, which nobody shows.
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "3".into(), window: Some(wid(PID, 2)), follow: false })
        .unwrap();
    assert_eq!(unhidden_then_hidden(&actions(&mut h)), vec![(false, 2)]);

    h.cmd(Command::Workspace("3".into())).unwrap();

    let acts = h.reactor.pending_visibility_actions().to_vec();
    assert_eq!(unhidden_then_hidden(&acts), vec![(true, 2), (false, 1)], "unhide first");
    assert_eq!(acts[0].target(), rect(300., 500., 200., 100.));
    assert_eq!(h.reactor.visibility().parked().collect::<Vec<_>>(), vec![wid(PID, 1)]);
    // Window 1 is registered as parked; window 2 stays registered until it is seen.
    assert_eq!(
        registry_targets(&h),
        vec![(PID, Point::new(300., 500.)), (PID, Point::new(100., 100.))]
    );
    let txid = flush(&mut h, 12);
    landed(&mut h, 2, rect(300., 500., 200., 100.), txid);
    assert_eq!(registry_targets(&h), vec![(PID, Point::new(100., 100.))]);
}

#[test]
fn the_registry_always_matches_the_parked_set() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    let parked_keys = |h: &Harness| -> Vec<u64> {
        let mut keys: Vec<u64> = h.reactor.visibility().parked().map(registry_key).collect();
        keys.sort_unstable();
        keys
    };
    for step in [3, 1, 4, 2, 1, 3] {
        h.cmd(Command::Workspace(step.to_string())).unwrap();
        // The invariant check inside `cmd` compares the two; this pins the count too.
        let expected = parked_keys(&h).len() + h.reactor.visibility().unconfirmed_count();
        assert_eq!(h.reactor.parked_registry().len(), expected, "after workspace {step}");
    }
}

#[test]
fn restore_survives_a_resolution_change_of_the_same_display() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame())]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    actions(&mut h);

    // LEFT doubles in size while the window is parked.
    h.screens_named(&[("LEFT", 1, rect(0., 0., 2000., 2000.), Some("Left")), (
        "RIGHT",
        2,
        rect(2000., 0., 1000., 1000.),
        Some("Right"),
    )]);
    // The termination handler's target follows, so a crash now still restores sensibly.
    assert_eq!(registry_targets(&h), vec![(PID, Point::new(200., 200.))]);

    h.cmd(Command::Workspace("1".into())).unwrap();
    let acts = actions(&mut h);
    assert_eq!(acts.len(), 1);
    assert_eq!(acts[0].target(), rect(200., 200., 400., 300.));
}

#[test]
fn restore_falls_back_to_the_current_display_when_the_saved_one_is_gone() {
    let mut h = pinned_harness();
    h.launch(PID, &[w(1, 11, rect(1200., 100., 400., 300.))]); // on RIGHT, workspace 2
    h.cmd(Command::FocusMonitor(crate::model::workspace::MonitorTarget::Direction {
        direction: crate::model::workspace::Direction::Right,
        wrap: false,
    }))
    .unwrap();
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    actions(&mut h);

    h.screens_named(&[left(1000.)]); // RIGHT is unplugged
    assert!(h.reactor.visibility().is_parked(wid(PID, 1)));
    assert_eq!(h.display_of(wid(PID, 1)), Some(DisplayUuid::new("LEFT")));

    h.cmd(Command::Workspace("2".into())).unwrap();
    let acts = actions(&mut h);
    let [VisibilityAction::Unhide { display, to, .. }] = acts.as_slice() else {
        panic!("expected one unhide, got {acts:?}");
    };
    assert_eq!(display.as_str(), "LEFT");
    // Saved at (0.2, 0.1) of RIGHT; applied to LEFT.
    assert_eq!(*to, rect(200., 100., 400., 300.));
}

#[test]
fn the_restored_window_is_clamped_onto_the_screen() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, rect(700., 100., 400., 300.))]); // mostly on LEFT
    assert_eq!(h.display_of(wid(PID, 1)), Some(DisplayUuid::new("LEFT")));
    h.cmd(Command::Workspace("3".into())).unwrap();
    actions(&mut h);

    h.screens_named(&[left(800.), right()]); // LEFT shrinks: 0.7 * 800 = 560, +400 > 800
    h.cmd(Command::Workspace("1".into())).unwrap();

    let acts = actions(&mut h);
    assert_eq!(acts[0].target(), rect(400., 100., 400., 300.));
}

#[test]
fn a_window_destroyed_while_parked_leaves_the_registry() {
    let mut h = harness();
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::Workspace("3".into())).unwrap();
    assert_eq!(h.reactor.parked_registry().len(), 2);

    h.cgs_destroyed(11);

    assert_eq!(registry_targets(&h), vec![(PID, Point::new(300., 500.))]);
    assert!(!h.reactor.visibility().is_parked(wid(PID, 1)));
    assert!(h.reactor.visibility().is_parked(wid(PID, 2)));
}

#[test]
fn minimized_windows_and_windows_without_a_display_are_left_alone() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.launch(PID, &[w(1, 11, frame())]); // no display is known yet
    assert!(actions(&mut h).is_empty());
    assert_eq!(h.reactor.parked_registry().len(), 0);

    h.screens_named(&[left(1000.), right()]);
    h.launch(PID + 1, &[w(1, 21, frame())]);
    h.send(Event::WindowMinimized(wid(PID + 1, 1)));
    h.cmd(Command::Workspace("3".into())).unwrap();
    let parked: Vec<_> = actions(&mut h).iter().map(|a| a.window()).collect();
    assert_eq!(parked, vec![wid(PID, 1)], "only the unminimized window is hidden");
}

#[test]
fn instant_mode_writes_positions_unhide_call_first() {
    let mut h = harness();
    h.reactor.set_visibility_config(VisibilityConfig { animate_switches: false });
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "3".into(), window: Some(wid(PID, 2)), follow: false })
        .unwrap();
    h.reactor.flush_visibility();
    h.app_requests(PID);

    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();

    assert_eq!(
        set_requests(&mut h),
        vec![
            (FrameMode::Position, vec![(2, rect(300., 500., 200., 100.))]),
            (FrameMode::Position, vec![(1, rect(-399., 999., 400., 300.))]),
        ]
    );
}

#[test]
fn animated_mode_without_a_presenter_still_lands_every_window_unhide_first() {
    let mut h = harness();
    assert!(h.reactor.visibility_config().animate_switches, "animated is the default");
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "3".into(), window: Some(wid(PID, 2)), follow: false })
        .unwrap();
    h.reactor.flush_visibility();
    h.app_requests(PID);

    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();

    assert_eq!(
        set_requests(&mut h),
        vec![
            (FrameMode::Full, vec![(2, rect(300., 500., 200., 100.))]),
            (FrameMode::Full, vec![(1, rect(-399., 999., 400., 300.))]),
        ]
    );
}

#[test]
fn animated_mode_hands_the_whole_switch_to_the_presenter_in_one_transition() {
    let mut h = harness();
    let (tx, rx) = AnimationSender::channel();
    h.reactor.set_presenter(tx);
    h.launch(PID, &[w(1, 11, frame()), w(2, 12, rect(300., 500., 200., 100.))]);
    h.cmd(Command::MoveNodeToWorkspace { workspace: "3".into(), window: Some(wid(PID, 2)), follow: false })
        .unwrap();
    h.reactor.flush_visibility();
    while rx.commands.try_recv().is_ok() {}
    h.app_requests(PID);

    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();

    let messages: Vec<Message> = std::iter::from_fn(|| rx.commands.try_recv().ok()).collect();
    let transitions: Vec<_> =
        messages.iter().filter(|m| matches!(m, Message::Replace(_))).collect();
    assert_eq!(transitions.len(), 1, "one transition for LEFT: {messages:?}");
    // Incoming and outgoing travel together, so neither is positioned directly.
    assert!(set_requests(&mut h).is_empty());
    let text = format!("{:?}", transitions[0]);
    assert!(text.contains("display: DisplayId(1)"), "{text}");
}

#[test]
fn instant_mode_never_starts_a_presenter_transition() {
    let mut h = harness();
    let (tx, rx) = AnimationSender::channel();
    h.reactor.set_presenter(tx);
    h.reactor.set_visibility_config(VisibilityConfig { animate_switches: false });
    h.launch(PID, &[w(1, 11, frame())]);

    h.cmd(Command::Workspace("3".into())).unwrap();
    h.reactor.flush_visibility();

    let messages: Vec<Message> = std::iter::from_fn(|| rx.commands.try_recv().ok()).collect();
    assert!(!messages.iter().any(|m| matches!(m, Message::Replace(_) | Message::SkipToEnd(_))));
    assert_eq!(
        set_requests(&mut h),
        vec![(FrameMode::Position, vec![(1, rect(-399., 999., 400., 300.))])]
    );
    let _ = DisplayId::new(1);
}
