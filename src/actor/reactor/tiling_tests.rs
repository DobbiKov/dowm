//! M3 tiling scenarios driven by synthetic events (see `testing`).
//!
//! One display (900x600) unless a test says otherwise, so equal thirds come out as exact
//! 300-pixel columns. `Harness::send`, `cmd` and `tile` check every reactor invariant,
//! the tiling-tree ones included, after each step.

use std::collections::BTreeMap;

use objc2_core_foundation::CGRect;

use super::testing::{Harness, W, rect, w, wid};
use super::{Command, CommandError, Event, Requested};
use crate::actor::app::{Quiet, WindowId};
use crate::layout::ops::{LayoutArg, ResizeDimension, SplitArg, TilingError};
use crate::model::tree::NormalizeOptions;
use crate::model::workspace::{Direction, WorkspaceConfig, WorkspaceId};

const PID: i32 = 100;

fn ws(n: u32) -> WorkspaceId { WorkspaceId::new(n) }

/// `n` windows (indexes 1..=n, WindowServer ids 10+idx) on one 900x600 display.
fn setup(n: u32) -> Harness {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(9));
    h.screens(&[("D", 1, rect(0., 0., 900., 600.))]);
    if n > 0 {
        let windows: Vec<W> = (1..=n).map(|i| w(i, 10 + i, rect(10., 10., 100., 100.))).collect();
        h.launch(PID, &windows);
    }
    h
}

fn focus(h: &mut Harness, idx: u32) {
    h.send(Event::ApplicationMainWindowChanged(PID, Some(wid(PID, idx)), Quiet::No));
}

fn tree(h: &Harness, n: u32) -> String {
    h.reactor.trees().get(ws(n)).map(|t| t.describe()).unwrap_or_default()
}

fn frames(h: &Harness, n: u32) -> BTreeMap<WindowId, CGRect> {
    h.reactor.workspace_frames(ws(n)).expect("workspace has a display").into_iter().collect()
}

/// (x, y, width, height) of window `idx` on workspace 1, rounded to whole pixels.
fn px(h: &Harness, idx: u32) -> (i64, i64, i64, i64) { px_on(h, 1, idx) }

fn px_on(h: &Harness, n: u32, idx: u32) -> (i64, i64, i64, i64) {
    let f = frames(h, n)[&wid(PID, idx)];
    (f.origin.x.round() as i64, f.origin.y.round() as i64, f.size.width.round() as i64, f.size.height.round() as i64)
}

fn focus_cmd(direction: Direction) -> Command { Command::Focus { direction, wrap: false } }

fn move_node(workspace: u32, window: u32) -> Command {
    Command::MoveNodeToWorkspace { workspace: workspace.to_string(), window: Some(wid(PID, window)), follow: false }
}

// ---- hooks into the workspace model ----

#[test]
fn launched_windows_become_tiled_leaves_laid_out_side_by_side() {
    let h = setup(3);
    assert_eq!(tree(&h, 1), "h[1 2 3]");
    assert_eq!(px(&h, 1), (0, 0, 300, 600));
    assert_eq!(px(&h, 2), (300, 0, 300, 600));
    assert_eq!(px(&h, 3), (600, 0, 300, 600));
}

#[test]
fn a_new_window_lands_after_the_focused_one() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.ax_created(PID, w(4, 14, rect(10., 10., 100., 100.)));
    assert_eq!(tree(&h, 1), "h[1 4 2 3]");
    assert_eq!(px(&h, 4), (225, 0, 225, 600));
}

#[test]
fn a_destroyed_window_leaves_the_tree_and_the_others_reflow() {
    let mut h = setup(3);
    h.cgs_destroyed(12);
    assert_eq!(tree(&h, 1), "h[1 3]");
    assert_eq!(px(&h, 1), (0, 0, 450, 600));
    assert_eq!(px(&h, 3), (450, 0, 450, 600));
}

#[test]
fn move_node_to_workspace_moves_the_leaf() {
    let mut h = setup(3);
    h.cmd(move_node(2, 2)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 3]");
    assert_eq!(tree(&h, 2), "h[2]");
    assert_eq!(h.reactor.trees().get(ws(2)).unwrap().tiled_windows(), vec![wid(PID, 2)]);
}

#[test]
fn a_floating_window_stays_floating_when_it_changes_workspace() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    h.cmd(move_node(2, 2)).unwrap();
    let t = h.reactor.trees().get(ws(2)).unwrap();
    assert!(t.is_floating(wid(PID, 2)) && !t.is_tiled(wid(PID, 2)));
    assert_eq!(tree(&h, 1), "h[1 3]");
}

