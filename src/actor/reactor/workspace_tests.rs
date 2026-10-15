//! Workspace scenarios driven by synthetic events (see `testing`).
//!
//! Ported from rift's `global_workspaces.rs` scenario suite (lines 538-1151). Rift
//! anchors workspaces to native Spaces (`SpaceId`) and shares a pool between displays
//! by per-display counts; dowm has neither. Wherever a scenario leans on Space
//! mechanics, it is restated over display UUIDs, force-assignment rules and the
//! workspace->display priority chain, and the adaptation is noted on the test.
//!
//! `Harness::send` and `Harness::cmd` check the workspace invariants after every
//! step, so each scenario below also proves the invariants hold throughout.

use super::testing::{Harness, W, rect, uuid, w, wid};
use super::{Command, Event, Requested};
use crate::actor::app::Quiet;
use crate::model::workspace::{
    Direction, MonitorPattern, MonitorTarget, WorkspaceConfig, WorkspaceError, WorkspaceId,
};

const PID: i32 = 100;

fn ws(n: u32) -> WorkspaceId { WorkspaceId::new(n) }

fn left() -> crate::model::display::DisplayUuid { uuid("LEFT") }

fn on_left() -> objc2_core_foundation::CGRect { rect(100., 100., 400., 300.) }

fn on_right() -> objc2_core_foundation::CGRect { rect(1200., 100., 400., 300.) }

fn screen_left() -> (&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>) {
    ("LEFT", 1, rect(0., 0., 1000., 1000.), Some("Left"))
}

fn screen_right(cg_id: u32) -> (&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>) {
    ("RIGHT", cg_id, rect(1000., 0., 1000., 1000.), Some("Right"))
}

fn both() -> Vec<(&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>)> {
    vec![screen_left(), screen_right(2)]
}

/// Two side-by-side 1000x1000 displays, LEFT (main) and RIGHT, over `config`.
fn two(config: WorkspaceConfig) -> Harness {
    let mut h = Harness::with_workspaces(config);
    h.screens_named(&both());
    h
}

/// Six workspaces, 1-3 pinned to the first display and 4-6 to the second (rift's
/// `display_workspace_counts: [3, 3]`, restated as force-assignment).
fn split_config() -> WorkspaceConfig {
    let mut config = WorkspaceConfig::numbered(6);
    for n in 1..=3 {
        config = config.with_force(n, vec![MonitorPattern::Sequence(1)]);
    }
    for n in 4..=6 {
        config = config.with_force(n, vec![MonitorPattern::Sequence(2)]);
    }
    config
}

fn focus_window(h: &mut Harness, idx: u32) {
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, idx)), Quiet::No));
}

fn move_node(workspace: u32, window: Option<u32>, follow: bool) -> Command {
    Command::MoveNodeToWorkspace { workspace, window: window.map(|i| wid(PID, i)), follow }
}

fn to_monitor(direction: Direction) -> Command {
    Command::MoveWorkspaceToMonitor(MonitorTarget::Direction { direction, wrap: false })
}

fn focus_monitor(direction: Direction) -> Command {
    Command::FocusMonitor(MonitorTarget::Direction { direction, wrap: false })
}

fn dragged(idx: u32, frame: objc2_core_foundation::CGRect) -> Event {
    Event::WindowFrameChanged(wid(PID, idx), frame, None, Requested(false), None)
}

/// Workspaces 2 and 5 belong to the display named "Right". LEFT shows 1; RIGHT shows 2
/// (window 1) with 5 hidden but last seen on RIGHT (window 2).
fn parked_setup() -> Harness {
    let config = WorkspaceConfig::numbered(6)
        .with_force(2, vec![MonitorPattern::Name("^Right$".into())])
        .with_force(5, vec![MonitorPattern::Name("^Right$".into())]);
    let mut h = two(config);
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));
    h.cmd(Command::Workspace(5)).unwrap();
    h.cmd(Command::Workspace(2)).unwrap();
    h.cmd(focus_monitor(Direction::Left)).unwrap();
    let windows: [W; 2] = [w(1, 11, on_right()), w(2, 12, on_right())];
    h.launch(PID, &windows);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(2));
    h.cmd(move_node(5, Some(2), false)).unwrap();
    h
}

// ---- rift: switching_to_a_number_on_another_display_focuses_that_display ----

#[test]
fn switching_to_a_workspace_pinned_to_another_display_focuses_that_display() {
    let mut h = two(split_config());
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(4)));

    h.cmd(Command::Workspace(5)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!(h.active("RIGHT"), Some(5));
    assert_eq!(h.active("LEFT"), Some(1));
}

// ---- rift: switching_to_a_number_on_the_current_display_stays_local ----

#[test]
fn switching_to_a_workspace_on_the_current_display_stays_local() {
    let mut h = two(split_config());

    h.cmd(Command::Workspace(2)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    assert_eq!(h.active("LEFT"), Some(2));
    assert_eq!(h.active("RIGHT"), Some(4));
}

// ---- rift: switching_to_a_workspace_visible_elsewhere_only_moves_focus ----

#[test]
fn switching_to_a_workspace_visible_elsewhere_only_moves_focus() {
    let mut config = split_config();
    config.auto_back_and_forth = true;
    let mut h = two(config);

    h.cmd(Command::Workspace(4)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(4)));

    // Auto back-and-forth: asking for the focused workspace again goes back to the
    // previous one, which is visible on LEFT, so that only moves focus too.
    h.cmd(Command::Workspace(4)).unwrap();
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(4)));
}

