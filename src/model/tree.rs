//! The per-workspace tiling tree.
//!
//! Each workspace owns one [`WorkspaceTree`]: containers and window leaves in an
//! arena, plus a set of floating windows that live outside the tree. A container has an
//! [`Orientation`] and a [`LayoutKind`] (an opaque identifier resolved to a layout
//! system elsewhere; nothing here knows what `tiles` or `accordion` mean), and every
//! child carries a sibling weight. Each container remembers its children in
//! most-recently-focused order, so focus can be followed from the root to a leaf.
//!
//! The tree is pure data: no AX, no CGS, no geometry. Structural primitives
//! ([`WorkspaceTree::wrap`], [`WorkspaceTree::move_node`], ...) may leave the tree in a
//! shape that violates the invariants; [`WorkspaceTree::normalize`] puts it right again
//! and every command (see `layout::ops`) runs it after mutating. The high-level
//! mutators (`insert_tiled`, `remove`, `float`, ...) normalize themselves.
//!
//! Normalization, after AeroSpace's two tree clean-up options
//! ([`NormalizeOptions`]):
//! - flatten: an empty container is dropped; a non-root container with one child is
//!   replaced by that child (which inherits the container's weight); a root whose only
//!   child is a container adopts that container's orientation, layout and children.
//! - opposite orientation: the root's orientation is authoritative and every nested
//!   container alternates with its parent, top-down.
//!
//! [`WorkspaceTree::check_invariants`] verifies the whole structure.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashSet as StdHashSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::actor::app::WindowId;
use crate::common::collections::HashMap;
use crate::model::workspace::Direction;

/// The axis a container lays its children out along.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Orientation {
    Horizontal,
    Vertical,
}

impl Orientation {
    pub fn opposite(self) -> Self {
        match self {
            Orientation::Horizontal => Orientation::Vertical,
            Orientation::Vertical => Orientation::Horizontal,
        }
    }

    /// The axis a direction moves along.
    pub fn of(direction: Direction) -> Self {
        match direction {
            Direction::Left | Direction::Right => Orientation::Horizontal,
            Direction::Up | Direction::Down => Orientation::Vertical,
        }
    }
}

/// Names a layout system. The tree stores one per container and never interprets it;
/// `layout::LayoutRegistry` maps it to behaviour. A new system is a new constant (or a
/// name interned from config, see the `Deserialize` impl).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayoutKind(&'static str);

impl LayoutKind {
    pub const fn new(name: &'static str) -> Self { Self(name) }

    pub fn name(self) -> &'static str { self.0 }
}

impl fmt::Display for LayoutKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.0) }
}

impl Serialize for LayoutKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0)
    }
}

/// Names seen so far, so a kind read from config gets a `'static` name without leaking
/// the same string twice. Bounded by the number of distinct layout names.
fn intern(name: &str) -> &'static str {
    static NAMES: OnceLock<Mutex<StdHashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&known) = names.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

impl<'de> Deserialize<'de> for LayoutKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Ok(LayoutKind(intern(&name)))
    }
}

/// Whatever a value needs to be to live in a [`LayoutState`].
pub trait StateValue: Any + Send + Sync + fmt::Debug {
    fn as_any(&self) -> &dyn Any;
}

impl<T: Any + Send + Sync + fmt::Debug> StateValue for T {
    fn as_any(&self) -> &dyn Any { self }
}

/// Per-container state of a layout system (a bsp split ratio, a master count, a scroll
/// offset ...). The tree stores it without looking inside; the system that made it reads
/// it back with [`LayoutState::get`]. It travels with its container through
/// normalization (including when a lone-child root absorbs the container) and is
/// dropped when the container's layout kind changes.
#[derive(Clone, Debug)]
pub struct LayoutState(Arc<dyn StateValue>);

impl LayoutState {
    pub fn new<T: StateValue>(value: T) -> Self { LayoutState(Arc::new(value)) }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        // Through the trait object: calling on the `Arc` itself would see the `Arc` as `Any`.
        let value: &dyn StateValue = &*self.0;
        value.as_any().downcast_ref()
    }
}

/// What the tree must know about a layout kind to normalize and validate itself; the
/// layout registry derives it from each system's hints. A kind nobody described gets the
/// default, which is AeroSpace's behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KindPolicy {
    /// May be the layout of a container. `false` for systems that arrange the floating
    /// set only.
    pub in_tree: bool,
    /// A nested container of this kind alternates orientation with its parent.
    pub alternates: bool,
    /// A single-child container of this kind is replaced by its child.
    pub flattens: bool,
}

impl Default for KindPolicy {
    fn default() -> Self { KindPolicy { in_tree: true, alternates: true, flattens: true } }
}

/// Which clean-up passes [`WorkspaceTree::normalize`] runs. Both on by default, as in
/// AeroSpace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NormalizeOptions {
    /// Replace single-child containers by their child.
    pub flatten_containers: bool,
    /// Nested containers take the opposite orientation of their parent.
    pub opposite_orientation: bool,
}

impl Default for NormalizeOptions {
    fn default() -> Self { NormalizeOptions { flatten_containers: true, opposite_orientation: true } }
}

/// Identifies a node within one workspace's tree. Ids are never reused, so a stale one
/// (its node was removed) is simply unknown: every accessor tolerates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

#[derive(Clone, Debug)]
struct Container {
    orientation: Orientation,
    layout: LayoutKind,
    /// The layout system's own per-container data; `None` means "the system's default".
    state: Option<LayoutState>,
    children: Vec<NodeId>,
    /// The same nodes as `children`, most recently focused first.
    mru: Vec<NodeId>,
}

#[derive(Clone, Debug)]
enum Kind {
    Window(WindowId),
    Container(Container),
}

/// One arena slot: a window leaf or a container, with its place in the tree.
#[derive(Clone, Debug)]
struct Node {
    parent: Option<NodeId>,
    /// Share of the parent's extent along its orientation, relative to the siblings.
    weight: f64,
    kind: Kind,
}

/// No child is resized below this share of its container (capped for large containers).
const MIN_SHARE: f64 = 0.05;

#[derive(Clone, Debug)]
pub struct WorkspaceTree {
    nodes: BTreeMap<NodeId, Node>,
    root: NodeId,
    next_id: u32,
    leaves: HashMap<WindowId, NodeId>,
    floating: BTreeSet<WindowId>,
    fullscreen: Option<WindowId>,
    /// The layout of containers the tree creates itself.
    default_layout: LayoutKind,
    options: NormalizeOptions,
    policies: Arc<BTreeMap<LayoutKind, KindPolicy>>,
}

impl WorkspaceTree {
    pub fn new(root_orientation: Orientation, default_layout: LayoutKind) -> Self {
        let root = NodeId(0);
        let mut nodes = BTreeMap::new();
        nodes.insert(root, Node {
            parent: None,
            weight: 1.0,
            kind: Kind::Container(Container {
                orientation: root_orientation,
                layout: default_layout,
                state: None,
                children: Vec::new(),
                mru: Vec::new(),
            }),
        });
        WorkspaceTree {
            nodes,
            root,
            next_id: 1,
            leaves: HashMap::default(),
            floating: BTreeSet::new(),
            fullscreen: None,
            default_layout,
            options: NormalizeOptions::default(),
            policies: Arc::default(),
        }
    }

    // ---- queries ----

    pub fn root(&self) -> NodeId { self.root }

    pub fn default_layout(&self) -> LayoutKind { self.default_layout }

    pub fn options(&self) -> NormalizeOptions { self.options }

    /// Change the passes; takes effect at the next [`Self::normalize`].
    pub fn set_options(&mut self, options: NormalizeOptions) { self.options = options; }

    /// Per-kind normalization rules (see [`KindPolicy`]); takes effect at the next
    /// [`Self::normalize`].
    pub fn set_policies(&mut self, policies: Arc<BTreeMap<LayoutKind, KindPolicy>>) {
        self.policies = policies;
    }

    pub fn policy(&self, kind: LayoutKind) -> KindPolicy {
        self.policies.get(&kind).copied().unwrap_or_default()
    }

    fn kind_flattens(&self, kind: LayoutKind) -> bool {
        self.options.flatten_containers && self.policy(kind).flattens
    }

    fn kind_alternates(&self, kind: LayoutKind) -> bool {
        self.options.opposite_orientation && self.policy(kind).alternates
    }

