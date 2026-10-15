//! `accordion`: children overlap, each shifted by a padding so a strip of every
//! neighbour peeks out. After AeroSpace's `accordion` layout. Weights are ignored.
//!
//! Geometry does not depend on which child is focused: the focused one is simply the
//! window drawn on top, which `layout::compute` reports as a `raise` hint. (I could not
//! confirm from memory whether AeroSpace's accordion shifts rects with its most recent
//! child as well; the visible result, strips of the others peeking out on both sides of
//! the focused one, is the same.)

use crate::geometry::Rect;
use crate::layout::{Arrange, Hints, LayoutSystem};
use crate::model::tree::{LayoutKind, Orientation};

pub const ACCORDION: LayoutKind = LayoutKind::new("accordion");

/// AeroSpace's default `accordion-padding`.
pub const DEFAULT_PADDING: f64 = 30.0;

#[derive(Clone, Copy, Debug)]
pub struct Accordion {
    padding: f64,
}

impl Accordion {
    pub fn new(padding: f64) -> Self { Accordion { padding: padding.max(0.0) } }
}

impl Default for Accordion {
    fn default() -> Self { Accordion::new(DEFAULT_PADDING) }
}

impl LayoutSystem for Accordion {
    fn kind(&self) -> LayoutKind { ACCORDION }

    fn hints(&self) -> Hints { Hints { in_tree: true, overlapping: true, weighted: false, alternates: true, flattens: true } }

    /// Child `i` starts `i * padding` into the area and all children share one size, so
    /// child 0 sticks out `padding` before the next, and the last ends at the area's edge.
    /// Which child is on top is the presenter's business (see `compute`'s `raise`), not
    /// geometry. The padding shrinks when the area is too small for the main rect to keep
    /// half the extent.
    fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> {
        let n = input.children.len();
        let (start, extent) = match input.orientation {
            Orientation::Horizontal => (input.area.origin.x, input.area.size.w),
            Orientation::Vertical => (input.area.origin.y, input.area.size.h),
        };
        let padding = if n > 1 { self.padding.min(extent / (2.0 * (n - 1) as f64)) } else { 0.0 };
        let size = extent - padding * n.saturating_sub(1) as f64;
        (0..n)
            .map(|i| {
                let at = start + padding * i as f64;
                match input.orientation {
                    Orientation::Horizontal => Rect::new(at, input.area.origin.y, size, input.area.size.h),
                    Orientation::Vertical => Rect::new(input.area.origin.x, at, input.area.size.w, size),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Child;

    fn arrange(orientation: Orientation, area: Rect, n: usize, focused: usize, padding: f64) -> Vec<Rect> {
        let children: Vec<Child> = (0..n).map(|_| Child { weight: 1., frame: None }).collect();
        Accordion::new(padding).arrange(&Arrange { area, orientation, children: &children, focused, state: None })
    }

    #[test]
    fn one_child_fills_the_area() {
        let area = Rect::new(0., 0., 400., 100.);
        assert_eq!(arrange(Orientation::Horizontal, area, 1, 0, 30.), vec![area]);
    }

    #[test]
    fn children_overlap_and_each_peeks_out_by_the_padding() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 400., 100.), 3, 1, 30.);
        assert_eq!(out, vec![
            Rect::new(0., 0., 340., 100.),
            Rect::new(30., 0., 340., 100.),
            Rect::new(60., 0., 340., 100.),
        ]);
    }

    #[test]
    fn the_peeks_are_fixed_by_position_and_do_not_depend_on_which_child_is_focused() {
        // The geometry is a cascade: child i starts i * padding in. Focus only decides
        // which child is drawn on top (the `raise` hint of `compute`), so every focus
        // gives the same rects. Pinned here so changing it is a decision.
        let area = Rect::new(0., 0., 400., 100.);
        let first = arrange(Orientation::Horizontal, area, 3, 0, 30.);
        for focused in 1..3 {
            assert_eq!(arrange(Orientation::Horizontal, area, 3, focused, 30.), first);
        }
        for (i, r) in first.iter().enumerate() {
            assert_eq!(r.min_x(), i as f64 * 30., "child {i} peeks by one padding per predecessor");
        }
    }

    #[test]
    fn vertical_accordions_overlap_along_y() {
        let out = arrange(Orientation::Vertical, Rect::new(10., 20., 200., 400.), 2, 0, 50.);
        assert_eq!(out, vec![Rect::new(10., 20., 200., 350.), Rect::new(10., 70., 200., 350.)]);
    }

    #[test]
    fn the_whole_accordion_stays_inside_the_area() {
        let area = Rect::new(0., 0., 400., 100.);
        for out in [arrange(Orientation::Horizontal, area, 5, 2, 30.), arrange(Orientation::Horizontal, area, 5, 0, 30.)] {
            assert_eq!(out.iter().map(Rect::min_x).fold(f64::MAX, f64::min), 0.);
            assert_eq!(out.iter().map(Rect::max_x).fold(f64::MIN, f64::max), 400.);
        }
    }

    #[test]
    fn padding_shrinks_so_the_main_rect_keeps_at_least_half_the_extent() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 40., 10.), 3, 1, 30.);
        assert!(out.iter().all(|r| r.size.w >= 20.), "{out:?}");
        assert_eq!(out[2].max_x(), 40.);
    }

    #[test]
    fn an_out_of_range_focus_is_clamped() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 100., 10.), 2, 9, 10.);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn it_overlaps_and_is_not_weighted() {
        let hints = Accordion::new(30.).hints();
        assert!(hints.in_tree && hints.overlapping && !hints.weighted);
        assert_eq!(Accordion::new(30.).kind(), ACCORDION);
    }
}
