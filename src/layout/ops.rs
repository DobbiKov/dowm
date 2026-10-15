//! The tiling commands, as operations on one workspace's [`WorkspaceTree`].
//!
//! Semantics follow AeroSpace's commands of the same names. Each operation either
//! succeeds having left the tree normalized, or fails having changed nothing. Which
//! sibling is "next" in a direction is the container's layout system's call
//! ([`LayoutSystem::step`](crate::layout::LayoutSystem::step) and friends), so these
//! commands work unchanged for systems added later; left/up are backwards and right/down
//! forwards for the default rule.
//!
//! Operations act on a tiled window; a floating or unknown one is an error, except
//! [`apply_layout`], which is how a floating window is tiled again.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::actor::app::WindowId;
use crate::layout::{
    ACCORDION, ComputedLayout, FLOATING, LayoutRegistry, Nav, TILES, is_forward,
};
use crate::model::tree::{LayoutKind, NodeId, Orientation, WorkspaceTree};
use crate::model::workspace::Direction;

/// Why a tiling command did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TilingError {
    /// There is no window to act on.
    NoFocusedWindow,
    /// The focused display has no workspace.
    NoWorkspaceHere,
    /// The workspace has no display, so its geometry is unknown (resize needs pixels).
    NoDisplay,
    NotInTree(WindowId),
    /// The command works on tiled windows only.
    WindowFloating(WindowId),
    /// `move` hit the edge of the workspace.
    AtBoundary,
    /// `join-with` found nothing in that direction.
    NoNeighbor,
    /// No ancestor along that axis has a sibling to trade space with, or the ones that do
    /// do not use weights.
    NothingToResize,
    /// `split` among siblings would create a one-child container, which flattening
    /// removes at once. It works on a lone window, or with flattening disabled.
    SplitDisabled,
    /// `layout` was given no layouts.
    EmptyLayoutList,
    /// `layout` named a system nobody registered.
    UnknownLayout(LayoutKind),
    /// `layout` named a system that cannot be a container's layout.
    NotAContainerLayout(LayoutKind),
}

impl fmt::Display for TilingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFocusedWindow => write!(f, "no focused window"),
            Self::NoWorkspaceHere => write!(f, "the focused display has no workspace"),
            Self::NoDisplay => write!(f, "the workspace is not on a display"),
            Self::NotInTree(wid) => write!(f, "window {wid:?} is not in the workspace tree"),
            Self::WindowFloating(wid) => write!(f, "window {wid:?} is floating"),
            Self::AtBoundary => write!(f, "no room to move in that direction"),
            Self::NoNeighbor => write!(f, "no neighbour in that direction"),
            Self::NothingToResize => write!(f, "nothing to resize along that axis"),
            Self::SplitDisabled => write!(
                f,
                "split among siblings is disabled while single-child containers are flattened"
            ),
            Self::EmptyLayoutList => write!(f, "layout needs at least one argument"),
            Self::UnknownLayout(kind) => write!(f, "no layout called {kind}"),
            Self::NotAContainerLayout(kind) => write!(f, "layout {kind} cannot lay out a container"),
        }
    }
}

impl std::error::Error for TilingError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplitArg {
    Horizontal,
    Vertical,
    /// The opposite of the parent's orientation.
    Opposite,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResizeDimension {
    Width,
    Height,
    /// The axis the window's parent container lays out along.
    Smart,
}

/// An argument of the `layout` command: AeroSpace's vocabulary, plus [`Self::Named`] for
/// any other registered system, so a new system needs no change here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutArg {
    Tiles,
    Accordion,
    /// Toggle the window out of (or back into) the tree.
    Floating,
    /// Put the window (back) into the tree; changes no container. AeroSpace's `tiling`,
    /// mostly seen as the second argument of `layout floating tiling`.
    Tiling,
    Horizontal,
    Vertical,
    HTiles,
    VTiles,
    HAccordion,
    VAccordion,
    /// Any registered system, by kind. The kind of the floating system toggles floating,
    /// like [`Self::Floating`].
    Named(LayoutKind),
}

impl LayoutArg {
    /// The container layout and orientation this sets, where it sets one.
    fn parts(self) -> (Option<LayoutKind>, Option<Orientation>) {
        match self {
            LayoutArg::Tiles => (Some(TILES), None),
            LayoutArg::Accordion => (Some(ACCORDION), None),
            LayoutArg::Floating | LayoutArg::Tiling => (None, None),
            LayoutArg::Horizontal => (None, Some(Orientation::Horizontal)),
            LayoutArg::Vertical => (None, Some(Orientation::Vertical)),
            LayoutArg::HTiles => (Some(TILES), Some(Orientation::Horizontal)),
            LayoutArg::VTiles => (Some(TILES), Some(Orientation::Vertical)),
            LayoutArg::HAccordion => (Some(ACCORDION), Some(Orientation::Horizontal)),
            LayoutArg::VAccordion => (Some(ACCORDION), Some(Orientation::Vertical)),
            LayoutArg::Named(kind) => (Some(kind), None),
        }
    }

