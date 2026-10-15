//! The daemon's [`Restorer`]: puts parked windows back through AX, without any reactor
//! state, from a restore worker thread.
//!
//! Matching a registry entry to an AX window:
//! 1. By WindowServer id (`_AXUIElementGetWindow`). This is exact, so windows of one app
//!    that share a park origin cannot swap targets, and a window whose parked position the
//!    app clamped is still found.
//! 2. Only when the entry has no id (or no AX window reports it): by the parked position,
//!    within [`MATCH_TOLERANCE`], preferring the window whose size is closest to the
//!    recorded one. Windows that report an id for another entry are never taken.
//! 3. A final sweep per app moves a window that is not visibly on a display
//!    ([`SWEEP_MIN_VISIBLE`]) onto the main display, whatever the registry says, so a
//!    window that slipped through is still reachable. It is deliberately narrow: only
//!    windows that look corner-parked (at a known parked position, or in the bottom-left
//!    or bottom-right park pattern against a display edge), never minimized ones, and not
//!    at all when fewer displays are active than the reactor last saw (a sleeping monitor
//!    would make every window on it look stranded).
//!
//! The AX calls sit behind [`AxBackend`], so the matching is tested with a fake. Each
//! window is restored under `catch_unwind`: one bad window must not cost the others.

use std::panic::{self, AssertUnwindSafe};

use super::ax::AxElement;
use crate::geometry::{Point, Rect, Size};
use crate::parking::clamp_on_screen;
use std::sync::Arc;

use crate::restore::{ParkedHandle, SharedRegistry};
use crate::termination::{Restorer, log_line};

/// How far a window may be from its recorded parked position and still be that window,
/// when it has to be matched by position.
pub const MATCH_TOLERANCE: f64 = 3.0;

/// A window must show at least this many pixels (each way) on a display to count as
/// reachable; otherwise the sweep moves it onto the main display.
pub const SWEEP_MIN_VISIBLE: f64 = 50.0;

/// Where on the main display a swept window is put: this far in from its corner.
const SWEEP_INSET: f64 = 40.0;

/// The AX operations the restorer needs.
pub trait AxBackend: Send + Sync {
    type Window;
    /// The app's AX windows.
    fn windows(&self, pid: i32) -> Result<Vec<Self::Window>, String>;
    fn window_id(&self, window: &Self::Window) -> Option<u32>;
    fn frame(&self, window: &Self::Window) -> Option<Rect>;
    fn is_minimized(&self, window: &Self::Window) -> bool;
    fn set_position(&self, window: &Self::Window, to: Point) -> Result<(), String>;
    /// The active displays in AX coordinates; the first is the main display.
    fn displays(&self) -> Vec<Rect>;
}

/// The real AX.
pub struct SystemAx;

impl AxBackend for SystemAx {
    type Window = AxElement;

    fn windows(&self, pid: i32) -> Result<Vec<AxElement>, String> {
        AxElement::application(pid).windows()
    }

    fn window_id(&self, window: &AxElement) -> Option<u32> { window.window_id() }

    fn frame(&self, window: &AxElement) -> Option<Rect> { window.frame() }

    fn is_minimized(&self, window: &AxElement) -> bool { window.is_minimized() }

    fn set_position(&self, window: &AxElement, to: Point) -> Result<(), String> {
        window.set_position(to)
    }

    fn displays(&self) -> Vec<Rect> {
        use objc2_core_graphics::{CGDisplayBounds, CGGetActiveDisplayList};
        let mut ids = [0u32; 32];
        let mut count = 0u32;
        let err = unsafe { CGGetActiveDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
        if err != objc2_core_graphics::CGError::Success {
            return Vec::new();
        }
        ids[..count as usize]
            .iter()
            .map(|&id| {
                let b = CGDisplayBounds(id);
                Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height)
            })
            .collect()
    }
}

/// Restores parked windows through AX, one app per [`Restorer::group`].
pub struct AxRestorer<B: AxBackend = SystemAx> {
    backend: B,
    /// Tells the sweep how many displays the reactor last saw.
    registry: Arc<SharedRegistry<ParkedHandle>>,
}

