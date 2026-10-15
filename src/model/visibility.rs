//! Turns workspace visibility *state* into park and unpark *actions*.
//!
//! [`WorkspaceStore`] only says which workspaces are showing. After every event or command
//! [`VisibilityTracker::plan`] compares that with which windows are parked and returns the
//! difference: windows whose workspace became visible are unhidden, windows whose workspace
//! became hidden are hidden. Unhiding always comes first, AeroSpace's order
//! (`refresh.swift`), so the incoming windows are in place before the outgoing ones leave.
//!
//! Hiding moves a window just past a bottom corner of its own display (see [`parking`]).
//! Before that, its position is saved as a fraction of the display, keyed by the display's
//! UUID, so it comes back to the right place even if the display was resized or replugged.
//! Every parked window is also recorded in a [`SharedRegistry`] with where it would be
//! restored to, for the termination handler.
//!
//! Nothing here touches AX; [`layout_calls`] turns the actions into the presenter calls the
//! reactor makes.

use std::fmt::Debug;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use serde::{Deserialize, Serialize};

use crate::actor::app::WindowId;
use crate::actor::reactor::transaction_manager::TransactionId;
use crate::common::collections::BTreeMap;
use crate::geometry::{Corner, Point, Rect, Size};
use crate::model::display::{DisplayStore, DisplayUuid};
use crate::model::window_store::WindowStore;
use crate::model::workspace::WorkspaceStore;
use crate::parking::{
    SavedPosition, choose_corner, park_origin, resolve_restore, save_position,
};
use crate::restore::{ParkedHandle, SharedRegistry};
use crate::sys::screen::DisplayId;

/// How workspace switches are shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct VisibilityConfig {
    /// Slide the outgoing windows to their park corner and the incoming ones in, through
    /// the presenter. Off: windows jump.
    pub animate_switches: bool,
}

impl Default for VisibilityConfig {
    fn default() -> Self { Self { animate_switches: true } }
}

/// One window to show or hide.
#[derive(Clone, Debug, PartialEq)]
pub enum VisibilityAction {
    /// Bring a parked window back to `to`, on `display`.
    Unhide { window: WindowId, display: DisplayUuid, from: CGRect, to: CGRect },
    /// Move a window past `corner` of `display`, to `to`.
    Hide { window: WindowId, display: DisplayUuid, corner: Corner, from: CGRect, to: CGRect },
}

impl VisibilityAction {
    pub fn window(&self) -> WindowId {
        match self {
            Self::Unhide { window, .. } | Self::Hide { window, .. } => *window,
        }
    }

    /// The display the window ends up on (or beside, when parked).
    pub fn display(&self) -> &DisplayUuid {
        match self {
            Self::Unhide { display, .. } | Self::Hide { display, .. } => display,
        }
    }

    /// Where the window is going.
    pub fn target(&self) -> CGRect {
        match self {
            Self::Unhide { to, .. } | Self::Hide { to, .. } => *to,
        }
    }

    pub fn is_unhide(&self) -> bool { matches!(self, Self::Unhide { .. }) }
}

/// The registry key of a window.
pub fn registry_key(window: WindowId) -> u64 {
    (u64::from(window.pid as u32) << 32) | u64::from(window.idx.get())
}

/// Whether `frame` is visibly inside some display.
fn is_on_screen(frame: CGRect, displays: &DisplayStore) -> bool {
    let window = rect_of(frame);
    let need_w = ON_SCREEN_MIN.min(window.size.w);
    let need_h = ON_SCREEN_MIN.min(window.size.h);
    displays.iter().any(|d| {
        let d = rect_of(d.frame);
        let w = window.max_x().min(d.max_x()) - window.min_x().max(d.min_x());
        let h = window.max_y().min(d.max_y()) - window.min_y().max(d.min_y());
        w >= need_w && h >= need_h && w > 0.0 && h > 0.0
    })
}

fn rect_of(r: CGRect) -> Rect {
    Rect::new(r.origin.x, r.origin.y, r.size.width, r.size.height)
}

