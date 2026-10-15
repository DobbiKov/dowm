//! `move-mouse` geometry: where (if anywhere) to put the cursor.

use objc2_core_foundation::{CGPoint, CGRect};

/// The point to warp the cursor to for `move-mouse` aimed at `rect`: its center. With
/// `lazy`, none if the cursor is already inside `rect` (so a click-focused window does
/// not make the pointer jump). An unknown cursor position counts as outside.
pub fn warp_target(rect: CGRect, cursor: Option<CGPoint>, lazy: bool) -> Option<CGPoint> {
    if lazy && cursor.is_some_and(|p| contains(rect, p)) {
        return None;
    }
    Some(CGPoint::new(rect.origin.x + rect.size.width / 2.0, rect.origin.y + rect.size.height / 2.0))
}

/// Half-open: the top and left edges are inside, the bottom and right are not.
fn contains(rect: CGRect, p: CGPoint) -> bool {
    p.x >= rect.origin.x
        && p.x < rect.origin.x + rect.size.width
        && p.y >= rect.origin.y
        && p.y < rect.origin.y + rect.size.height
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::CGSize;

    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect { CGRect::new(CGPoint::new(x, y), CGSize::new(w, h)) }

    fn at(x: f64, y: f64) -> Option<CGPoint> { Some(CGPoint::new(x, y)) }

    #[test]
    fn lazy_does_nothing_when_the_cursor_is_already_inside() {
        assert_eq!(warp_target(rect(100., 100., 200., 100.), at(150., 120.), true), None);
        assert_eq!(warp_target(rect(100., 100., 200., 100.), at(100., 100.), true), None, "top-left edge is inside");
    }

    #[test]
    fn lazy_moves_to_the_center_when_the_cursor_is_outside() {
        let target = warp_target(rect(100., 100., 200., 100.), at(0., 0.), true).unwrap();
        assert_eq!((target.x, target.y), (200., 150.));
        assert!(warp_target(rect(100., 100., 200., 100.), at(300., 150.), true).is_some(), "right edge is outside");
        assert!(warp_target(rect(100., 100., 200., 100.), at(150., 200.), true).is_some(), "bottom edge is outside");
        assert!(warp_target(rect(100., 100., 200., 100.), at(99.9, 150.), true).is_some());
    }

    #[test]
    fn force_always_moves_to_the_center() {
        let target = warp_target(rect(0., 0., 100., 100.), at(10., 10.), false).unwrap();
        assert_eq!((target.x, target.y), (50., 50.));
    }

    #[test]
    fn an_unknown_cursor_position_counts_as_outside() {
        assert!(warp_target(rect(0., 0., 100., 100.), None, true).is_some());
    }

    #[test]
    fn rects_on_other_displays_use_global_coordinates() {
        let target = warp_target(rect(-1920., 0., 1920., 1080.), at(10., 10.), true).unwrap();
        assert_eq!((target.x, target.y), (-960., 540.));
    }
}
