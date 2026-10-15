//! The tiling trees of all workspaces, kept in step with workspace membership.
//!
//! [`WorkspaceStore`] owns which workspace a window belongs to; this store owns where
//! the window sits within that workspace's [`WorkspaceTree`]. The two are reconciled by
//! [`TreeStore::sync`], which the reactor runs after anything that can change
//! membership (events, commands): windows that left the window store are dropped,
//! windows whose workspace changed move their leaf (keeping their floating state), and
//! new windows are tiled by default, in window-id order so the result is deterministic.
//! A minimized window takes no space: it leaves its tree and, if it was floating, comes
//! back floating when it is restored. Syncing never changes which window a tree has
//! focused; the reactor does that from focus events and commands.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::actor::app::WindowId;
use crate::common::collections::HashSet;
use crate::layout::TILES;
use crate::model::tree::{KindPolicy, LayoutKind, NormalizeOptions, Orientation, WorkspaceTree};
use crate::model::window_store::WindowStore;
use crate::model::workspace::{WorkspaceId, WorkspaceStore};

fn is_empty(tree: &WorkspaceTree) -> bool {
    tree.tiled_windows().is_empty() && tree.floating_windows().is_empty()
}

#[derive(Debug)]
pub struct TreeStore {
    trees: BTreeMap<WorkspaceId, WorkspaceTree>,
    options: NormalizeOptions,
    policies: Arc<BTreeMap<LayoutKind, KindPolicy>>,
    default_layout: LayoutKind,
    /// The orientation of the roots the store creates.
    root_orientation: Orientation,
    /// Windows that were floating when they left a tree (to another workspace, or by
    /// being minimized) and come back floating.
    floating_memory: HashSet<WindowId>,
}

impl Default for TreeStore {
    fn default() -> Self { Self::new(TILES) }
}

impl TreeStore {
    /// Containers the trees create use `default_layout`; root containers are horizontal.
    pub fn new(default_layout: LayoutKind) -> Self {
        TreeStore {
            trees: BTreeMap::new(),
            options: NormalizeOptions::default(),
            policies: Arc::default(),
            default_layout,
            root_orientation: Orientation::Horizontal,
            floating_memory: HashSet::default(),
        }
    }

    pub fn get(&self, workspace: WorkspaceId) -> Option<&WorkspaceTree> { self.trees.get(&workspace) }

    pub fn get_mut(&mut self, workspace: WorkspaceId) -> Option<&mut WorkspaceTree> {
        self.trees.get_mut(&workspace)
    }

    pub fn iter(&self) -> impl Iterator<Item = (WorkspaceId, &WorkspaceTree)> {
        self.trees.iter().map(|(&id, tree)| (id, tree))
    }

    pub fn options(&self) -> NormalizeOptions { self.options }

    /// Change the normalization passes of every tree, now and for trees made later.
    pub fn set_options(&mut self, options: NormalizeOptions) {
        self.options = options;
        for tree in self.trees.values_mut() {
            tree.set_options(options);
            tree.normalize();
        }
    }

    /// The layout and orientation of root containers (`default-root-container-*`).
    /// Workspaces that hold no windows are given new roots now; populated ones keep the
    /// tree they have (as in AeroSpace, the default applies to roots created afterwards).
    pub fn set_root_defaults(&mut self, layout: LayoutKind, orientation: Orientation) {
        self.default_layout = layout;
        self.root_orientation = orientation;
        let empty: Vec<WorkspaceId> =
            self.trees.iter().filter(|(_, t)| is_empty(t)).map(|(&id, _)| id).collect();
        for id in empty {
            self.trees.remove(&id);
            self.tree_mut(id);
        }
    }

    /// Re-orient the root of every empty workspace by `orientation_of` (`None` keeps it).
    /// For `default-root-container-orientation = auto`, where the answer depends on the
    /// monitor a workspace is on.
    pub fn orient_empty_roots(&mut self, orientation_of: &dyn Fn(WorkspaceId) -> Option<Orientation>) {
        for (&id, tree) in self.trees.iter_mut() {
            if !is_empty(tree) {
                continue;
            }
            if let Some(orientation) = orientation_of(id) {
                let root = tree.root();
                if tree.orientation(root) != Some(orientation) {
                    tree.set_orientation(root, orientation);
                }
            }
        }
    }

    /// Give every tree the per-kind normalization rules of the layout registry, now and
    /// for trees made later.
    pub fn set_policies(&mut self, policies: Arc<BTreeMap<LayoutKind, KindPolicy>>) {
        self.policies = policies;
        for tree in self.trees.values_mut() {
            tree.set_policies(self.policies.clone());
            tree.normalize();
        }
    }