    /// `Named(floating)` is `Floating`.
    fn canonical(self) -> Self {
        match self {
            LayoutArg::Named(kind) if kind == FLOATING => LayoutArg::Floating,
            other => other,
        }
    }
}

fn require_tiled(tree: &WorkspaceTree, window: WindowId) -> Result<NodeId, TilingError> {
    if tree.is_floating(window) {
        return Err(TilingError::WindowFloating(window));
    }
    tree.leaf(window).ok_or(TilingError::NotInTree(window))
}

/// What `parent`'s layout system is told when navigating from its child `from`.
fn nav(tree: &WorkspaceTree, parent: NodeId, from: NodeId, direction: Direction) -> Option<Nav<'_>> {
    Some(Nav {
        len: tree.children(parent).len(),
        from: tree.index_in_parent(from)?,
        direction,
        orientation: tree.orientation(parent)?,
        state: tree.layout_state(parent),
    })
}

/// From `start` upwards: the nearest container whose layout system finds a neighbour of
/// the path child in `direction`. Returns (container, path child, neighbour's index).
fn find_neighbor(
    tree: &WorkspaceTree,
    start: NodeId,
    direction: Direction,
    registry: &LayoutRegistry,
) -> Option<(NodeId, NodeId, usize)> {
    let mut at = start;
    while let Some(parent) = tree.parent(at) {
        if let (Some(kind), Some(nav)) = (tree.layout(parent), nav(tree, parent, at, direction))
            && let Some(next) = registry.get_or_default(kind).step(&nav)
            && next < nav.len
            && next != nav.from
        {
            return Some((parent, at, next));
        }
        at = parent;
    }
    None
}

/// `focus`: the window to focus next to `window` in `direction`: the neighbour found by
/// [`find_neighbor`], entered at its most recently focused window. `Ok(None)` at the
/// workspace edge (AeroSpace's default `stop`); with `wrap`, the window the nearest
/// container's system wraps to.
pub fn focus_target(
    tree: &WorkspaceTree,
    window: WindowId,
    direction: Direction,
    wrap: bool,
    registry: &LayoutRegistry,
) -> Result<Option<WindowId>, TilingError> {
    let leaf = require_tiled(tree, window)?;
    if let Some((parent, _, next)) = find_neighbor(tree, leaf, direction, registry) {
        return Ok(tree.children(parent).get(next).and_then(|&c| tree.focus_leaf(c)));
    }
    if !wrap {
        return Ok(None);
    }
    let mut at = leaf;
    while let Some(parent) = tree.parent(at) {
        if let (Some(kind), Some(nav)) = (tree.layout(parent), nav(tree, parent, at, direction))
            && let Some(far) = registry.get_or_default(kind).wrap_target(&nav)
            && let Some(&child) = tree.children(parent).get(far)
        {
            return Ok(tree.focus_leaf(child));
        }
        at = parent;
    }
    Ok(None)
}

/// `move`: move `window` one step in `direction`.
///
/// Within its parent, where the parent's system finds a neighbour, it swaps with a
/// window neighbour or enters a container neighbour at the edge facing it. Otherwise it
/// leaves the container and lands next to it, in the nearest ancestor whose system has a
/// side in that direction ([`LayoutSystem::place_beside`](crate::layout::LayoutSystem::place_beside)).
/// With no such ancestor it is [`TilingError::AtBoundary`].
pub fn move_window(
    tree: &mut WorkspaceTree,
    window: WindowId,
    direction: Direction,
    registry: &LayoutRegistry,
) -> Result<(), TilingError> {
    let leaf = require_tiled(tree, window)?;
    let forward = is_forward(direction);
    let mut at = leaf;
    while let Some(parent) = tree.parent(at) {
        if let (Some(kind), Some(nav)) = (tree.layout(parent), nav(tree, parent, at, direction)) {
            let system = registry.get_or_default(kind);
            if at == leaf {
                if let Some(next) = system.step(&nav).filter(|&n| n < nav.len && n != nav.from)
                    && let Some(&sibling) = tree.children(parent).get(next)
                {
                    if tree.is_container(sibling) {
                        let edge = if forward { 0 } else { tree.children(sibling).len() };
                        tree.move_node(leaf, sibling, edge);
                    } else {
                        tree.swap_siblings(leaf, sibling);
                    }
                    tree.set_focus(window);
                    tree.normalize();
                    return Ok(());
                }
            } else if let Some(index) = system.place_beside(&nav) {
                tree.move_node(leaf, parent, index);
                tree.set_focus(window);
                tree.normalize();
                return Ok(());
            }
        }
        at = parent;
    }
    Err(TilingError::AtBoundary)
}