// ---- rift: uneven_display_counts_shift_the_ranges ----

#[test]
fn uneven_force_assignments_pin_the_remaining_numbers_to_the_second_display() {
    let mut config = WorkspaceConfig::numbered(6).with_force(1, vec![MonitorPattern::Sequence(1)]);
    for n in 2..=6 {
        config = config.with_force(n, vec![MonitorPattern::Sequence(2)]);
    }
    let mut h = two(config);

    h.cmd(Command::Workspace(3)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!(h.active("RIGHT"), Some(3));
    assert_eq!(h.active("LEFT"), Some(1));
}

// ---- rift: numbers_on_an_inactive_display_are_ignored ----

/// Adaptation: dowm has no "inactive display". The equivalent is a number pinned to a
/// display that is not connected, which must fall through the chain to the primary
/// display instead of being dropped or stranding the workspace.
#[test]
fn a_number_pinned_to_an_absent_display_falls_back_to_the_primary_display() {
    let config = WorkspaceConfig::numbered(6).with_force(6, vec![MonitorPattern::Sequence(3)]);
    let mut h = two(config);
    h.cmd(focus_monitor(Direction::Right)).unwrap();

    h.cmd(Command::Workspace(6)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    assert_eq!(h.active("LEFT"), Some(6));
    assert_eq!(h.active("RIGHT"), Some(2));
}

// ---- rift: moving_a_window_to_a_number_on_another_display_lands_it_there ----

#[test]
fn moving_a_window_to_a_workspace_on_another_display_lands_it_there() {
    for follow in [false, true] {
        let mut h = two(split_config());
        h.launch(PID, &[w(1, 11, on_left())]);
        focus_window(&mut h, 1);
        assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));

        h.cmd(move_node(5, None, follow)).unwrap();

        assert_eq!(h.workspace_of(wid(PID, 1)), Some(5));
        assert_eq!(h.active("RIGHT") == Some(5), follow);
        assert_eq!(h.reactor.workspaces().window_is_visible(wid(PID, 1)), follow);
        let expected = if follow { "RIGHT" } else { "LEFT" };
        assert_eq!(h.focused().as_deref(), Some(expected));
    }
}

#[test]
fn moving_an_unknown_window_or_workspace_is_an_error_and_changes_nothing() {
    let mut h = two(WorkspaceConfig::numbered(4));
    h.launch(PID, &[w(1, 11, on_left())]);

    assert_eq!(h.cmd(move_node(9, Some(1), false)), Err(WorkspaceError::NoSuchWorkspace(9)));
    assert!(matches!(
        h.cmd(move_node(3, Some(7), false)),
        Err(WorkspaceError::UnknownWindow(_))
    ));
    // No window focused and none named.
    assert_eq!(h.cmd(move_node(3, None, false)), Err(WorkspaceError::NoFocusedWindow));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));
}

// ---- rift: moved_workspace_keeps_its_number_on_the_new_display ----

#[test]
fn moved_workspace_keeps_its_number_and_windows_on_the_new_display() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.launch(PID, &[w(1, 11, on_left())]);
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));

    h.cmd(to_monitor(Direction::Right)).unwrap();

    assert_eq!(h.active("RIGHT"), Some(1));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));
    // The hole is plugged with a workspace that resolves to LEFT, not the evicted one.
    assert_eq!(h.active("LEFT"), Some(3));
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert!(h.reactor.workspaces().window_is_visible(wid(PID, 1)));

    // Number 1 now follows the workspace to the right display.
    h.cmd(focus_monitor(Direction::Left)).unwrap();
    h.cmd(Command::Workspace(1)).unwrap();
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!(h.active("RIGHT"), Some(1));
}

// ---- rift: an_empty_workspace_can_move_to_another_display ----

#[test]
fn an_empty_workspace_can_move_to_another_display() {
    let mut h = two(WorkspaceConfig::numbered(6));

    h.cmd(to_monitor(Direction::Right)).unwrap();

    assert_eq!(h.active("RIGHT"), Some(1));
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert!(h.active("LEFT").is_some());
}

// ---- rift: workspace_queries_report_global_numbers ----

/// Adaptation: dowm has no query surface yet (IPC is step 6). The behaviour under
/// test is the part that backs it: one fixed, numbered, named pool with no lazy
/// creation.
#[test]
fn the_pool_is_fixed_numbered_and_named_from_config() {
    let mut config = WorkspaceConfig::numbered(6);
    config.names[0] = "web".into();
    let mut h = two(config);

    let store = h.reactor.workspaces();
    assert_eq!(store.len(), 6);
    assert_eq!(store.name(ws(1)), Some("web"));
    assert_eq!(store.name(ws(2)), Some("2"));
    assert_eq!(store.ids().map(WorkspaceId::number).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6]);

    // Neither commands nor lookups create workspaces.
    assert_eq!(h.cmd(Command::Workspace(7)), Err(WorkspaceError::NoSuchWorkspace(7)));
    assert_eq!(h.cmd(Command::Workspace(0)), Err(WorkspaceError::NoSuchWorkspace(0)));
    assert_eq!(h.reactor.workspaces().len(), 6);
}