#[test]
fn a_window_dragged_to_another_display_moves_to_its_workspace_tree() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.screens(&[("L", 1, rect(0., 0., 1000., 600.)), ("R", 2, rect(1000., 0., 1000., 600.))]);
    h.launch(PID, &[w(1, 11, rect(10., 10., 100., 100.)), w(2, 12, rect(20., 20., 100., 100.))]);
    assert_eq!(tree(&h, 1), "h[1 2]");
    h.send(Event::WindowFrameChanged(wid(PID, 2), rect(1200., 100., 100., 100.), None, Requested(false), None));
    assert_eq!(tree(&h, 1), "h[1]");
    assert_eq!(tree(&h, 2), "h[2]");
    assert_eq!(px_on(&h, 2, 2), (1000, 0, 1000, 600), "laid out on the right display");
}

#[test]
fn hidden_workspaces_keep_their_trees_and_frames() {
    let mut h = setup(2);
    h.cmd(Command::Workspace("2".into())).unwrap();
    assert_eq!(tree(&h, 1), "h[1 2]");
    assert_eq!(px(&h, 1), (0, 0, 450, 600), "still laid out for when it returns");
    assert!(h.reactor.layout_targets().iter().all(|t| t.window != wid(PID, 1)), "but not a target");
}

#[test]
fn layout_targets_cover_the_visible_workspaces_only() {
    let mut h = setup(3);
    h.cmd(move_node(2, 3)).unwrap();
    let targets: BTreeMap<WindowId, CGRect> =
        h.reactor.layout_targets().into_iter().map(|t| (t.window, t.frame)).collect();
    assert_eq!(targets.keys().copied().collect::<Vec<_>>(), vec![wid(PID, 1), wid(PID, 2)]);
}

#[test]
fn two_displays_lay_out_their_own_workspaces() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(4));
    h.screens(&[("L", 1, rect(0., 0., 1000., 600.)), ("R", 2, rect(1000., 0., 1000., 600.))]);
    h.launch(PID, &[
        w(1, 11, rect(10., 10., 100., 100.)),
        w(2, 12, rect(20., 20., 100., 100.)),
        w(3, 13, rect(1100., 20., 100., 100.)),
    ]);
    assert_eq!(px_on(&h, 1, 1), (0, 0, 500, 600));
    assert_eq!(px_on(&h, 2, 3), (1000, 0, 1000, 600));
}

// ---- focus ----

#[test]
fn focus_moves_to_the_neighbour_and_updates_the_focused_window() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(focus_cmd(Direction::Right)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
    h.tile(focus_cmd(Direction::Right)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 3)));
    h.tile(focus_cmd(Direction::Left)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 2)));
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().focused_window(), Some(wid(PID, 2)));
}

#[test]
fn focus_stops_at_the_edge_and_wraps_on_request() {
    let mut h = setup(3);
    focus(&mut h, 3);
    h.tile(focus_cmd(Direction::Right)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 3)));
    h.tile(focus_cmd(Direction::Up)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 3)));
    h.tile(Command::Focus { direction: Direction::Right, wrap: true }).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn focus_from_a_floating_window_works_from_the_tree_focus() {
    let mut h = setup(3);
    focus(&mut h, 2);
    focus(&mut h, 3);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap(); // 3 floats; 2 was focused before
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 3)));
    h.tile(focus_cmd(Direction::Left)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)), "tree focus was 2; its left is 1");
}

#[test]
fn focus_without_any_window_says_so() {
    let mut h = setup(0);
    assert_eq!(h.tile(focus_cmd(Direction::Left)), Err(TilingError::NoFocusedWindow));
}

#[test]
fn tiling_commands_need_a_workspace() {
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(3));
    assert_eq!(h.tile(Command::Fullscreen), Err(TilingError::NoWorkspaceHere));
}

// ---- move ----

#[test]
fn move_swaps_the_window_with_its_neighbour() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(Command::Move(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[2 1 3]");
    assert_eq!(px(&h, 1), (300, 0, 300, 600));
    assert_eq!(px(&h, 2), (0, 0, 300, 600));
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 1)));
}

#[test]
fn move_at_the_edge_fails_without_changing_the_tree() {
    let mut h = setup(3);
    focus(&mut h, 1);
    assert_eq!(h.tile(Command::Move(Direction::Left)), Err(TilingError::AtBoundary));
    assert_eq!(h.tile(Command::Move(Direction::Up)), Err(TilingError::AtBoundary));
    assert_eq!(tree(&h, 1), "h[1 2 3]");
}

