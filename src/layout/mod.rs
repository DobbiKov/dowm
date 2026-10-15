//! Layout systems and the geometry they produce.
//!
//! [`LayoutSystem`] is the seam: given one container's area, orientation, children
//! (weights, and the window's own frame where it has one) and the system's own
//! per-container state, a system returns each child's rect. Containers store only a
//! [`LayoutKind`] identifier and an opaque [`LayoutState`]; the [`LayoutRegistry`]
//! resolves the kind to a system. Adding bsp, master-stack, scrolling or traditional
//! later is a new `impl` and one `register` call, with no change to the tree, the trait,
//! [`compute`] or the commands in [`ops`]:
//!
//! - geometry: [`LayoutSystem::arrange`] (with [`LayoutSystem::init_state`] for systems
//!   that keep per-container data);
//! - navigation: [`LayoutSystem::step`], [`LayoutSystem::place_beside`] and
//!   [`LayoutSystem::wrap_target`] get the direction and the container's orientation, so
//!   a system with its own notion of adjacency (a grid, a master/stack split) decides
//!   what `focus`/`move` do inside its containers;
//! - behaviour flags: [`Hints`] say whether the system may be a container's layout,
//!   whether its children overlap (z-order), whether `resize` means anything, and
//!   whether the tree's orientation-alternation and single-child flattening apply to
//!   its containers.
//!
//! Three systems exist, all fresh code after AeroSpace: [`Tiles`], [`Accordion`] and
//! [`Floating`]. Everything here is pure: no AX, no CGS.
//!
//! [`ops`] holds the tiling commands (focus, move, join-with, ...) over a
//! [`WorkspaceTree`].

mod accordion;
mod floating;
pub mod gaps;
pub mod ops;
mod tiles;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub use accordion::{ACCORDION, Accordion};
pub use floating::{FLOATING, Floating};
pub use ops::TilingError;
pub use tiles::{TILES, Tiles};

use crate::actor::app::WindowId;
pub use crate::geometry::Rect;
use crate::model::tree::{KindPolicy, LayoutKind, LayoutState, NodeId, Orientation, WorkspaceTree};
use crate::model::workspace::Direction;

/// Whether `direction` moves toward higher indexes (right and down) in reading order.
pub fn is_forward(direction: Direction) -> bool { matches!(direction, Direction::Right | Direction::Down) }

/// One child as a layout system sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Child {
    /// Sibling weight; only meaningful to systems whose [`Hints::weighted`] is set.
    pub weight: f64,
    /// The window's own frame, for a window leaf; `None` for a container.
    pub frame: Option<Rect>,
}

/// What a system is asked to arrange.
#[derive(Clone, Copy, Debug)]
pub struct Arrange<'a> {
    pub area: Rect,
    pub orientation: Orientation,
    pub children: &'a [Child],
    /// Index of the most recently focused child (0 if none). For systems whose geometry
    /// follows focus (scrolling, stacking); the built-in ones do not need it.
    pub focused: usize,
    /// The container's own state, or the system's [`LayoutSystem::init_state`] if it has
    /// none yet.
    pub state: Option<&'a LayoutState>,
}

/// A container's children as navigation sees them.
#[derive(Clone, Copy, Debug)]
pub struct Nav<'a> {
    pub len: usize,
    /// The child navigation starts from (the one on the path to the focused window).
    pub from: usize,
    pub direction: Direction,
    pub orientation: Orientation,
    pub state: Option<&'a LayoutState>,
}

impl Nav<'_> {
    /// The container is laid out along the axis `direction` moves on.
    pub fn along_axis(&self) -> bool { Orientation::of(self.direction) == self.orientation }
}

/// What a system says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hints {
    /// A container may use it. `false` for systems that arrange the floating set.
    pub in_tree: bool,
    /// Children overlap, so z-order matters: the presenter raises the focused one.
    pub overlapping: bool,
    /// Sibling weights are meaningful (and `resize` is).
    pub weighted: bool,
    /// Nested containers of this kind take the opposite orientation of their parent.
    pub alternates: bool,
    /// A single-child container of this kind is replaced by its child.
    pub flattens: bool,
}

impl Hints {
    /// The tree-side view of these hints.
    pub fn policy(&self) -> KindPolicy {
        KindPolicy { in_tree: self.in_tree, alternates: self.alternates, flattens: self.flattens }
    }
}

pub trait LayoutSystem: fmt::Debug + Send + Sync {
    fn kind(&self) -> LayoutKind;