// ---- rift: unplugged_display_workspaces_move_to_the_main_display_hidden ----

#[test]
fn unplugged_display_workspaces_stay_hidden_with_their_windows() {
    let mut h = parked_setup();
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(5));

    h.screens_named(&[screen_left()]);

    assert_eq!(h.active("LEFT"), Some(1), "main display keeps its workspace");
    assert_eq!(h.active("RIGHT"), None);
    let store = h.reactor.workspaces();
    assert_eq!(store.workspace_of(wid(PID, 1)), Some(ws(2)));
    assert_eq!(store.workspace_of(wid(PID, 2)), Some(ws(5)));
    assert!(!store.window_is_visible(wid(PID, 1)));
    assert!(!store.window_is_visible(wid(PID, 2)));
    let shown = store.home_display(ws(2)).unwrap();
    assert_eq!((shown.display.as_str(), shown.was_active), ("RIGHT", true));
    let hidden = store.home_display(ws(5)).unwrap();
    assert_eq!((hidden.display.as_str(), hidden.was_active), ("RIGHT", false));

    // Both can be summoned onto the remaining display.
    h.cmd(Command::Workspace(5)).unwrap();
    assert_eq!(h.active("LEFT"), Some(5));
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
}

// ---- rift: windows_moved_before_the_display_is_reported_gone_keep_their_workspace ----

/// Adaptation: in rift, macOS moves windows to another Space before the display is
/// reported gone. The dowm analogue is a frame change landing a hidden workspace's
/// window on another display before the disconnect (the same effect as parking, or of
/// macOS rescuing windows). Hidden workspaces own their windows, so the move must not
/// reassign them.
#[test]
fn windows_moved_before_the_display_is_reported_gone_keep_their_workspace() {
    let mut h = parked_setup();

    h.send(dragged(2, on_left()));
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(5));
    assert_eq!(h.display_of(wid(PID, 2)), Some(left()));

    h.screens_named(&[screen_left()]);
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(5));
    assert_eq!(h.reactor.workspaces().home_display(ws(5)).unwrap().display.as_str(), "RIGHT");

    h.screens_named(&both());
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(5));
    assert!(h.reactor.workspaces().home_display(ws(5)).is_none());
}

#[test]
fn dragging_a_window_to_another_display_moves_it_to_that_displays_workspace() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.launch(PID, &[w(1, 11, on_left())]);
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(1));

    h.send(dragged(1, on_right()));

    assert_eq!(h.workspace_of(wid(PID, 1)), Some(2));
}

#[test]
fn windows_created_on_a_display_join_its_visible_workspace_and_leave_with_the_window() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.launch(PID, &[]);

    h.ax_created(PID, w(1, 11, on_right()));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(2));

    h.cgs_destroyed(11);
    assert_eq!(h.workspace_of(wid(PID, 1)), None);
    assert!(h.reactor.workspaces().windows_in(ws(2)).is_empty());
}

// ---- rift: reconnected_display_gets_its_workspaces_back ----

#[test]
fn reconnected_display_gets_its_workspaces_back() {
    let mut h = parked_setup();
    h.screens_named(&[screen_left()]);
    // While parked, the user pulls one of them onto the main display.
    h.cmd(Command::Workspace(5)).unwrap();
    assert_eq!(h.active("LEFT"), Some(5));

    // The display returns with a new CG id (matched by UUID).
    h.screens_named(&[screen_left(), screen_right(7)]);

    assert_eq!(h.active("RIGHT"), Some(2), "the workspace that was showing is showing again");
    assert_eq!(h.active("LEFT"), Some(1), "5 returned home; LEFT is plugged with its own");
    let store = h.reactor.workspaces();
    assert!(store.home_display(ws(2)).is_none() && store.home_display(ws(5)).is_none());
    assert_eq!(store.display_of(ws(5)), None, "5 is hidden, on its home display's share");
    assert_eq!(store.workspace_of(wid(PID, 1)), Some(ws(2)));
    assert!(store.window_is_visible(wid(PID, 1)));
    assert_eq!(h.reactor.workspaces().focused_display().map(|d| d.as_str()), Some("LEFT"));
}

// ---- rift: a_different_display_does_not_take_parked_workspaces ----

#[test]
fn a_different_display_does_not_take_parked_workspaces() {
    let mut h = parked_setup();
    h.screens_named(&[screen_left()]);

    h.screens_named(&[screen_left(), ("OTHER", 8, rect(1000., 0., 1000., 1000.), Some("Other"))]);

    // OTHER gets a fresh workspace, not RIGHT's parked ones, which keep their home.
    assert_eq!(h.active("OTHER"), Some(3));
    let store = h.reactor.workspaces();
    assert_eq!(store.home_display(ws(2)).unwrap().display.as_str(), "RIGHT");
    assert_eq!(store.home_display(ws(5)).unwrap().display.as_str(), "RIGHT");
    assert_eq!(store.display_of(ws(2)), None);
}

// ---- rift: moving_a_parked_workspace_by_hand_keeps_it_where_it_was_put ----