    /// Whether no window, tiled or floating, is in the tree.
    pub fn is_empty(&self) -> bool { self.leaves.is_empty() && self.floating.is_empty() }

    pub fn contains(&self, window: WindowId) -> bool {
        self.leaves.contains_key(&window) || self.floating.contains(&window)
    }

    pub fn is_tiled(&self, window: WindowId) -> bool { self.leaves.contains_key(&window) }

    pub fn is_floating(&self, window: WindowId) -> bool { self.floating.contains(&window) }

    pub fn leaf(&self, window: WindowId) -> Option<NodeId> { self.leaves.get(&window).copied() }

    pub fn fullscreen(&self) -> Option<WindowId> { self.fullscreen }

    pub fn floating_windows(&self) -> Vec<WindowId> { self.floating.iter().copied().collect() }

    /// Every tiled window, depth first in tree order.
    pub fn tiled_windows(&self) -> Vec<WindowId> {
        let mut out = Vec::with_capacity(self.leaves.len());
        self.collect_windows(self.root, &mut out);
        out
    }

    fn collect_windows(&self, id: NodeId, out: &mut Vec<WindowId>) {
        match &self.nodes[&id].kind {
            Kind::Window(w) => out.push(*w),
            Kind::Container(c) => c.children.iter().for_each(|&child| self.collect_windows(child, out)),
        }
    }