#[test]
fn move_into_and_out_of_a_container() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 v[2 3]]");
    h.tile(Command::Move(Direction::Left)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 2 3]", "leaving a two-window column flattens it");
    h.tile(Command::Move(Direction::Left)).unwrap(); // 2 swaps with 1
    h.tile(Command::Move(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 2 3]");
}

// ---- join-with ----

#[test]
fn join_with_stacks_the_window_and_its_neighbour_in_a_column() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 v[2 3]]");
    assert_eq!(px(&h, 1), (0, 0, 300, 600));
    assert_eq!(px(&h, 2), (300, 0, 600, 300));
    assert_eq!(px(&h, 3), (300, 300, 600, 300));
}

#[test]
fn join_with_nothing_there_fails() {
    let mut h = setup(3);
    focus(&mut h, 3);
    assert_eq!(h.tile(Command::JoinWith(Direction::Right)), Err(TilingError::NoNeighbor));
    assert_eq!(h.tile(Command::JoinWith(Direction::Down)), Err(TilingError::NoNeighbor));
}

// ---- split ----

#[test]
fn split_on_a_lone_window_picks_the_direction_new_windows_arrive_in() {
    let mut h = setup(1);
    focus(&mut h, 1);
    h.tile(Command::Split(SplitArg::Vertical)).unwrap();
    assert_eq!(tree(&h, 1), "v[1]");
    h.ax_created(PID, w(2, 12, rect(10., 10., 100., 100.)));
    assert_eq!(tree(&h, 1), "v[1 2]");
    assert_eq!(px(&h, 2), (0, 300, 900, 300));
    h.cgs_destroyed(12);
    h.tile(Command::Split(SplitArg::Opposite)).unwrap();
    assert_eq!(tree(&h, 1), "h[1]");
}

#[test]
fn split_among_siblings_is_refused_while_containers_flatten() {
    let mut h = setup(3);
    focus(&mut h, 2);
    assert_eq!(h.tile(Command::Split(SplitArg::Vertical)), Err(TilingError::SplitDisabled));
    assert_eq!(tree(&h, 1), "h[1 2 3]");
}

#[test]
fn split_wraps_the_window_when_flattening_is_off() {
    let mut h = setup(3);
    h.reactor.set_normalization(NormalizeOptions { flatten_containers: false, opposite_orientation: true });
    focus(&mut h, 2);
    h.tile(Command::Split(SplitArg::Vertical)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 v[2] 3]");
    assert_eq!(px(&h, 2), (300, 0, 300, 600));
}

// ---- resize ----

fn near(h: &Harness, idx: u32, expect: (i64, i64, i64, i64)) {
    let (x, y, wd, ht) = px(h, idx);
    let close = |a: i64, b: i64| (a - b).abs() <= 1;
    assert!(
        close(x, expect.0) && close(y, expect.1) && close(wd, expect.2) && close(ht, expect.3),
        "window {idx}: {:?} vs {expect:?}",
        (x, y, wd, ht)
    );
}

#[test]
fn resize_width_takes_pixels_from_the_siblings() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: 90 }).unwrap();
    near(&h, 1, (0, 0, 390, 600));
    near(&h, 2, (390, 0, 255, 600));
    near(&h, 3, (645, 0, 255, 600));
    h.tile(Command::Resize { dimension: ResizeDimension::Smart, amount: -90 }).unwrap();
    near(&h, 1, (0, 0, 300, 600));
}

#[test]
fn resize_height_needs_a_vertical_split() {
    let mut h = setup(3);
    focus(&mut h, 1);
    assert_eq!(
        h.tile(Command::Resize { dimension: ResizeDimension::Height, amount: 10 }),
        Err(TilingError::NothingToResize)
    );
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    h.tile(Command::Resize { dimension: ResizeDimension::Height, amount: 60 }).unwrap();
    near(&h, 2, (300, 0, 600, 360));
    near(&h, 3, (300, 360, 600, 240));
}

#[test]
fn resize_smart_with_a_lone_window_has_nothing_to_do() {
    let mut h = setup(1);
    focus(&mut h, 1);
    assert_eq!(
        h.tile(Command::Resize { dimension: ResizeDimension::Smart, amount: 10 }),
        Err(TilingError::NothingToResize)
    );
}

// ---- layout ----

#[test]
fn layout_accordion_overlaps_the_windows_with_a_peek() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Accordion])).unwrap();
    assert_eq!(tree(&h, 1), "h@accordion[1 2 3]");
    assert_eq!(px(&h, 1), (0, 0, 840, 600));
    assert_eq!(px(&h, 2), (30, 0, 840, 600));
    assert_eq!(px(&h, 3), (60, 0, 840, 600));
}