#[test]
fn moving_a_parked_workspace_by_hand_keeps_it_where_it_was_put() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(6));
    let three = [
        ("LEFT", 1, rect(0., 0., 1000., 1000.), None),
        ("MID", 2, rect(1000., 0., 1000., 1000.), None),
        ("FAR", 3, rect(2000., 0., 1000., 1000.), None),
    ];
    h.screens_named(&three);
    assert_eq!(h.active("FAR"), Some(3));
    h.screens_named(&three[..2]);
    assert_eq!(h.reactor.workspaces().home_display(ws(3)).unwrap().display.as_str(), "FAR");

    // The user summons it to LEFT and sends it on to MID.
    h.cmd(Command::Workspace(3)).unwrap();
    assert_eq!(h.active("LEFT"), Some(3));
    h.cmd(to_monitor(Direction::Right)).unwrap();
    assert!(h.reactor.workspaces().home_display(ws(3)).is_none());

    h.screens_named(&three);

    assert_eq!(h.active("MID"), Some(3));
    assert_eq!(h.reactor.workspaces().display_of(ws(3)).map(|d| d.as_str()), Some("MID"));
    assert!(h.active("FAR").is_some());
}

// ---- rift: unplugging_without_global_mode_leaves_workspaces_in_place ----

/// Adaptation: dowm has no global/per-display mode. The nearest behaviour: unplugging
/// a display only parks workspaces that were last seen on it; workspaces that never
/// lived there are untouched.
#[test]
fn unplugging_a_display_leaves_workspaces_that_never_lived_there_untouched() {
    let mut h = two(WorkspaceConfig::numbered(6));

    h.screens_named(&[screen_left()]);

    let store = h.reactor.workspaces();
    for n in 3..=6 {
        assert!(store.home_display(ws(n)).is_none(), "workspace {n}");
    }
    assert!(store.home_display(ws(2)).is_some());
    assert_eq!(store.visible_workspace(&left()), Some(ws(1)));
}

// ---- rift: the_whole_pool_exists_with_one_display_and_shares_out_when_another_connects ----

#[test]
fn the_whole_pool_exists_with_one_display_and_shares_out_when_another_connects() {
    let mut h = Harness::with_workspaces(split_config());
    h.screens_named(&[screen_left()]);

    // All six exist with one display, even the second display's share.
    assert_eq!(h.reactor.workspaces().len(), 6);
    assert_eq!(h.active("LEFT"), Some(1));
    h.cmd(Command::Workspace(6)).unwrap();
    assert_eq!(h.active("LEFT"), Some(6));
    h.cmd(Command::Workspace(1)).unwrap();

    // A second display takes the first hidden workspace of its share.
    h.screens_named(&both());

    assert_eq!(h.active("RIGHT"), Some(4));
    assert_eq!(h.active("LEFT"), Some(1));
    assert_eq!(h.reactor.workspaces().len(), 6);
}

// ---- rift: only_a_displays_primary_space_is_managed ----

/// Adaptation: Space switching on a display has no dowm analogue. The equivalent
/// identity property: a display keeps its workspace when its CG id and frame change
/// (macOS re-numbering or rearrangement), because it is tracked by UUID.
#[test]
fn a_display_keeps_its_workspace_when_its_cg_id_and_frame_change() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.cmd(Command::Workspace(4)).unwrap();
    h.cmd(focus_monitor(Direction::Right)).unwrap();

    h.screens_named(&[
        screen_left(),
        ("RIGHT", 42, rect(-1000., 0., 1000., 1000.), Some("Right")),
    ]);

    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(4), Some(2)));
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
}

// ---- rift: per_display_mode_is_unchanged_when_global_is_off ----

/// Adaptation: no modes in dowm. The nearest invariant is the last link of the chain:
/// a workspace with no history lands on the primary display even when another display
/// is focused.
#[test]
fn a_workspace_with_no_history_lands_on_the_primary_display() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.cmd(focus_monitor(Direction::Right)).unwrap();

    h.cmd(Command::Workspace(5)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    assert_eq!(h.active("LEFT"), Some(5));
    assert_eq!(h.active("RIGHT"), Some(2));
}

// ---- dowm: the priority chain ----

#[test]
fn force_assignment_beats_the_currently_visible_display() {
    let config = WorkspaceConfig::numbered(6).with_force(1, vec![MonitorPattern::Sequence(2)]);
    let mut h = Harness::with_workspaces(config);
    h.screens_named(&[screen_left()]);
    assert_eq!(h.active("LEFT"), Some(1));

    // RIGHT arrives: 1 is pinned to it, so it moves there and LEFT is plugged.
    h.screens_named(&both());
    assert_eq!(h.active("RIGHT"), Some(1));
    assert_ne!(h.active("LEFT"), Some(1));
    assert!(h.active("LEFT").is_some());
}