    fn hints(&self) -> Hints;

    /// The state a new container of this kind starts with. Systems without per-container
    /// data keep the default (`None`). A container whose state is `None` is arranged with
    /// this too, so the tree can make containers without knowing the system.
    fn init_state(&self) -> Option<LayoutState> { None }

    /// One rect per child, in order.
    fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect>;

    /// The sibling reached by one step in `nav.direction` from child `nav.from`, if any.
    /// `focus` and `move` use it. The default is the next or previous index when the
    /// container is laid out along the direction's axis, and nothing across it.
    fn step(&self, nav: &Nav<'_>) -> Option<usize> {
        if !nav.along_axis() {
            return None;
        }
        if is_forward(nav.direction) {
            (nav.from + 1 < nav.len).then_some(nav.from + 1)
        } else {
            nav.from.checked_sub(1).filter(|&i| i < nav.len)
        }
    }

    /// Where a window that is leaving a descendant container lands in this container,
    /// as an insertion index, when it moves in `nav.direction` past child `nav.from`
    /// (the descendant's ancestor). `None`: this container has no side in that direction,
    /// so the window keeps climbing. The default is right beside it, along the axis.
    fn place_beside(&self, nav: &Nav<'_>) -> Option<usize> {
        nav.along_axis().then_some(nav.from + usize::from(is_forward(nav.direction)))
    }

    /// The child `focus` wraps to when `nav.direction` runs off the end of the workspace
    /// and wrapping is on. The default is the opposite end along the axis.
    fn wrap_target(&self, nav: &Nav<'_>) -> Option<usize> {
        (nav.along_axis() && nav.len > 1)
            .then(|| if is_forward(nav.direction) { 0 } else { nav.len - 1 })
    }
}

/// Maps a [`LayoutKind`] to its system.
#[derive(Debug)]
pub struct LayoutRegistry {
    systems: Vec<Box<dyn LayoutSystem>>,
}

impl LayoutRegistry {
    /// The three systems: tiles, accordion, floating.
    pub fn with_builtin() -> Self {
        LayoutRegistry {
            systems: vec![Box::new(Tiles), Box::new(Accordion::default()), Box::new(Floating)],
        }
    }

    /// Add a system, replacing any registered under the same kind. Trees must be given
    /// the new [`Self::policies`] afterwards.
    pub fn register(&mut self, system: Box<dyn LayoutSystem>) {
        self.systems.retain(|s| s.kind() != system.kind());
        self.systems.push(system);
    }

    pub fn get(&self, kind: LayoutKind) -> Option<&dyn LayoutSystem> {
        self.systems.iter().find(|s| s.kind() == kind).map(|s| s.as_ref())
    }

    /// The system for `kind`, or `tiles` for a kind nobody registered (a container must
    /// always be laid out).
    pub fn get_or_default(&self, kind: LayoutKind) -> &dyn LayoutSystem {
        self.get(kind)
            .or_else(|| self.get(TILES))
            .or_else(|| self.systems.first().map(|s| s.as_ref()))
            .expect("the registry holds at least one system")
    }

    pub fn kinds(&self) -> Vec<LayoutKind> { self.systems.iter().map(|s| s.kind()).collect() }

    pub fn hints(&self, kind: LayoutKind) -> Option<Hints> { self.get(kind).map(|s| s.hints()) }

    /// A fresh container's state for `kind`.
    pub fn init_state(&self, kind: LayoutKind) -> Option<LayoutState> {
        self.get(kind).and_then(|s| s.init_state())
    }

    /// Every registered kind's normalization rules, for [`WorkspaceTree::set_policies`].
    pub fn policies(&self) -> Arc<BTreeMap<LayoutKind, KindPolicy>> {
        Arc::new(self.systems.iter().map(|s| (s.kind(), s.hints().policy())).collect())
    }
}

impl Default for LayoutRegistry {
    fn default() -> Self { Self::with_builtin() }
}

/// A workspace laid out in an area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComputedLayout {
    /// Every window the tree holds, tiled or floating.
    pub windows: BTreeMap<WindowId, Rect>,
    /// Every container's rect (for resize, which needs pixel extents).
    pub containers: BTreeMap<NodeId, Rect>,
    /// Windows that may overlap another because a system on their path has overlapping
    /// children (accordion containers, the floating set).
    pub overlapping: BTreeSet<WindowId>,
    /// The windows to bring to the front: the focused one of every overlapping container.
    pub raise: Vec<WindowId>,
}