impl AxRestorer<SystemAx> {
    /// The restorer for the real AX.
    pub fn system(registry: Arc<SharedRegistry<ParkedHandle>>) -> Self {
        Self::new(SystemAx, registry)
    }
}

/// How close to the park point a window must be to look parked.
const PARK_PATTERN_TOLERANCE: f64 = 3.0;

fn near(a: Point, b: Point) -> bool {
    (a.x - b.x).abs() <= MATCH_TOLERANCE && (a.y - b.y).abs() <= MATCH_TOLERANCE
}

fn visible_on(frame: Rect, display: Rect) -> bool {
    let w = frame.max_x().min(display.max_x()) - frame.min_x().max(display.min_x());
    let h = frame.max_y().min(display.max_y()) - frame.min_y().max(display.min_y());
    w >= SWEEP_MIN_VISIBLE.min(frame.size.w) && h >= SWEEP_MIN_VISIBLE.min(frame.size.h) && w > 0.0 && h > 0.0
}

/// Whether `frame` is where corner parking puts windows: at a position an entry recorded,
/// or one pixel in from a display's bottom corner (bottom-left: the window ends one pixel
/// inside the left edge; bottom-right: it starts one pixel inside the right edge).
fn looks_parked(frame: Rect, displays: &[Rect], known: &[Point]) -> bool {
    let close = |a: f64, b: f64| (a - b).abs() <= PARK_PATTERN_TOLERANCE;
    known.iter().any(|p| near(frame.origin, *p))
        || displays.iter().any(|d| {
            close(frame.min_y(), d.max_y() - 1.0)
                && (close(frame.max_x(), d.min_x() + 1.0) || close(frame.min_x(), d.max_x() - 1.0))
        })
}

impl<B: AxBackend> AxRestorer<B> {
    /// A restorer over `backend`; `registry` supplies the display count for the sweep.
    pub fn new(backend: B, registry: Arc<SharedRegistry<ParkedHandle>>) -> Self {
        AxRestorer { backend, registry }
    }

    fn move_window(&self, pid: i32, window: &B::Window, to: Point) {
        let moved = panic::catch_unwind(AssertUnwindSafe(|| self.backend.set_position(window, to)));
        match moved {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log_line(format_args!("dowm: failed to restore a window of pid {pid}: {e}")),
            Err(_) => log_line(format_args!("dowm: restoring a window of pid {pid} panicked")),
        }
    }

    fn restore_app(&self, pid: i32, entries: &[&(ParkedHandle, Point)]) {
        let windows = match self.backend.windows(pid) {
            Ok(windows) => windows,
            Err(e) => {
                log_line(format_args!("dowm: cannot list windows of pid {pid} to restore them: {e}"));
                return;
            }
        };
        let ids: Vec<Option<u32>> = windows.iter().map(|w| self.backend.window_id(w)).collect();
        let frames: Vec<Option<Rect>> = windows.iter().map(|w| self.backend.frame(w)).collect();
        let mut claimed = vec![false; windows.len()];

        // 1. Exact: by WindowServer id.
        let mut by_position = Vec::new();
        for entry in entries {
            let (handle, target) = **entry;
            let found = handle.window_id.and_then(|id| {
                (0..windows.len()).find(|&i| !claimed[i] && ids[i] == Some(id))
            });
            match found {
                Some(i) => {
                    claimed[i] = true;
                    self.move_window(pid, &windows[i], target);
                }
                None => by_position.push((handle, target)),
            }
        }

        // 2. Fallback: by parked position, among windows that no entry owns by id.
        let owned_ids: Vec<u32> = entries.iter().filter_map(|e| e.0.window_id).collect();
        for (handle, target) in by_position {
            let best = (0..windows.len())
                .filter(|&i| !claimed[i] && ids[i].is_none_or(|id| !owned_ids.contains(&id)))
                .filter_map(|i| frames[i].map(|f| (i, f)))
                .filter(|(_, f)| near(f.origin, handle.parked))
                .min_by(|(_, a), (_, b)| {
                    let cost = |s: Size| (s.w - handle.size.w).abs() + (s.h - handle.size.h).abs();
                    cost(a.size).total_cmp(&cost(b.size))
                });
            if let Some((i, _)) = best {
                claimed[i] = true;
                self.move_window(pid, &windows[i], target);
            }
        }

        // 3. Sweep: a corner-parked window still out of reach goes onto the main display.
        if claimed.iter().all(|&c| c) {
            return;
        }
        let displays = self.backend.displays();
        let Some(main) = displays.first().copied() else { return };
        if displays.len() < self.registry.display_count() {
            log_line(format_args!("dowm: fewer displays active than before; skipping the sweep"));
            return;
        }
        let known: Vec<Point> = entries.iter().map(|e| e.0.parked).collect();
        for (i, window) in windows.iter().enumerate() {
            let Some(frame) = frames[i] else { continue };
            if claimed[i]
                || displays.iter().any(|d| visible_on(frame, *d))
                || !looks_parked(frame, &displays, &known)
                || self.backend.is_minimized(window)
            {
                continue;
            }
            let inset = Point::new(main.min_x() + SWEEP_INSET, main.min_y() + SWEEP_INSET);
            self.move_window(pid, window, clamp_on_screen(inset, frame.size, main));
        }
    }
}