    /// Every container, root first.
    pub fn containers(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| matches!(n.kind, Kind::Container(_)))
            .map(|(&id, _)| id)
            .collect()
    }

    pub fn parent(&self, node: NodeId) -> Option<NodeId> {
        self.nodes.get(&node).and_then(|n| n.parent)
    }

    pub fn is_container(&self, node: NodeId) -> bool {
        matches!(self.nodes.get(&node).map(|n| &n.kind), Some(Kind::Container(_)))
    }

    /// The window at a leaf; `None` for a container.
    pub fn window(&self, node: NodeId) -> Option<WindowId> {
        match self.nodes.get(&node)?.kind {
            Kind::Window(w) => Some(w),
            Kind::Container(_) => None,
        }
    }

    pub fn orientation(&self, node: NodeId) -> Option<Orientation> {
        self.container(node).map(|c| c.orientation)
    }

    pub fn layout(&self, node: NodeId) -> Option<LayoutKind> { self.container(node).map(|c| c.layout) }

    /// Children in order; empty for a leaf.
    pub fn children(&self, node: NodeId) -> &[NodeId] {
        self.container(node).map_or(&[], |c| &c.children)
    }

    /// Children, most recently focused first; empty for a leaf.
    pub fn mru(&self, node: NodeId) -> &[NodeId] { self.container(node).map_or(&[], |c| &c.mru) }

    /// The node's weight, or `None` for an unknown (stale) node.
    pub fn weight(&self, node: NodeId) -> Option<f64> { self.nodes.get(&node).map(|n| n.weight) }

    /// The layout system's state for a container, if it has set any.
    pub fn layout_state(&self, node: NodeId) -> Option<&LayoutState> {
        self.container(node)?.state.as_ref()
    }

    pub fn index_in_parent(&self, node: NodeId) -> Option<usize> {
        let parent = self.parent(node)?;
        self.children(parent).iter().position(|&c| c == node)
    }

    /// The window focus rests on: the most recent child at every level from the root.
    pub fn focused_window(&self) -> Option<WindowId> {
        let mut at = self.root;
        loop {
            match &self.nodes[&at].kind {
                Kind::Window(w) => return Some(*w),
                Kind::Container(c) => at = *c.mru.first()?,
            }
        }
    }

    /// The most recently focused window at or below `node`.
    pub fn focus_leaf(&self, node: NodeId) -> Option<WindowId> {
        let mut at = node;
        loop {
            match &self.nodes.get(&at)?.kind {
                Kind::Window(w) => return Some(*w),
                Kind::Container(c) => at = *c.mru.first()?,
            }
        }
    }

    fn container(&self, node: NodeId) -> Option<&Container> {
        match &self.nodes.get(&node)?.kind {
            Kind::Container(c) => Some(c),
            Kind::Window(_) => None,
        }
    }

    fn container_mut(&mut self, node: NodeId) -> Option<&mut Container> {
        match &mut self.nodes.get_mut(&node)?.kind {
            Kind::Container(c) => Some(c),
            Kind::Window(_) => None,
        }
    }

    fn alloc(&mut self, kind: Kind, weight: f64) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(id, Node { parent: None, weight, kind });
        id
    }

    // ---- window-level mutators (normalize themselves) ----

    /// Add a tiled window right after the focused one, in the same container, and focus
    /// it within the tree. With nothing focused it goes at the end of the root. A window
    /// already in the tree is left alone.
    pub fn insert_tiled(&mut self, window: WindowId) {
        if self.insert_tiled_after(window, None) {
            self.set_focus(window);
        }
    }

    /// Add a tiled window right after `anchor` (default: the focused window), in the same
    /// container, *without* changing focus: the window is last in the focus order of the
    /// containers it joins. Returns whether it was added. A window already in the tree,
    /// or an anchor that is not tiled, is handled as for [`Self::insert_tiled`].
    pub fn insert_tiled_after(&mut self, window: WindowId, anchor: Option<WindowId>) -> bool {
        if self.contains(window) {
            return false;
        }
        let (parent, index) = self.insertion_point(anchor);
        let node = self.alloc(Kind::Window(window), 1.0);
        self.leaves.insert(window, node);
        self.attach(node, parent, index, None);
        self.normalize();
        true
    }

    fn insertion_point(&self, anchor: Option<WindowId>) -> (NodeId, usize) {
        let at = anchor
            .filter(|&a| self.is_tiled(a))
            .or_else(|| self.focused_window())
            .and_then(|w| self.leaf(w))
            .and_then(|leaf| Some((self.parent(leaf)?, self.index_in_parent(leaf)? + 1)));
        at.unwrap_or((self.root, self.children(self.root).len()))
    }

    /// Add a floating window. A window already in the tree is left alone.
    pub fn insert_floating(&mut self, window: WindowId) {
        if !self.contains(window) {
            self.floating.insert(window);
        }
    }

    /// Forget a window, tiled or floating. Returns whether it was present.
    pub fn remove(&mut self, window: WindowId) -> bool {
        let was_tiled = self.remove_leaf(window);
        let was_floating = self.floating.remove(&window);
        if !(was_tiled || was_floating) {
            return false;
        }
        if self.fullscreen == Some(window) {
            self.fullscreen = None;
        }
        self.normalize();
        true
    }

    fn remove_leaf(&mut self, window: WindowId) -> bool {
        let Some(node) = self.leaves.remove(&window) else { return false };
        self.detach(node);
        self.nodes.remove(&node);
        true
    }

    /// Take a tiled window out of the tree and let it keep its own frame.
    pub fn float(&mut self, window: WindowId) -> bool {
        if !self.remove_leaf(window) {
            return false;
        }
        self.floating.insert(window);
        self.normalize();
        true
    }

    /// Put a floating window into the tree, after the focused window.
    pub fn tile(&mut self, window: WindowId) -> bool {
        if !self.floating.remove(&window) {
            return false;
        }
        self.insert_tiled(window);
        true
    }

    /// Make `window` the focus of every container on its path. Only tiled windows have
    /// a place in the focus order.
    pub fn set_focus(&mut self, window: WindowId) -> bool {
        let Some(mut at) = self.leaf(window) else { return false };
        while let Some(parent) = self.parent(at) {
            if let Some(c) = self.container_mut(parent) {
                c.mru.retain(|&n| n != at);
                c.mru.insert(0, at);
            }
            at = parent;
        }
        true
    }

    /// Toggle fullscreen for `window`; `None` if the tree does not hold it. At most one
    /// window per workspace is fullscreen.
    pub fn toggle_fullscreen(&mut self, window: WindowId) -> Option<bool> {
        if !self.contains(window) {
            return None;
        }
        if self.fullscreen == Some(window) {
            self.fullscreen = None;
            Some(false)
        } else {
            self.fullscreen = Some(window);
            Some(true)
        }
    }

    // ---- structural primitives (the caller normalizes) ----

    /// Take `node` out of its parent. It stays in the arena, unparented, and must be
    /// attached again or dropped.
    pub fn detach(&mut self, node: NodeId) {
        let Some(parent) = self.parent(node) else { return };
        if let Some(c) = self.container_mut(parent) {
            c.children.retain(|&n| n != node);
            c.mru.retain(|&n| n != node);
        }
        if let Some(n) = self.nodes.get_mut(&node) {
            n.parent = None;
        }
    }

    /// Insert an unparented `node` into `parent` at `index` (clamped), last in the focus
    /// order. `weight` defaults to the mean of the siblings' (1 with none).
    /// Does nothing if `node` is unknown or still attached, or `parent` is not a container.
    pub fn attach(&mut self, node: NodeId, parent: NodeId, index: usize, weight: Option<f64>) {
        if !self.nodes.contains_key(&node) || self.parent(node).is_some() || !self.is_container(parent) {
            return;
        }
        let siblings = self.children(parent);
        let weight = weight.unwrap_or_else(|| {
            if siblings.is_empty() {
                1.0
            } else {
                siblings.iter().map(|&s| self.nodes[&s].weight).sum::<f64>() / siblings.len() as f64
            }
        });
        if let Some(c) = self.container_mut(parent) {
            let index = index.min(c.children.len());
            c.children.insert(index, node);
            c.mru.push(node);
        }
        if let Some(n) = self.nodes.get_mut(&node) {
            n.parent = Some(parent);
            n.weight = weight;
        }
    }

    /// Move `node` to `index` in `parent`, where `index` counts the children as they are
    /// before the move. Within one parent the focus order is kept. The weight becomes the
    /// destination siblings' mean.
    /// Does nothing for an unknown node or a destination inside the node itself.
    pub fn move_node(&mut self, node: NodeId, parent: NodeId, index: usize) {
        if !self.nodes.contains_key(&node) || !self.is_container(parent) || self.is_within(parent, node) {
            return;
        }
        let same = self.parent(node) == Some(parent);
        let mru_at = same.then(|| self.mru(parent).iter().position(|&n| n == node)).flatten();
        let index = match self.index_in_parent(node) {
            Some(old) if same && old < index => index - 1,
            _ => index,
        };
        self.detach(node);
        self.attach(node, parent, index, None);
        if let Some(at) = mru_at
            && let Some(c) = self.container_mut(parent)
        {
            c.mru.retain(|&n| n != node);
            c.mru.insert(at.min(c.mru.len()), node);
        }
    }

    /// Whether `node` is `ancestor` or below it.
    fn is_within(&self, node: NodeId, ancestor: NodeId) -> bool {
        let mut at = Some(node);
        while let Some(n) = at {
            if n == ancestor {
                return true;
            }
            at = self.parent(n);
        }
        false
    }

    /// Replace `node` by a new single-child container of `orientation` that takes its slot,
    /// focus position and weight. Returns the container, or `None` for the root or an
    /// unknown node.
    pub fn wrap(&mut self, node: NodeId, orientation: Orientation) -> Option<NodeId> {
        let parent = self.parent(node)?;
        let weight = self.weight(node)?;
        let wrapper = self.alloc(
            Kind::Container(Container {
                orientation,
                layout: self.default_layout,
                state: None,
                children: vec![node],
                mru: vec![node],
            }),
            weight,
        );
        if let Some(c) = self.container_mut(parent) {
            for list in [&mut c.children, &mut c.mru] {
                if let Some(slot) = list.iter_mut().find(|n| **n == node) {
                    *slot = wrapper;
                }
            }
        }
        if let Some(n) = self.nodes.get_mut(&wrapper) {
            n.parent = Some(parent);
        }
        if let Some(n) = self.nodes.get_mut(&node) {
            n.parent = Some(wrapper);
            n.weight = 1.0;
        }
        Some(wrapper)
    }

    /// Exchange two siblings' positions. Weights belong to the slot and stay put.
    /// `false` (and no change) unless both are known siblings.
    pub fn swap_siblings(&mut self, a: NodeId, b: NodeId) -> bool {
        let Some(parent) = self.parent(a).filter(|&p| self.parent(b) == Some(p)) else {
            return false;
        };
        let (Some(ia), Some(ib)) = (self.index_in_parent(a), self.index_in_parent(b)) else {
            return false;
        };
        let (Some(wa), Some(wb)) = (self.weight(a), self.weight(b)) else { return false };
        if let Some(c) = self.container_mut(parent) {
            c.children.swap(ia, ib);
        }
        self.set_weight(a, wb);
        self.set_weight(b, wa);
        true
    }

    pub fn set_orientation(&mut self, node: NodeId, orientation: Orientation) {
        if let Some(c) = self.container_mut(node) {
            c.orientation = orientation;
        }
    }

    /// Give a container a layout kind, dropping the state of the previous one. Refused
    /// (`false`) for an unknown node or a kind whose [`KindPolicy::in_tree`] is off.
    pub fn set_layout(&mut self, node: NodeId, layout: LayoutKind) -> bool {
        self.set_layout_with(node, layout, None)
    }

    /// [`Self::set_layout`], with the new system's initial state.
    pub fn set_layout_with(&mut self, node: NodeId, layout: LayoutKind, state: Option<LayoutState>) -> bool {
        if !self.policy(layout).in_tree {
            return false;
        }
        match self.container_mut(node) {
            Some(c) => {
                c.layout = layout;
                c.state = state;
                true
            }
            None => false,
        }
    }

    /// Replace a container's layout state (the kind stays).
    pub fn set_state(&mut self, node: NodeId, state: Option<LayoutState>) {
        if let Some(c) = self.container_mut(node) {
            c.state = state;
        }
    }

    /// Ignores unknown nodes and weights that are not positive and finite.
    pub fn set_weight(&mut self, node: NodeId, weight: f64) {
        if !(weight.is_finite() && weight > 0.0) {
            return;
        }
        if let Some(n) = self.nodes.get_mut(&node) {
            n.weight = weight;
        }
    }

    /// Grow `node` by `delta` weight units at its siblings' expense (shrink if negative).
    ///
    /// The total stays the same and the change is direction-safe: growing never shrinks
    /// the node and shrinking never grows it, even when it is already outside the limits.
    /// The node is limited so each sibling keeps a minimum share of the total; the
    /// siblings give up (or take) the change in proportion to their weights, except that
    /// one that would fall below the minimum is pinned there and the others share the
    /// rest (water-filling). `false` if there is no sibling to trade with.
    pub fn resize_child(&mut self, node: NodeId, delta: f64) -> bool {
        let Some(parent) = self.parent(node) else { return false };
        let siblings = self.children(parent).to_vec();
        let n = siblings.len();
        if n < 2 || !delta.is_finite() {
            return false;
        }
        let weight_of = |t: &Self, id: NodeId| t.weight(id).unwrap_or(1.0);
        let total: f64 = siblings.iter().map(|&s| weight_of(self, s)).sum();
        let min = total * MIN_SHARE.min(0.5 / n as f64);
        let old = weight_of(self, node);
        let new = if delta >= 0.0 {
            old.max((old + delta).min(total - min * (n - 1) as f64))
        } else {
            old.min((old + delta).max(min))
        };
        // Water-filling over the others: pin whoever would drop below `min`.
        let others: Vec<(NodeId, f64)> =
            siblings.iter().filter(|&&s| s != node).map(|&s| (s, weight_of(self, s))).collect();
        // A sibling is pinned at the minimum, or at its own weight if that is already lower
        // (it is never made smaller than it was).
        let pin_level = |i: usize| min.min(others[i].1);
        let mut pinned = vec![false; others.len()];
        let share = loop {
            let free: f64 = others.iter().zip(&pinned).filter(|(_, p)| !**p).map(|((_, w), _)| w).sum();
            let pins: f64 = (0..others.len()).filter(|&i| pinned[i]).map(pin_level).sum();
            let factor = (total - new - pins) / free;
            let newly: Vec<usize> = (0..others.len())
                .filter(|&i| !pinned[i] && others[i].1 * factor < pin_level(i))
                .collect();
            if newly.is_empty() {
                break factor;
            }
            for i in newly {
                pinned[i] = true;
            }
            if pinned.iter().all(|p| *p) {
                break 0.0;
            }
        };
        self.set_weight(node, new);
        for (i, &(id, w)) in others.iter().enumerate() {
            self.set_weight(id, if pinned[i] { pin_level(i) } else { w * share });
        }
        true
    }

    /// Equal weights everywhere.
    pub fn balance(&mut self) {
        for node in self.nodes.values_mut() {
            node.weight = 1.0;
        }
    }

    /// Windows in recency order: depth first, each container's children most recently
    /// focused first.
    fn windows_by_recency(&self, id: NodeId, out: &mut Vec<WindowId>) {
        match self.nodes.get(&id).map(|n| &n.kind) {
            Some(Kind::Window(w)) => out.push(*w),
            Some(Kind::Container(c)) => c.mru.iter().for_each(|&child| self.windows_by_recency(child, out)),
            None => {}
        }
    }

    /// Collapse the tree to the root and its tiled windows in tree order, equally
    /// weighted. The root keeps its orientation, layout and state; the focus order
    /// becomes the old recency order (most recent first), so focus survives.
    pub fn flatten_to_root(&mut self) {
        let mut recency = Vec::new();
        self.windows_by_recency(self.root, &mut recency);
        let windows = self.tiled_windows();
        let root = self.root;
        self.nodes.retain(|&id, _| id == root);
        self.leaves.clear();
        if let Some(c) = self.container_mut(root) {
            c.children.clear();
            c.mru.clear();
        }
        for window in windows {
            let node = self.alloc(Kind::Window(window), 1.0);
            self.leaves.insert(window, node);
            self.attach(node, root, usize::MAX, Some(1.0));
        }
        let order: Vec<NodeId> = recency.iter().filter_map(|w| self.leaf(*w)).collect();
        if let Some(c) = self.container_mut(root) {
            c.mru = order;
        }
        self.normalize();
    }

    // ---- normalization ----

    /// Run the enabled clean-up passes. Idempotent.
    pub fn normalize(&mut self) {
        self.prune_and_flatten(self.root);
        self.absorb_lone_root_child();
        self.alternate(self.root);
    }

    fn prune_and_flatten(&mut self, id: NodeId) {
        let children = self.children(id).to_vec();
        for &child in &children {
            if self.is_container(child) {
                self.prune_and_flatten(child);
            }
        }
        for child in children {
            if !self.is_container(child) {
                continue;
            }
            match self.children(child).len() {
                0 => {
                    self.detach(child);
                    self.nodes.remove(&child);
                }
                1 if self.layout(child).is_some_and(|k| self.kind_flattens(k)) => {
                    self.replace_with_only_child(child);
                }
                _ => {}
            }
        }
    }

    fn replace_with_only_child(&mut self, container: NodeId) {
        let (Some(parent), Some(&child), Some(weight)) =
            (self.parent(container), self.children(container).first(), self.weight(container))
        else {
            return;
        };
        if let Some(c) = self.container_mut(container) {
            c.children.clear();
            c.mru.clear();
        }
        if let Some(p) = self.container_mut(parent) {
            for list in [&mut p.children, &mut p.mru] {
                if let Some(slot) = list.iter_mut().find(|n| **n == container) {
                    *slot = child;
                }
            }
        }
        if let Some(n) = self.nodes.get_mut(&child) {
            n.parent = Some(parent);
            n.weight = weight;
        }
        self.nodes.remove(&container);
    }

    /// A root whose only child is a container becomes that container (orientation,
    /// layout, state and children), when both kinds flatten.
    fn absorb_lone_root_child(&mut self) {
        let root = self.root;
        while let [only] = *self.children(root)
            && let Some(inner) = self.container(only).cloned()
            && self.kind_flattens(inner.layout)
            && self.layout(root).is_some_and(|k| self.kind_flattens(k))
        {
            for &grandchild in &inner.children {
                if let Some(n) = self.nodes.get_mut(&grandchild) {
                    n.parent = Some(root);
                }
            }
            if let Some(c) = self.container_mut(root) {
                *c = inner;
            }
            self.nodes.remove(&only);
        }
    }

    /// Top-down: a container whose kind alternates takes its parent's opposite.
    fn alternate(&mut self, id: NodeId) {
        let Some(orientation) = self.orientation(id) else { return };
        for child in self.children(id).to_vec() {
            if let Some(kind) = self.layout(child) {
                if self.kind_alternates(kind) {
                    self.set_orientation(child, orientation.opposite());
                }
                self.alternate(child);
            }
        }
    }

    /// Build a tree from `h[1 v[2 3]@name]`-style text: `h`/`v` is a container's
    /// orientation, `@name` its layout (else the default), numbers are window indexes
    /// (pid 1). Weights are 1 and focus rests on the first leaf; nothing is normalized,
    /// so a spec may describe an invalid tree.
    #[cfg(test)]
    pub(crate) fn from_spec(spec: &str, default_layout: LayoutKind) -> WorkspaceTree {
        let mut chars = spec.chars().peekable();
        let orientation = Self::spec_orientation(chars.next());
        let mut tree = WorkspaceTree::new(orientation, default_layout);
        tree.spec_layout(&mut chars, tree.root);
        assert_eq!(chars.next(), Some('['), "spec: container body");
        tree.spec_items(&mut chars, tree.root);
        tree
    }

    #[cfg(test)]
    fn spec_orientation(c: Option<char>) -> Orientation {
        match c {
            Some('h') => Orientation::Horizontal,
            Some('v') => Orientation::Vertical,
            other => panic!("spec: expected h or v, got {other:?}"),
        }
    }

    #[cfg(test)]
    fn spec_layout(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>, node: NodeId) {
        if chars.peek() == Some(&'@') {
            chars.next();
            let mut name = String::new();
            while let Some(&c) = chars.peek().filter(|c| c.is_alphabetic()) {
                name.push(c);
                chars.next();
            }
            let kind = LayoutKind::new(Box::leak(name.into_boxed_str()));
            self.set_layout(node, kind);
        }
    }

    #[cfg(test)]
    fn spec_items(&mut self, chars: &mut std::iter::Peekable<std::str::Chars<'_>>, parent: NodeId) {
        loop {
            match chars.peek().copied() {
                Some(' ') => {
                    chars.next();
                }
                Some(']') => {
                    chars.next();
                    return;
                }
                Some(d) if d.is_ascii_digit() => {
                    let mut n = 0u32;
                    while let Some(d) = chars.peek().and_then(|c| c.to_digit(10)) {
                        n = n * 10 + d;
                        chars.next();
                    }
                    let window = WindowId::new(1, n);
                    let node = self.alloc(Kind::Window(window), 1.0);
                    self.leaves.insert(window, node);
                    self.attach(node, parent, usize::MAX, Some(1.0));
                }
                Some(o) => {
                    chars.next();
                    let orientation = Self::spec_orientation(Some(o));
                    let node = self.alloc(
                        Kind::Container(Container {
                            orientation,
                            layout: self.default_layout,
                            state: None,
                            children: Vec::new(),
                            mru: Vec::new(),
                        }),
                        1.0,
                    );
                    self.attach(node, parent, usize::MAX, Some(1.0));
                    self.spec_layout(chars, node);
                    assert_eq!(chars.next(), Some('['), "spec: container body");
                    self.spec_items(chars, node);
                }
                None => panic!("spec: unterminated container"),
            }
        }
    }

    /// Everything observable about the tree as one string (structure, weights, focus
    /// order, layout state, floating set, fullscreen), for asserting that a failed
    /// command changed nothing.
    pub fn fingerprint(&self) -> String {
        format!("{:?}|{:?}|{:?}", self.nodes, self.floating, self.fullscreen)
    }

    // ---- verification ----

    /// Check every structural invariant; the error says which one broke.
    pub fn check_invariants(&self) -> Result<(), String> {
        let root = self.nodes.get(&self.root).ok_or("the root is missing")?;
        if root.parent.is_some() || !matches!(root.kind, Kind::Container(_)) {
            return Err("the root must be a parentless container".into());
        }
        let mut seen = 0usize;
        let mut windows = Vec::new();
        self.check_node(self.root, &mut seen, &mut windows)?;
        if seen != self.nodes.len() {
            return Err(format!("{} nodes are unreachable from the root", self.nodes.len() - seen));
        }
        let mut sorted = windows.clone();
        sorted.sort();
        sorted.dedup();
        if sorted.len() != windows.len() {
            return Err("a window has more than one leaf".into());
        }
        if windows.len() != self.leaves.len() {
            return Err("the leaf index and the tree disagree on the tiled windows".into());
        }
        for (window, node) in &self.leaves {
            if self.window(*node) != Some(*window) {
                return Err(format!("the leaf index points {window:?} at the wrong node"));
            }
        }
        if let Some(both) = self.floating.iter().find(|w| self.leaves.contains_key(*w)) {
            return Err(format!("{both:?} is both tiled and floating"));
        }
        if let Some(window) = self.fullscreen
            && !self.contains(window)
        {
            return Err(format!("fullscreen window {window:?} is not in the tree"));
        }
        if let [only] = *self.children(self.root)
            && let (Some(root_kind), Some(inner_kind)) = (self.layout(self.root), self.layout(only))
            && self.kind_flattens(root_kind)
            && self.kind_flattens(inner_kind)
        {
            return Err("the root's only child is a container".into());
        }
        Ok(())
    }

    fn check_node(
        &self,
        id: NodeId,
        seen: &mut usize,
        windows: &mut Vec<WindowId>,
    ) -> Result<(), String> {
        *seen += 1;
        let node = &self.nodes[&id];
        if !(node.weight.is_finite() && node.weight > 0.0) {
            return Err(format!("{id:?} has weight {}", node.weight));
        }
        let c = match &node.kind {
            Kind::Window(w) => {
                windows.push(*w);
                return Ok(());
            }
            Kind::Container(c) => c,
        };
        let mut mru = c.mru.clone();
        let mut children = c.children.clone();
        mru.sort();
        children.sort();
        if mru != children {
            return Err(format!("{id:?}: focus order is not a permutation of the children"));
        }
        if !self.policy(c.layout).in_tree {
            return Err(format!("{id:?} uses layout {} which is not a container layout", c.layout));
        }
        if id != self.root {
            if c.children.is_empty() {
                return Err(format!("{id:?} is an empty container"));
            }
            if self.kind_flattens(c.layout) && c.children.len() == 1 {
                return Err(format!("{id:?} is a container with a single child"));
            }
            let parent_orientation = node.parent.and_then(|p| self.orientation(p));
            if self.kind_alternates(c.layout) && parent_orientation == Some(c.orientation) {
                return Err(format!("{id:?} has the same orientation as its parent"));
            }
        }
        for &child in &c.children {
            let Some(n) = self.nodes.get(&child) else {
                return Err(format!("{id:?} lists a missing child {child:?}"));
            };
            if n.parent != Some(id) {
                return Err(format!("{child:?} does not point back at {id:?}"));
            }
            self.check_node(child, seen, windows)?;
        }
        Ok(())
    }

    /// The tree on one line, for assertions and logs: `h[1 v[2 3]]`. `h`/`v` is the
    /// orientation; a layout other than the default is added as `@name`.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        self.describe_node(self.root, &mut out);
        out
    }

    fn describe_node(&self, id: NodeId, out: &mut String) {
        match &self.nodes[&id].kind {
            Kind::Window(w) => out.push_str(&w.idx.get().to_string()),
            Kind::Container(c) => {
                out.push(match c.orientation {
                    Orientation::Horizontal => 'h',
                    Orientation::Vertical => 'v',
                });
                if c.layout != self.default_layout {
                    out.push('@');
                    out.push_str(c.layout.name());
                }
                out.push('[');
                for (i, &child) in c.children.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    self.describe_node(child, out);
                }
                out.push(']');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::app::WindowId;

    const TILES: LayoutKind = LayoutKind::new("tiles");
    const ACC: LayoutKind = LayoutKind::new("accordion");

    fn w(n: u32) -> WindowId { WindowId::new(1, n) }

    fn tree() -> WorkspaceTree { WorkspaceTree::new(Orientation::Horizontal, TILES) }

    fn tree_of(ids: &[u32]) -> WorkspaceTree {
        let mut t = tree();
        for &i in ids {
            t.insert_tiled(w(i));
        }
        t
    }

    /// A tree that tolerates the shapes normalization would remove, so tests can build them.
    fn raw() -> WorkspaceTree {
        let mut t = tree();
        t.set_options(NormalizeOptions { flatten_containers: false, opposite_orientation: false });
        t
    }

    struct Lcg(u64);

    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            self.0 >> 33
        }

        fn below(&mut self, n: u64) -> u64 { self.next() % n }
    }

    // ---- structure ----

    #[test]
    fn a_new_tree_is_an_empty_root() {
        let t = tree();
        assert_eq!(t.describe(), "h[]");
        assert!(t.is_empty());
        assert_eq!(t.focused_window(), None);
        assert_eq!(t.parent(t.root()), None);
        t.check_invariants().unwrap();
    }

    #[test]
    fn windows_are_tiled_by_default_and_land_in_order() {
        let t = tree_of(&[1, 2, 3]);
        assert_eq!(t.describe(), "h[1 2 3]");
        assert_eq!(t.tiled_windows(), vec![w(1), w(2), w(3)]);
        assert!(t.is_tiled(w(2)) && !t.is_floating(w(2)));
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_new_window_lands_after_the_focused_one_and_takes_focus() {
        let mut t = tree_of(&[1, 2, 3]);
        t.set_focus(w(1));
        t.insert_tiled(w(4));
        assert_eq!(t.describe(), "h[1 4 2 3]");
        assert_eq!(t.focused_window(), Some(w(4)));
    }

    #[test]
    fn inserting_a_window_twice_changes_nothing() {
        let mut t = tree_of(&[1, 2]);
        t.insert_tiled(w(1));
        t.insert_floating(w(2));
        assert_eq!(t.describe(), "h[1 2]");
        assert!(t.floating_windows().is_empty());
    }

    #[test]
    fn a_new_window_gets_the_mean_sibling_weight() {
        let mut t = tree_of(&[1, 2]);
        let (a, b) = (t.leaf(w(1)).unwrap(), t.leaf(w(2)).unwrap());
        t.set_weight(a, 1.0);
        t.set_weight(b, 3.0);
        t.insert_tiled(w(3));
        assert_eq!(t.weight(t.leaf(w(3)).unwrap()).unwrap(), 2.0);
    }

    #[test]
    fn removing_a_window_drops_its_leaf_and_focus_falls_back() {
        let mut t = tree_of(&[1, 2, 3]);
        assert!(t.remove(w(3)));
        assert_eq!(t.describe(), "h[1 2]");
        assert_eq!(t.focused_window(), Some(w(2)), "the next most recent window is focused");
        assert!(!t.remove(w(3)), "removing it again reports nothing happened");
        t.remove(w(1));
        t.remove(w(2));
        assert_eq!(t.describe(), "h[]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn focus_follows_the_most_recent_child_of_each_container() {
        let mut t = raw();
        for i in 1..=4 {
            t.insert_tiled(w(i));
        }
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), c, 1);
        t.set_options(NormalizeOptions::default());
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2 3] 4]");
        t.set_focus(w(2));
        t.set_focus(w(4));
        assert_eq!(t.focused_window(), Some(w(4)));
        t.set_focus(w(1));
        let inner = t.parent(t.leaf(w(2)).unwrap()).unwrap();
        // The inner container remembers 2 even though the root's focus is elsewhere.
        assert_eq!(t.mru(inner)[0], t.leaf(w(2)).unwrap());
        t.set_focus(w(3));
        assert_eq!(t.focused_window(), Some(w(3)));
        t.remove(w(3));
        // The inner container shrinks to the lone 2, which keeps the container's focus slot.
        assert_eq!(t.describe(), "h[1 2 4]");
        assert_eq!(t.focused_window(), Some(w(2)));
    }

    #[test]
    fn set_focus_ignores_unknown_and_floating_windows() {
        let mut t = tree_of(&[1]);
        t.insert_floating(w(2));
        assert!(!t.set_focus(w(9)));
        assert!(!t.set_focus(w(2)));
        assert!(t.set_focus(w(1)));
    }

    // ---- floating and fullscreen ----

    #[test]
    fn floating_windows_live_outside_the_tree() {
        let mut t = tree_of(&[1, 2]);
        assert!(t.float(w(1)));
        assert_eq!(t.describe(), "h[2]");
        assert!(t.is_floating(w(1)) && !t.is_tiled(w(1)));
        assert_eq!(t.floating_windows(), vec![w(1)]);
        assert!(!t.float(w(1)), "already floating");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_floating_window_can_be_tiled_again() {
        let mut t = tree_of(&[1, 2]);
        t.float(w(1));
        assert!(t.tile(w(1)));
        assert_eq!(t.describe(), "h[2 1]", "it lands after the focused window");
        assert!(t.floating_windows().is_empty());
        assert!(!t.tile(w(1)));
        assert!(!t.tile(w(9)), "unknown window");
    }

    #[test]
    fn removing_a_floating_window_forgets_it() {
        let mut t = tree_of(&[1]);
        t.insert_floating(w(2));
        assert!(t.remove(w(2)));
        assert!(!t.contains(w(2)));
        t.check_invariants().unwrap();
    }

    #[test]
    fn fullscreen_toggles_and_is_cleared_when_the_window_leaves() {
        let mut t = tree_of(&[1, 2]);
        assert_eq!(t.toggle_fullscreen(w(1)), Some(true));
        assert_eq!(t.fullscreen(), Some(w(1)));
        assert_eq!(t.toggle_fullscreen(w(2)), Some(true), "one fullscreen window per workspace");
        assert_eq!(t.fullscreen(), Some(w(2)));
        assert_eq!(t.toggle_fullscreen(w(2)), Some(false));
        assert_eq!(t.fullscreen(), None);
        t.toggle_fullscreen(w(1));
        t.remove(w(1));
        assert_eq!(t.fullscreen(), None);
        assert_eq!(t.toggle_fullscreen(w(9)), None, "unknown window");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_floating_window_can_be_fullscreen() {
        let mut t = tree_of(&[1]);
        t.insert_floating(w(2));
        assert_eq!(t.toggle_fullscreen(w(2)), Some(true));
        t.check_invariants().unwrap();
    }

    // ---- primitives ----

    #[test]
    fn wrap_replaces_a_node_with_a_container_that_inherits_its_slot_and_weight() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        t.set_weight(t.leaf(w(2)).unwrap(), 3.0);
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        assert_eq!(t.describe(), "h[1 v[2]]");
        assert_eq!(t.weight(c).unwrap(), 3.0);
        assert_eq!(t.weight(t.leaf(w(2)).unwrap()).unwrap(), 1.0);
        assert_eq!(t.index_in_parent(c), Some(1));
        t.check_invariants().unwrap();
    }

    #[test]
    fn move_node_reinserts_at_an_index_and_adjusts_for_its_own_removal() {
        let mut t = tree_of(&[1, 2, 3]);
        let root = t.root();
        t.move_node(t.leaf(w(1)).unwrap(), root, 3);
        assert_eq!(t.describe(), "h[2 3 1]");
        t.move_node(t.leaf(w(1)).unwrap(), root, 0);
        assert_eq!(t.describe(), "h[1 2 3]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn swap_siblings_exchanges_positions_and_weights() {
        let mut t = tree_of(&[1, 2]);
        let (a, b) = (t.leaf(w(1)).unwrap(), t.leaf(w(2)).unwrap());
        t.set_weight(a, 1.0);
        t.set_weight(b, 4.0);
        t.swap_siblings(a, b);
        assert_eq!(t.describe(), "h[2 1]");
        assert_eq!(t.weight(t.leaf(w(2)).unwrap()).unwrap(), 1.0, "weights belong to the slot");
        assert_eq!(t.weight(t.leaf(w(1)).unwrap()).unwrap(), 4.0);
        t.check_invariants().unwrap();
    }

    #[test]
    fn balance_makes_every_weight_equal() {
        let mut t = tree_of(&[1, 2, 3]);
        t.set_weight(t.leaf(w(1)).unwrap(), 5.0);
        t.set_weight(t.leaf(w(3)).unwrap(), 0.5);
        t.balance();
        assert!(t.tiled_windows().iter().all(|&x| t.weight(t.leaf(x).unwrap()).unwrap() == 1.0));
    }

    #[test]
    fn resize_child_conserves_the_total_and_keeps_weights_positive() {
        let mut t = tree_of(&[1, 2, 3]);
        let a = t.leaf(w(1)).unwrap();
        assert!(t.resize_child(a, 0.5));
        let weights: Vec<f64> = t.children(t.root()).iter().map(|&c| t.weight(c).unwrap()).collect();
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!((weights[0] - 1.5).abs() < 1e-9);
        assert!((weights[1] - 0.75).abs() < 1e-9 && (weights[2] - 0.75).abs() < 1e-9);
    }

    #[test]
    fn resize_child_clamps_to_a_minimum_share() {
        let mut t = tree_of(&[1, 2, 3]);
        let a = t.leaf(w(1)).unwrap();
        t.resize_child(a, 100.0);
        let weights: Vec<f64> = t.children(t.root()).iter().map(|&c| t.weight(c).unwrap()).collect();
        assert!(weights.iter().all(|&x| x > 0.0));
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        t.resize_child(a, -100.0);
        let weights: Vec<f64> = t.children(t.root()).iter().map(|&c| t.weight(c).unwrap()).collect();
        assert!(weights.iter().all(|&x| x > 0.0));
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!(!tree_of(&[1]).resize_child(tree_of(&[1]).leaf(w(1)).unwrap(), 1.0), "no sibling to trade with");
    }

    #[test]
    fn flatten_to_root_collects_every_tiled_window_in_order() {
        let mut t = raw();
        for i in 1..=4 {
            t.insert_tiled(w(i));
        }
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), c, 1);
        assert_eq!(t.describe(), "h[1 v[2 3] 4]");
        t.set_focus(w(3));
        t.set_options(NormalizeOptions::default());
        t.flatten_to_root();
        assert_eq!(t.describe(), "h[1 2 3 4]");
        assert_eq!(t.focused_window(), Some(w(3)), "focus survives");
        assert!(t.tiled_windows().iter().all(|&x| t.weight(t.leaf(x).unwrap()).unwrap() == 1.0));
        t.check_invariants().unwrap();
    }

    // ---- normalization: flatten ----

    #[test]
    fn a_container_with_one_child_is_replaced_by_that_child() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        t.set_weight(t.leaf(w(2)).unwrap(), 3.0);
        t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.set_options(NormalizeOptions::default());
        assert!(t.check_invariants().is_err(), "the single-child container is a violation");
        t.normalize();
        assert_eq!(t.describe(), "h[1 2]");
        assert_eq!(t.weight(t.leaf(w(2)).unwrap()).unwrap(), 3.0, "the child takes the container's weight");
        t.check_invariants().unwrap();
    }

    #[test]
    fn single_child_chains_collapse_all_the_way() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        let c1 = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.wrap(c1, Orientation::Horizontal).unwrap();
        assert_eq!(t.describe(), "h[1 h[v[2]]]");
        t.set_options(NormalizeOptions::default());
        t.normalize();
        assert_eq!(t.describe(), "h[1 2]");
    }

    #[test]
    fn empty_containers_are_pruned() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.detach(t.leaf(w(2)).unwrap());
        t.normalize(); // even with flattening off, an empty container is never valid
        assert_eq!(t.parent(c), None);
        assert!(t.leaf(w(2)).is_none() || t.parent(t.leaf(w(2)).unwrap()).is_none());
    }

    #[test]
    fn the_root_keeps_a_lone_window_child() {
        let mut t = tree_of(&[1]);
        t.normalize();
        assert_eq!(t.describe(), "h[1]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_root_whose_only_child_is_a_container_adopts_it() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        let c = t.wrap(t.leaf(w(1)).unwrap(), Orientation::Vertical).unwrap();
        t.move_node(t.leaf(w(2)).unwrap(), c, 1);
        t.set_layout(c, ACC);
        assert_eq!(t.describe(), "h[v@accordion[1 2]]");
        t.set_options(NormalizeOptions::default());
        t.normalize();
        assert_eq!(t.describe(), "v@accordion[1 2]", "the root takes the child's orientation and layout");
        assert_eq!(t.parent(t.root()), None);
        t.check_invariants().unwrap();
    }

    #[test]
    fn flatten_can_be_switched_off() {
        let mut t = raw();
        t.insert_tiled(w(1));
        t.insert_tiled(w(2));
        t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2]]");
        t.check_invariants().unwrap();
    }

    // ---- normalization: opposite orientation ----

    #[test]
    fn nested_containers_get_the_opposite_orientation() {
        let mut t = raw();
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), c, 1);
        assert_eq!(t.describe(), "h[1 h[2 3]]");
        t.set_options(NormalizeOptions::default());
        assert!(t.check_invariants().is_err());
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2 3]]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn deep_nesting_alternates_at_every_level() {
        let mut t = raw();
        for i in 1..=5 {
            t.insert_tiled(w(i));
        }
        let a = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), a, 1);
        let b = t.wrap(t.leaf(w(3)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(4)).unwrap(), b, 1);
        let c = t.wrap(t.leaf(w(4)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(5)).unwrap(), c, 1);
        assert_eq!(t.describe(), "h[1 h[2 h[3 h[4 5]]]]");
        t.set_options(NormalizeOptions::default());
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2 h[3 v[4 5]]]]");
    }

    #[test]
    fn changing_the_root_orientation_flips_the_whole_tree() {
        let mut t = raw();
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), c, 1);
        t.set_options(NormalizeOptions::default());
        t.set_orientation(t.root(), Orientation::Vertical);
        t.normalize();
        assert_eq!(t.describe(), "v[1 h[2 3]]");
    }

    #[test]
    fn opposite_orientation_can_be_switched_off() {
        let mut t = raw();
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), c, 1);
        t.set_options(NormalizeOptions { flatten_containers: true, opposite_orientation: false });
        t.normalize();
        assert_eq!(t.describe(), "h[1 h[2 3]]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn flattening_then_flipping_interact_correctly() {
        // h[1 v[h[2 3]]]: the v has one child, which flattens into the root, where the
        // inner h now sits under an h and must flip.
        let mut t = raw();
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        let inner = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), inner, 1);
        t.wrap(inner, Orientation::Vertical).unwrap();
        assert_eq!(t.describe(), "h[1 v[h[2 3]]]");
        t.set_options(NormalizeOptions::default());
        t.normalize();
        assert_eq!(t.describe(), "h[1 v[2 3]]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn normalization_is_idempotent_on_a_hand_built_mess() {
        let mut t = raw();
        for i in 1..=6 {
            t.insert_tiled(w(i));
        }
        let a = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Horizontal).unwrap();
        t.move_node(t.leaf(w(3)).unwrap(), a, 1);
        let b = t.wrap(t.leaf(w(5)).unwrap(), Orientation::Vertical).unwrap();
        t.wrap(b, Orientation::Vertical).unwrap();
        t.set_options(NormalizeOptions::default());
        t.normalize();
        let once = (t.describe(), weights(&t));
        t.normalize();
        assert_eq!((t.describe(), weights(&t)), once);
        t.check_invariants().unwrap();
    }

    fn weights(t: &WorkspaceTree) -> Vec<u64> {
        t.tiled_windows().iter().map(|&x| t.weight(t.leaf(x).unwrap()).unwrap().to_bits()).collect()
    }

    // ---- review fixes: resize water-filling ----

    fn root_weights(t: &WorkspaceTree) -> Vec<f64> {
        t.children(t.root()).iter().map(|&c| t.weight(c).unwrap()).collect()
    }

    #[test]
    fn growing_two_windows_in_turn_keeps_every_sibling_at_the_minimum_share() {
        // The scenario that used to drive a sibling below the floor: [1,2,3], grow 1 to
        // the limit, then grow 2 to the limit.
        let mut t = tree_of(&[1, 2, 3]);
        let (a, b) = (t.leaf(w(1)).unwrap(), t.leaf(w(2)).unwrap());
        t.resize_child(a, 100.0);
        t.resize_child(b, 100.0);
        let weights = root_weights(&t);
        let min = 3.0 * 0.05f64.min(0.5 / 3.0);
        assert!(weights.iter().all(|&x| x >= min - 1e-9), "{weights:?}");
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9);
        assert!((weights[1] - (3.0 - 2.0 * min)).abs() < 1e-9, "the second window got the limit: {weights:?}");
    }

    #[test]
    fn resizing_is_direction_safe_outside_the_limits() {
        let mut t = tree_of(&[1, 2, 3]);
        let ids: Vec<NodeId> = t.children(t.root()).to_vec();
        // Already below the floor: shrinking must not raise it.
        for (id, weight) in ids.iter().zip([0.02, 1.49, 1.49]) {
            t.set_weight(*id, weight);
        }
        t.resize_child(ids[0], -1.0);
        assert_eq!(root_weights(&t), vec![0.02, 1.49, 1.49]);
        // Already above the limit: growing must not shrink it.
        for (id, weight) in ids.iter().zip([2.9, 0.05, 0.05]) {
            t.set_weight(*id, weight);
        }
        t.resize_child(ids[0], 1.0);
        let weights = root_weights(&t);
        assert_eq!(weights[0], 2.9);
        assert!((weights.iter().sum::<f64>() - 3.0).abs() < 1e-9, "{weights:?}");
        assert!(weights[1] <= 0.05 + 1e-12, "siblings are not raised by a grow: {weights:?}");
    }

    #[test]
    fn resize_child_ignores_non_finite_amounts_and_stale_nodes() {
        let mut t = tree_of(&[1, 2]);
        let a = t.leaf(w(1)).unwrap();
        assert!(!t.resize_child(a, f64::NAN));
        t.remove(w(1));
        assert!(!t.resize_child(a, 1.0), "a stale node has no siblings");
    }

    // ---- review fixes: tolerant accessors ----

    #[test]
    fn a_stale_node_id_is_unknown_not_a_panic() {
        let mut t = tree_of(&[1, 2, 3]);
        let stale = t.leaf(w(2)).unwrap();
        let other = t.leaf(w(1)).unwrap();
        t.remove(w(2));
        assert_eq!(t.weight(stale), None);
        assert_eq!(t.wrap(stale, Orientation::Vertical), None);
        assert!(!t.swap_siblings(stale, other));
        t.move_node(stale, t.root(), 0);
        t.attach(stale, t.root(), 0, None);
        t.detach(stale);
        t.set_weight(stale, 2.0);
        t.set_orientation(stale, Orientation::Vertical);
        assert!(!t.set_layout(stale, ACC));
        assert_eq!((t.parent(stale), t.window(stale), t.index_in_parent(stale)), (None, None, None));
        assert!(t.children(stale).is_empty() && t.mru(stale).is_empty());
        assert_eq!(t.describe(), "h[1 3]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn the_root_cannot_be_wrapped_and_a_node_cannot_move_into_itself() {
        let mut t = raw();
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        assert_eq!(t.wrap(t.root(), Orientation::Vertical), None);
        let c = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        let before = t.describe();
        t.move_node(c, c, 0);
        assert_eq!(t.describe(), before);
    }

    #[test]
    fn invalid_weights_are_ignored() {
        let mut t = tree_of(&[1, 2]);
        let a = t.leaf(w(1)).unwrap();
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            t.set_weight(a, bad);
            assert_eq!(t.weight(a), Some(1.0));
        }
    }

    // ---- review fixes: focus, recency, insertion ----

    #[test]
    fn a_batch_insert_lines_up_without_taking_focus() {
        let mut t = tree_of(&[1]);
        assert!(t.insert_tiled_after(w(2), None));
        assert!(t.insert_tiled_after(w(3), Some(w(2))));
        assert!(t.insert_tiled_after(w(4), Some(w(3))));
        assert_eq!(t.describe(), "h[1 2 3 4]");
        assert_eq!(t.focused_window(), Some(w(1)), "focus did not move");
        assert!(!t.insert_tiled_after(w(3), None), "already there");
        assert!(t.insert_tiled_after(w(5), Some(w(99))), "an unknown anchor falls back to the focus");
        assert_eq!(t.describe(), "h[1 5 2 3 4]");
    }

    #[test]
    fn flatten_to_root_keeps_the_recency_order() {
        let mut t = WorkspaceTree::from_spec("h[1 v[2 3] 4]", TILES);
        t.set_focus(w(4));
        t.set_focus(w(2));
        t.set_focus(w(3));
        t.flatten_to_root();
        assert_eq!(t.describe(), "h[1 2 3 4]");
        let order: Vec<WindowId> = t.mru(t.root()).iter().filter_map(|&n| t.window(n)).collect();
        // Most recent first: 3 (last focused), 2 (its column-mate), then 4, then 1.
        assert_eq!(order, vec![w(3), w(2), w(4), w(1)]);
        assert_eq!(t.focused_window(), Some(w(3)));
    }

    // ---- review fixes: per-kind policy ----

    fn policies(entries: &[(LayoutKind, KindPolicy)]) -> Arc<BTreeMap<LayoutKind, KindPolicy>> {
        Arc::new(entries.iter().copied().collect())
    }

    const NOALT: LayoutKind = LayoutKind::new("noalt");
    const KEEP: LayoutKind = LayoutKind::new("keep");

    #[test]
    fn a_kind_can_opt_out_of_alternation() {
        let mut t = WorkspaceTree::from_spec("h[1 h@noalt[2 3] h[4 5]]", TILES);
        t.set_policies(policies(&[(NOALT, KindPolicy { alternates: false, ..Default::default() })]));
        t.normalize();
        assert_eq!(t.describe(), "h[1 h@noalt[2 3] v[4 5]]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_kind_can_opt_out_of_flattening() {
        let mut t = raw();
        t.set_policies(policies(&[(KEEP, KindPolicy { flattens: false, ..Default::default() })]));
        t.set_options(NormalizeOptions::default());
        for i in 1..=3 {
            t.insert_tiled(w(i));
        }
        let kept = t.wrap(t.leaf(w(2)).unwrap(), Orientation::Vertical).unwrap();
        t.set_layout(kept, KEEP);
        let plain = t.wrap(t.leaf(w(3)).unwrap(), Orientation::Vertical).unwrap();
        t.normalize();
        assert_eq!(t.parent(plain), None, "the tiles container with one child went");
        assert_eq!(t.describe(), "h[1 v@keep[2] 3]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_lone_root_child_is_only_absorbed_when_both_kinds_flatten() {
        let mut t = WorkspaceTree::from_spec("h[v@keep[1 2]]", TILES);
        t.set_policies(policies(&[(KEEP, KindPolicy { flattens: false, ..Default::default() })]));
        t.normalize();
        assert_eq!(t.describe(), "h[v@keep[1 2]]");
        t.check_invariants().unwrap();
    }

    #[test]
    fn a_kind_that_cannot_be_a_container_is_refused_and_flagged() {
        let mut t = tree_of(&[1]);
        let free = LayoutKind::new("free");
        t.set_policies(policies(&[(free, KindPolicy { in_tree: false, ..Default::default() })]));
        let root = t.root();
        assert!(!t.set_layout(root, free));
        assert_eq!(t.layout(root), Some(TILES));
        // A tree that already holds one (policies changed under it) is reported.
        let mut u = WorkspaceTree::from_spec("h@free[1]", TILES);
        u.set_policies(policies(&[(free, KindPolicy { in_tree: false, ..Default::default() })]));
        assert!(u.check_invariants().is_err());
    }

    // ---- review fixes: layout state ----

    #[derive(Debug, PartialEq)]
    struct Ratio(u32);

    #[test]
    fn layout_state_is_opaque_typed_and_cloneable() {
        let state = LayoutState::new(Ratio(7));
        assert_eq!(state.get::<Ratio>(), Some(&Ratio(7)));
        assert_eq!(state.get::<u32>(), None, "asking for another type finds nothing");
        assert_eq!(state.clone().get::<Ratio>(), Some(&Ratio(7)));
        assert!(format!("{state:?}").contains("Ratio(7)"));
    }

    #[test]
    fn a_container_keeps_its_state_when_the_tree_around_it_is_flattened() {
        let mut t = WorkspaceTree::from_spec("h[1 v[h[2 3]]]", TILES);
        let v = t.children(t.root())[1];
        let inner = t.children(v)[0];
        t.set_layout_with(inner, ACC, Some(LayoutState::new(Ratio(3))));
        t.normalize();
        assert_eq!(t.describe(), "h[1 v@accordion[2 3]]");
        let survivor = t.children(t.root())[1];
        assert_eq!(t.layout_state(survivor).and_then(|s| s.get::<Ratio>()), Some(&Ratio(3)));
    }

    #[test]
    fn set_state_replaces_the_state_and_the_kind_change_clears_it() {
        let mut t = tree_of(&[1, 2]);
        let root = t.root();
        t.set_state(root, Some(LayoutState::new(Ratio(1))));
        assert!(t.layout_state(root).is_some());
        t.set_layout(root, ACC);
        assert!(t.layout_state(root).is_none());
        t.set_layout_with(root, TILES, Some(LayoutState::new(Ratio(2))));
        assert_eq!(t.layout_state(root).and_then(|s| s.get::<Ratio>()), Some(&Ratio(2)));
    }

    #[test]
    fn a_layout_kind_read_from_config_is_interned() {
        use serde::de::IntoDeserializer;
        let read = |name: &str| {
            let d: serde::de::value::StrDeserializer<'_, serde::de::value::Error> = name.into_deserializer();
            LayoutKind::deserialize(d).unwrap()
        };
        let (a, b) = (read("bsp"), read("bsp"));
        assert_eq!(a, b);
        assert!(std::ptr::eq(a.name(), b.name()), "the same name is stored once");
        assert_eq!(read("tiles"), TILES);
    }

    #[test]
    fn the_fingerprint_sees_every_kind_of_change() {
        let mut t = tree_of(&[1, 2, 3]);
        let base = t.fingerprint();
        assert_eq!(t.fingerprint(), base);
        let a = t.leaf(w(1)).unwrap();
        t.set_weight(a, 2.0);
        assert_ne!(t.fingerprint(), base, "weights");
        let after_weight = t.fingerprint();
        t.set_focus(w(1));
        assert_ne!(t.fingerprint(), after_weight, "focus order");
        let after_focus = t.fingerprint();
        t.toggle_fullscreen(w(2));
        assert_ne!(t.fingerprint(), after_focus, "fullscreen");
    }

    // ---- invariants under random mutation ----

    #[test]
    fn random_mutations_keep_every_invariant_and_normalization_stays_idempotent() {
        for seed in 0..300u64 {
            let mut rng = Lcg(seed);
            let mut t = tree();
            for step in 0..80 {
                let id = w(1 + rng.below(8) as u32);
                match rng.below(10) {
                    0 | 1 => { t.insert_tiled(id); }
                    2 => { t.remove(id); }
                    3 => { t.float(id); }
                    4 => { t.tile(id); }
                    5 => { t.set_focus(id); }
                    6 => {
                        if let Some(leaf) = t.leaf(id) {
                            let o = if rng.below(2) == 0 { Orientation::Horizontal } else { Orientation::Vertical };
                            t.wrap(leaf, o).unwrap();
                            t.normalize();
                        }
                    }
                    7 => {
                        if let (Some(leaf), Some(&target)) = (t.leaf(id), t.containers().get(rng.below(3) as usize)) {
                            let len = t.children(target).len();
                            t.move_node(leaf, target, rng.below(len as u64 + 1) as usize);
                            t.normalize();
                        }
                    }
                    8 => {
                        let o = if rng.below(2) == 0 { Orientation::Horizontal } else { Orientation::Vertical };
                        t.set_orientation(t.root(), o);
                        t.normalize();
                    }
                    _ => {
                        if let Some(leaf) = t.leaf(id) {
                            let delta = rng.below(801) as f64 / 100.0 - 4.0;
                            // Resize the leaf or one of its ancestors.
                            let node = if rng.below(2) == 0 { leaf } else { t.parent(leaf).unwrap_or(leaf) };
                            if let Some(parent) = t.parent(node) {
                                let before: Vec<f64> = t.children(parent).iter().map(|&c| t.weight(c).unwrap()).collect();
                                let at = t.index_in_parent(node).unwrap();
                                let resized = t.resize_child(node, delta);
                                let after: Vec<f64> = t.children(parent).iter().map(|&c| t.weight(c).unwrap()).collect();
                                if resized {
                                    let total: f64 = before.iter().sum();
                                    assert!((after.iter().sum::<f64>() - total).abs() < 1e-9, "seed {seed}: total changed");
                                    let min = total * 0.05f64.min(0.5 / before.len() as f64);
                                    for (i, (&b, &a)) in before.iter().zip(&after).enumerate() {
                                        assert!(a > 0.0, "seed {seed}: weight {a}");
                                        // Nobody ends below the floor unless it started there.
                                        assert!(a >= min.min(b) - 1e-9, "seed {seed}: {i} fell to {a} from {b} (floor {min})");
                                    }
                                    if delta >= 0.0 {
                                        assert!(after[at] >= before[at] - 1e-12, "seed {seed}: growing shrank");
                                    } else {
                                        assert!(after[at] <= before[at] + 1e-12, "seed {seed}: shrinking grew");
                                    }
                                }
                            }
                        }
                    }
                }
                if let Err(e) = t.check_invariants() {
                    panic!("seed {seed} step {step}: {e}\n{}", t.describe());
                }
                let before = (t.describe(), weights(&t));
                t.normalize();
                assert_eq!((t.describe(), weights(&t)), before, "seed {seed} step {step}: not idempotent");
            }
        }
    }
}