    fn tree_mut(&mut self, workspace: WorkspaceId) -> &mut WorkspaceTree {
        let (options, layout, policies) = (self.options, self.default_layout, self.policies.clone());
        let orientation = self.root_orientation;
        self.trees.entry(workspace).or_insert_with(|| {
            let mut tree = WorkspaceTree::new(orientation, layout);
            tree.set_options(options);
            tree.set_policies(policies);
            tree
        })
    }

    /// Make every tree hold exactly the windows its workspace owns that are not minimized.
    pub fn sync(&mut self, workspaces: &WorkspaceStore, windows: &WindowStore) {
        for id in workspaces.ids() {
            self.tree_mut(id);
        }
        let home = |window: WindowId| -> Option<WorkspaceId> {
            let record = windows.get(window)?;
            if record.is_minimized { None } else { workspaces.workspace_of(window) }
        };
        // Drop what left, remembering the floating windows that will be back.
        for (&id, tree) in self.trees.iter_mut() {
            let mut held = tree.tiled_windows();
            held.extend(tree.floating_windows());
            for window in held {
                let wanted = home(window);
                if wanted != Some(id) {
                    let returns = wanted.is_some() || windows.get(window).is_some_and(|r| r.is_minimized);
                    if tree.is_floating(window) && returns {
                        self.floating_memory.insert(window);
                    }
                    tree.remove(window);
                }
            }
        }
        self.floating_memory.retain(|&w| windows.contains(w));
        let placed: HashSet<WindowId> = self
            .trees
            .values()
            .flat_map(|t| t.tiled_windows().into_iter().chain(t.floating_windows()))
            .collect();
        let mut fresh: Vec<(WindowId, WorkspaceId)> = windows
            .iter()
            .filter(|r| !placed.contains(&r.wid))
            .filter_map(|r| home(r.wid).map(|ws| (r.wid, ws)))
            .collect();
        fresh.sort();
        // A batch of new windows lines up in order: each goes after the previous one,
        // the first after the workspace's focused window. None of them takes focus.
        let mut previous: BTreeMap<WorkspaceId, WindowId> = BTreeMap::new();
        for (window, id) in fresh {
            let floating = self.floating_memory.remove(&window);
            let tree = self.tree_mut(id);
            if floating {
                tree.insert_floating(window);
            } else if tree.insert_tiled_after(window, previous.get(&id).copied()) {
                previous.insert(id, window);
            }
        }
    }

