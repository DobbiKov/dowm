//! Gaps: space between tiled windows (`inner`) and between them and the monitor's
//! edges (`outer`), as in AeroSpace's `[gaps]`.
//!
//! The layout systems know nothing about gaps. [`compute_with_gaps`] lays the tree out
//! in the monitor area shrunk by the outer gaps and then grown by half the inner gap on
//! every side; each tiled window is then inset by half the inner gap. A window against
//! an edge ends exactly at the outer-gap boundary, and two neighbours end up exactly one
//! inner gap apart, whatever the nesting. Floating and fullscreen windows ignore gaps.

use std::collections::BTreeSet;

use crate::actor::app::WindowId;
use crate::geometry::Rect;
use crate::layout::{ComputedLayout, LayoutRegistry, compute};
use crate::model::tree::WorkspaceTree;

/// Gap sizes in pixels for one monitor.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GapSizes {
    /// Between horizontally adjacent windows.
    pub inner_horizontal: f64,
    /// Between vertically adjacent windows.
    pub inner_vertical: f64,
    pub outer_left: f64,
    pub outer_right: f64,
    pub outer_top: f64,
    pub outer_bottom: f64,
}

impl GapSizes {
    pub fn is_zero(&self) -> bool { *self == GapSizes::default() }

    /// `frame` shrunk by the outer gaps. Whatever the gaps, the result is at least a pixel
    /// in each direction (when `frame` is) and stays inside `frame`: a gap is clamped to
    /// what is left of the monitor after the gap before it.
    pub fn outer_area(&self, frame: Rect) -> Rect {
        let room = |total: f64| (total - 1.0).max(0.0);
        let left = self.outer_left.min(room(frame.size.w));
        let right = self.outer_right.min(room(frame.size.w) - left);
        let top = self.outer_top.min(room(frame.size.h));
        let bottom = self.outer_bottom.min(room(frame.size.h) - top);
        Rect::new(
            frame.origin.x + left,
            frame.origin.y + top,
            frame.size.w - left - right,
            frame.size.h - top - bottom,
        )
    }
}

/// `compute`, with `gaps` applied to the tiled windows of `tree` laid out in `frame`.
pub fn compute_with_gaps(
    tree: &WorkspaceTree,
    frame: Rect,
    gaps: &GapSizes,
    registry: &LayoutRegistry,
    frames: &dyn Fn(WindowId) -> Option<Rect>,
) -> ComputedLayout {
    if gaps.is_zero() {
        return compute(tree, frame, registry, frames);
    }
    let (half_h, half_v) = (gaps.inner_horizontal / 2.0, gaps.inner_vertical / 2.0);
    let outer = gaps.outer_area(frame);
    let area = Rect::new(
        outer.origin.x - half_h,
        outer.origin.y - half_v,
        outer.size.w + 2.0 * half_h,
        outer.size.h + 2.0 * half_v,
    );
    let mut layout = compute(tree, area, registry, frames);
    let tiled: BTreeSet<WindowId> = tree.tiled_windows().into_iter().collect();
    for (window, rect) in layout.windows.iter_mut() {
        if tiled.contains(window) && tree.fullscreen() != Some(*window) {
            *rect = inset(*rect, half_h, half_v);
        }
    }
    if let Some(window) = tree.fullscreen() {
        layout.windows.insert(window, frame);
    }
    layout
}

