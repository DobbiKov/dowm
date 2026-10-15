//! Display tracking keyed by the display's stable UUID.
//!
//! A `CGDirectDisplayID` can change when a display is reconnected, and a display's
//! origin changes whenever the arrangement does, so neither identifies a display.
//! The UUID from `CGDisplayCreateUUIDFromDisplayID` survives both.

use std::fmt;

use objc2_core_foundation::{CGPoint, CGRect};
use tracing::warn;

use crate::sys::screen::{DisplayId, ScreenInfo};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayUuid(String);

impl DisplayUuid {
    pub fn new(uuid: impl Into<String>) -> Self { Self(uuid.into()) }

    pub fn as_str(&self) -> &str { &self.0 }
}

impl fmt::Display for DisplayUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(&self.0) }
}

/// One display as reported by a screen refresh.
#[derive(Clone, Debug, PartialEq)]
pub struct DisplaySnapshot {
    pub uuid: DisplayUuid,
    /// Current CoreGraphics id; may change across reconnects.
    pub cg_id: DisplayId,
    /// Usable frame in global top-left coordinates (the space AX frames use).
    pub frame: CGRect,
    pub name: Option<String>,
}

impl From<&ScreenInfo> for DisplaySnapshot {
    fn from(screen: &ScreenInfo) -> Self {
        DisplaySnapshot {
            uuid: DisplayUuid::new(screen.display_uuid.clone()),
            cg_id: screen.id,
            frame: screen.frame,
            name: screen.name.clone(),
        }
    }
}

/// What a snapshot changed, by UUID.
#[derive(Debug, Default, PartialEq)]
pub struct DisplayChanges {
    pub added: Vec<DisplayUuid>,
    pub removed: Vec<DisplayUuid>,
    /// Still present, but with a different CG id, frame or name.
    pub changed: Vec<DisplayUuid>,
}

impl DisplayChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The current displays, in snapshot order; the first is the main display.
#[derive(Debug, Default)]
pub struct DisplayStore {
    displays: Vec<DisplaySnapshot>,
}

impl DisplayStore {
    /// Replace the display set with `snapshot` and report the difference.
    ///
    /// A UUID that appears twice (mirroring) keeps its first entry.
    pub fn apply(&mut self, snapshot: Vec<DisplaySnapshot>) -> DisplayChanges {
        let mut next: Vec<DisplaySnapshot> = Vec::with_capacity(snapshot.len());
        for snap in snapshot {
            if next.iter().any(|d| d.uuid == snap.uuid) {
                warn!(uuid = %snap.uuid, cg_id = ?snap.cg_id, "duplicate display uuid; ignoring");
                continue;
            }
            next.push(snap);
        }

        let mut changes = DisplayChanges::default();
        for display in &next {
            match self.get(&display.uuid) {
                None => changes.added.push(display.uuid.clone()),
                Some(old) if old != display => changes.changed.push(display.uuid.clone()),
                Some(_) => {}
            }
        }
        for old in &self.displays {
            if !next.iter().any(|d| d.uuid == old.uuid) {
                changes.removed.push(old.uuid.clone());
            }
        }

        self.displays = next;
        changes
    }

    pub fn get(&self, uuid: &DisplayUuid) -> Option<&DisplaySnapshot> {
        self.displays.iter().find(|d| &d.uuid == uuid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &DisplaySnapshot> { self.displays.iter() }

    pub fn main(&self) -> Option<&DisplaySnapshot> { self.displays.first() }

    pub fn len(&self) -> usize { self.displays.len() }

    pub fn is_empty(&self) -> bool { self.displays.is_empty() }

    /// The display a window with this frame belongs to: the one it overlaps most,
    /// or, when it overlaps none (a parked window), the one whose center is nearest.
    pub fn display_for_rect(&self, rect: CGRect) -> Option<&DisplayUuid> {
        let overlapping = self
            .displays
            .iter()
            .map(|d| (d, overlap_area(d.frame, rect)))
            .filter(|&(_, area)| area > 0.0)
            .max_by(|(_, a), (_, b)| a.total_cmp(b));
        if let Some((display, _)) = overlapping {
            return Some(&display.uuid);
        }
        let center = rect_center(rect);
        self.displays
            .iter()
            .min_by(|a, b| {
                distance_sq(rect_center(a.frame), center)
                    .total_cmp(&distance_sq(rect_center(b.frame), center))
            })
            .map(|d| &d.uuid)
    }
}

fn overlap_area(a: CGRect, b: CGRect) -> f64 {
    let w = (a.max().x.min(b.max().x) - a.origin.x.max(b.origin.x)).max(0.0);
    let h = (a.max().y.min(b.max().y) - a.origin.y.max(b.origin.y)).max(0.0);
    w * h
}

fn rect_center(r: CGRect) -> CGPoint {
    CGPoint::new(r.origin.x + r.size.width / 2.0, r.origin.y + r.size.height / 2.0)
}

fn distance_sq(a: CGPoint, b: CGPoint) -> f64 {
    let (dx, dy) = (a.x - b.x, a.y - b.y);
    dx * dx + dy * dy
}

#[cfg(test)]
pub(crate) mod tests {
    use objc2_core_foundation::CGSize;

    use super::*;

    pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
    }

    pub(crate) fn display(uuid: &str, cg_id: u32, frame: CGRect) -> DisplaySnapshot {
        DisplaySnapshot {
            uuid: DisplayUuid::new(uuid),
            cg_id: DisplayId::new(cg_id),
            frame,
            name: None,
        }
    }

    fn uuids(ids: &[&str]) -> Vec<DisplayUuid> { ids.iter().map(|s| DisplayUuid::new(*s)).collect() }

    #[test]
    fn first_snapshot_adds_every_display_in_order() {
        let mut store = DisplayStore::default();
        let changes = store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("B", 2, rect(1440., 0., 1920., 1080.)),
        ]);
        assert_eq!(changes.added, uuids(&["A", "B"]));
        assert!(changes.removed.is_empty() && changes.changed.is_empty());
        assert_eq!(store.main().unwrap().uuid.as_str(), "A");
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn identical_snapshot_changes_nothing() {
        let mut store = DisplayStore::default();
        let snap = vec![display("A", 1, rect(0., 0., 1440., 900.))];
        store.apply(snap.clone());
        assert!(store.apply(snap).is_empty());
    }