impl<B: AxBackend> Restorer<ParkedHandle> for AxRestorer<B> {
    fn group(&self, handle: &ParkedHandle) -> i64 { i64::from(handle.pid) }

    fn restore(&self, windows: &[(ParkedHandle, Point)]) {
        let mut pids: Vec<i32> = windows.iter().map(|(h, _)| h.pid).collect();
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            let entries: Vec<_> = windows.iter().filter(|(h, _)| h.pid == pid).collect();
            self.restore_app(pid, &entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Clone)]
    struct FakeWindow {
        id: Option<u32>,
        frame: Option<Rect>,
        /// Set_position on this window panics.
        explodes: bool,
        minimized: bool,
    }

    fn win(id: Option<u32>, frame: Rect) -> FakeWindow {
        FakeWindow { id, frame: Some(frame), explodes: false, minimized: false }
    }

    struct FakeAx {
        windows: Vec<FakeWindow>,
        displays: Vec<Rect>,
        /// (index into `windows`, target) in the order written.
        moves: Mutex<Vec<(usize, Point)>>,
    }

    impl FakeAx {
        fn new(windows: Vec<FakeWindow>) -> Self {
            FakeAx { windows, displays: vec![DISPLAY], moves: Mutex::new(Vec::new()) }
        }

        fn moves(&self) -> Vec<(usize, Point)> {
            let mut m = self.moves.lock().unwrap().clone();
            m.sort_by_key(|m| m.0);
            m
        }
    }

    impl AxBackend for FakeAx {
        type Window = usize;

        fn windows(&self, _: i32) -> Result<Vec<usize>, String> { Ok((0..self.windows.len()).collect()) }

        fn window_id(&self, w: &usize) -> Option<u32> { self.windows[*w].id }

        fn frame(&self, w: &usize) -> Option<Rect> { self.windows[*w].frame }

        fn set_position(&self, w: &usize, to: Point) -> Result<(), String> {
            if self.windows[*w].explodes {
                panic!("bad window");
            }
            self.moves.lock().unwrap().push((*w, to));
            Ok(())
        }

        fn is_minimized(&self, w: &usize) -> bool { self.windows[*w].minimized }

        fn displays(&self) -> Vec<Rect> { self.displays.clone() }
    }

    const DISPLAY: Rect = Rect::new(0.0, 0.0, 1920.0, 1080.0);
    const SIZE: Size = Size::new(400.0, 300.0);
    const PARKED: Point = Point::new(-399.0, 1079.0);