#[test]
fn layout_with_several_arguments_toggles() {
    let mut h = setup(2);
    focus(&mut h, 1);
    let toggle = Command::Layout(vec![LayoutArg::Tiles, LayoutArg::Accordion]);
    h.tile(toggle.clone()).unwrap();
    assert_eq!(tree(&h, 1), "h@accordion[1 2]");
    h.tile(toggle).unwrap();
    assert_eq!(tree(&h, 1), "h[1 2]");
}

#[test]
fn layout_vertical_restacks_the_windows() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(Command::Layout(vec![LayoutArg::Vertical])).unwrap();
    assert_eq!(tree(&h, 1), "v[1 2 3]");
    assert_eq!(px(&h, 2), (0, 200, 900, 200));
}

#[test]
fn layout_floating_takes_a_window_out_of_the_tiling_and_back() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    assert_eq!(tree(&h, 1), "h[1 3]");
    assert_eq!(px(&h, 1), (0, 0, 450, 600), "the others fill the space");
    assert_eq!(px(&h, 2), (10, 10, 100, 100), "the floating window keeps its own frame");
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    assert!(h.reactor.trees().get(ws(1)).unwrap().is_tiled(wid(PID, 2)));
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().floating_windows(), vec![]);
}

#[test]
fn tree_commands_refuse_a_floating_window() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    assert_eq!(h.tile(Command::Move(Direction::Left)), Err(TilingError::WindowFloating(wid(PID, 2))));
    assert_eq!(h.tile(Command::JoinWith(Direction::Left)), Err(TilingError::WindowFloating(wid(PID, 2))));
}

#[test]
fn layout_without_arguments_fails() {
    let mut h = setup(1);
    focus(&mut h, 1);
    assert_eq!(h.tile(Command::Layout(vec![])), Err(TilingError::EmptyLayoutList));
}

// ---- fullscreen ----

#[test]
fn fullscreen_fills_the_display_and_toggles_back() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Fullscreen).unwrap();
    assert_eq!(px(&h, 2), (0, 0, 900, 600));
    assert_eq!(px(&h, 1), (0, 0, 300, 600), "the others are laid out underneath");
    h.tile(Command::Fullscreen).unwrap();
    assert_eq!(px(&h, 2), (300, 0, 300, 600));
}

#[test]
fn fullscreen_ends_when_the_window_goes() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Fullscreen).unwrap();
    h.cgs_destroyed(12);
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), None);
    assert_eq!(px(&h, 3), (450, 0, 450, 600));
}

#[test]
fn fullscreen_ends_when_the_window_changes_workspace() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Fullscreen).unwrap();
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), Some(wid(PID, 2)));
    h.cmd(move_node(2, 2)).unwrap();
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), None, "cleared where it was set");
    assert_eq!(h.reactor.trees().get(ws(2)).unwrap().fullscreen(), None);
    h.cmd(Command::Workspace("2".into())).unwrap();
    // Alone on workspace 2 it fills the display because it is the only tile, not because
    // fullscreen followed it: put a second window beside it and it must share the row.
    h.cmd(move_node(2, 3)).unwrap();
    let f2 = frames(&h, 2);
    assert_eq!(f2[&wid(PID, 2)].size.width.round() as i64, 450, "window 2 is not full-display in ws 2");
    assert_ne!(f2[&wid(PID, 2)], rect(0., 0., 900., 600.));
}

#[test]
fn a_floating_window_can_go_fullscreen() {
    let mut h = setup(2);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    h.tile(Command::Fullscreen).unwrap();
    assert_eq!(px(&h, 2), (0, 0, 900, 600));
}

// ---- balance-sizes and flatten-workspace-tree ----

#[test]
fn balance_sizes_equalizes_every_container() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: 120 }).unwrap();
    assert_ne!(px(&h, 1).2, 300);
    h.tile(Command::BalanceSizes).unwrap();
    near(&h, 1, (0, 0, 300, 600));
    near(&h, 2, (300, 0, 300, 600));
}

#[test]
fn balance_sizes_reaches_nested_containers() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    h.tile(Command::Resize { dimension: ResizeDimension::Height, amount: 100 }).unwrap();
    h.tile(Command::BalanceSizes).unwrap();
    near(&h, 1, (0, 0, 450, 600));
    near(&h, 2, (450, 0, 450, 300));
    near(&h, 3, (450, 300, 450, 300));
}

#[test]
fn flatten_workspace_tree_collapses_to_one_row() {
    let mut h = setup(4);
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 v[2 3] 4]");
    h.tile(Command::FlattenWorkspaceTree).unwrap();
    assert_eq!(tree(&h, 1), "h[1 2 3 4]");
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().focused_window(), Some(wid(PID, 2)));
    near(&h, 4, (675, 0, 225, 600));
}