/// `rect` shrunk by `dx` on the left and right and `dy` on top and bottom, keeping at
/// least a pixel of size.
fn inset(rect: Rect, dx: f64, dy: f64) -> Rect {
    let width = (rect.size.w - 2.0 * dx).max(1.0);
    let height = (rect.size.h - 2.0 * dy).max(1.0);
    // A rect already smaller than the gap keeps its centre instead of drifting.
    let x = rect.origin.x + (rect.size.w - width) / 2.0;
    let y = rect.origin.y + (rect.size.h - height) / 2.0;
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::TILES;
    use crate::model::tree::Orientation;

    fn w(n: u32) -> WindowId { WindowId::new(1, n) }

    const FRAME: Rect = Rect::new(0., 0., 900., 600.);

    fn no_frames(_: WindowId) -> Option<Rect> { None }

    fn tree(spec: &str) -> WorkspaceTree { WorkspaceTree::from_spec(spec, TILES) }

    fn laid(spec: &str, gaps: GapSizes) -> ComputedLayout {
        compute_with_gaps(&tree(spec), FRAME, &gaps, &LayoutRegistry::with_builtin(), &no_frames)
    }

    fn all(inner_h: f64, inner_v: f64, l: f64, r: f64, t: f64, b: f64) -> GapSizes {
        GapSizes {
            inner_horizontal: inner_h,
            inner_vertical: inner_v,
            outer_left: l,
            outer_right: r,
            outer_top: t,
            outer_bottom: b,
        }
    }

    #[test]
    fn zero_gaps_are_plain_compute() {
        let reg = LayoutRegistry::with_builtin();
        let t = tree("h[1 2 3]");
        assert_eq!(
            compute_with_gaps(&t, FRAME, &GapSizes::default(), &reg, &no_frames),
            compute(&t, FRAME, &reg, &no_frames)
        );
    }

    #[test]
    fn outer_gaps_shrink_the_area_on_each_side() {
        let out = laid("h[1]", all(0., 0., 10., 20., 30., 40.));
        assert_eq!(out.windows[&w(1)], Rect::new(10., 30., 870., 530.));
    }

    #[test]
    fn inner_gaps_separate_neighbours_by_exactly_the_gap() {
        let out = laid("h[1 2 3]", all(30., 0., 0., 0., 0., 0.));
        let (a, b, c) = (out.windows[&w(1)], out.windows[&w(2)], out.windows[&w(3)]);
        assert_eq!(b.min_x() - a.max_x(), 30.);
        assert_eq!(c.min_x() - b.max_x(), 30.);
        assert_eq!(a.min_x(), 0., "no gap against the monitor edge from inner gaps");
        assert_eq!(c.max_x(), 900.);
        assert_eq!((a.size.h, a.origin.y), (600., 0.), "horizontal gaps leave heights alone");
    }

    #[test]
    fn vertical_inner_gaps_apply_between_stacked_windows() {
        let out = laid("v[1 2]", all(0., 20., 0., 0., 0., 0.));
        let (a, b) = (out.windows[&w(1)], out.windows[&w(2)]);
        assert_eq!(b.min_y() - a.max_y(), 20.);
        assert_eq!((a.min_y(), b.max_y()), (0., 600.));
        assert_eq!(a.size.w, 900.);
    }

    #[test]
    fn inner_and_outer_gaps_compose_and_nested_containers_keep_the_gap() {
        let g = all(10., 10., 5., 5., 5., 5.);
        let out = laid("h[1 v[2 3]]", g);
        let (a, b, c) = (out.windows[&w(1)], out.windows[&w(2)], out.windows[&w(3)]);
        assert_eq!((a.min_x(), a.min_y(), a.max_y()), (5., 5., 595.));
        assert_eq!(b.min_x() - a.max_x(), 10., "between the column and the stack");
        assert_eq!(c.min_y() - b.max_y(), 10., "inside the stack");
        assert_eq!((b.max_x(), c.max_x(), c.max_y()), (895., 895., 595.));
    }

    #[test]
    fn floating_windows_ignore_gaps() {
        let reg = LayoutRegistry::with_builtin();
        let mut t = tree("h[1 2]");
        t.float(w(2));
        let own = Rect::new(100., 100., 200., 200.);
        let frames = move |win: WindowId| (win == w(2)).then_some(own);
        let out = compute_with_gaps(&t, FRAME, &all(20., 20., 20., 20., 20., 20.), &reg, &frames);
        assert_eq!(out.windows[&w(2)], own);
        assert_eq!(out.windows[&w(1)], Rect::new(20., 20., 860., 560.));
    }

    #[test]
    fn a_fullscreen_window_fills_the_whole_monitor() {
        let reg = LayoutRegistry::with_builtin();
        let mut t = tree("h[1 2]");
        t.toggle_fullscreen(w(1));
        let out = compute_with_gaps(&t, FRAME, &all(20., 20., 20., 20., 20., 20.), &reg, &no_frames);
        assert_eq!(out.windows[&w(1)], FRAME);
    }

    #[test]
    fn gaps_larger_than_the_screen_degrade_to_one_pixel_not_negative_sizes() {
        let out = laid("h[1 2 3]", all(2000., 2000., 2000., 2000., 2000., 2000.));
        for rect in out.windows.values() {
            assert!(rect.size.w >= 1. && rect.size.h >= 1., "{rect:?}");
            assert!(rect.origin.x.is_finite() && rect.origin.y.is_finite());
        }
    }

    #[test]
    fn root_orientation_does_not_matter_to_the_helper() {
        let reg = LayoutRegistry::with_builtin();
        let mut t = WorkspaceTree::new(Orientation::Vertical, TILES);
        t.insert_tiled(w(1));
        let out = compute_with_gaps(&t, FRAME, &all(10., 10., 0., 0., 0., 0.), &reg, &no_frames);
        assert_eq!(out.windows[&w(1)], FRAME, "a lone window has no neighbour to leave a gap to");
    }

    #[test]
    fn outer_area_never_collapses_and_stays_inside_the_frame() {
        for frame in [FRAME, Rect::new(-1920., 100., 40., 30.), Rect::new(5., 5., 1., 1.)] {
            for g in [
                all(0., 0., 600., 600., 0., 0.),
                all(0., 0., 1e9, 1e9, 1e9, 1e9),
                all(0., 0., 0., 0., 2000., 2000.),
                all(0., 0., 30., 5., 7., 100.),
            ] {
                let area = g.outer_area(frame);
                assert!(area.size.w >= 1. && area.size.h >= 1., "{area:?} from {frame:?}");
                assert!(area.min_x() >= frame.min_x() && area.max_x() <= frame.max_x(), "{area:?} in {frame:?}");
                assert!(area.min_y() >= frame.min_y() && area.max_y() <= frame.max_y(), "{area:?} in {frame:?}");
            }
        }
    }

    #[test]
    fn modest_outer_gaps_are_not_clamped() {
        assert_eq!(all(0., 0., 10., 20., 30., 40.).outer_area(FRAME), Rect::new(10., 30., 870., 530.));
    }
}