    fn handle(window_id: Option<u32>) -> ParkedHandle {
        ParkedHandle { pid: 7, window_id, parked: PARKED, size: SIZE }
    }

    /// An entry whose window is not at the parked frame, so only the sweep could find it.
    fn elsewhere() -> ParkedHandle {
        ParkedHandle { parked: Point::new(900.0, 900.0), ..handle(Some(1)) }
    }

    fn parked_frame() -> Rect { Rect { origin: PARKED, size: SIZE } }

    fn restore(ax: FakeAx, entries: &[(ParkedHandle, Point)]) -> Vec<(usize, Point)> {
        restore_seeing(ax, entries, 0)
    }

    /// Restore when the reactor last saw `displays_seen` displays.
    fn restore_seeing(ax: FakeAx, entries: &[(ParkedHandle, Point)], displays_seen: usize) -> Vec<(usize, Point)> {
        let registry = Arc::new(SharedRegistry::new());
        registry.set_display_count(displays_seen);
        let restorer = AxRestorer::new(ax, registry);
        restorer.restore(entries);
        restorer.backend.moves()
    }

    #[test]
    fn windows_sharing_a_park_origin_each_get_their_own_target() {
        // Three same-sized windows parked in the same corner; AX lists them in another
        // order than the registry. Position alone could not tell them apart.
        let ax = FakeAx::new(vec![
            win(Some(30), parked_frame()),
            win(Some(10), parked_frame()),
            win(Some(20), parked_frame()),
        ]);
        let entries = [
            (handle(Some(10)), Point::new(100.0, 100.0)),
            (handle(Some(20)), Point::new(200.0, 200.0)),
            (handle(Some(30)), Point::new(300.0, 300.0)),
        ];
        assert_eq!(
            restore(ax, &entries),
            vec![
                (0, Point::new(300.0, 300.0)),
                (1, Point::new(100.0, 100.0)),
                (2, Point::new(200.0, 200.0)),
            ]
        );
    }

    #[test]
    fn a_window_whose_parked_position_was_clamped_is_still_found_by_id() {
        let clamped = Rect { origin: Point::new(-120.0, 760.0), size: SIZE };
        let ax = FakeAx::new(vec![win(Some(10), clamped)]);
        let moves = restore(ax, &[(handle(Some(10)), Point::new(100.0, 100.0))]);
        assert_eq!(moves, vec![(0, Point::new(100.0, 100.0))]);
    }

    #[test]
    fn without_an_id_the_parked_position_matches_and_the_closest_size_wins() {
        let wrong_size = Rect { origin: PARKED, size: Size::new(900.0, 700.0) };
        let ax = FakeAx::new(vec![win(None, wrong_size), win(None, parked_frame())]);
        let moves = restore(ax, &[(handle(None), Point::new(50.0, 60.0))]);
        // The closer-sized window got the target; the other is stranded, so the sweep
        // moved it onto the display.
        assert_eq!(moves, vec![(0, Point::new(40.0, 40.0)), (1, Point::new(50.0, 60.0))]);
    }

    #[test]
    fn an_id_less_entry_does_not_take_a_window_another_entry_owns() {
        let ax = FakeAx::new(vec![win(Some(10), parked_frame())]);
        let entries = [
            (handle(None), Point::new(1.0, 1.0)),
            (handle(Some(10)), Point::new(2.0, 2.0)),
        ];
        assert_eq!(restore(ax, &entries), vec![(0, Point::new(2.0, 2.0))]);
    }

    #[test]
    fn an_entry_with_no_matching_window_moves_nothing() {
        let elsewhere = Rect { origin: Point::new(300.0, 300.0), size: SIZE };
        let ax = FakeAx::new(vec![win(Some(99), elsewhere), win(None, elsewhere)]);
        let moves = restore(ax, &[(handle(Some(10)), Point::new(5.0, 5.0)), (handle(None), Point::new(6.0, 6.0))]);
        assert!(moves.is_empty());
    }