#[test]
fn flatten_and_balance_work_on_an_empty_workspace() {
    let mut h = setup(0);
    h.tile(Command::FlattenWorkspaceTree).unwrap();
    h.tile(Command::BalanceSizes).unwrap();
    assert_eq!(tree(&h, 1), "h[]");
}

// ---- plumbing ----

#[test]
fn the_command_event_runs_tiling_commands_and_survives_their_errors() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.send(Event::Command(Command::Move(Direction::Right)));
    assert_eq!(tree(&h, 1), "h[2 1 3]");
    h.send(Event::Command(Command::Move(Direction::Up))); // fails, logged
    assert_eq!(tree(&h, 1), "h[2 1 3]");
}

#[test]
fn command_errors_display_and_convert() {
    let tiling: CommandError = TilingError::AtBoundary.into();
    assert_eq!(tiling.to_string(), TilingError::AtBoundary.to_string());
    let workspace: CommandError = crate::model::workspace::WorkspaceError::NoDisplays.into();
    assert!(!workspace.to_string().is_empty());
}

#[test]
fn a_custom_layout_system_can_be_registered_on_the_reactor() {
    use crate::layout::{Arrange, Hints, LayoutSystem, Rect};
    use crate::model::tree::LayoutKind;

    #[derive(Debug)]
    struct Stack;
    impl LayoutSystem for Stack {
        fn kind(&self) -> LayoutKind { LayoutKind::new("stack") }

        fn hints(&self) -> Hints {
            Hints { in_tree: true, overlapping: true, weighted: false, alternates: true, flattens: true }
        }

        fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> { vec![input.area; input.children.len()] }
    }

    let mut h = setup(2);
    h.reactor.register_layout(Box::new(Stack));
    let tree = h.reactor.tree_mut_for_test(ws(1));
    let root = tree.root();
    tree.set_layout(root, LayoutKind::new("stack"));
    assert_eq!(px(&h, 1), (0, 0, 900, 600));
    assert_eq!(px(&h, 2), (0, 0, 900, 600));
}

// ---- review fixes ----

#[test]
fn resize_to_the_extremes_never_squeezes_a_sibling_below_the_minimum() {
    let mut h = setup(3);
    focus(&mut h, 1);
    h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: 100_000 }).unwrap();
    focus(&mut h, 2);
    // The double grow: the second window also takes everything it may.
    h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: 100_000 }).unwrap();
    let widths: Vec<i64> = (1..=3).map(|i| px(&h, i).2).collect();
    assert!(widths.iter().all(|&x| x >= 44), "{widths:?}");
    assert!((widths.iter().sum::<i64>() - 900).abs() <= 1, "{widths:?}");
    assert!(widths[1] > 800, "{widths:?}");
    h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: -100_000 }).unwrap();
    let widths: Vec<i64> = (1..=3).map(|i| px(&h, i).2).collect();
    assert!(widths.iter().all(|&x| x >= 44), "{widths:?}");
    assert!((widths.iter().sum::<i64>() - 900).abs() <= 1, "{widths:?}");
}

#[test]
fn focus_enters_a_nested_container_at_its_most_recent_window() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::JoinWith(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 v[2 3]]");
    for (remembered, expected) in [(3, 3), (2, 2)] {
        focus(&mut h, remembered);
        focus(&mut h, 1);
        h.tile(focus_cmd(Direction::Right)).unwrap();
        assert_eq!(h.reactor.focused_window(), Some(wid(PID, expected)), "entered at {remembered}");
    }
}

#[test]
fn new_windows_and_syncs_do_not_steal_the_trees_focus() {
    let mut h = setup(3);
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().focused_window(), Some(wid(PID, 1)));
    focus(&mut h, 2);
    h.ax_created(PID, w(4, 14, rect(10., 10., 100., 100.)));
    assert_eq!(tree(&h, 1), "h[1 2 4 3]");
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().focused_window(), Some(wid(PID, 2)));
    h.cmd(move_node(2, 3)).unwrap();
    assert_eq!(h.reactor.trees().get(ws(2)).unwrap().focused_window(), Some(wid(PID, 3)));
}

#[test]
fn focus_while_fullscreen_leaves_fullscreen_and_moves_focus() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Fullscreen).unwrap();
    h.tile(focus_cmd(Direction::Right)).unwrap();
    assert_eq!(h.reactor.focused_window(), Some(wid(PID, 3)));
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), None);
    assert_eq!(px(&h, 2), (300, 0, 300, 600));
    // A focus that goes nowhere keeps it.
    focus(&mut h, 3);
    h.tile(Command::Fullscreen).unwrap();
    h.tile(focus_cmd(Direction::Right)).unwrap();
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), Some(wid(PID, 3)));
}

