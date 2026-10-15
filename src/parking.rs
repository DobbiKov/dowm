//! Corner parking geometry: where a hidden window goes, and where a shown one comes back.
//!
//! All rects are in AX coordinates (origin top-left, y down), the space display frames
//! and window frames share in the reactor. Everything here is pure.
//!
//! A window is hidden by moving it just past a bottom corner of its own display, offset by
//! its own size so that one pixel of it stays on the display (macOS will not let a window
//! leave the desktop entirely). After AeroSpace `MacWindow.swift` (park position) and
//! `refresh.swift:151-188` (corner choice).

use crate::geometry::{Corner, Point, Proportional, Rect, Size, restore_proportional, save_proportional};

/// How much a probe that lands on a diagonal neighbour counts for, against 1 for a
/// neighbour directly beside or below. A parked window that only grazes a side neighbour
/// is a nuisance; one that lands on the diagonal one is visible in the middle of the work.
pub const DIAGONAL_WEIGHT: u32 = 10;

fn contains(rect: &Rect, x: f64, y: f64) -> bool {
    x >= rect.min_x() && x < rect.max_x() && y >= rect.min_y() && y < rect.max_y()
}

/// How badly parking in each bottom corner of `displays[index]` would show up on other
/// displays: `(bottom_left, bottom_right)`. Each corner is probed one pixel outside the
/// display on its side, below it, and diagonally.
pub fn corner_scores(displays: &[Rect], index: usize) -> (u32, u32) {
    let Some(me) = displays.get(index) else { return (0, 0) };
    let score = |probes: [(f64, f64, u32); 3]| -> u32 {
        probes
            .iter()
            .map(|&(x, y, weight)| {
                let hits = displays
                    .iter()
                    .enumerate()
                    .filter(|&(i, d)| i != index && contains(d, x, y))
                    .count() as u32;
                hits * weight
            })
            .sum()
    };
    let (left, right, bottom) = (me.min_x(), me.max_x(), me.max_y());
    let bl = score([
        (left - 1.0, bottom - 1.0, 1),
        (left + 1.0, bottom + 1.0, 1),
        (left - 1.0, bottom + 1.0, DIAGONAL_WEIGHT),
    ]);
    let br = score([
        (right + 1.0, bottom - 1.0, 1),
        (right - 2.0, bottom + 1.0, 1),
        (right + 1.0, bottom + 1.0, DIAGONAL_WEIGHT),
    ]);
    (bl, br)
}

/// The bottom corner of `displays[index]` to park in: the one with the lower score,
/// bottom-left on a tie (and for an unknown index).
pub fn choose_corner(displays: &[Rect], index: usize) -> Corner {
    let (bl, br) = corner_scores(displays, index);
    if br < bl { Corner::BottomRight } else { Corner::BottomLeft }
}

/// The top-left a window of `size` is moved to in order to hide it past `corner` of `display`.
pub fn park_origin(display: Rect, corner: Corner, size: Size) -> Point {
    match corner {
        Corner::BottomLeft => Point::new(display.min_x() + 1.0 - size.w, display.max_y() - 1.0),
        Corner::BottomRight => Point::new(display.max_x() - 1.0, display.max_y() - 1.0),
    }
}

/// A window's top-left as a fraction of the display it was on, kept while it is parked.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedPosition<K> {
    /// The display the fraction is relative to, by its stable key (the display UUID).
    pub display: K,
    /// The window's top-left as a fraction of that display's rect.
    pub frac: Proportional,
}

/// Save `origin` as a fraction of `display_rect`, remembering which display that was.
pub fn save_position<K>(display: K, display_rect: Rect, origin: Point) -> SavedPosition<K> {
    SavedPosition { display, frac: save_proportional(origin, display_rect) }
}

/// Move `origin` so that a window of `size` lies inside `bounds`. A window larger than
/// `bounds` is pinned to its top-left.
pub fn clamp_on_screen(origin: Point, size: Size, bounds: Rect) -> Point {
    let clamp = |v: f64, min: f64, max: f64| if max < min { min } else { v.clamp(min, max) };
    Point::new(
        clamp(origin.x, bounds.min_x(), bounds.max_x() - size.w),
        clamp(origin.y, bounds.min_y(), bounds.max_y() - size.h),
    )
}

