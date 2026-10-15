//! Reactor scenarios driven entirely by synthetic events (see `testing`).

use objc2_core_foundation::CGRect;

use super::WindowInvalidationSource::{AxDestroyedNotification, InvalidUiElement};
use super::testing::{Harness, W, rect, uuid, w, wid};
use super::{Event, Requested};
use crate::actor::app::{self, WindowInventoryToken};
use crate::model::window_store::tests::info;
use crate::sys::window_server::WindowServerId;

const PID: i32 = 100;
const OTHER: i32 = 200;

/// Two side-by-side 1000x1000 displays: "LEFT" (main) and "RIGHT".
fn two_displays() -> Harness {
    let mut h = Harness::new();
    h.screens(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.)),
        ("RIGHT", 2, rect(1000., 0., 1000., 1000.)),
    ]);
    h
}

fn on_left() -> CGRect { rect(100., 100., 400., 300.) }

fn on_right() -> CGRect { rect(1200., 100., 400., 300.) }

fn moved(pid: i32, idx: u32, frame: CGRect) -> Event {
    Event::WindowFrameChanged(wid(pid, idx), frame, None, Requested(false), None)
}

fn discovered(pid: i32, token: WindowInventoryToken, windows: &[W]) -> Event {
    let new: Vec<_> = windows
        .iter()
        .map(|w| (wid(pid, w.idx), info(&format!("w{}", w.idx), w.frame, w.sys_id)))
        .collect();
    let known_visible = new.iter().map(|(wid, _)| *wid).collect();
    Event::WindowsDiscovered {
        pid,
        token,
        successful: true,
        new,
        known_visible,
    }
}

// ---- window create ----

#[test]
fn launched_app_windows_are_tracked_on_their_displays() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left()), w(2, 12, on_right())]);

    assert_eq!(h.window_count(), 2);
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("LEFT")));
    assert_eq!(h.display_of(wid(PID, 2)), Some(uuid("RIGHT")));
}

#[test]
fn ax_window_created_is_tracked_once() {
    let mut h = two_displays();
    h.launch(PID, &[]);
    h.ax_created(PID, w(1, 11, on_right()));
    h.ax_created(PID, w(1, 11, on_right()));

    assert_eq!(h.window_count(), 1);
    let rec = h.window(wid(PID, 1)).unwrap();
    assert_eq!(rec.display, Some(uuid("RIGHT")));
    assert_eq!(rec.sys_id.map(|s| s.as_u32()), Some(11));
}

#[test]
fn cgs_creation_of_unknown_window_asks_its_app_for_inventory() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.cgs_created(PID, 12, on_left());
    let tokens = h.inventory_requests(PID);
    assert_eq!(tokens.len(), 1);

    // The inventory answers with the new window; it becomes tracked.
    h.send(discovered(PID, tokens[0], &[w(1, 11, on_left()), w(2, 12, on_left())]));
    assert_eq!(h.window_count(), 2);
    assert_eq!(h.display_of(wid(PID, 2)), Some(uuid("LEFT")));
}

#[test]
fn cgs_creation_of_known_window_or_unmanaged_app_is_ignored() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.cgs_created(PID, 11, on_left());
    assert!(h.inventory_requests(PID).is_empty());

    // No app actor for OTHER: nothing to ask, nothing tracked.
    h.cgs_created(OTHER, 99, on_left());
    assert_eq!(h.window_count(), 1);
}

#[test]
fn window_creation_is_watched_for_cgs_close() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    assert_eq!(h.watched_sys_ids(), Some(vec![11]));

    h.ax_created(PID, w(2, 12, on_left()));
    assert_eq!(h.watched_sys_ids(), Some(vec![11, 12]));

    // A title change does not alter the set, so nothing is re-sent.
    h.send(Event::WindowTitleChanged(wid(PID, 2), "renamed".into()));
    assert_eq!(h.watched_sys_ids(), None);
    assert_eq!(h.window(wid(PID, 2)).unwrap().title, "renamed");
}

// ---- window destroy ----

#[test]
fn cgs_destroy_removes_window_by_server_id() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left()), w(2, 12, on_left())]);
    h.watched_sys_ids();

    h.cgs_destroyed(11);
    assert!(h.window(wid(PID, 1)).is_none());
    assert!(h.window(wid(PID, 2)).is_some());
    assert_eq!(h.watched_sys_ids(), Some(vec![12]));

    // A duplicate (SpaceWindowDestroyed and WindowClosed both fire) is harmless.
    h.cgs_destroyed(11);
    assert_eq!(h.window_count(), 1);
}

#[test]
fn ax_invalidation_alone_does_not_destroy_a_server_backed_window() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.send(Event::WindowInvalidated(wid(PID, 1), AxDestroyedNotification));
    let rec = h.window(wid(PID, 1)).expect("kept until confirmed");
    assert!(rec.ax_invalidated);
    assert_eq!(h.inventory_requests(PID).len(), 1, "verify via inventory");

    h.cgs_destroyed(11);
    assert_eq!(h.window_count(), 0);
}

