//! Every window the reactor knows about, keyed by [`WindowId`], with a reverse index
//! from the WindowServer id that CGS notifications carry.

use objc2_core_foundation::CGRect;
use tracing::debug;

use crate::actor::app::{WindowId, WindowInfo, pid_t};
use crate::common::collections::{HashMap, HashSet};
use crate::model::display::DisplayUuid;
use crate::sys::window_server::WindowServerId;

#[derive(Clone, Debug, PartialEq)]
pub struct WindowRecord {
    pub wid: WindowId,
    pub sys_id: Option<WindowServerId>,
    pub title: String,
    pub bundle_id: Option<String>,
    pub frame: CGRect,
    pub is_standard: bool,
    pub is_minimized: bool,
    /// The display the window is on, by UUID; `None` only while no display is known.
    pub display: Option<DisplayUuid>,
    /// The app actor dropped its AX handle. The window may still exist; the next
    /// inventory or a CGS destroy decides.
    pub ax_invalidated: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    Updated,
}

#[derive(Debug, Default)]
pub struct WindowStore {
    windows: HashMap<WindowId, WindowRecord>,
    by_sys_id: HashMap<WindowServerId, WindowId>,
}

impl WindowStore {
    /// Insert a window, or refresh an existing one from fresh AX info. The display
    /// assignment is kept; the caller reassigns it from the new frame.
    pub fn upsert(&mut self, wid: WindowId, info: &WindowInfo) -> Upsert {
        let (display, outcome) = match self.windows.get(&wid) {
            Some(old) => {
                if old.sys_id != info.sys_id
                    && let Some(old_sys_id) = old.sys_id
                    && self.by_sys_id.get(&old_sys_id) == Some(&wid)
                {
                    self.by_sys_id.remove(&old_sys_id);
                }
                (old.display.clone(), Upsert::Updated)
            }
            None => (None, Upsert::Inserted),
        };
        if let Some(sys_id) = info.sys_id
            && let Some(previous) = self.by_sys_id.insert(sys_id, wid)
            && previous != wid
        {
            // WindowServer ids are unique among live windows, so the previous owner
            // is a stale record for a window whose id was recycled.
            debug!(?previous, ?wid, ?sys_id, "sys id reused; dropping stale window");
            self.windows.remove(&previous);
        }
        self.windows.insert(
            wid,
            WindowRecord {
                wid,
                sys_id: info.sys_id,
                title: info.title.clone(),
                bundle_id: info.bundle_id.clone(),
                frame: info.frame,
                is_standard: info.is_standard,
                is_minimized: info.is_minimized,
                display,
                ax_invalidated: false,
            },
        );
        outcome
    }

    pub fn remove(&mut self, wid: WindowId) -> Option<WindowRecord> {
        let record = self.windows.remove(&wid)?;
        if let Some(sys_id) = record.sys_id
            && self.by_sys_id.get(&sys_id) == Some(&wid)
        {
            self.by_sys_id.remove(&sys_id);
        }
        Some(record)
    }

    pub fn remove_by_sys_id(&mut self, sys_id: WindowServerId) -> Option<WindowRecord> {
        let wid = *self.by_sys_id.get(&sys_id)?;
        self.remove(wid)
    }

    /// Remove every window of `pid`, ordered by id.
    pub fn remove_app(&mut self, pid: pid_t) -> Vec<WindowRecord> {
        self.remove_where(|wid| wid.pid == pid)
    }

    /// Remove the windows of `pid` that are not in `keep`, ordered by id.
    pub fn retain_app(&mut self, pid: pid_t, keep: &HashSet<WindowId>) -> Vec<WindowRecord> {
        self.remove_where(|wid| wid.pid == pid && !keep.contains(wid))
    }

    fn remove_where(&mut self, pred: impl Fn(&WindowId) -> bool) -> Vec<WindowRecord> {
        let mut doomed: Vec<WindowId> = self.windows.keys().copied().filter(|w| pred(w)).collect();
        doomed.sort();
        doomed.into_iter().filter_map(|wid| self.remove(wid)).collect()
    }

    pub fn get(&self, wid: WindowId) -> Option<&WindowRecord> { self.windows.get(&wid) }

    pub fn get_mut(&mut self, wid: WindowId) -> Option<&mut WindowRecord> {
        self.windows.get_mut(&wid)
    }

    pub fn by_sys_id(&self, sys_id: WindowServerId) -> Option<&WindowRecord> {
        self.windows.get(self.by_sys_id.get(&sys_id)?)
    }

    pub fn contains(&self, wid: WindowId) -> bool { self.windows.contains_key(&wid) }

    pub fn len(&self) -> usize { self.windows.len() }

    pub fn is_empty(&self) -> bool { self.windows.is_empty() }

    pub fn iter(&self) -> impl Iterator<Item = &WindowRecord> { self.windows.values() }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut WindowRecord> {
        self.windows.values_mut()
    }

    /// Windows on `display`, ordered by id.
    pub fn on_display(&self, display: &DisplayUuid) -> Vec<WindowId> {
        let mut wids: Vec<WindowId> = self
            .windows
            .values()
            .filter(|r| r.display.as_ref() == Some(display))
            .map(|r| r.wid)
            .collect();
        wids.sort();
        wids
    }

    /// Every known WindowServer id, sorted; the set CGS should notify us about.
    pub fn sys_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.by_sys_id.keys().map(|id| id.as_u32()).collect();
        ids.sort_unstable();
        ids
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use objc2_core_foundation::{CGPoint, CGSize};

    use super::*;