fn cg_rect(origin: Point, size: Size) -> CGRect {
    CGRect::new(CGPoint::new(origin.x, origin.y), CGSize::new(size.w, size.h))
}

/// Time source for the confirmation TTL, injectable so tests are deterministic.
pub trait Clock: Send + Sync + Debug {
    fn now(&self) -> Instant;
}

/// The real clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant { Instant::now() }
}

/// A clock that only moves when told to.
#[derive(Debug)]
pub struct ManualClock(Mutex<Instant>);

impl ManualClock {
    pub fn new() -> Self { Self(Mutex::new(Instant::now())) }

    pub fn advance(&self, by: Duration) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Default for ManualClock {
    fn default() -> Self { Self::new() }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant { *self.0.lock().unwrap_or_else(|e| e.into_inner()) }
}

/// How long after its last write an unhide may stay unconfirmed before it is checked
/// against what the reactor believes about the window.
pub const UNCONFIRMED_TTL: Duration = Duration::from_secs(2);

/// The write that put a window where the plan wanted it, once it has been issued.
#[derive(Clone, Copy, Debug, Default)]
struct Write {
    /// The reactor has issued the write; events before that belong to older writes.
    armed: bool,
    /// The transaction the write carried. A reported frame only counts if it carries the
    /// same one: an older write's late event must not confirm (or relocate) a newer one.
    txid: Option<TransactionId>,
    deadline: Option<Instant>,
}

impl Write {
    fn answers(&self, txid: Option<TransactionId>) -> bool {
        self.armed && self.txid.is_none_or(|expected| txid == Some(expected))
    }
}

/// A window the tracker has parked.
#[derive(Clone, Debug)]
struct Parked {
    write: Write,
    saved: SavedPosition<DisplayUuid>,
    /// Where it was put, and the registry target last published for it.
    parked_at: Point,
    restore_to: Point,
}

/// A window that must be at least this many pixels (each way, or its whole extent if
/// smaller) inside a display to count as shown again.
const ON_SCREEN_MIN: f64 = 50.0;

#[derive(Debug, Default)]
pub struct VisibilityTracker {
    parked: BTreeMap<WindowId, Parked>,
    /// Windows an unhide was planned for whose frame has not yet been seen on a display.
    /// They stay in the registry until it is: the plan is not the write, and the write is
    /// not the app accepting it.
    unconfirmed: BTreeMap<WindowId, Write>,
}

/// The display rects, in store order, as the parking geometry wants them.
struct Layout {
    rects: Vec<Rect>,
    keyed: Vec<(DisplayUuid, Rect)>,
}

impl Layout {
    fn new(displays: &DisplayStore) -> Self {
        let keyed: Vec<_> = displays.iter().map(|d| (d.uuid.clone(), rect_of(d.frame))).collect();
        Layout {
            rects: keyed.iter().map(|(_, r)| *r).collect(),
            keyed,
        }
    }

    fn index_of(&self, display: &DisplayUuid) -> Option<usize> {
        self.keyed.iter().position(|(uuid, _)| uuid == display)
    }

    /// The rect a window falls back to when its saved display is gone: the display it is
    /// on now, else the main one.
    fn fallback(&self, current: Option<&DisplayUuid>) -> Option<(DisplayUuid, Rect)> {
        current
            .and_then(|d| self.keyed.iter().find(|(uuid, _)| uuid == d))
            .or_else(|| self.keyed.first())
            .cloned()
    }
}

impl VisibilityTracker {
    pub fn is_parked(&self, window: WindowId) -> bool { self.parked.contains_key(&window) }

    pub fn parked(&self) -> impl Iterator<Item = WindowId> + '_ { self.parked.keys().copied() }

    pub fn parked_count(&self) -> usize { self.parked.len() }

    /// Windows unhidden but not yet seen on a display (still registered for restore).
    pub fn unconfirmed_count(&self) -> usize { self.unconfirmed.len() }

    pub fn is_unconfirmed(&self, window: WindowId) -> bool { self.unconfirmed.contains_key(&window) }