#[test]
fn ax_invalidation_of_window_without_server_id_destroys_it() {
    let mut h = two_displays();
    h.launch(PID, &[W { idx: 1, sys_id: None, frame: on_left() }]);

    h.send(Event::WindowInvalidated(wid(PID, 1), InvalidUiElement));
    assert_eq!(h.window_count(), 0);
}

#[test]
fn inventory_drops_windows_the_app_no_longer_reports() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left()), w(2, 12, on_left())]);
    h.app_requests(PID);

    h.send(Event::WindowInventoryRefreshRequested(PID));
    let tokens = h.inventory_requests(PID);
    assert_eq!(tokens.len(), 1);
    h.send(discovered(PID, tokens[0], &[w(2, 12, on_left())]));

    assert!(h.window(wid(PID, 1)).is_none());
    assert!(h.window(wid(PID, 2)).is_some());
}

#[test]
fn failed_or_unrequested_inventory_changes_nothing() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    let bogus = WindowInventoryToken { request_id: 999, topology_revision: 0 };
    h.send(discovered(PID, bogus, &[]));
    assert_eq!(h.window_count(), 1);

    h.send(Event::WindowInventoryRefreshRequested(PID));
    let token = h.inventory_requests(PID)[0];
    h.send(Event::WindowsDiscovered {
        pid: PID,
        token,
        successful: false,
        new: Vec::new(),
        known_visible: Vec::new(),
    });
    assert_eq!(h.window_count(), 1);
}

#[test]
fn inventory_from_before_a_display_change_is_stale_and_reissued() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.send(Event::WindowInventoryRefreshRequested(PID));
    let old = h.inventory_requests(PID)[0];

    h.screens(&[("LEFT", 1, rect(0., 0., 1000., 1000.))]);
    let reissued = h.inventory_requests(PID);
    assert_eq!(reissued.len(), 1, "pending inventory is re-requested");
    assert_ne!(reissued[0], old);

    h.send(discovered(PID, old, &[]));
    assert_eq!(h.window_count(), 1, "stale inventory ignored");

    h.send(discovered(PID, reissued[0], &[]));
    assert_eq!(h.window_count(), 0);
}

#[test]
fn app_termination_removes_all_its_windows() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left()), w(2, 12, on_right())]);
    h.launch(OTHER, &[w(1, 21, on_left())]);
    h.watched_sys_ids();

    h.send(Event::ApplicationTerminated(PID));
    assert_eq!(h.window_count(), 1);
    assert!(h.window(wid(OTHER, 1)).is_some());
    assert_eq!(h.watched_sys_ids(), Some(vec![21]));

    // Its actor is gone: CGS creations for it are no longer chased.
    h.cgs_created(PID, 13, on_left());
    assert_eq!(h.window_count(), 1);
}

// ---- displays, by UUID ----

#[test]
fn display_added_by_uuid_keeps_existing_assignments() {
    let mut h = Harness::new();
    h.screens(&[("LEFT", 1, rect(0., 0., 1000., 1000.))]);
    h.launch(PID, &[w(1, 11, on_left())]);
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("LEFT")));

    h.screens(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.)),
        ("RIGHT", 2, rect(1000., 0., 1000., 1000.)),
    ]);
    assert!(h.reactor.displays().get(&uuid("RIGHT")).is_some());
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("LEFT")));

    // A window that appears on the new display lands there.
    h.ax_created(PID, w(2, 12, on_right()));
    assert_eq!(h.display_of(wid(PID, 2)), Some(uuid("RIGHT")));
}

#[test]
fn display_removed_by_uuid_moves_its_windows_to_a_remaining_display() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left()), w(2, 12, on_right())]);

    h.screens(&[("LEFT", 1, rect(0., 0., 1000., 1000.))]);
    assert!(h.reactor.displays().get(&uuid("RIGHT")).is_none());
    assert_eq!(h.display_of(wid(PID, 2)), Some(uuid("LEFT")));
    assert!(h.reactor.windows().on_display(&uuid("RIGHT")).is_empty());
    assert_eq!(h.window_count(), 2, "windows survive display removal");
}

#[test]
fn reconnected_display_is_matched_by_uuid_not_cg_id_or_origin() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_right())]);

    // Unplug RIGHT; its window falls back to LEFT.
    h.screens(&[("LEFT", 1, rect(0., 0., 1000., 1000.))]);
    // Replug it with a new CG id, arranged on the other side.
    h.screens(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.)),
        ("RIGHT", 9, rect(-1000., 0., 1000., 1000.)),
    ]);
    let right = h.reactor.displays().get(&uuid("RIGHT")).unwrap();
    assert_eq!(right.cg_id.as_u32(), 9);

    // macOS moves the window back onto the replugged display.
    h.send(moved(PID, 1, rect(-800., 100., 400., 300.)));
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("RIGHT")));
}