    pub(crate) fn info(title: &str, frame: CGRect, sys_id: Option<u32>) -> WindowInfo {
        WindowInfo {
            has_native_tabs: false,
            is_standard: true,
            is_root: true,
            is_minimized: false,
            is_resizable: true,
            title: title.into(),
            frame,
            min_size: None,
            max_size: None,
            sys_id: sys_id.map(WindowServerId::new),
            bundle_id: Some("com.example.app".into()),
            path: None,
            ax_role: Some("AXWindow".into()),
            ax_subrole: Some("AXStandardWindow".into()),
        }
    }

    fn frame() -> CGRect { CGRect::new(CGPoint::new(10., 20.), CGSize::new(300., 200.)) }

    fn wid(pid: pid_t, idx: u32) -> WindowId { WindowId::new(pid, idx) }

    #[test]
    fn upsert_inserts_then_updates() {
        let mut store = WindowStore::default();
        assert_eq!(store.upsert(wid(1, 1), &info("a", frame(), Some(100))), Upsert::Inserted);
        assert_eq!(store.upsert(wid(1, 1), &info("b", frame(), Some(100))), Upsert::Updated);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(wid(1, 1)).unwrap().title, "b");
    }

    #[test]
    fn upsert_keeps_display_and_clears_invalidation() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("a", frame(), Some(100)));
        let rec = store.get_mut(wid(1, 1)).unwrap();
        rec.display = Some(DisplayUuid::new("A"));
        rec.ax_invalidated = true;
        store.upsert(wid(1, 1), &info("a", frame(), Some(100)));
        let rec = store.get(wid(1, 1)).unwrap();
        assert_eq!(rec.display, Some(DisplayUuid::new("A")));
        assert!(!rec.ax_invalidated);
    }

    #[test]
    fn sys_id_index_follows_inserts_updates_and_removals() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("a", frame(), Some(100)));
        assert_eq!(store.by_sys_id(WindowServerId::new(100)).unwrap().wid, wid(1, 1));

        // The WindowServer id changed (rare, but the index must not go stale).
        store.upsert(wid(1, 1), &info("a", frame(), Some(101)));
        assert!(store.by_sys_id(WindowServerId::new(100)).is_none());
        assert_eq!(store.by_sys_id(WindowServerId::new(101)).unwrap().wid, wid(1, 1));

        store.remove(wid(1, 1));
        assert!(store.by_sys_id(WindowServerId::new(101)).is_none());
        assert!(store.sys_ids().is_empty());
    }

    #[test]
    fn remove_by_sys_id_removes_the_window() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("a", frame(), Some(100)));
        store.upsert(wid(1, 2), &info("b", frame(), Some(200)));
        let removed = store.remove_by_sys_id(WindowServerId::new(100)).unwrap();
        assert_eq!(removed.wid, wid(1, 1));
        assert!(!store.contains(wid(1, 1)));
        assert!(store.remove_by_sys_id(WindowServerId::new(100)).is_none());
        assert_eq!(store.sys_ids(), vec![200]);
    }

    #[test]
    fn windows_without_sys_id_are_not_indexed() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("a", frame(), None));
        assert!(store.sys_ids().is_empty());
        assert!(store.contains(wid(1, 1)));
    }

    #[test]
    fn remove_app_removes_only_that_pid_in_order() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 2), &info("b", frame(), Some(102)));
        store.upsert(wid(1, 1), &info("a", frame(), Some(101)));
        store.upsert(wid(2, 1), &info("c", frame(), Some(201)));
        let removed: Vec<_> = store.remove_app(1).into_iter().map(|r| r.wid).collect();
        assert_eq!(removed, vec![wid(1, 1), wid(1, 2)]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.sys_ids(), vec![201]);
    }

    #[test]
    fn retain_app_drops_windows_missing_from_inventory() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("a", frame(), Some(101)));
        store.upsert(wid(1, 2), &info("b", frame(), Some(102)));
        store.upsert(wid(2, 1), &info("c", frame(), Some(201)));
        let keep: HashSet<_> = [wid(1, 2)].into_iter().collect();
        let removed: Vec<_> = store.retain_app(1, &keep).into_iter().map(|r| r.wid).collect();
        assert_eq!(removed, vec![wid(1, 1)]);
        assert!(store.contains(wid(1, 2)) && store.contains(wid(2, 1)));
        assert_eq!(store.sys_ids(), vec![102, 201]);
    }

    #[test]
    fn a_new_window_claiming_a_recycled_sys_id_evicts_the_stale_owner() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 1), &info("old", frame(), Some(100)));
        store.upsert(wid(1, 2), &info("new", frame(), Some(100)));
        assert!(!store.contains(wid(1, 1)), "stale owner of the id is gone");
        assert_eq!(store.by_sys_id(WindowServerId::new(100)).unwrap().wid, wid(1, 2));
        assert_eq!(store.sys_ids(), vec![100]);
    }

    #[test]
    fn on_display_lists_windows_by_uuid() {
        let mut store = WindowStore::default();
        store.upsert(wid(1, 2), &info("b", frame(), Some(102)));
        store.upsert(wid(1, 1), &info("a", frame(), Some(101)));
        store.upsert(wid(2, 1), &info("c", frame(), Some(201)));
        for w in [wid(1, 1), wid(1, 2)] {
            store.get_mut(w).unwrap().display = Some(DisplayUuid::new("A"));
        }
        store.get_mut(wid(2, 1)).unwrap().display = Some(DisplayUuid::new("B"));
        assert_eq!(store.on_display(&DisplayUuid::new("A")), vec![wid(1, 1), wid(1, 2)]);
        assert_eq!(store.on_display(&DisplayUuid::new("B")), vec![wid(2, 1)]);
    }
}