#[test]
fn a_hidden_workspace_returns_to_its_last_seen_display() {
    let mut h = two(WorkspaceConfig::numbered(6));
    // 3 goes to LEFT, then is sent to RIGHT, so 2 is evicted from RIGHT and hidden.
    h.cmd(Command::Workspace(3)).unwrap();
    h.cmd(to_monitor(Direction::Right)).unwrap();
    assert_eq!(h.active("RIGHT"), Some(3));
    h.cmd(focus_monitor(Direction::Left)).unwrap();

    h.cmd(Command::Workspace(2)).unwrap();

    assert_eq!(h.focused().as_deref(), Some("RIGHT"), "2 was last seen on RIGHT");
    assert_eq!(h.active("RIGHT"), Some(2));
}

#[test]
fn force_assignment_patterns_are_tried_in_order_and_skip_absent_displays() {
    let config = WorkspaceConfig::numbered(6).with_force(
        4,
        vec![
            MonitorPattern::Name("^Projector$".into()),
            MonitorPattern::Sequence(5),
            MonitorPattern::Secondary,
        ],
    );
    let mut h = two(config);

    h.cmd(Command::Workspace(4)).unwrap();

    assert_eq!(h.active("RIGHT"), Some(4));
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
}

#[test]
fn force_assignment_by_name_regex_main_and_sequence_order_left_to_right() {
    // The snapshot lists RIGHT first, so RIGHT is "main" but LEFT is sequence 1.
    let config = WorkspaceConfig::numbered(6)
        .with_force(3, vec![MonitorPattern::Main])
        .with_force(4, vec![MonitorPattern::Sequence(1)])
        .with_force(5, vec![MonitorPattern::Name("(?i)^left$".into())]);
    let mut h = Harness::with_workspaces(config);
    h.screens_named(&[screen_right(2), screen_left()]);

    h.cmd(Command::Workspace(3)).unwrap();
    assert_eq!(h.active("RIGHT"), Some(3));
    h.cmd(Command::Workspace(4)).unwrap();
    assert_eq!(h.active("LEFT"), Some(4));
    h.cmd(Command::Workspace(5)).unwrap();
    assert_eq!(h.active("LEFT"), Some(5));
}

#[test]
fn secondary_only_matches_when_there_is_exactly_one_non_main_display() {
    let config = WorkspaceConfig::numbered(6).with_force(4, vec![MonitorPattern::Secondary]);
    let mut h = Harness::with_workspaces(config);
    h.screens_named(&[
        screen_left(),
        screen_right(2),
        ("FAR", 3, rect(2000., 0., 1000., 1000.), None),
    ]);
    h.cmd(Command::Workspace(4)).unwrap();
    // Ambiguous: no match, so the chain falls through to the primary display.
    assert_eq!(h.active("LEFT"), Some(4));

    h.screens_named(&both());
    h.cmd(Command::Workspace(4)).unwrap();
    assert_eq!(h.active("RIGHT"), Some(4));
}

#[test]
fn invalid_force_assignment_config_is_rejected() {
    use crate::actor::reactor::Reactor;
    let bad_regex = WorkspaceConfig::numbered(3).with_force(1, vec![MonitorPattern::Name("(".into())]);
    assert!(matches!(
        Reactor::with_workspaces(None, bad_regex),
        Err(WorkspaceError::BadPattern(_))
    ));
    let bad_ws = WorkspaceConfig::numbered(3).with_force(9, vec![MonitorPattern::Main]);
    assert!(matches!(
        Reactor::with_workspaces(None, bad_ws),
        Err(WorkspaceError::NoSuchWorkspace(9))
    ));
    let bad_seq = WorkspaceConfig::numbered(3).with_force(1, vec![MonitorPattern::Sequence(0)]);
    assert!(matches!(
        Reactor::with_workspaces(None, bad_seq),
        Err(WorkspaceError::BadPattern(_))
    ));
    let empty = WorkspaceConfig { names: vec![], ..WorkspaceConfig::default() };
    assert!(matches!(Reactor::with_workspaces(None, empty), Err(WorkspaceError::EmptyPool)));
}

// ---- dowm: holes ----

#[test]
fn a_hole_is_plugged_with_a_workspace_that_resolves_to_that_display() {
    let config = WorkspaceConfig::numbered(6).with_force(4, vec![MonitorPattern::Sequence(1)]);
    let mut h = two(config);
    // LEFT=1, RIGHT=2. Sending 1 right leaves a hole on LEFT; 3 would be the lowest
    // free number, but 4 is pinned to LEFT.
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));

    h.cmd(to_monitor(Direction::Right)).unwrap();

    assert_eq!(h.active("LEFT"), Some(3));
    h.cmd(to_monitor(Direction::Right)).unwrap_err(); // already rightmost
}

#[test]
fn a_hole_falls_back_to_any_workspace_when_all_are_pinned_elsewhere() {
    let config = WorkspaceConfig::numbered(3)
        .with_force(1, vec![MonitorPattern::Sequence(1)])
        .with_force(2, vec![MonitorPattern::Sequence(2)])
        .with_force(3, vec![MonitorPattern::Sequence(2)]);
    let mut h = two(config);
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));

    h.cmd(to_monitor(Direction::Right)).unwrap();

    assert_eq!(h.active("RIGHT"), Some(1));
    assert_eq!(h.active("LEFT"), Some(2), "nothing else is free, so pinned or not it plugs");
}

#[test]
fn more_displays_than_workspaces_leaves_the_extra_display_empty() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(1));
    h.screens_named(&both());

    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), None));
    h.cmd(to_monitor(Direction::Right)).unwrap();
    // The single workspace moved; LEFT is now the empty one.
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (None, Some(1)));
}