    /// Where `window` is parked, if it is.
    pub fn parked_at(&self, window: WindowId) -> Option<Point> {
        self.parked.get(&window).map(|p| p.parked_at)
    }

    /// Bring the parked set in line with workspace visibility; the registry follows.
    /// Returns the unhides (in window order) followed by the hides (in window order).
    ///
    /// A window is hidden when its workspace is not showing, it has a known display, and it
    /// is not minimized. A window whose workspace is not known yet is left alone.
    pub fn plan(
        &mut self,
        windows: &WindowStore,
        workspaces: &WorkspaceStore,
        displays: &DisplayStore,
        registry: &SharedRegistry<ParkedHandle>,
    ) -> Vec<VisibilityAction> {
        // A window that is gone has nothing to restore.
        self.parked.retain(|&wid, _| {
            let present = windows.contains(wid);
            if !present {
                registry.unpark(registry_key(wid));
            }
            present
        });
        self.unconfirmed.retain(|&wid, _| {
            let present = windows.contains(wid);
            if !present {
                registry.unpark(registry_key(wid));
            }
            present
        });

        let layout = Layout::new(displays);
        let mut unhides = Vec::new();
        let mut hides = Vec::new();

        let shown: Vec<WindowId> = self
            .parked
            .keys()
            .copied()
            .filter(|&wid| workspaces.workspace_of(wid).is_some() && workspaces.window_is_visible(wid))
            .collect();
        for wid in shown {
            let record = windows.get(wid).expect("retained above");
            let parked = &self.parked[&wid];
            let Some((fallback_display, fallback)) = layout.fallback(record.display.as_ref()) else {
                continue; // no display to put it on; it stays parked and registered
            };
            let size = rect_of(record.frame).size;
            let to = resolve_restore(&parked.saved, size, &layout.keyed, fallback);
            let display = if layout.index_of(&parked.saved.display).is_some() {
                parked.saved.display.clone()
            } else {
                fallback_display
            };
            self.parked.remove(&wid);
            // Stays registered, now pointing where it is going, until it is seen there.
            registry.set_target(registry_key(wid), to);
            self.unconfirmed.insert(wid, Write::default());
            unhides.push(VisibilityAction::Unhide {
                window: wid,
                display,
                from: record.frame,
                to: cg_rect(to, size),
            });
        }

        let mut candidates: Vec<_> = windows.iter().collect();
        candidates.sort_by_key(|r| r.wid);
        if registry.is_shutting_down() {
            candidates.clear(); // nothing may be hidden once the restore has begun
        }
        for record in candidates {
            let wid = record.wid;
            if self.parked.contains_key(&wid)
                || record.is_minimized
                || workspaces.workspace_of(wid).is_none()
                || workspaces.window_is_visible(wid)
            {
                continue;
            }
            let Some(display) = record.display.clone() else { continue };
            let Some(index) = layout.index_of(&display) else { continue };
            let rect = layout.rects[index];
            let frame = rect_of(record.frame);

            let corner = choose_corner(&layout.rects, index);
            let parked_at = park_origin(rect, corner, frame.size);
            let saved = save_position(display.clone(), rect, frame.origin);
            let restore_to = resolve_restore(&saved, frame.size, &layout.keyed, rect);

            let handle = ParkedHandle {
                pid: wid.pid,
                window_id: record.sys_id.map(|id| id.as_u32()),
                parked: parked_at,
                size: frame.size,
            };
            // Hidden again before the last unhide was seen: replace the entry in one step
            // (not keeping the stale target), so a refusal cannot lose it.
            let accepted = if self.unconfirmed.contains_key(&wid) {
                registry.park_replacing(registry_key(wid), handle, restore_to)
            } else {
                registry.park(registry_key(wid), handle, restore_to)
            };
            if !accepted {
                continue; // shutdown began; leave the window (and any entry) as it is
            }
            self.unconfirmed.remove(&wid);
            self.parked.insert(wid, Parked { saved, parked_at, restore_to, write: Write::default() });
            hides.push(VisibilityAction::Hide {
                window: wid,
                display,
                corner,
                from: record.frame,
                to: cg_rect(parked_at, frame.size),
            });
        }

        self.refresh_targets(windows, &layout, registry);

        unhides.extend(hides);
        unhides
    }