#[test]
fn move_while_fullscreen_keeps_the_window_fullscreen() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Fullscreen).unwrap();
    h.tile(Command::Move(Direction::Right)).unwrap();
    assert_eq!(tree(&h, 1), "h[1 3 2]");
    assert_eq!(h.reactor.trees().get(ws(1)).unwrap().fullscreen(), Some(wid(PID, 2)));
    assert_eq!(px(&h, 2), (0, 0, 900, 600));
    assert_eq!(px(&h, 3), (300, 0, 300, 600), "the rest reflowed underneath");
}

#[test]
fn minimized_windows_take_no_space_and_come_back() {
    let mut h = setup(3);
    h.send(Event::WindowMinimized(wid(PID, 2)));
    assert_eq!(tree(&h, 1), "h[1 3]");
    assert_eq!(px(&h, 1), (0, 0, 450, 600));
    assert_eq!(h.workspace_of(wid(PID, 2)), Some(1), "still owned by its workspace");
    h.send(Event::WindowDeminiaturized(wid(PID, 2)));
    assert!(h.reactor.trees().get(ws(1)).unwrap().is_tiled(wid(PID, 2)));
    assert_eq!(px(&h, 1).2, 300);
}

#[test]
fn a_floating_window_is_still_floating_after_a_minimize_cycle() {
    let mut h = setup(3);
    focus(&mut h, 2);
    h.tile(Command::Layout(vec![LayoutArg::Floating])).unwrap();
    h.send(Event::WindowMinimized(wid(PID, 2)));
    assert!(!h.reactor.trees().get(ws(1)).unwrap().contains(wid(PID, 2)));
    h.send(Event::WindowDeminiaturized(wid(PID, 2)));
    assert!(h.reactor.trees().get(ws(1)).unwrap().is_floating(wid(PID, 2)));
}

#[test]
fn layout_targets_say_which_windows_overlap_and_which_to_raise() {
    let mut h = setup(3);
    focus(&mut h, 2);
    let tiles = h.reactor.layout_targets();
    assert!(tiles.iter().all(|t| !t.overlapping && !t.raise));
    h.tile(Command::Layout(vec![LayoutArg::Accordion])).unwrap();
    let stacked = h.reactor.layout_targets();
    assert!(stacked.iter().all(|t| t.overlapping));
    let raised: Vec<_> = stacked.iter().filter(|t| t.raise).map(|t| t.window).collect();
    assert_eq!(raised, vec![wid(PID, 2)], "the focused window is the one to bring forward");
    focus(&mut h, 3);
    let raised: Vec<_> = h.reactor.layout_targets().iter().filter(|t| t.raise).map(|t| t.window).collect();
    assert_eq!(raised, vec![wid(PID, 3)]);
}

#[test]
fn resize_passes_over_unweighted_layouts() {
    let mut h = setup(2);
    focus(&mut h, 1);
    h.tile(Command::Layout(vec![LayoutArg::Accordion])).unwrap();
    assert_eq!(
        h.tile(Command::Resize { dimension: ResizeDimension::Width, amount: 50 }),
        Err(TilingError::NothingToResize),
        "accordions have no weights to trade"
    );
}

#[test]
fn registered_systems_are_reachable_by_name_and_policed_by_their_hints() {
    use crate::layout::{Arrange, Hints, LayoutSystem, Rect};
    use crate::model::tree::LayoutKind;

    #[derive(Debug)]
    struct Free;
    impl LayoutSystem for Free {
        fn kind(&self) -> LayoutKind { LayoutKind::new("free") }

        fn hints(&self) -> Hints {
            Hints { in_tree: false, overlapping: true, weighted: false, alternates: false, flattens: false }
        }

        fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> { vec![input.area; input.children.len()] }
    }

    let mut h = setup(2);
    focus(&mut h, 1);
    h.reactor.register_layout(Box::new(Free));
    let free = LayoutKind::new("free");
    assert_eq!(
        h.tile(Command::Layout(vec![LayoutArg::Named(free)])),
        Err(TilingError::NotAContainerLayout(free))
    );
    let nope = LayoutKind::new("nope");
    assert_eq!(h.tile(Command::Layout(vec![LayoutArg::Named(nope)])), Err(TilingError::UnknownLayout(nope)));
    h.tile(Command::Layout(vec![LayoutArg::Named(LayoutKind::new("accordion"))])).unwrap();
    assert_eq!(tree(&h, 1), "h@accordion[1 2]");
}