#[test]
fn losing_every_display_keeps_all_windows_and_restores_on_return() {
    let mut h = parked_setup();

    h.screens_named(&[]);
    assert_eq!(h.focused(), None);
    assert_eq!(h.cmd(Command::Workspace(1)), Err(WorkspaceError::NoDisplays));
    assert_eq!(h.workspace_of(wid(PID, 1)), Some(2));
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(5));

    h.screens_named(&both());
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), Some(2)));
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
}

// ---- dowm: back-and-forth ----

#[test]
fn workspace_back_and_forth_returns_to_the_previous_workspace() {
    let mut h = two(WorkspaceConfig::numbered(6));
    assert_eq!(h.cmd(Command::WorkspaceBackAndForth), Err(WorkspaceError::NoPreviousWorkspace));

    h.cmd(Command::Workspace(4)).unwrap();
    assert_eq!(h.active("LEFT"), Some(4));

    h.cmd(Command::WorkspaceBackAndForth).unwrap();
    assert_eq!(h.active("LEFT"), Some(1));
    h.cmd(Command::WorkspaceBackAndForth).unwrap();
    assert_eq!(h.active("LEFT"), Some(4));
}

#[test]
fn workspace_without_auto_back_and_forth_is_idempotent() {
    let mut h = two(WorkspaceConfig::numbered(6));
    h.cmd(Command::Workspace(4)).unwrap();
    h.cmd(Command::Workspace(4)).unwrap();
    assert_eq!(h.active("LEFT"), Some(4));
}

// ---- dowm: monitor targets ----

fn three_displays() -> Harness {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(6));
    // Listed out of left-to-right order on purpose; LEFT is still main.
    h.screens_named(&[
        ("LEFT", 1, rect(0., 0., 1000., 1000.), Some("Left")),
        ("FAR", 3, rect(2000., 0., 1000., 1000.), Some("Far")),
        ("MID", 2, rect(1000., 0., 1000., 1000.), Some("Mid")),
    ]);
    h
}

#[test]
fn focus_monitor_by_direction_next_prev_and_pattern() {
    let mut h = three_displays();
    assert_eq!(h.focused().as_deref(), Some("LEFT"));

    h.cmd(focus_monitor(Direction::Right)).unwrap();
    assert_eq!(h.focused().as_deref(), Some("MID"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Next { wrap: false })).unwrap();
    assert_eq!(h.focused().as_deref(), Some("FAR"));
    assert_eq!(
        h.cmd(Command::FocusMonitor(MonitorTarget::Next { wrap: false })),
        Err(WorkspaceError::NoSuchDisplay)
    );
    h.cmd(Command::FocusMonitor(MonitorTarget::Next { wrap: true })).unwrap();
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Prev { wrap: true })).unwrap();
    assert_eq!(h.focused().as_deref(), Some("FAR"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Prev { wrap: false })).unwrap();
    assert_eq!(h.focused().as_deref(), Some("MID"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Pattern(MonitorPattern::Name("^Far$".into()))))
        .unwrap();
    assert_eq!(h.focused().as_deref(), Some("FAR"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Pattern(MonitorPattern::Main))).unwrap();
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    h.cmd(Command::FocusMonitor(MonitorTarget::Pattern(MonitorPattern::Sequence(3)))).unwrap();
    assert_eq!(h.focused().as_deref(), Some("FAR"));
}

#[test]
fn focus_monitor_direction_wraps_to_the_opposite_edge_and_rejects_dead_ends() {
    let mut h = three_displays();
    assert_eq!(h.cmd(focus_monitor(Direction::Left)), Err(WorkspaceError::NoSuchDisplay));
    assert_eq!(h.cmd(focus_monitor(Direction::Up)), Err(WorkspaceError::NoSuchDisplay));

    h.cmd(Command::FocusMonitor(MonitorTarget::Direction { direction: Direction::Left, wrap: true }))
        .unwrap();
    assert_eq!(h.focused().as_deref(), Some("FAR"));
    assert_eq!(h.cmd(focus_monitor(Direction::Right)), Err(WorkspaceError::NoSuchDisplay));
}

#[test]
fn focus_monitor_vertical_directions_use_top_left_coordinates() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.screens_named(&[
        ("TOP", 1, rect(0., 0., 1000., 1000.), None),
        ("BOTTOM", 2, rect(0., 1000., 1000., 1000.), None),
    ]);
    h.cmd(focus_monitor(Direction::Down)).unwrap();
    assert_eq!(h.focused().as_deref(), Some("BOTTOM"));
    h.cmd(focus_monitor(Direction::Up)).unwrap();
    assert_eq!(h.focused().as_deref(), Some("TOP"));
}