/// `join-with`: put `window` and its neighbour in `direction` into a new container that
/// takes the neighbour's slot. The container is perpendicular to the axis its parent
/// lays out along (so it already alternates with it), holds the window on the side it
/// came from, and weighs as much as the slots it replaced.
pub fn join_with(
    tree: &mut WorkspaceTree,
    window: WindowId,
    direction: Direction,
    registry: &LayoutRegistry,
) -> Result<(), TilingError> {
    let leaf = require_tiled(tree, window)?;
    let (parent, _, next) =
        find_neighbor(tree, leaf, direction, registry).ok_or(TilingError::NoNeighbor)?;
    let target = *tree.children(parent).get(next).ok_or(TilingError::NoNeighbor)?;
    let adjacent = tree.parent(leaf) == Some(parent);
    let leaf_weight = tree.weight(leaf).unwrap_or(1.0);
    let orientation = tree.orientation(parent).ok_or(TilingError::NoNeighbor)?.opposite();

    let joined = tree.wrap(target, orientation).ok_or(TilingError::NoNeighbor)?;
    if adjacent {
        tree.set_weight(joined, tree.weight(joined).unwrap_or(1.0) + leaf_weight);
    }
    tree.detach(leaf);
    tree.attach(leaf, joined, usize::from(!is_forward(direction)), Some(1.0));
    tree.set_focus(window);
    tree.normalize();
    Ok(())
}

/// `split`: make `window` the start of a new split of the given orientation.
///
/// A window alone in its container just changes that container's orientation. Among
/// siblings it would wrap the window in a one-child container, which flattening removes
/// again, so the command is refused ([`TilingError::SplitDisabled`]) unless flattening
/// is switched off.
///
/// Unverified against AeroSpace: I believe AeroSpace disables `split` for the same
/// reason when its flatten normalization is on, but this was written from memory of
/// its behaviour, not from its source or a side-by-side run.
pub fn split(tree: &mut WorkspaceTree, window: WindowId, arg: SplitArg) -> Result<(), TilingError> {
    let leaf = require_tiled(tree, window)?;
    let parent = tree.parent(leaf).ok_or(TilingError::NotInTree(window))?;
    let current = tree.orientation(parent).ok_or(TilingError::NotInTree(window))?;
    let wanted = match arg {
        SplitArg::Horizontal => Orientation::Horizontal,
        SplitArg::Vertical => Orientation::Vertical,
        SplitArg::Opposite => current.opposite(),
    };
    if tree.children(parent).len() == 1 {
        tree.set_orientation(parent, wanted);
    } else if tree.options().flatten_containers {
        return Err(TilingError::SplitDisabled);
    } else {
        tree.wrap(leaf, wanted);
    }
    tree.normalize();
    Ok(())
}

/// `resize`: grow (or, if negative, shrink) `window` by `amount` pixels along an axis.
///
/// The node that grows is the nearest ancestor-or-self whose parent lays out along the
/// axis, has a sibling, and uses a weighted system ([`Hints::weighted`](crate::layout::Hints)); parents that do not
/// are passed over. Its siblings give up the space in proportion to their weights, so
/// the parent's weights keep their total. `layout` supplies the pixel extent of the parent.
pub fn resize(
    tree: &mut WorkspaceTree,
    window: WindowId,
    dimension: ResizeDimension,
    amount: i32,
    layout: &ComputedLayout,
    registry: &LayoutRegistry,
) -> Result<(), TilingError> {
    let leaf = require_tiled(tree, window)?;
    let axis = match dimension {
        ResizeDimension::Width => Orientation::Horizontal,
        ResizeDimension::Height => Orientation::Vertical,
        ResizeDimension::Smart => tree
            .parent(leaf)
            .and_then(|p| tree.orientation(p))
            .ok_or(TilingError::NothingToResize)?,
    };
    let mut at = leaf;
    while let Some(parent) = tree.parent(at) {
        let weighted = tree.layout(parent).is_some_and(|k| {
            registry.get_or_default(k).hints().weighted
        });
        if weighted && tree.orientation(parent) == Some(axis) && tree.children(parent).len() > 1 {
            let extent = layout.containers.get(&parent).map_or(0.0, |r| match axis {
                Orientation::Horizontal => r.size.w,
                Orientation::Vertical => r.size.h,
            });
            if extent > 0.0 {
                let total: f64 = tree.children(parent).iter().filter_map(|&c| tree.weight(c)).sum();
                tree.resize_child(at, f64::from(amount) * total / extent);
                tree.normalize();
                return Ok(());
            }
        }
        at = parent;
    }
    Err(TilingError::NothingToResize)
}

/// Whether `window` is currently in the state `arg` describes (for cycling).
fn in_state(tree: &WorkspaceTree, window: WindowId, arg: LayoutArg) -> bool {
    let arg = arg.canonical();
    if arg == LayoutArg::Floating {
        return tree.is_floating(window);
    }
    if arg == LayoutArg::Tiling {
        return !tree.is_floating(window);
    }
    let Some(parent) = tree.leaf(window).and_then(|l| tree.parent(l)) else { return false };
    let (kind, orientation) = arg.parts();
    kind.is_none_or(|k| tree.layout(parent) == Some(k))
        && orientation.is_none_or(|o| tree.orientation(parent) == Some(o))
}