#[test]
fn command_errors_expose_their_source() {
    use std::error::Error;
    let tiling: CommandError = TilingError::AtBoundary.into();
    assert_eq!(tiling.source().map(ToString::to_string), Some(TilingError::AtBoundary.to_string()));
    let workspace: CommandError = crate::model::workspace::WorkspaceError::NoDisplays.into();
    assert!(workspace.source().is_some());
}

// ---- randomized invariants ----

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }

    fn below(&mut self, n: u64) -> u64 { self.next() % n }
}

/// The tree invariants, restated from the public accessors so that a bug in the reactor's
/// own `check_invariants` cannot hide one: no single-child non-root container, nested
/// containers alternate, weights are positive, every tiled window sits in exactly one
/// leaf, and tree membership is the workspace store's.
fn assert_tree_invariants(h: &Harness) {
    let r = &h.reactor;
    let mut seen: BTreeMap<WindowId, WorkspaceId> = BTreeMap::new();
    for id in r.workspaces().ids() {
        let Some(t) = r.trees().get(id) else { continue };
        for container in t.containers() {
            let kids = t.children(container);
            if container != t.root() {
                assert!(kids.len() >= 2, "ws {id}: {container:?} has {} children in {}", kids.len(), t.describe());
                assert_ne!(
                    t.orientation(container),
                    t.parent(container).and_then(|p| t.orientation(p)),
                    "ws {id}: nested containers must alternate in {}",
                    t.describe()
                );
            }
            for &kid in kids {
                let weight = t.weight(kid).unwrap_or(0.0);
                assert!(weight > 0.0 && weight.is_finite(), "ws {id}: bad weight in {}", t.describe());
            }
        }
        for window in t.tiled_windows().into_iter().chain(t.floating_windows()) {
            assert!(seen.insert(window, id).is_none(), "{window:?} is in two places");
            assert_eq!(r.workspaces().workspace_of(window), Some(id), "{window:?}: tree and store disagree");
        }
        let leaves = t.tiled_windows();
        assert!(leaves.iter().all(|&win| t.is_tiled(win) && !t.is_floating(win)));
        // Frames exist for exactly the windows in the tree.
        if let Some(frames) = r.workspace_frames(id) {
            let mut got: Vec<_> = frames.iter().map(|(win, _)| *win).collect();
            let mut want: Vec<_> = leaves.into_iter().chain(t.floating_windows()).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "ws {id}: frames and tree windows differ");
        }
    }
    for record in r.windows().iter() {
        assert_eq!(
            seen.contains_key(&record.wid),
            !record.is_minimized,
            "{:?}: minimized windows are in no tree, the others in one",
            record.wid
        );
    }
}

fn fingerprints(h: &Harness) -> Vec<(WorkspaceId, String)> {
    h.reactor.trees().iter().map(|(id, t)| (id, t.fingerprint())).collect()
}

/// Every container's children and their weights, by (workspace, container).
fn container_weights(h: &Harness) -> BTreeMap<(WorkspaceId, crate::model::tree::NodeId), (Vec<crate::model::tree::NodeId>, Vec<f64>)> {
    let mut out = BTreeMap::new();
    for (id, t) in h.reactor.trees().iter() {
        for c in t.containers() {
            let kids = t.children(c).to_vec();
            let weights = kids.iter().map(|&k| t.weight(k).unwrap()).collect();
            out.insert((id, c), (kids, weights));
        }
    }
    out
}

/// `Harness::tile`, plus: a failed command left every tree (weights, focus order,
/// fullscreen included) exactly as it was, and a successful resize kept the weights of
/// the container it touched positive, conserved, and at least the minimum share.
fn tile_checked(h: &mut Harness, command: Command) -> Result<(), TilingError> {
    let trees_before = fingerprints(h);
    let weights_before = container_weights(h);
    let result = h.tile(command.clone());
    match (&result, &command) {
        (Err(error), _) => {
            assert_eq!(fingerprints(h), trees_before, "{command:?} failed with {error} but changed a tree");
        }
        (Ok(()), Command::Resize { .. }) => {
            for (key, (kids, after)) in container_weights(h) {
                let Some((old_kids, before)) = weights_before.get(&key) else { continue };
                if *old_kids != kids || *before == after {
                    continue;
                }
                let total: f64 = before.iter().sum();
                assert!((after.iter().sum::<f64>() - total).abs() < 1e-6, "{command:?}: total changed");
                let floor = total * 0.05f64.min(0.5 / kids.len() as f64);
                for (&b, &a) in before.iter().zip(&after) {
                    assert!(a >= floor.min(b) - 1e-6, "{command:?}: {a} below the floor {floor} (was {b})");
                }
            }
        }
        _ => {}
    }
    result
}