#[test]
fn move_workspace_to_monitor_by_pattern_and_no_op_on_the_same_display() {
    let mut h = three_displays();
    assert_eq!((h.active("LEFT"), h.active("MID"), h.active("FAR")), (Some(1), Some(3), Some(2)));

    h.cmd(Command::MoveWorkspaceToMonitor(MonitorTarget::Pattern(MonitorPattern::Name(
        "^Far$".into(),
    ))))
    .unwrap();
    assert_eq!(h.active("FAR"), Some(1));
    assert_eq!(h.focused().as_deref(), Some("FAR"));

    // Moving onto the display it is already on changes nothing.
    h.cmd(Command::MoveWorkspaceToMonitor(MonitorTarget::Pattern(MonitorPattern::Name(
        "^Far$".into(),
    ))))
    .unwrap();
    assert_eq!(h.active("FAR"), Some(1));
    assert_eq!(
        h.cmd(Command::MoveWorkspaceToMonitor(MonitorTarget::Pattern(MonitorPattern::Name(
            "^Nope$".into()
        )))),
        Err(WorkspaceError::NoSuchDisplay)
    );
}

#[test]
fn invalid_pattern_in_a_command_is_an_error() {
    let mut h = three_displays();
    assert!(matches!(
        h.cmd(Command::FocusMonitor(MonitorTarget::Pattern(MonitorPattern::Name("(".into())))),
        Err(WorkspaceError::BadPattern(_))
    ));
}

// ---- dowm: robustness ----

#[test]
fn many_windows_and_repeated_reconfiguration_keep_the_invariants() {
    let mut h = two(WorkspaceConfig::numbered(9));
    let windows: Vec<W> =
        (1..=200).map(|i| w(i, 1000 + i, if i % 2 == 0 { on_left() } else { on_right() })).collect();
    h.launch(PID, &windows);
    for round in 0..20u32 {
        h.cmd(Command::Workspace(round % 9 + 1)).unwrap();
        let _ = h.cmd(to_monitor(Direction::Right));
        h.screens_named(&[screen_left()]);
        h.screens_named(&both());
    }
    let owned: usize = (1..=9).map(|n| h.reactor.workspaces().windows_in(ws(n)).len()).sum();
    assert_eq!(owned, 200);
}

fn screen_mid() -> (&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>) {
    ("MID", 3, rect(1000., 0., 1000., 1000.), Some("Mid"))
}

fn screen_far() -> (&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>) {
    ("FAR", 4, rect(2000., 0., 1000., 1000.), Some("Far"))
}

fn all_three() -> Vec<(&'static str, u32, objc2_core_foundation::CGRect, Option<&'static str>)> {
    vec![screen_left(), screen_mid(), screen_far()]
}

/// The workspace state as one line, for assertion messages.
fn dump(h: &Harness) -> String {
    let s = h.reactor.workspaces();
    format!(
        "L={:?} M={:?} F={:?} focus={:?} prev={:?} homes={:?}",
        h.active("LEFT"),
        h.active("MID"),
        h.active("FAR"),
        h.focused(),
        s.previous_workspace().map(WorkspaceId::number),
        s.ids().map(|id| (id.number(), s.home_display(id).cloned(), s.display_of(id).cloned())).collect::<Vec<_>>()
    )
}

/// A tiny deterministic generator (LCG); the tests need repeatable randomness, not quality.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

fn random_op(rng: &mut Lcg, pool: u32, present: &mut [bool; 3], h: &mut Harness, log: &mut Vec<String>) {
    let dirs = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
    let dir = dirs[rng.below(4) as usize];
    match rng.below(9) {
        0..=2 => {
            let n = 1 + rng.below(u64::from(pool)) as u32;
            log.push(format!("workspace {n}"));
            let _ = h.cmd(Command::Workspace(n));
        }
        3 => {
            log.push(format!("to_monitor {dir:?}"));
            let _ = h.cmd(to_monitor(dir));
        }
        4 => {
            log.push(format!("focus_monitor {dir:?}"));
            let _ = h.cmd(focus_monitor(dir));
        }
        5 => {
            log.push("back_and_forth".into());
            let _ = h.cmd(Command::WorkspaceBackAndForth);
        }
        6 => {
            let n = 1 + rng.below(u64::from(pool)) as u32;
            let follow = rng.below(2) == 0;
            log.push(format!("move_node {n} follow={follow}"));
            let _ = h.cmd(move_node(n, None, follow));
        }
        _ => {
            // Connect or disconnect one of three displays (never leaving zero).
            let i = rng.below(3) as usize;
            let mut next = *present;
            next[i] = !next[i];
            if next.iter().any(|&p| p) {
                *present = next;
                let all = all_three();
                let uuid_i = all[i].0;
                let returning: Vec<WorkspaceId> = h
                    .reactor
                    .workspaces()
                    .ids()
                    .filter(|&id| {
                        h.reactor.workspaces().home_display(id).is_some_and(|home| {
                            home.display.as_str() == uuid_i && !home.was_active
                        })
                    })
                    .collect();
                let has_active_returning = h.reactor.workspaces().ids().any(|id| {
                    h.reactor
                        .workspaces()
                        .home_display(id)
                        .is_some_and(|home| home.display.as_str() == uuid_i && home.was_active)
                });
                let set: Vec<_> = all.iter().zip(next).filter(|&(_, p)| p).map(|(s, _)| *s).collect();
                log.push(format!("displays {next:?}"));
                let before_state = dump(h);
                h.screens_named(&set);
                if next[i] && has_active_returning && pool >= 8 {
                    // Workspaces that were hidden when this display left go back hidden.
                    for id in returning {
                        assert_eq!(h.reactor.workspaces().display_of(id), None, "ws {id}\nBEFORE {before_state}\nAFTER {}", dump(h));
                    }
                }
            }
        }
    }
}