    #[test]
    fn removal_and_addition_are_reported_by_uuid() {
        let mut store = DisplayStore::default();
        store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("B", 2, rect(1440., 0., 1920., 1080.)),
        ]);
        let changes = store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("C", 3, rect(-1920., 0., 1920., 1080.)),
        ]);
        assert_eq!(changes.added, uuids(&["C"]));
        assert_eq!(changes.removed, uuids(&["B"]));
        assert!(changes.changed.is_empty());
        assert!(store.get(&DisplayUuid::new("B")).is_none());
    }

    #[test]
    fn reconnect_with_new_cg_id_is_a_change_not_a_new_display() {
        let mut store = DisplayStore::default();
        store.apply(vec![display("A", 1, rect(0., 0., 1440., 900.))]);
        let changes = store.apply(vec![display("A", 7, rect(0., 0., 1440., 900.))]);
        assert_eq!(changes.changed, uuids(&["A"]));
        assert!(changes.added.is_empty() && changes.removed.is_empty());
        assert_eq!(store.get(&DisplayUuid::new("A")).unwrap().cg_id, DisplayId::new(7));
    }

    #[test]
    fn rearranged_origin_keeps_identity() {
        let mut store = DisplayStore::default();
        store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("B", 2, rect(1440., 0., 1920., 1080.)),
        ]);
        let changes = store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("B", 2, rect(-1920., 0., 1920., 1080.)),
        ]);
        assert_eq!(changes.changed, uuids(&["B"]));
        assert!(changes.added.is_empty() && changes.removed.is_empty());
    }

    #[test]
    fn duplicate_uuid_keeps_first_entry() {
        let mut store = DisplayStore::default();
        let changes = store.apply(vec![
            display("A", 1, rect(0., 0., 1440., 900.)),
            display("A", 2, rect(1440., 0., 1440., 900.)),
        ]);
        assert_eq!(changes.added, uuids(&["A"]));
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(&DisplayUuid::new("A")).unwrap().cg_id, DisplayId::new(1));
    }

    #[test]
    fn rect_maps_to_display_with_largest_overlap() {
        let mut store = DisplayStore::default();
        store.apply(vec![
            display("A", 1, rect(0., 0., 1000., 1000.)),
            display("B", 2, rect(1000., 0., 1000., 1000.)),
        ]);
        // 300px of width on A, 100px on B.
        let uuid = store.display_for_rect(rect(700., 100., 400., 400.)).unwrap();
        assert_eq!(uuid.as_str(), "A");
        let uuid = store.display_for_rect(rect(1500., 100., 100., 100.)).unwrap();
        assert_eq!(uuid.as_str(), "B");
    }

    #[test]
    fn offscreen_rect_maps_to_nearest_display() {
        let mut store = DisplayStore::default();
        store.apply(vec![
            display("A", 1, rect(0., 0., 1000., 1000.)),
            display("B", 2, rect(1000., 0., 1000., 1000.)),
        ]);
        // Parked just past B's bottom-right corner.
        let uuid = store.display_for_rect(rect(1999., 1000., 800., 600.)).unwrap();
        assert_eq!(uuid.as_str(), "B");
        let uuid = store.display_for_rect(rect(2500., 3000., 10., 10.)).unwrap();
        assert_eq!(uuid.as_str(), "B");
    }

    #[test]
    fn rect_with_no_displays_has_no_display() {
        let store = DisplayStore::default();
        assert!(store.display_for_rect(rect(0., 0., 10., 10.)).is_none());
    }

    #[test]
    fn snapshot_from_screen_info_uses_uuid() {
        let info = ScreenInfo {
            id: DisplayId::new(4),
            frame: rect(0., 25., 1440., 875.),
            backing_scale: 2.0,
            display_uuid: "37D8832A-2D66-02CA-B9F7-8F30A301B230".into(),
            name: Some("Built-in".into()),
        };
        let snap = DisplaySnapshot::from(&info);
        assert_eq!(snap.uuid.as_str(), "37D8832A-2D66-02CA-B9F7-8F30A301B230");
        assert_eq!(snap.cg_id, DisplayId::new(4));
        assert_eq!(snap.name.as_deref(), Some("Built-in"));
    }
}