#[test]
fn display_swap_with_same_cg_id_is_tracked_by_uuid() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_right())]);

    // A different monitor now has CG id 2 at the same place: by UUID it is
    // RIGHT removed and OTHER added, not "display 2 changed".
    h.screens(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.)),
        ("OTHER", 2, rect(1000., 0., 1000., 1000.)),
    ]);
    assert!(h.reactor.displays().get(&uuid("RIGHT")).is_none());
    assert!(h.reactor.displays().get(&uuid("OTHER")).is_some());
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("OTHER")));
}

#[test]
fn window_moved_across_displays_follows_its_frame() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);

    h.send(moved(PID, 1, on_right()));
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("RIGHT")));
    assert_eq!(h.window(wid(PID, 1)).unwrap().frame, on_right());
}

#[test]
fn windows_known_before_any_display_get_one_when_displays_arrive() {
    let mut h = Harness::new();
    h.launch(PID, &[w(1, 11, on_right())]);
    assert_eq!(h.display_of(wid(PID, 1)), None);

    h.screens(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.)),
        ("RIGHT", 2, rect(1000., 0., 1000., 1000.)),
    ]);
    assert_eq!(h.display_of(wid(PID, 1)), Some(uuid("RIGHT")));
}

// ---- other sources ----

#[test]
fn global_activation_is_forwarded_to_the_app_actor() {
    let mut h = two_displays();
    h.launch(PID, &[]);
    h.app_requests(PID);

    h.send(Event::ApplicationGloballyActivated(PID));
    let reqs = h.app_requests(PID);
    assert!(matches!(reqs.as_slice(), [app::Request::ApplicationGloballyActivated(PID)]));
}

#[test]
fn minimize_and_reorder_keep_the_window() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);

    h.send(Event::WindowMinimized(wid(PID, 1)));
    assert!(h.window(wid(PID, 1)).unwrap().is_minimized);
    h.send(Event::WindowServerReordered(WindowServerId::new(11)));
    h.send(Event::WindowDeminiaturized(wid(PID, 1)));
    assert!(!h.window(wid(PID, 1)).unwrap().is_minimized);
    assert_eq!(h.window_count(), 1);
}

// ---- AX/CGS ordering races ----

#[test]
fn window_created_after_inventory_snapshot_survives_its_answer() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.send(Event::WindowInventoryRefreshRequested(PID));
    let token = h.inventory_requests(PID)[0];
    // The app snapshots its windows, then W2 opens and its AX create lands first.
    h.ax_created(PID, w(2, 12, on_left()));
    h.send(discovered(PID, token, &[w(1, 11, on_left())]));

    assert!(h.window(wid(PID, 2)).is_some(), "W2 must not be dropped by the older inventory");
    assert!(h.window(wid(PID, 1)).is_some());
}

#[test]
fn late_ax_create_for_an_already_destroyed_window_is_ignored() {
    let mut h = two_displays();
    h.launch(PID, &[]);
    h.watched_sys_ids();

    // A short-lived sheet: CGS reports its destruction before AX reports its creation.
    h.cgs_destroyed(12);
    h.ax_created(PID, w(2, 12, on_left()));
    assert_eq!(h.window_count(), 0);
    assert_eq!(h.watched_sys_ids(), None, "dead id never watched");
}

#[test]
fn recycled_server_id_is_accepted_after_cgs_reports_it_again() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.cgs_destroyed(11);

    h.cgs_created(PID, 11, on_left());
    h.ax_created(PID, w(2, 11, on_left()));
    assert!(h.window(wid(PID, 2)).is_some());
}

#[test]
fn space_departure_of_a_live_window_triggers_verification() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    h.send(Event::WindowServerMaybeDestroyed(WindowServerId::new(11)));
    assert_eq!(h.window_count(), 1);
    assert_eq!(h.inventory_requests(PID).len(), 1);
}

#[test]
fn cgs_appearances_of_non_normal_layers_and_bursts_are_coalesced() {
    let mut h = two_displays();
    h.launch(PID, &[w(1, 11, on_left())]);
    h.app_requests(PID);

    // A menu or tooltip (non-zero layer) is not a managed window.
    h.send(Event::WindowServerAppeared(crate::sys::window_server::WindowServerInfo {
        id: WindowServerId::new(50),
        pid: PID,
        layer: 101,
        frame: on_left(),
        min_frame: Default::default(),
        max_frame: Default::default(),
    }));
    assert!(h.inventory_requests(PID).is_empty());

    // Several new windows at once need only one inventory.
    h.cgs_created(PID, 12, on_left());
    h.cgs_created(PID, 13, on_left());
    assert_eq!(h.inventory_requests(PID).len(), 1);
}