/// `layout`: set the layout kind and/or orientation of `window`'s parent container, or
/// toggle the window floating.
///
/// With one argument, apply it. With several, apply the one after the first that
/// describes the current state (wrapping), or the first if none does, so
/// `layout tiles accordion` toggles. A floating window is tiled again by any argument
/// but `floating`. Setting an orientation equal to the grandparent's is undone by
/// normalization (nested containers alternate); on the root it flips the whole tree.
/// A named system must be registered and usable as a container layout; a new kind
/// starts with the system's [`init_state`](crate::layout::LayoutSystem::init_state).
pub fn apply_layout(
    tree: &mut WorkspaceTree,
    window: WindowId,
    args: &[LayoutArg],
    registry: &LayoutRegistry,
) -> Result<(), TilingError> {
    if args.is_empty() {
        return Err(TilingError::EmptyLayoutList);
    }
    if !tree.contains(window) {
        return Err(TilingError::NotInTree(window));
    }
    let arg = if args.len() == 1 {
        args[0]
    } else {
        match args.iter().position(|&a| in_state(tree, window, a)) {
            Some(i) => args[(i + 1) % args.len()],
            None => args[0],
        }
    }
    .canonical();
    if arg == LayoutArg::Floating {
        if tree.is_floating(window) {
            tree.tile(window);
        } else {
            tree.float(window);
        }
        return Ok(());
    }
    let (kind, orientation) = arg.parts();
    // Everything that can fail comes before the first change.
    if let Some(kind) = kind {
        let hints = registry.hints(kind).ok_or(TilingError::UnknownLayout(kind))?;
        if !hints.in_tree {
            return Err(TilingError::NotAContainerLayout(kind));
        }
    }
    tree.tile(window);
    let parent = tree
        .leaf(window)
        .and_then(|l| tree.parent(l))
        .ok_or(TilingError::NotInTree(window))?;
    if let Some(kind) = kind
        && tree.layout(parent) != Some(kind)
    {
        tree.set_layout_with(parent, kind, registry.init_state(kind));
    }
    if let Some(orientation) = orientation {
        tree.set_orientation(parent, orientation);
    }
    tree.normalize();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;
    use crate::model::tree::NormalizeOptions;
    use crate::layout::{TILES, compute};

    fn w(n: u32) -> WindowId { WindowId::new(1, n) }

    fn t(spec: &str) -> WorkspaceTree { WorkspaceTree::from_spec(spec, TILES) }

    fn reg() -> LayoutRegistry { LayoutRegistry::with_builtin() }

    // ---- focus ----

    #[test]
    fn focus_steps_to_the_sibling_in_the_direction() {
        let tree = t("h[1 2 3]");
        assert_eq!(focus_target(&tree, w(2), Direction::Right, false, &reg()), Ok(Some(w(3))));
        assert_eq!(focus_target(&tree, w(2), Direction::Left, false, &reg()), Ok(Some(w(1))));
    }

    #[test]
    fn focus_stops_at_the_boundary_without_wrap() {
        let tree = t("h[1 2 3]");
        assert_eq!(focus_target(&tree, w(3), Direction::Right, false, &reg()), Ok(None));
        assert_eq!(focus_target(&tree, w(1), Direction::Left, false, &reg()), Ok(None));
        assert_eq!(focus_target(&tree, w(2), Direction::Up, false, &reg()), Ok(None), "no vertical axis");
    }

    #[test]
    fn focus_wraps_around_the_workspace_when_asked() {
        let tree = t("h[1 2 3]");
        assert_eq!(focus_target(&tree, w(3), Direction::Right, true, &reg()), Ok(Some(w(1))));
        assert_eq!(focus_target(&tree, w(1), Direction::Left, true, &reg()), Ok(Some(w(3))));
        assert_eq!(focus_target(&tree, w(1), Direction::Down, true, &reg()), Ok(None), "nothing to wrap to");
        let nested = t("h[1 v[2 3]]");
        assert_eq!(focus_target(&nested, w(3), Direction::Down, true, &reg()), Ok(Some(w(2))));
    }

    #[test]
    fn focus_climbs_out_of_nested_containers() {
        let tree = t("h[1 v[2 3]]");
        assert_eq!(focus_target(&tree, w(3), Direction::Left, false, &reg()), Ok(Some(w(1))));
        assert_eq!(focus_target(&tree, w(2), Direction::Down, false, &reg()), Ok(Some(w(3))));
        assert_eq!(focus_target(&tree, w(3), Direction::Up, false, &reg()), Ok(Some(w(2))));
        assert_eq!(focus_target(&tree, w(2), Direction::Up, false, &reg()), Ok(None));
    }

    #[test]
    fn focus_enters_a_container_at_its_most_recent_window() {
        let mut tree = t("h[1 v[2 3]]");
        tree.set_focus(w(3));
        tree.set_focus(w(1));
        assert_eq!(focus_target(&tree, w(1), Direction::Right, false, &reg()), Ok(Some(w(3))));
        tree.set_focus(w(2));
        tree.set_focus(w(1));
        assert_eq!(focus_target(&tree, w(1), Direction::Right, false, &reg()), Ok(Some(w(2))));
    }

    #[test]
    fn focus_follows_sibling_order_in_an_accordion() {
        let tree = t("h@accordion[1 2 3]");
        assert_eq!(focus_target(&tree, w(1), Direction::Right, false, &reg()), Ok(Some(w(2))));
        assert_eq!(focus_target(&tree, w(3), Direction::Left, false, &reg()), Ok(Some(w(2))));
    }

    #[test]
    fn tiling_commands_refuse_floating_and_unknown_windows() {
        let mut tree = t("h[1 2]");
        tree.insert_floating(w(9));
        assert_eq!(
            focus_target(&tree, w(9), Direction::Left, false, &reg()),
            Err(TilingError::WindowFloating(w(9)))
        );
        assert_eq!(
            focus_target(&tree, w(8), Direction::Left, false, &reg()),
            Err(TilingError::NotInTree(w(8)))
        );
    }

    // ---- move ----

    fn moved(spec: &str, window: u32, dir: Direction) -> Result<String, TilingError> {
        let mut tree = t(spec);
        move_window(&mut tree, w(window), dir, &reg())?;
        tree.check_invariants().unwrap();
        assert_eq!(tree.focused_window(), Some(w(window)), "the moved window keeps focus");
        Ok(tree.describe())
    }

    #[test]
    fn move_swaps_with_a_sibling_window() {
        assert_eq!(moved("h[1 2 3]", 2, Direction::Right).as_deref(), Ok("h[1 3 2]"));
        assert_eq!(moved("h[1 2 3]", 2, Direction::Left).as_deref(), Ok("h[2 1 3]"));
        assert_eq!(moved("h[1 v[2 3]]", 2, Direction::Down).as_deref(), Ok("h[1 v[3 2]]"));
    }

    #[test]
    fn move_swaps_weights_with_the_slot() {
        let mut tree = t("h[1 2]");
        tree.set_weight(tree.leaf(w(1)).unwrap(), 3.0);
        move_window(&mut tree, w(1), Direction::Right, &reg()).unwrap();
        assert_eq!(tree.weight(tree.leaf(w(2)).unwrap()).unwrap(), 3.0);
    }

    #[test]
    fn move_at_the_workspace_edge_is_a_boundary_error_and_changes_nothing() {
        assert_eq!(moved("h[1 2 3]", 3, Direction::Right), Err(TilingError::AtBoundary));
        assert_eq!(moved("h[1 2 3]", 1, Direction::Up), Err(TilingError::AtBoundary));
        assert_eq!(moved("h[1 v[2 3]]", 2, Direction::Up), Err(TilingError::AtBoundary));
    }

    #[test]
    fn move_into_a_neighbouring_container_joins_it_at_the_near_edge() {
        assert_eq!(moved("h[1 v[2 3] 4]", 1, Direction::Right).as_deref(), Ok("h[v[1 2 3] 4]"));
        assert_eq!(moved("h[1 v[2 3] 4]", 4, Direction::Left).as_deref(), Ok("h[1 v[2 3 4]]"));
    }

    #[test]
    fn move_out_of_a_container_lands_beside_it() {
        assert_eq!(moved("h[1 v[2 3 4]]", 3, Direction::Right).as_deref(), Ok("h[1 v[2 4] 3]"));
        assert_eq!(moved("h[1 v[2 3 4]]", 3, Direction::Left).as_deref(), Ok("h[1 3 v[2 4]]"));
        // The container shrinks to one window and is flattened away.
        assert_eq!(moved("h[1 v[2 3]]", 3, Direction::Right).as_deref(), Ok("h[1 2 3]"));
        assert_eq!(moved("h[1 v[2 3]]", 2, Direction::Left).as_deref(), Ok("h[1 2 3]"));
    }

    #[test]
    fn move_out_from_a_deeper_container_crosses_one_level() {
        assert_eq!(moved("h[1 v[2 h[3 4 5]]]", 3, Direction::Up).as_deref(), Ok("h[1 v[2 3 h[4 5]]]"));
    }

    #[test]
    fn move_works_in_an_accordion() {
        assert_eq!(moved("h@accordion[1 2 3]", 1, Direction::Right).as_deref(), Ok("h@accordion[2 1 3]"));
    }

    // ---- join-with ----

    fn joined(spec: &str, window: u32, dir: Direction) -> Result<String, TilingError> {
        let mut tree = t(spec);
        join_with(&mut tree, w(window), dir, &reg())?;
        tree.check_invariants().unwrap();
        assert_eq!(tree.focused_window(), Some(w(window)));
        Ok(tree.describe())
    }

    #[test]
    fn join_with_wraps_the_window_and_its_neighbour_in_a_perpendicular_container() {
        assert_eq!(joined("h[1 2 3]", 2, Direction::Right).as_deref(), Ok("h[1 v[2 3]]"));
        assert_eq!(joined("h[1 2 3]", 2, Direction::Left).as_deref(), Ok("h[v[1 2] 3]"));
        assert_eq!(joined("v[1 2 3]", 2, Direction::Down).as_deref(), Ok("v[1 h[2 3]]"));
    }

    #[test]
    fn join_with_the_only_neighbour_collapses_into_the_root() {
        assert_eq!(joined("h[1 2]", 1, Direction::Right).as_deref(), Ok("v[1 2]"));
    }

    #[test]
    fn join_with_a_container_nests_it() {
        assert_eq!(joined("h[1 v[2 3]]", 1, Direction::Right).as_deref(), Ok("v[1 h[2 3]]"));
    }

    #[test]
    fn join_with_reaches_across_containers() {
        assert_eq!(joined("h[1 v[2 3]]", 3, Direction::Left).as_deref(), Ok("h[v[1 3] 2]"));
    }

    #[test]
    fn join_with_without_a_neighbour_fails() {
        assert_eq!(joined("h[1 2 3]", 3, Direction::Right), Err(TilingError::NoNeighbor));
        assert_eq!(joined("h[1 2 3]", 2, Direction::Up), Err(TilingError::NoNeighbor));
    }

    #[test]
    fn join_with_gives_the_new_container_both_slots() {
        let mut tree = t("h[1 2 3]");
        for (i, weight) in [(1, 1.0), (2, 2.0), (3, 3.0)] {
            tree.set_weight(tree.leaf(w(i)).unwrap(), weight);
        }
        join_with(&mut tree, w(1), Direction::Right, &reg()).unwrap();
        let joined = tree.parent(tree.leaf(w(1)).unwrap()).unwrap();
        assert_eq!(tree.weight(joined).unwrap(), 3.0, "1 and 2 together keep their combined share");
        assert_eq!(tree.weight(tree.leaf(w(3)).unwrap()).unwrap(), 3.0);
        assert_eq!(tree.weight(tree.leaf(w(1)).unwrap()).unwrap(), 1.0);
        assert_eq!(tree.weight(tree.leaf(w(2)).unwrap()).unwrap(), 1.0);
    }

    // ---- split ----

    #[test]
    fn split_on_a_lone_window_sets_the_root_orientation() {
        for (arg, expect) in [
            (SplitArg::Vertical, "v[1]"),
            (SplitArg::Horizontal, "h[1]"),
            (SplitArg::Opposite, "v[1]"),
        ] {
            let mut tree = t("h[1]");
            split(&mut tree, w(1), arg).unwrap();
            assert_eq!(tree.describe(), expect);
            tree.check_invariants().unwrap();
        }
    }

    #[test]
    fn split_among_siblings_is_refused_while_containers_are_flattened() {
        let mut tree = t("h[1 2]");
        assert_eq!(split(&mut tree, w(1), SplitArg::Vertical), Err(TilingError::SplitDisabled));
        assert_eq!(tree.describe(), "h[1 2]");
    }

    #[test]
    fn split_wraps_the_window_when_flattening_is_off() {
        for (arg, expect) in [
            (SplitArg::Vertical, "h[v[1] 2]"),
            (SplitArg::Opposite, "h[v[1] 2]"),
            (SplitArg::Horizontal, "h[h[1] 2]"),
        ] {
            let mut tree = t("h[1 2]");
            tree.set_options(NormalizeOptions { flatten_containers: false, opposite_orientation: false });
            split(&mut tree, w(1), arg).unwrap();
            assert_eq!(tree.describe(), expect);
            tree.check_invariants().unwrap();
        }
    }

    // ---- resize ----

    fn sized(tree: &WorkspaceTree) -> crate::layout::ComputedLayout {
        compute(tree, Rect::new(0., 0., 400., 200.), &reg(), &|_| None)
    }

    fn grow(
        tree: &mut WorkspaceTree,
        window: WindowId,
        dimension: ResizeDimension,
        amount: i32,
    ) -> Result<(), TilingError> {
        let layout = sized(tree);
        resize(tree, window, dimension, amount, &layout, &reg())
    }

    #[test]
    fn resize_width_trades_pixels_with_the_sibling() {
        let mut tree = t("h[1 2]");
        grow(&mut tree, w(1), ResizeDimension::Width, 40).unwrap();
        let out = sized(&tree);
        assert!((out.windows[&w(1)].size.w - 240.).abs() < 1e-9);
        assert!((out.windows[&w(2)].size.w - 160.).abs() < 1e-9);
        grow(&mut tree, w(1), ResizeDimension::Width, -80).unwrap();
        assert!((sized(&tree).windows[&w(1)].size.w - 160.).abs() < 1e-9);
    }

    #[test]
    fn resize_redistributes_among_all_siblings_and_keeps_the_total() {
        let mut tree = t("h[1 2 3]");
        grow(&mut tree, w(2), ResizeDimension::Width, 60).unwrap();
        let weights: Vec<f64> = tree.children(tree.root()).iter().map(|&c| tree.weight(c).unwrap()).collect();
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!((weights[0] - weights[2]).abs() < 1e-9, "the others give up equally");
        assert!(weights[1] > 1.0 && weights[0] < 1.0);
    }

    #[test]
    fn resize_height_needs_a_vertical_container() {
        let mut tree = t("h[1 2]");
        assert_eq!(
            grow(&mut tree, w(1), ResizeDimension::Height, 10),
            Err(TilingError::NothingToResize)
        );
        let mut nested = t("h[1 v[2 3]]");
        grow(&mut nested, w(2), ResizeDimension::Height, 20).unwrap();
        let out = sized(&nested);
        assert!((out.windows[&w(2)].size.h - 120.).abs() < 1e-9);
        assert!((out.windows[&w(3)].size.h - 80.).abs() < 1e-9);
    }

    #[test]
    fn resize_width_climbs_to_the_nearest_horizontal_ancestor() {
        let mut tree = t("h[1 v[2 3]]");
        grow(&mut tree, w(3), ResizeDimension::Width, 40).unwrap();
        let out = sized(&tree);
        assert!((out.windows[&w(3)].size.w - 240.).abs() < 1e-9, "the whole column grows");
        assert!((out.windows[&w(2)].size.w - 240.).abs() < 1e-9);
        assert!((out.windows[&w(1)].size.w - 160.).abs() < 1e-9);
    }

    #[test]
    fn resize_smart_uses_the_axis_of_the_parent() {
        let mut tree = t("h[1 v[2 3]]");
        grow(&mut tree, w(2), ResizeDimension::Smart, 20).unwrap();
        assert!((sized(&tree).windows[&w(2)].size.h - 120.).abs() < 1e-9, "2's parent is vertical");
        grow(&mut tree, w(1), ResizeDimension::Smart, 40).unwrap();
        assert!((sized(&tree).windows[&w(1)].size.w - 240.).abs() < 1e-9, "1's parent is horizontal");
    }

    #[test]
    fn resize_never_makes_a_weight_non_positive() {
        let mut tree = t("h[1 2 3]");
        for amount in [10_000, -10_000, 10_000, -10_000] {
            grow(&mut tree, w(1), ResizeDimension::Width, amount).unwrap();
            tree.check_invariants().unwrap();
        }
    }

    #[test]
    fn resize_with_nothing_to_trade_fails() {
        let mut tree = t("h[1]");
        assert_eq!(
            grow(&mut tree, w(1), ResizeDimension::Smart, 10),
            Err(TilingError::NothingToResize)
        );
        let mut tree = t("h[1 2]");
        let empty = crate::layout::ComputedLayout::default();
        assert_eq!(
            resize(&mut tree, w(1), ResizeDimension::Width, 10, &empty, &reg()),
            Err(TilingError::NothingToResize),
            "no known extent"
        );
    }

    // ---- layout ----

    fn laid(spec: &str, window: u32, args: &[LayoutArg]) -> Result<String, TilingError> {
        let mut tree = t(spec);
        apply_layout(&mut tree, w(window), args, &reg())?;
        tree.check_invariants().unwrap();
        Ok(tree.describe())
    }

    #[test]
    fn layout_sets_the_kind_of_the_parent_container() {
        assert_eq!(laid("h[1 2]", 1, &[LayoutArg::Accordion]).as_deref(), Ok("h@accordion[1 2]"));
        assert_eq!(laid("h@accordion[1 2]", 1, &[LayoutArg::Tiles]).as_deref(), Ok("h[1 2]"));
        assert_eq!(laid("h[1 v[2 3]]", 2, &[LayoutArg::Accordion]).as_deref(), Ok("h[1 v@accordion[2 3]]"));
    }

    #[test]
    fn layout_sets_the_orientation() {
        assert_eq!(laid("h[1 2]", 1, &[LayoutArg::Vertical]).as_deref(), Ok("v[1 2]"));
        assert_eq!(laid("h[1 2]", 1, &[LayoutArg::VAccordion]).as_deref(), Ok("v@accordion[1 2]"));
        assert_eq!(laid("v[1 2]", 1, &[LayoutArg::HTiles]).as_deref(), Ok("h[1 2]"));
        assert_eq!(laid("v@accordion[1 2]", 1, &[LayoutArg::HAccordion]).as_deref(), Ok("h@accordion[1 2]"));
        assert_eq!(laid("h@accordion[1 2]", 1, &[LayoutArg::VTiles]).as_deref(), Ok("v[1 2]"));
    }

    #[test]
    fn layout_on_the_root_flips_the_nested_containers_with_it() {
        assert_eq!(laid("h[1 v[2 3]]", 1, &[LayoutArg::Vertical]).as_deref(), Ok("v[1 h[2 3]]"));
    }

    #[test]
    fn layout_orientation_that_matches_the_grandparent_is_undone_by_normalization() {
        assert_eq!(laid("h[1 v[2 3]]", 2, &[LayoutArg::Horizontal]).as_deref(), Ok("h[1 v[2 3]]"));
    }

    #[test]
    fn several_layouts_cycle_from_the_current_one() {
        let args = [LayoutArg::Tiles, LayoutArg::Accordion];
        assert_eq!(laid("h[1 2]", 1, &args).as_deref(), Ok("h@accordion[1 2]"));
        assert_eq!(laid("h@accordion[1 2]", 1, &args).as_deref(), Ok("h[1 2]"));
        let args = [LayoutArg::Horizontal, LayoutArg::Vertical];
        assert_eq!(laid("h[1 2]", 1, &args).as_deref(), Ok("v[1 2]"));
        assert_eq!(laid("v[1 2]", 1, &args).as_deref(), Ok("h[1 2]"));
        let args = [LayoutArg::VAccordion, LayoutArg::HTiles];
        assert_eq!(laid("h[1 2]", 1, &args).as_deref(), Ok("v@accordion[1 2]"), "HTiles matches, so the next in the list (wrapping) applies");
    }

    #[test]
    fn layout_floating_toggles_the_window_out_of_and_into_the_tree() {
        let mut tree = t("h[1 2 3]");
        apply_layout(&mut tree, w(2), &[LayoutArg::Floating], &reg()).unwrap();
        assert_eq!(tree.describe(), "h[1 3]");
        assert!(tree.is_floating(w(2)));
        apply_layout(&mut tree, w(2), &[LayoutArg::Floating], &reg()).unwrap();
        assert!(tree.is_tiled(w(2)) && !tree.is_floating(w(2)));
        tree.check_invariants().unwrap();
    }

    #[test]
    fn layout_floating_tiling_toggles_between_the_two() {
        let args = [LayoutArg::Floating, LayoutArg::Tiling];
        let mut tree = t("h[1 2 3]");
        apply_layout(&mut tree, w(2), &args, &reg()).unwrap();
        assert!(tree.is_floating(w(2)), "tiled: the argument after `tiling` wraps to `floating`");
        apply_layout(&mut tree, w(2), &args, &reg()).unwrap();
        assert!(tree.is_tiled(w(2)), "floating: the argument after `floating` is `tiling`");
        tree.check_invariants().unwrap();
    }

    #[test]
    fn layout_tiling_alone_tiles_a_floating_window_and_changes_no_container() {
        let mut tree = t("v@accordion[1 2]");
        tree.float(w(1));
        apply_layout(&mut tree, w(1), &[LayoutArg::Tiling], &reg()).unwrap();
        assert!(tree.is_tiled(w(1)));
        assert!(tree.describe().starts_with("v@accordion["), "{}", tree.describe());
    }

    #[test]
    fn layout_tiles_brings_a_floating_window_back() {
        let mut tree = t("h[1 2]");
        tree.float(w(1));
        apply_layout(&mut tree, w(1), &[LayoutArg::Accordion], &reg()).unwrap();
        assert!(tree.is_tiled(w(1)));
        assert_eq!(tree.layout(tree.parent(tree.leaf(w(1)).unwrap()).unwrap()), Some(ACCORDION));
    }

    #[test]
    fn layout_can_cycle_through_floating() {
        let args = [LayoutArg::Tiles, LayoutArg::Floating];
        let mut tree = t("h[1 2]");
        apply_layout(&mut tree, w(1), &args, &reg()).unwrap();
        assert!(tree.is_floating(w(1)));
        apply_layout(&mut tree, w(1), &args, &reg()).unwrap();
        assert!(tree.is_tiled(w(1)));
    }

    #[test]
    fn layout_needs_arguments_and_a_known_window() {
        assert_eq!(laid("h[1]", 1, &[]), Err(TilingError::EmptyLayoutList));
        assert_eq!(laid("h[1]", 9, &[LayoutArg::Tiles]), Err(TilingError::NotInTree(w(9))));
    }

    #[test]
    fn errors_say_what_went_wrong() {
        for error in [
            TilingError::NoFocusedWindow,
            TilingError::NoWorkspaceHere,
            TilingError::NoDisplay,
            TilingError::NotInTree(w(1)),
            TilingError::WindowFloating(w(1)),
            TilingError::AtBoundary,
            TilingError::NoNeighbor,
            TilingError::NothingToResize,
            TilingError::SplitDisabled,
            TilingError::EmptyLayoutList,
        ] {
            assert!(!error.to_string().is_empty());
        }
    }
}