    /// The reactor issued the write for `window` (carrying `txid`). Frames reported before
    /// this, or for another transaction, are not answers to it.
    pub fn arm(&mut self, window: WindowId, txid: Option<TransactionId>, now: Instant) {
        let write = Write { armed: true, txid, deadline: Some(now + UNCONFIRMED_TTL) };
        if let Some(entry) = self.unconfirmed.get_mut(&window) {
            *entry = write;
        } else if let Some(parked) = self.parked.get_mut(&window) {
            parked.write = write;
        }
    }

    /// The app actor reported `window`'s frame; `requested` is whether it answers a write
    /// of ours and `txid` which one.
    ///
    /// - A window being unhidden that is now on a display is confirmed: it leaves the
    ///   registry.
    /// - A parked window that sits somewhere other than where it was parked (the app
    ///   clamped the move) has its real position recorded for the restorer.
    ///
    /// Only frames answering the outstanding write count. Frames the user or system chose
    /// (`requested == false`) are not recorded.
    pub fn frame_observed(
        &mut self,
        window: WindowId,
        frame: CGRect,
        txid: Option<TransactionId>,
        requested: bool,
        displays: &DisplayStore,
        registry: &SharedRegistry<ParkedHandle>,
    ) {
        if !requested {
            return;
        }
        if let Some(write) = self.unconfirmed.get(&window) {
            if write.answers(txid) && is_on_screen(frame, displays) {
                self.unconfirmed.remove(&window);
                registry.unpark(registry_key(window));
            }
            return;
        }
        if let Some(parked) = self.parked.get_mut(&window)
            && parked.write.answers(txid)
        {
            let at = Point::new(frame.origin.x, frame.origin.y);
            let moved = (at.x - parked.parked_at.x).abs() > 1.0 || (at.y - parked.parked_at.y).abs() > 1.0;
            if moved {
                parked.parked_at = at;
                registry.update_handle(registry_key(window), |h| h.parked = at);
            }
        }
    }

    /// A minimized window is not in a corner, and no frame event will confirm it.
    pub fn window_minimized(&mut self, window: WindowId, registry: &SharedRegistry<ParkedHandle>) {
        if self.unconfirmed.remove(&window).is_some() {
            registry.unpark(registry_key(window));
        }
    }

    /// Settle unhides whose confirmation never came: past their deadline, a window the
    /// reactor believes is on a display leaves the registry. One it believes is still off
    /// every display stays, with a fresh deadline, because a stranded window must remain
    /// restorable.
    pub fn expire(
        &mut self,
        now: Instant,
        windows: &WindowStore,
        displays: &DisplayStore,
        registry: &SharedRegistry<ParkedHandle>,
    ) {
        let due: Vec<WindowId> = self
            .unconfirmed
            .iter()
            .filter(|(_, w)| w.deadline.is_some_and(|d| d <= now))
            .map(|(&wid, _)| wid)
            .collect();
        for wid in due {
            let shown = windows.get(wid).is_some_and(|r| is_on_screen(r.frame, displays));
            if shown {
                self.unconfirmed.remove(&wid);
                registry.unpark(registry_key(wid));
            } else if let Some(write) = self.unconfirmed.get_mut(&wid) {
                write.deadline = Some(now + UNCONFIRMED_TTL);
            }
        }
    }