/// Random commands and display changes over 1-3 displays and pools smaller and larger than
/// the display count; `Harness::send`/`cmd` assert every workspace and parking invariant
/// after each step. Also checks, on every reconnect that brings back an active workspace,
/// that the display's other returning workspaces go back hidden.
fn random_walk(seed: u64, pool: u32, windows: u32, steps: usize) {
    let mut rng = Lcg(seed);
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(pool));
    let mut present = [true, true, true];
    h.screens_named(&all_three());
    let frames: Vec<W> =
        (1..=windows).map(|i| w(i, 100 + i, rect(100. + 1000. * f64::from(i % 3), 100., 200., 150.))).collect();
    h.launch(PID, &frames);
    focus_window(&mut h, 1);
    let mut log = Vec::new();
    for _ in 0..steps {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            random_op(&mut rng, pool, &mut present, &mut h, &mut log);
        }));
        if let Err(e) = result {
            panic!("seed {seed} pool {pool}: failed after {:?}: {e:?}", log.last());
        }
    }
}

#[test]
fn random_commands_and_display_changes_keep_every_invariant() {
    for seed in 0..400u64 {
        for pool in [1u32, 2, 3, 5, 8] {
            random_walk(seed, pool, 4, 60);
        }
    }
}

/// 3a-1: a reconnect must put the returning workspaces that were not showing back hidden.
/// Minimised from a random walk: the display the returning workspace had been summoned onto
/// is re-plugged, and the plug used to pick a workspace that was itself on its way home.
#[test]
fn a_reconnect_puts_non_active_returning_workspaces_back_hidden() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(6));
    h.screens_named(&all_three());
    h.cmd(Command::Workspace(4)).unwrap(); // LEFT shows 4; 1 is hidden, last seen on LEFT
    h.screens_named(&[screen_mid(), screen_far()]); // LEFT leaves: 4 was active, 1 was not
    assert_eq!(h.reactor.workspaces().home_display(ws(4)).map(|home| home.was_active), Some(true));
    assert_eq!(h.reactor.workspaces().home_display(ws(1)).map(|home| home.was_active), Some(false));
    h.cmd(to_monitor(Direction::Right)).unwrap(); // MID's workspace moves to FAR; MID is re-plugged

    h.screens_named(&all_three());

    assert_eq!(h.active("LEFT"), Some(4), "the active workspace takes its display back");
    assert_eq!(h.reactor.workspaces().display_of(ws(1)), None, "the other goes home hidden");
    assert!(h.reactor.workspaces().home_display(ws(1)).is_none());
}

/// 3a-2: with the focused display unplugged, "back" must go to the workspace that was
/// focused, not silently stay on the previous one that is now the focused one.
#[test]
fn back_and_forth_after_the_focused_display_is_unplugged_goes_to_the_workspace_that_was_focused() {
    let mut h = two(WorkspaceConfig::numbered(4));
    h.cmd(Command::Workspace(2)).unwrap(); // focus RIGHT (showing 2); previous is 1
    assert_eq!(h.focused().as_deref(), Some("RIGHT"));
    assert_eq!(h.reactor.workspaces().previous_workspace(), Some(ws(1)));

    h.screens_named(&[screen_left()]); // RIGHT goes: focus lands on LEFT, showing 1
    assert_eq!(h.focused().as_deref(), Some("LEFT"));
    assert_eq!(h.active("LEFT"), Some(1));

    h.cmd(Command::WorkspaceBackAndForth).unwrap();

    assert_eq!(h.active("LEFT"), Some(2), "back goes to the workspace that was focused");
}

/// 3a-3: an error must not leave the command half done.
#[test]
fn move_node_with_follow_fails_before_changing_membership_when_there_is_no_display() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.launch(PID, &[w(1, 11, on_left())]); // no display is known yet
    let before = h.workspace_of(wid(PID, 1));

    let result = h.cmd(move_node(3, Some(1), true));

    assert_eq!(result, Err(WorkspaceError::NoDisplays));
    assert_eq!(h.workspace_of(wid(PID, 1)), before, "the window did not move");
}

#[test]
fn move_workspace_to_monitor_on_a_display_without_a_workspace_says_so() {
    // One workspace, two displays: the second display has nothing to move.
    let mut h = two(WorkspaceConfig::numbered(1));
    assert_eq!((h.active("LEFT"), h.active("RIGHT")), (Some(1), None));
    h.cmd(focus_monitor(Direction::Right)).unwrap();

    let result = h.cmd(to_monitor(Direction::Left));

    assert_eq!(result, Err(WorkspaceError::NoWorkspaceHere));
}

#[test]
fn a_destroyed_window_is_no_longer_the_focused_window() {
    let mut h = two(WorkspaceConfig::numbered(4));
    h.launch(PID, &[w(1, 11, on_left())]);
    focus_window(&mut h, 1);
    h.cgs_destroyed(11);

    let result = h.cmd(move_node(3, None, false));

    assert_eq!(result, Err(WorkspaceError::NoFocusedWindow));
}