fn random_walk(seed: u64, steps: usize) {
    let mut rng = Lcg(seed);
    let mut h = Harness::with_workspaces(WorkspaceConfig::numbered(3));
    let screens = [("A", 1, rect(0., 0., 900., 600.)), ("B", 2, rect(900., 0., 900., 600.))];
    h.screens(&screens);
    let mut alive: Vec<u32> = Vec::new();
    let mut next_idx = 1u32;
    let dirs = [Direction::Left, Direction::Right, Direction::Up, Direction::Down];
    let mut log: Vec<String> = Vec::new();
    h.launch(PID, &[]);
    for _ in 0..steps {
        let dir = dirs[rng.below(4) as usize];
        let op = rng.below(26);
        log.push(format!("op {op} {dir:?}"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match op {
                0..=2 => {
                    let idx = next_idx;
                    next_idx += 1;
                    let x = if rng.below(2) == 0 { 10. } else { 1000. };
                    h.ax_created(PID, w(idx, 10 + idx, rect(x, 10., 100., 100.)));
                    alive.push(idx);
                }
                3 => {
                    if !alive.is_empty() {
                        let idx = alive.remove(rng.below(alive.len() as u64) as usize);
                        h.cgs_destroyed(10 + idx);
                    }
                }
                4 | 5 => {
                    if !alive.is_empty() {
                        focus(&mut h, alive[rng.below(alive.len() as u64) as usize]);
                    }
                }
                6 | 7 => { let _ = tile_checked(&mut h, Command::Focus { direction: dir, wrap: rng.below(2) == 0 }); }
                8 | 9 => { let _ = tile_checked(&mut h, Command::Move(dir)); }
                10 | 11 => { let _ = tile_checked(&mut h, Command::JoinWith(dir)); }
                12 => {
                    let arg = [SplitArg::Horizontal, SplitArg::Vertical, SplitArg::Opposite][rng.below(3) as usize];
                    let _ = tile_checked(&mut h, Command::Split(arg));
                }
                13 | 14 => {
                    let dimension = [ResizeDimension::Width, ResizeDimension::Height, ResizeDimension::Smart][rng.below(3) as usize];
                    let amount = rng.below(401) as i32 - 200;
                    let _ = tile_checked(&mut h, Command::Resize { dimension, amount });
                }
                15 | 16 => {
                    let all = [
                        LayoutArg::Tiles, LayoutArg::Accordion, LayoutArg::Floating, LayoutArg::Horizontal,
                        LayoutArg::Vertical, LayoutArg::HTiles, LayoutArg::VTiles, LayoutArg::HAccordion,
                        LayoutArg::VAccordion,
                    ];
                    let n = 1 + rng.below(2) as usize;
                    let args: Vec<_> = (0..n).map(|_| all[rng.below(9) as usize]).collect();
                    let _ = tile_checked(&mut h, Command::Layout(args));
                }
                17 => { let _ = tile_checked(&mut h, Command::Fullscreen); }
                18 => { let _ = tile_checked(&mut h, Command::BalanceSizes); }
                19 => { let _ = tile_checked(&mut h, Command::FlattenWorkspaceTree); }
                20 => { let _ = h.cmd(Command::Workspace((1 + rng.below(3)).to_string())); }
                21 | 22 => { let _ = h.cmd(move_node_focused(1 + rng.below(3) as u32)); }
                24 | 25 => {
                    if !alive.is_empty() {
                        let idx = alive[rng.below(alive.len() as u64) as usize];
                        let event = if op == 24 {
                            Event::WindowMinimized(wid(PID, idx))
                        } else {
                            Event::WindowDeminiaturized(wid(PID, idx))
                        };
                        h.send(event);
                    }
                }
                _ => {
                    // The second display comes and goes.
                    let set: &[_] = if rng.below(2) == 0 { &screens[..1] } else { &screens };
                    h.screens(set);
                }
            }
            assert_tree_invariants(&h);
        }));
        if let Err(e) = result {
            panic!("seed {seed}: failed after {:?}: {e:?}", log.last());
        }
    }
}

fn move_node_focused(workspace: u32) -> Command {
    Command::MoveNodeToWorkspace { workspace: workspace.to_string(), window: None, follow: false }
}

#[test]
fn random_commands_keep_the_tree_invariants() {
    for seed in 0..300u64 {
        random_walk(seed, 120);
    }
}