    /// Display changes move where a parked window would be restored to; keep the registry
    /// (which the termination handler reads) current.
    fn refresh_targets(
        &mut self,
        windows: &WindowStore,
        layout: &Layout,
        registry: &SharedRegistry<ParkedHandle>,
    ) {
        for (&wid, parked) in &mut self.parked {
            let Some(record) = windows.get(wid) else { continue };
            let Some((_, fallback)) = layout.fallback(record.display.as_ref()) else { continue };
            let size = rect_of(record.frame).size;
            let target = resolve_restore(&parked.saved, size, &layout.keyed, fallback);
            if target != parked.restore_to {
                parked.restore_to = target;
                registry.set_target(registry_key(wid), target);
            }
        }
    }
}

/// One call to the presenter.
#[derive(Clone, Debug, PartialEq)]
pub enum LayoutCall {
    /// `AnimationManager::animate_layout` for `display`.
    Animated { display: DisplayId, layout: Vec<(WindowId, CGRect)> },
    /// `AnimationManager::workspace_switch_layout`: apply now.
    Instant { layout: Vec<(WindowId, CGRect)> },
}

/// The presenter calls that carry out `actions`, in the order to make them.
///
/// Animated: one call per display, because a second animation on a display replaces the
/// first. Each carries that display's unhides, then its hides, so incoming and outgoing
/// windows travel together; displays with an unhide come first.
///
/// Instant: all unhides in one call, then all hides in another.
pub fn layout_calls(
    actions: &[VisibilityAction],
    config: VisibilityConfig,
    displays: &DisplayStore,
) -> Vec<LayoutCall> {
    let entry = |a: &VisibilityAction| (a.window(), a.target());
    if !config.animate_switches {
        let (unhides, hides): (Vec<_>, Vec<_>) = actions.iter().partition(|a| a.is_unhide());
        return [unhides, hides]
            .into_iter()
            .filter(|group| !group.is_empty())
            .map(|group| LayoutCall::Instant { layout: group.into_iter().map(entry).collect() })
            .collect();
    }

    let mut order: Vec<&DisplayUuid> = Vec::new();
    for action in actions.iter().filter(|a| a.is_unhide()).chain(actions.iter().filter(|a| !a.is_unhide())) {
        if !order.contains(&action.display()) {
            order.push(action.display());
        }
    }
    order
        .into_iter()
        .filter_map(|uuid| {
            let display = displays.get(uuid)?.cg_id;
            let on_display = |unhide: bool| {
                actions.iter().filter(move |a| a.display() == uuid && a.is_unhide() == unhide)
            };
            let layout = on_display(true).chain(on_display(false)).map(entry).collect();
            Some(LayoutCall::Animated { display, layout })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::display::DisplaySnapshot;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    fn wid(idx: u32) -> WindowId { WindowId::new(1, idx) }

    fn store(displays: &[(&str, u32)]) -> DisplayStore {
        let mut store = DisplayStore::default();
        store.apply(
            displays
                .iter()
                .enumerate()
                .map(|(i, &(uuid, cg))| DisplaySnapshot {
                    uuid: DisplayUuid::new(uuid),
                    cg_id: DisplayId::new(cg),
                    frame: rect(1000.0 * i as f64, 0.0, 1000.0, 800.0),
                    name: None,
                })
                .collect(),
        );
        store
    }

    fn unhide(idx: u32, display: &str) -> VisibilityAction {
        VisibilityAction::Unhide {
            window: wid(idx),
            display: DisplayUuid::new(display),
            from: rect(-300.0, 799.0, 100.0, 100.0),
            to: rect(10.0, 10.0, 100.0, 100.0),
        }
    }

    fn hide(idx: u32, display: &str) -> VisibilityAction {
        VisibilityAction::Hide {
            window: wid(idx),
            display: DisplayUuid::new(display),
            corner: Corner::BottomLeft,
            from: rect(10.0, 10.0, 100.0, 100.0),
            to: rect(-99.0, 799.0, 100.0, 100.0),
        }
    }

    fn ids(call: &LayoutCall) -> Vec<u32> {
        let layout = match call {
            LayoutCall::Animated { layout, .. } | LayoutCall::Instant { layout } => layout,
        };
        layout.iter().map(|(w, _)| w.idx.get()).collect()
    }

    #[test]
    fn config_defaults_to_animated_and_is_serde_derivable() {
        fn serde_both<T: Serialize + for<'de> Deserialize<'de>>() {}
        serde_both::<VisibilityConfig>();
        assert!(VisibilityConfig::default().animate_switches);
    }

    #[test]
    fn registry_keys_are_unique_per_window() {
        assert_ne!(registry_key(WindowId::new(1, 2)), registry_key(WindowId::new(2, 1)));
        assert_ne!(registry_key(WindowId::new(1, 2)), registry_key(WindowId::new(1, 3)));
        assert_eq!(registry_key(WindowId::new(1, 2)), (1u64 << 32) | 2);
        assert_ne!(registry_key(WindowId::new(-1, 1)), registry_key(WindowId::new(1, 1)));
    }

    #[test]
    fn instant_mode_unhides_everything_before_hiding_anything() {
        let displays = store(&[("A", 1), ("B", 2)]);
        let actions = [hide(1, "A"), unhide(2, "B"), hide(3, "B"), unhide(4, "A")];
        let calls = layout_calls(&actions, VisibilityConfig { animate_switches: false }, &displays);
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| matches!(c, LayoutCall::Instant { .. })));
        assert_eq!(ids(&calls[0]), vec![2, 4]);
        assert_eq!(ids(&calls[1]), vec![1, 3]);
    }

    #[test]
    fn instant_mode_skips_an_empty_group() {
        let displays = store(&[("A", 1)]);
        let only_hides = layout_calls(&[hide(1, "A")], VisibilityConfig { animate_switches: false }, &displays);
        assert_eq!(only_hides.len(), 1);
        assert!(layout_calls(&[], VisibilityConfig { animate_switches: false }, &displays).is_empty());
        assert!(layout_calls(&[], VisibilityConfig::default(), &displays).is_empty());
    }

    #[test]
    fn animated_mode_sends_one_call_per_display_with_unhides_first() {
        let displays = store(&[("A", 10), ("B", 20)]);
        // Display A: window 1 leaves, window 2 arrives. Display B: only a hide.
        let actions = [unhide(2, "A"), hide(3, "B"), hide(1, "A")];
        let calls = layout_calls(&actions, VisibilityConfig { animate_switches: true }, &displays);
        assert_eq!(calls.len(), 2);
        let LayoutCall::Animated { display, .. } = &calls[0] else { panic!("{calls:?}") };
        assert_eq!(*display, DisplayId::new(10));
        assert_eq!(ids(&calls[0]), vec![2, 1], "incoming before outgoing");
        let LayoutCall::Animated { display, .. } = &calls[1] else { panic!("{calls:?}") };
        assert_eq!(*display, DisplayId::new(20));
        assert_eq!(ids(&calls[1]), vec![3]);
    }

    #[test]
    fn animated_mode_orders_displays_with_unhides_before_displays_that_only_hide() {
        let displays = store(&[("A", 10), ("B", 20)]);
        let actions = [hide(1, "A"), unhide(2, "B")];
        let calls = layout_calls(&actions, VisibilityConfig::default(), &displays);
        let displays_in_order: Vec<_> = calls
            .iter()
            .map(|c| match c {
                LayoutCall::Animated { display, .. } => display.as_u32(),
                LayoutCall::Instant { .. } => panic!("animated mode"),
            })
            .collect();
        assert_eq!(displays_in_order, vec![20, 10]);
    }

    #[test]
    fn animated_mode_targets_are_the_actions_targets() {
        let displays = store(&[("A", 10)]);
        let calls = layout_calls(&[unhide(1, "A"), hide(2, "A")], VisibilityConfig::default(), &displays);
        let LayoutCall::Animated { layout, .. } = &calls[0] else { panic!() };
        assert_eq!(layout, &vec![(wid(1), rect(10.0, 10.0, 100.0, 100.0)), (wid(2), rect(-99.0, 799.0, 100.0, 100.0))]);
    }

    #[test]
    fn an_action_for_an_unknown_display_is_dropped_from_animated_calls() {
        let displays = store(&[("A", 10)]);
        assert!(layout_calls(&[hide(1, "GONE")], VisibilityConfig::default(), &displays).is_empty());
    }
}