/// Where a window of `size` goes when it is shown again: the saved fraction applied to the
/// *current* rect of the same display, or to `fallback` when that display is gone; then
/// clamped so the whole window is on that rect.
pub fn resolve_restore<K: PartialEq>(
    saved: &SavedPosition<K>,
    size: Size,
    displays: &[(K, Rect)],
    fallback: Rect,
) -> Point {
    let rect = displays
        .iter()
        .find(|(key, _)| *key == saved.display)
        .map_or(fallback, |&(_, rect)| rect);
    clamp_on_screen(restore_proportional(saved.frac, rect), size, rect)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SZ: Size = Size::new(400.0, 300.0);

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect { Rect::new(x, y, w, h) }

    fn main_display() -> Rect { r(0.0, 0.0, 1920.0, 1080.0) }

    #[test]
    fn park_origin_leaves_one_pixel_of_the_window_on_the_display() {
        let d = main_display();
        let bl = park_origin(d, Corner::BottomLeft, SZ);
        assert_eq!(bl, Point::new(1.0 - 400.0, 1079.0));
        let br = park_origin(d, Corner::BottomRight, SZ);
        assert_eq!(br, Point::new(1919.0, 1079.0));
        // The sliver: the window's rect overlaps the display by exactly one pixel.
        for origin in [bl, br] {
            let parked = Rect { origin, size: SZ };
            assert_eq!(parked.intersection_area(&d), 1.0, "{origin:?}");
        }
    }

    #[test]
    fn park_origin_scales_with_the_window_size_and_display_origin() {
        let d = r(-1280.0, 200.0, 1280.0, 1024.0);
        let a = park_origin(d, Corner::BottomLeft, Size::new(100.0, 50.0));
        let b = park_origin(d, Corner::BottomLeft, Size::new(300.0, 50.0));
        assert_eq!(a.x - b.x, 200.0);
        assert_eq!(a, Point::new(-1280.0 + 1.0 - 100.0, 200.0 + 1024.0 - 1.0));
        assert_eq!(park_origin(d, Corner::BottomRight, SZ).x, -1.0);
    }

    #[test]
    fn single_display_parks_bottom_left() {
        let displays = [main_display()];
        assert_eq!(corner_scores(&displays, 0), (0, 0));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomLeft);
    }

    #[test]
    fn two_side_by_side_displays_park_on_the_outer_corners() {
        let displays = [r(0.0, 0.0, 1000.0, 800.0), r(1000.0, 0.0, 1000.0, 800.0)];
        // Left display: its right corner touches the neighbour, its left does not.
        assert_eq!(corner_scores(&displays, 0), (0, 1));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomLeft);
        assert_eq!(corner_scores(&displays, 1), (1, 0));
        assert_eq!(choose_corner(&displays, 1), Corner::BottomRight);
    }

    #[test]
    fn two_stacked_displays_tie_on_the_upper_one_and_are_free_on_the_lower_one() {
        let displays = [r(0.0, 0.0, 1000.0, 800.0), r(0.0, 800.0, 1000.0, 800.0)];
        // Both corners of the upper display sit on the lower one.
        assert_eq!(corner_scores(&displays, 0), (1, 1));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomLeft);
        assert_eq!(corner_scores(&displays, 1), (0, 0));
        assert_eq!(choose_corner(&displays, 1), Corner::BottomLeft);
    }

    #[test]
    fn a_diagonal_neighbour_weighs_ten_times_a_direct_one() {
        // Lower-left display diagonal to the upper-right one's bottom-left corner.
        let displays = [r(1000.0, 0.0, 1000.0, 800.0), r(0.0, 800.0, 1000.0, 800.0)];
        assert_eq!(corner_scores(&displays, 0), (DIAGONAL_WEIGHT, 0));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomRight);

        // Two direct neighbours (right and below the right corner) still beat one diagonal.
        let displays = [
            r(1000.0, 0.0, 1000.0, 800.0),
            r(2000.0, 0.0, 1000.0, 800.0),
            r(1000.0, 800.0, 1000.0, 800.0),
            r(0.0, 800.0, 1000.0, 800.0),
        ];
        let (bl, br) = corner_scores(&displays, 0);
        assert_eq!((bl, br), (DIAGONAL_WEIGHT + 1, 2));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomRight);
    }

    #[test]
    fn three_displays_in_a_row_choose_per_display() {
        let displays = [
            r(0.0, 0.0, 1000.0, 800.0),
            r(1000.0, 0.0, 1000.0, 800.0),
            r(2000.0, 0.0, 1000.0, 800.0),
        ];
        assert_eq!(choose_corner(&displays, 0), Corner::BottomLeft);
        // The middle display has a neighbour on each side: tie, bottom-left.
        assert_eq!(corner_scores(&displays, 1), (1, 1));
        assert_eq!(choose_corner(&displays, 1), Corner::BottomLeft);
        assert_eq!(choose_corner(&displays, 2), Corner::BottomRight);
    }

    #[test]
    fn three_displays_in_an_l_shape() {
        // A big display with a small one to its right and one below its right half.
        let displays = [
            r(0.0, 0.0, 1000.0, 800.0),
            r(1000.0, 0.0, 600.0, 800.0),
            r(0.0, 800.0, 1000.0, 600.0),
        ];
        // Display 0: bottom-left probes hit display 2 below it (1); bottom-right probes
        // hit display 1 (right, 1) and display 2 (below, 1).
        assert_eq!(corner_scores(&displays, 0), (1, 2));
        assert_eq!(choose_corner(&displays, 0), Corner::BottomLeft);
        // Display 1: left probe hits display 0; below is empty; diagonal below-left is
        // display 2's x range? (999, 801) is inside display 2? No: display 2 ends at x=1000.
        assert_eq!(corner_scores(&displays, 1), (1 + DIAGONAL_WEIGHT, 0));
        assert_eq!(choose_corner(&displays, 1), Corner::BottomRight);
        assert_eq!(choose_corner(&displays, 2), Corner::BottomLeft);
    }

    #[test]
    fn an_unknown_display_index_is_not_a_panic() {
        assert_eq!(choose_corner(&[], 0), Corner::BottomLeft);
        assert_eq!(choose_corner(&[main_display()], 5), Corner::BottomLeft);
    }

    fn saved(display: &'static str, frac_x: f64, frac_y: f64) -> SavedPosition<&'static str> {
        SavedPosition { display, frac: Proportional { fx: frac_x, fy: frac_y } }
    }

    #[test]
    fn save_then_restore_on_the_same_rect_is_the_identity() {
        let rect = r(-1000.0, 100.0, 1000.0, 500.0);
        let origin = Point::new(-750.0, 150.0);
        let s = save_position("a", rect, origin);
        assert_eq!(s.frac, Proportional { fx: 0.25, fy: 0.1 });
        assert_eq!(resolve_restore(&s, Size::new(100.0, 100.0), &[("a", rect)], rect), origin);
    }

    #[test]
    fn restore_survives_a_resolution_change_of_the_same_display() {
        let before = r(0.0, 0.0, 1920.0, 1080.0);
        let after = r(0.0, 0.0, 2560.0, 1440.0);
        let s = save_position("a", before, Point::new(480.0, 270.0));
        let at = resolve_restore(&s, Size::new(400.0, 300.0), &[("a", after)], before);
        assert_eq!(at, Point::new(640.0, 360.0));
    }

    #[test]
    fn restore_follows_a_display_that_moved_in_the_arrangement() {
        let before = r(0.0, 0.0, 1000.0, 800.0);
        let after = r(1920.0, -200.0, 1000.0, 800.0);
        let s = save_position("a", before, Point::new(100.0, 80.0));
        let at = resolve_restore(&s, Size::new(100.0, 100.0), &[("a", after)], before);
        assert_eq!(at, Point::new(2020.0, -120.0));
    }

    #[test]
    fn restore_falls_back_when_the_display_is_disconnected() {
        let gone = r(2000.0, 0.0, 1000.0, 1000.0);
        let fallback = r(0.0, 0.0, 800.0, 600.0);
        let s = save_position("gone", gone, Point::new(2500.0, 500.0));
        let at = resolve_restore(&s, Size::new(100.0, 100.0), &[("other", fallback)], fallback);
        assert_eq!(at, Point::new(400.0, 300.0));
        // No displays at all: the fallback still decides.
        assert_eq!(resolve_restore(&s, Size::new(100.0, 100.0), &[], fallback), at);
    }

    #[test]
    fn restore_clamps_so_the_whole_window_is_on_screen() {
        let rect = r(100.0, 50.0, 1000.0, 600.0);
        let size = Size::new(300.0, 200.0);
        // Saved near the bottom-right: the window would hang off, so it is pulled back.
        let at = resolve_restore(&saved("a", 0.95, 0.95), size, &[("a", rect)], rect);
        assert_eq!(at, Point::new(100.0 + 1000.0 - 300.0, 50.0 + 600.0 - 200.0));
        // Saved off the top-left (negative fraction).
        let at = resolve_restore(&saved("a", -0.5, -0.5), size, &[("a", rect)], rect);
        assert_eq!(at, Point::new(100.0, 50.0));
    }

    #[test]
    fn clamp_pins_a_window_larger_than_the_display_to_its_top_left() {
        let bounds = r(0.0, 0.0, 500.0, 400.0);
        let at = clamp_on_screen(Point::new(300.0, 300.0), Size::new(900.0, 700.0), bounds);
        assert_eq!(at, Point::new(0.0, 0.0));
    }

    #[test]
    fn a_zero_sized_display_never_produces_nan() {
        let zero = r(10.0, 10.0, 0.0, 0.0);
        let s = save_position("a", zero, Point::new(5.0, 5.0));
        let at = resolve_restore(&s, SZ, &[("a", zero)], zero);
        assert!(at.x.is_finite() && at.y.is_finite());
    }

    #[test]
    fn a_parked_window_restored_is_on_screen_even_though_it_was_parked_off_it() {
        // The saved fraction comes from before parking, never from the parked position.
        let d = main_display();
        let window = Rect::new(300.0, 200.0, 400.0, 300.0);
        let s = save_position("a", d, window.origin);
        let parked = park_origin(d, Corner::BottomLeft, window.size);
        assert!(parked.x < 0.0);
        let back = resolve_restore(&s, window.size, &[("a", d)], d);
        assert_eq!(back, window.origin);
    }
}