    /// Check every tree, and that the trees and the stores agree on who owns what.
    pub fn check_invariants(
        &self,
        workspaces: &WorkspaceStore,
        windows: &WindowStore,
    ) -> Result<(), String> {
        for (&id, tree) in &self.trees {
            tree.check_invariants().map_err(|e| format!("workspace {id} tree: {e}"))?;
            let mut held = tree.tiled_windows();
            held.extend(tree.floating_windows());
            for window in held {
                if workspaces.workspace_of(window) != Some(id) {
                    return Err(format!("{window:?} is in workspace {id}'s tree but not its member"));
                }
                match windows.get(window) {
                    None => {
                        return Err(format!("{window:?} is in a tree but unknown to the window store"));
                    }
                    Some(record) if record.is_minimized => {
                        return Err(format!("{window:?} is minimized but still in a tree"));
                    }
                    Some(_) => {}
                }
            }
        }
        for record in windows.iter().filter(|r| !r.is_minimized) {
            let Some(id) = workspaces.workspace_of(record.wid) else { continue };
            if !self.trees.get(&id).is_some_and(|t| t.contains(record.wid)) {
                return Err(format!("{:?} is in no leaf of workspace {id}'s tree", record.wid));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::app::WindowId;
    use crate::model::display::{DisplaySnapshot, DisplayStore, DisplayUuid};
    use crate::model::window_store::tests::info;
    use crate::model::workspace::WorkspaceConfig;
    use crate::sys::screen::DisplayId;
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};

    fn w(n: u32) -> WindowId { WindowId::new(1, n) }

    fn frame() -> CGRect { CGRect::new(CGPoint::new(10., 10.), CGSize::new(100., 100.)) }

    /// Windows 1..=n in the workspace store (all on workspace 1), none in any tree yet.
    fn world(n: u32) -> (WorkspaceStore, WindowStore) {
        let mut displays = DisplayStore::default();
        displays.apply(vec![DisplaySnapshot {
            uuid: DisplayUuid::new("D"),
            cg_id: DisplayId::new(1),
            frame: CGRect::new(CGPoint::new(0., 0.), CGSize::new(900., 600.)),
            name: None,
        }]);
        let mut workspaces = WorkspaceStore::new(&WorkspaceConfig::numbered(3)).unwrap();
        workspaces.displays_changed(
            &crate::model::display::DisplayChanges { added: vec![DisplayUuid::new("D")], ..Default::default() },
            &displays,
        );
        let mut windows = WindowStore::default();
        for i in 1..=n {
            windows.upsert(w(i), &info("t", frame(), Some(100 + i)));
        }
        workspaces.sync_windows(&windows);
        (workspaces, windows)
    }

    #[test]
    fn sync_gives_every_workspace_a_tree_and_every_window_a_leaf() {
        let (workspaces, windows) = world(3);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        assert_eq!(store.iter().count(), 3);
        assert_eq!(store.get(WorkspaceId::new(1)).unwrap().describe(), "h[1 2 3]");
        assert_eq!(store.get(WorkspaceId::new(2)).unwrap().describe(), "h[]");
        store.check_invariants(&workspaces, &windows).unwrap();
    }

    #[test]
    fn sync_drops_windows_the_store_forgot() {
        let (mut workspaces, mut windows) = world(3);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        windows.remove(w(2));
        workspaces.sync_windows(&windows);
        store.sync(&workspaces, &windows);
        assert_eq!(store.get(WorkspaceId::new(1)).unwrap().describe(), "h[1 3]");
    }

    #[test]
    fn sync_moves_a_leaf_with_its_floating_state() {
        let (mut workspaces, windows) = world(3);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        store.get_mut(WorkspaceId::new(1)).unwrap().float(w(2));
        let displays = DisplayStore::default();
        workspaces.move_window_to_workspace(w(2), 2, false, &displays).unwrap();
        workspaces.move_window_to_workspace(w(3), 2, false, &displays).unwrap();
        store.sync(&workspaces, &windows);
        let two = store.get(WorkspaceId::new(2)).unwrap();
        assert!(two.is_floating(w(2)));
        assert!(two.is_tiled(w(3)));
        assert_eq!(store.get(WorkspaceId::new(1)).unwrap().describe(), "h[1]");
        store.check_invariants(&workspaces, &windows).unwrap();
    }

    #[test]
    fn the_checker_notices_a_window_missing_from_its_tree() {
        let (workspaces, windows) = world(2);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        store.get_mut(WorkspaceId::new(1)).unwrap().remove(w(1));
        assert!(store.check_invariants(&workspaces, &windows).is_err());
    }

    #[test]
    fn set_options_reaches_existing_and_future_trees() {
        let (workspaces, windows) = world(1);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        let off = NormalizeOptions { flatten_containers: false, opposite_orientation: false };
        store.set_options(off);
        assert_eq!(store.get(WorkspaceId::new(1)).unwrap().options(), off);
        assert_eq!(store.options(), off);
    }
    #[test]
    fn root_defaults_rebuild_empty_trees_and_leave_populated_ones() {
        let (workspaces, windows) = world(1);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        let populated = store.get(WorkspaceId::new(1)).unwrap().describe();
        let accordion = crate::layout::ACCORDION;
        store.set_root_defaults(accordion, Orientation::Vertical);
        assert_eq!(store.get(WorkspaceId::new(1)).unwrap().describe(), populated, "workspace 1 holds a window");
        let empty = store.get(WorkspaceId::new(2)).unwrap();
        assert_eq!(empty.orientation(empty.root()), Some(Orientation::Vertical));
        assert_eq!(empty.layout(empty.root()), Some(accordion));
        // Trees made later use the defaults too.
        let (workspaces, windows) = world(1);
        let mut fresh = TreeStore::default();
        fresh.set_root_defaults(accordion, Orientation::Vertical);
        fresh.sync(&workspaces, &windows);
        let tree = fresh.get(WorkspaceId::new(3)).unwrap();
        assert_eq!(tree.orientation(tree.root()), Some(Orientation::Vertical));
    }

    #[test]
    fn orient_empty_roots_only_touches_empty_workspaces() {
        let (workspaces, windows) = world(1);
        let mut store = TreeStore::default();
        store.sync(&workspaces, &windows);
        store.orient_empty_roots(&|_| Some(Orientation::Vertical));
        let busy = store.get(WorkspaceId::new(1)).unwrap();
        assert_eq!(busy.orientation(busy.root()), Some(Orientation::Horizontal));
        let idle = store.get(WorkspaceId::new(2)).unwrap();
        assert_eq!(idle.orientation(idle.root()), Some(Orientation::Vertical));
        store.orient_empty_roots(&|_| None);
        let idle = store.get(WorkspaceId::new(2)).unwrap();
        assert_eq!(idle.orientation(idle.root()), Some(Orientation::Vertical), "None keeps the root as it is");
    }
}