    #[test]
    fn the_sweep_rescues_a_corner_parked_window_the_registry_could_not_match() {
        // Parked in the corner with no id and not where the (clamped) entry says: neither
        // id nor position matches, but it is stranded in the park pattern.
        let ax = FakeAx::new(vec![win(None, parked_frame()), win(None, Rect::new(200.0, 200.0, 400.0, 300.0))]);
        let entry = ParkedHandle { parked: Point::new(-100.0, 100.0), ..handle(None) };
        let moves = restore(ax, &[(entry, Point::new(5.0, 5.0))]);
        assert_eq!(moves, vec![(0, Point::new(40.0, 40.0))], "only the unreachable window moved");
    }

    #[test]
    fn the_sweep_leaves_a_mostly_off_screen_user_window_alone() {
        // Dragged nearly off the display on purpose: 20px showing, nowhere near a corner.
        let ax = FakeAx::new(vec![win(None, Rect::new(-380.0, 200.0, 400.0, 300.0))]);
        assert!(restore(ax, &[(handle(Some(1)), Point::new(5.0, 5.0))]).is_empty());
    }

    #[test]
    fn the_sweep_leaves_a_minimized_window_alone() {
        let mut minimized = win(None, parked_frame());
        minimized.minimized = true;
        let ax = FakeAx::new(vec![minimized]);
        assert!(restore(ax, &[(elsewhere(), Point::new(5.0, 5.0))]).is_empty());
    }

    #[test]
    fn the_sweep_stands_down_when_fewer_displays_are_active_than_the_reactor_saw() {
        let ax = FakeAx::new(vec![win(None, parked_frame())]);
        let entry = (elsewhere(), Point::new(5.0, 5.0));
        // One display active, two last seen: a monitor is asleep or unplugged.
        assert!(restore_seeing(ax, std::slice::from_ref(&entry), 2).is_empty());
        let ax = FakeAx::new(vec![win(None, parked_frame())]);
        assert_eq!(restore_seeing(ax, &[entry], 1), vec![(0, Point::new(40.0, 40.0))]);
    }

    #[test]
    fn the_sweep_recognises_the_bottom_right_park_pattern() {
        let br = Rect { origin: Point::new(1919.0, 1079.0), size: SIZE };
        let ax = FakeAx::new(vec![win(None, br)]);
        assert_eq!(restore(ax, &[(handle(Some(1)), Point::new(0.0, 0.0))]), vec![(0, Point::new(40.0, 40.0))]);
    }

    #[test]
    fn the_sweep_leaves_windows_restored_by_id_and_visible_windows_alone() {
        let ax = FakeAx::new(vec![win(Some(10), parked_frame()), win(None, Rect::new(10.0, 10.0, 300.0, 300.0))]);
        let moves = restore(ax, &[(handle(Some(10)), Point::new(100.0, 100.0))]);
        assert_eq!(moves, vec![(0, Point::new(100.0, 100.0))]);
    }

    #[test]
    fn the_sweep_is_skipped_when_no_display_is_known() {
        let mut ax = FakeAx::new(vec![win(None, parked_frame())]);
        ax.displays.clear();
        let entry = ParkedHandle { parked: Point::new(900.0, 900.0), ..handle(Some(1)) };
        assert!(restore(ax, &[(entry, Point::new(0.0, 0.0))]).is_empty());
    }

    #[test]
    fn one_window_that_panics_does_not_stop_the_others() {
        let mut bad = win(Some(10), parked_frame());
        bad.explodes = true;
        let ax = FakeAx::new(vec![bad, win(Some(20), parked_frame())]);
        let entries = [(handle(Some(10)), Point::new(1.0, 1.0)), (handle(Some(20)), Point::new(2.0, 2.0))];
        assert_eq!(restore(ax, &entries), vec![(1, Point::new(2.0, 2.0))]);
    }

    #[test]
    fn restoring_is_grouped_by_app() {
        let restorer = AxRestorer::new(FakeAx::new(Vec::new()), Arc::new(SharedRegistry::new()));
        assert_eq!(restorer.group(&handle(None)), 7);
    }
}