/// Lay `tree` out in `area`. `frames` supplies the frame a floating window keeps. The
/// fullscreen window, if any, is given `area`; the rest are laid out as usual underneath.
pub fn compute(
    tree: &WorkspaceTree,
    area: Rect,
    registry: &LayoutRegistry,
    frames: &dyn Fn(WindowId) -> Option<Rect>,
) -> ComputedLayout {
    let mut out = ComputedLayout::default();
    place(tree, tree.root(), area, registry, false, &mut out);

    let floating = tree.floating_windows();
    let children: Vec<Child> =
        floating.iter().map(|&w| Child { weight: 1.0, frame: frames(w) }).collect();
    let input = Arrange {
        area,
        orientation: Orientation::Horizontal,
        children: &children,
        focused: 0,
        state: None,
    };
    let system = registry.get(FLOATING);
    let rects = system.map_or_else(|| vec![area; children.len()], |system| system.arrange(&input));
    if system.is_none_or(|s| s.hints().overlapping) {
        out.overlapping.extend(floating.iter().copied());
    }
    out.windows.extend(floating.into_iter().zip(rects));

    if let Some(window) = tree.fullscreen() {
        out.windows.insert(window, area);
    }
    out
}

fn place(
    tree: &WorkspaceTree,
    id: NodeId,
    area: Rect,
    registry: &LayoutRegistry,
    overlapped: bool,
    out: &mut ComputedLayout,
) {
    out.containers.insert(id, area);
    let (Some(kind), Some(orientation)) = (tree.layout(id), tree.orientation(id)) else {
        return;
    };
    let system = registry.get_or_default(kind);
    let hints = system.hints();
    let nodes = tree.children(id);
    let children: Vec<Child> = nodes
        .iter()
        .map(|&n| Child { weight: tree.weight(n).unwrap_or(1.0), frame: None })
        .collect();
    let focused = tree.mru(id).first().and_then(|f| nodes.iter().position(|n| n == f)).unwrap_or(0);
    let fallback = if tree.layout_state(id).is_none() { system.init_state() } else { None };
    let state = tree.layout_state(id).or(fallback.as_ref());
    let rects = system.arrange(&Arrange { area, orientation, children: &children, focused, state });
    debug_assert_eq!(rects.len(), nodes.len(), "a layout system returns one rect per child");
    let overlapped = overlapped || hints.overlapping;
    if hints.overlapping
        && let Some(front) = tree.focus_leaf(id)
    {
        out.raise.push(front);
    }
    for (&node, rect) in nodes.iter().zip(rects) {
        match tree.window(node) {
            Some(window) => {
                out.windows.insert(window, rect);
                if overlapped {
                    out.overlapping.insert(window);
                }
            }
            None => place(tree, node, rect, registry, overlapped, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::app::WindowId;
    use crate::model::workspace::Direction;

    fn w(n: u32) -> WindowId { WindowId::new(1, n) }

    const AREA: Rect = Rect::new(0., 0., 400., 200.);

    fn tree_of(ids: &[u32]) -> WorkspaceTree {
        let mut t = WorkspaceTree::new(Orientation::Horizontal, TILES);
        for &i in ids {
            t.insert_tiled(w(i));
        }
        t
    }

    fn no_frames(_: WindowId) -> Option<Rect> { None }

    #[test]
    fn the_registry_ships_the_three_systems() {
        let reg = LayoutRegistry::with_builtin();
        for kind in [TILES, ACCORDION, FLOATING] {
            assert_eq!(reg.get(kind).map(|s| s.kind()), Some(kind));
        }
        assert!(reg.get(LayoutKind::new("bsp")).is_none(), "only the three systems exist");
        assert_eq!(reg.kinds().len(), 3);
    }

    #[test]
    fn an_unknown_kind_falls_back_to_the_default_system() {
        let reg = LayoutRegistry::with_builtin();
        assert_eq!(reg.get_or_default(LayoutKind::new("nope")).kind(), TILES);
    }

    const GRID: LayoutKind = LayoutKind::new("grid");

    #[derive(Debug, PartialEq)]
    struct Cols(usize);

    /// A system added from outside, standing in for a future bsp or master-stack. It
    /// keeps per-container state (the column count), navigates by rows and columns rather
    /// than along the orientation, and opts out of orientation alternation. Nothing in
    /// the trait, the tree, `compute` or `ops` was changed for it.
    #[derive(Debug)]
    struct Grid;

    impl Grid {
        fn cols(state: Option<&LayoutState>) -> usize {
            state.and_then(|s| s.get::<Cols>()).map_or(2, |c| c.0.max(1))
        }
    }

    impl LayoutSystem for Grid {
        fn kind(&self) -> LayoutKind { GRID }

        fn hints(&self) -> Hints {
            Hints { in_tree: true, overlapping: false, weighted: false, alternates: false, flattens: true }
        }

        fn init_state(&self) -> Option<LayoutState> { Some(LayoutState::new(Cols(2))) }

        fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> {
            let cols = Self::cols(input.state);
            let n = input.children.len();
            let rows = n.div_ceil(cols).max(1);
            let (cw, ch) = (input.area.size.w / cols as f64, input.area.size.h / rows as f64);
            (0..n)
                .map(|i| {
                    Rect::new(
                        input.area.origin.x + (i % cols) as f64 * cw,
                        input.area.origin.y + (i / cols) as f64 * ch,
                        cw,
                        ch,
                    )
                })
                .collect()
        }

        fn step(&self, nav: &Nav<'_>) -> Option<usize> {
            let cols = Self::cols(nav.state);
            let i = nav.from;
            match nav.direction {
                Direction::Left => (!i.is_multiple_of(cols)).then(|| i - 1),
                Direction::Right => (i % cols + 1 < cols && i + 1 < nav.len).then_some(i + 1),
                Direction::Up => i.checked_sub(cols),
                Direction::Down => (i + cols < nav.len).then_some(i + cols),
            }
        }

        fn place_beside(&self, nav: &Nav<'_>) -> Option<usize> {
            Some(nav.from + usize::from(is_forward(nav.direction)))
        }
    }

    /// A system that cannot be a container's layout.
    #[derive(Debug)]
    struct Free;

    impl LayoutSystem for Free {
        fn kind(&self) -> LayoutKind { LayoutKind::new("free") }

        fn hints(&self) -> Hints {
            Hints { in_tree: false, overlapping: true, weighted: false, alternates: false, flattens: false }
        }

        fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> { vec![input.area; input.children.len()] }
    }

    fn with_grid() -> LayoutRegistry {
        let mut reg = LayoutRegistry::with_builtin();
        reg.register(Box::new(Grid));
        reg.register(Box::new(Free));
        reg
    }

    fn grid_tree(reg: &LayoutRegistry, ids: &[u32], state: Option<LayoutState>) -> WorkspaceTree {
        let mut t = tree_of(ids);
        t.set_policies(reg.policies());
        let root = t.root();
        assert!(t.set_layout_with(root, GRID, state));
        t
    }

    #[test]
    fn a_new_system_uses_its_own_per_container_state() {
        let reg = with_grid();
        let t = grid_tree(&reg, &[1, 2, 3, 4], reg.init_state(GRID));
        let out = compute(&t, AREA, &reg, &no_frames);
        assert_eq!(out.windows[&w(1)], Rect::new(0., 0., 200., 100.));
        assert_eq!(out.windows[&w(4)], Rect::new(200., 100., 200., 100.));

        let one_column = grid_tree(&reg, &[1, 2, 3, 4], Some(LayoutState::new(Cols(1))));
        let out = compute(&one_column, AREA, &reg, &no_frames);
        assert_eq!(out.windows[&w(3)], Rect::new(0., 100., 400., 50.), "the state said one column");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_container_without_state_is_arranged_with_the_systems_default() {
        let reg = with_grid();
        let t = grid_tree(&reg, &[1, 2, 3, 4], None);
        assert_eq!(compute(&t, AREA, &reg, &no_frames).windows[&w(2)], Rect::new(200., 0., 200., 100.));
    }

    #[test]
    fn a_new_system_decides_what_focus_and_move_do() {
        let reg = with_grid();
        let mut t = grid_tree(&reg, &[1, 2, 3, 4], reg.init_state(GRID));
        // The root is horizontal, yet Down works: the grid, not the axis rule, answers.
        assert_eq!(ops::focus_target(&t, w(1), Direction::Down, false, &reg), Ok(Some(w(3))));
        assert_eq!(ops::focus_target(&t, w(3), Direction::Right, false, &reg), Ok(Some(w(4))));
        assert_eq!(ops::focus_target(&t, w(2), Direction::Left, false, &reg), Ok(Some(w(1))));
        assert_eq!(ops::focus_target(&t, w(1), Direction::Up, false, &reg), Ok(None));
        assert_eq!(ops::focus_target(&t, w(2), Direction::Right, false, &reg), Ok(None), "row end");
        ops::move_window(&mut t, w(1), Direction::Down, &reg).unwrap();
        assert_eq!(t.describe(), "h@grid[3 2 1 4]");
        assert_eq!(ops::move_window(&mut t, w(3), Direction::Up, &reg), Err(TilingError::AtBoundary));
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_new_system_can_opt_out_of_orientation_alternation() {
        let reg = with_grid();
        let mut t = WorkspaceTree::from_spec("h@grid[1 h@grid[2 3]]", TILES);
        t.set_policies(reg.policies());
        t.normalize();
        assert_eq!(t.describe(), "h@grid[1 h@grid[2 3]]", "grids keep their own orientation");
        t.check_invariants().unwrap();
        // The same shape of tiles containers alternates.
        let mut tiles = WorkspaceTree::from_spec("h[1 h[2 3]]", TILES);
        tiles.set_policies(reg.policies());
        tiles.normalize();
        assert_eq!(tiles.describe(), "h[1 v[2 3]]");
    }

    #[test]
    fn a_systems_state_travels_with_its_container_through_normalization() {
        let reg = with_grid();
        let mut t = WorkspaceTree::from_spec("h[h@grid[1 2 3 4]]", TILES);
        t.set_policies(reg.policies());
        let inner = t.children(t.root())[0];
        t.set_state(inner, Some(LayoutState::new(Cols(3))));
        t.normalize();
        assert_eq!(t.describe(), "h@grid[1 2 3 4]", "the root absorbed the lone container");
        assert_eq!(t.layout_state(t.root()).and_then(|s| s.get::<Cols>()), Some(&Cols(3)));
    }

    #[test]
    fn changing_a_containers_kind_drops_its_state() {
        let reg = with_grid();
        let mut t = grid_tree(&reg, &[1, 2], Some(LayoutState::new(Cols(3))));
        let root = t.root();
        assert!(t.set_layout(root, TILES));
        assert!(t.layout_state(root).is_none());
    }

    #[test]
    fn the_layout_command_reaches_new_systems_by_name() {
        let reg = with_grid();
        let mut t = tree_of(&[1, 2]);
        t.set_policies(reg.policies());
        ops::apply_layout(&mut t, w(1), &[ops::LayoutArg::Named(GRID)], &reg).unwrap();
        assert_eq!(t.describe(), "h@grid[1 2]");
        assert_eq!(t.layout_state(t.root()).and_then(|s| s.get::<Cols>()), Some(&Cols(2)), "init_state");
        // Applying the kind it already has keeps the state.
        t.set_state(t.root(), Some(LayoutState::new(Cols(5))));
        ops::apply_layout(&mut t, w(1), &[ops::LayoutArg::Named(GRID)], &reg).unwrap();
        assert_eq!(t.layout_state(t.root()).and_then(|s| s.get::<Cols>()), Some(&Cols(5)));
    }

    #[test]
    fn the_layout_command_rejects_unknown_and_non_container_systems_without_changes() {
        let reg = with_grid();
        let mut t = tree_of(&[1, 2]);
        t.set_policies(reg.policies());
        t.float(w(2));
        let before = t.fingerprint();
        let unknown = LayoutKind::new("nope");
        assert_eq!(
            ops::apply_layout(&mut t, w(2), &[ops::LayoutArg::Named(unknown)], &reg),
            Err(TilingError::UnknownLayout(unknown))
        );
        let free = LayoutKind::new("free");
        assert_eq!(
            ops::apply_layout(&mut t, w(2), &[ops::LayoutArg::Named(free)], &reg),
            Err(TilingError::NotAContainerLayout(free))
        );
        assert_eq!(t.fingerprint(), before, "the floating window was not tiled by a failing command");
    }

    #[test]
    fn named_floating_toggles_floating_like_the_floating_argument() {
        let reg = with_grid();
        let mut t = tree_of(&[1, 2]);
        ops::apply_layout(&mut t, w(1), &[ops::LayoutArg::Named(FLOATING)], &reg).unwrap();
        assert!(t.is_floating(w(1)));
    }

    #[test]
    fn the_tree_refuses_a_non_container_layout() {
        let reg = with_grid();
        let mut t = tree_of(&[1]);
        t.set_policies(reg.policies());
        let root = t.root();
        assert!(!t.set_layout(root, FLOATING));
        assert!(!t.set_layout(root, LayoutKind::new("free")));
        assert_eq!(t.layout(root), Some(TILES));
        assert!(t.set_layout(root, ACCORDION));
    }

    #[test]
    fn every_registered_system_has_a_policy() {
        let reg = with_grid();
        let policies = reg.policies();
        assert_eq!(policies.len(), 5);
        assert!(!policies[&GRID].alternates && policies[&GRID].flattens);
        assert!(!policies[&FLOATING].in_tree);
    }

    #[test]
    fn overlapping_systems_report_overlap_and_the_window_to_raise() {
        let mut t = tree_of(&[1, 2, 3]);
        t.set_layout(t.root(), ACCORDION);
        t.set_focus(w(2));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.overlapping.len(), 3);
        assert_eq!(out.raise, vec![w(2)]);
        t.set_focus(w(3));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.raise, vec![w(3)], "focus moves the raise hint, not the rects");
    }

    #[test]
    fn tiles_overlap_nothing_and_raise_nothing() {
        let out = compute(&tree_of(&[1, 2]), AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert!(out.overlapping.is_empty() && out.raise.is_empty());
    }

    #[test]
    fn windows_below_an_overlapping_container_overlap_too() {
        let mut t = WorkspaceTree::from_spec("h@accordion[1 v[2 3]]", TILES);
        t.set_focus(w(3));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.overlapping.len(), 3);
    }

    #[test]
    fn floating_windows_are_reported_as_overlapping() {
        let mut t = tree_of(&[1]);
        t.insert_floating(w(2));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert!(out.overlapping.contains(&w(2)) && !out.overlapping.contains(&w(1)));
    }

    #[test]
    fn tiles_lay_the_windows_out_by_weight() {
        let t = tree_of(&[1, 2]);
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.windows[&w(1)], Rect::new(0., 0., 200., 200.));
        assert_eq!(out.windows[&w(2)], Rect::new(200., 0., 200., 200.));
        assert_eq!(out.containers[&t.root()], AREA);
    }

    #[test]
    fn nested_containers_split_their_own_rect() {
        let mut t = tree_of(&[1, 2, 3]);
        let leaf = t.leaf(w(3)).unwrap();
        t.wrap(leaf, Orientation::Vertical);
        t.move_node(t.leaf(w(2)).unwrap(), t.parent(leaf).unwrap(), 0);
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2 3]]");
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.windows[&w(1)], Rect::new(0., 0., 200., 200.));
        assert_eq!(out.containers[&t.parent(leaf).unwrap()], Rect::new(200., 0., 200., 200.));
        assert_eq!(out.windows[&w(2)], Rect::new(200., 0., 200., 100.));
        assert_eq!(out.windows[&w(3)], Rect::new(200., 100., 200., 100.));
    }

    #[test]
    fn an_accordion_container_overlaps_its_children() {
        let mut t = tree_of(&[1, 2]);
        t.set_layout(t.root(), ACCORDION);
        t.set_focus(w(1));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.windows[&w(1)].origin.x, 0.);
        assert_eq!(out.windows[&w(2)].origin.x, 30.);
        assert_eq!(out.windows[&w(1)].size.w, 370.);
    }

    #[test]
    fn floating_windows_keep_their_frames_and_leave_the_tiles_alone() {
        let mut t = tree_of(&[1, 2]);
        t.insert_floating(w(3));
        let own = Rect::new(50., 60., 70., 80.);
        let frames = |wid: WindowId| (wid == w(3)).then_some(own);
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &frames);
        assert_eq!(out.windows[&w(3)], own);
        assert_eq!(out.windows[&w(1)], Rect::new(0., 0., 200., 200.), "tiled windows ignore it");
    }

    #[test]
    fn fullscreen_fills_the_area_without_disturbing_the_others() {
        let mut t = tree_of(&[1, 2]);
        t.toggle_fullscreen(w(2));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.windows[&w(2)], AREA);
        assert_eq!(out.windows[&w(1)], Rect::new(0., 0., 200., 200.));
    }

    #[test]
    fn every_window_in_the_tree_gets_exactly_one_frame() {
        let mut t = tree_of(&[1, 2, 3, 4]);
        t.insert_floating(w(5));
        let out = compute(&t, AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert_eq!(out.windows.len(), 5);
    }

    #[test]
    fn an_empty_tree_has_no_frames() {
        let out = compute(&tree_of(&[]), AREA, &LayoutRegistry::with_builtin(), &no_frames);
        assert!(out.windows.is_empty());
    }
}
