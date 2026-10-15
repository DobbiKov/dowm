//! `tiles`: children share the container along its orientation in proportion to
//! their weights and fill the other axis. After AeroSpace's `tiles` layout.

use crate::geometry::Rect;
use crate::layout::{Arrange, Child, Hints, LayoutSystem};
use crate::model::tree::{LayoutKind, Orientation};

pub const TILES: LayoutKind = LayoutKind::new("tiles");

#[derive(Clone, Copy, Debug, Default)]
pub struct Tiles;

impl LayoutSystem for Tiles {
    fn kind(&self) -> LayoutKind { TILES }

    fn hints(&self) -> Hints { Hints { in_tree: true, overlapping: false, weighted: true, alternates: true, flattens: true } }

    fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> {
        let n = input.children.len();
        let extent = along(input.area.size.w, input.area.size.h, input.orientation);
        let start = along(input.area.origin.x, input.area.origin.y, input.orientation);
        let weights = usable_weights(input.children);
        let total: f64 = weights.iter().sum();
        // Shared boundaries, so neighbours meet exactly and the last one ends at the edge.
        let mut edges = Vec::with_capacity(n + 1);
        let mut cumulative = 0.0;
        edges.push(start);
        for (i, weight) in weights.iter().enumerate() {
            cumulative += weight;
            edges.push(if i + 1 == n { start + extent } else { start + extent * cumulative / total });
        }
        edges
            .windows(2)
            .map(|pair| {
                let (from, to) = (pair[0], pair[1]);
                match input.orientation {
                    Orientation::Horizontal => {
                        Rect::new(from, input.area.origin.y, to - from, input.area.size.h)
                    }
                    Orientation::Vertical => {
                        Rect::new(input.area.origin.x, from, input.area.size.w, to - from)
                    }
                }
            })
            .collect()
    }
}

/// `horizontal` or `vertical` component of a pair, by orientation.
fn along(x: f64, y: f64, orientation: Orientation) -> f64 {
    match orientation {
        Orientation::Horizontal => x,
        Orientation::Vertical => y,
    }
}

/// The weights, or all ones when any is unusable (so a bad weight cannot blank a window).
fn usable_weights(children: &[Child]) -> Vec<f64> {
    if children.iter().all(|c| c.weight.is_finite() && c.weight > 0.0) {
        children.iter().map(|c| c.weight).collect()
    } else {
        vec![1.0; children.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Nav;
    use crate::model::workspace::Direction;

    fn arrange(orientation: Orientation, area: Rect, weights: &[f64]) -> Vec<Rect> {
        let children: Vec<Child> = weights.iter().map(|&weight| Child { weight, frame: None }).collect();
        Tiles.arrange(&Arrange { area, orientation, children: &children, focused: 0, state: None })
    }

    #[test]
    fn equal_weights_split_the_extent_evenly() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 300., 100.), &[1., 1., 1.]);
        assert_eq!(out, vec![
            Rect::new(0., 0., 100., 100.),
            Rect::new(100., 0., 100., 100.),
            Rect::new(200., 0., 100., 100.),
        ]);
    }

    #[test]
    fn weights_are_proportions() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 400., 50.), &[1., 3.]);
        assert_eq!(out, vec![Rect::new(0., 0., 100., 50.), Rect::new(100., 0., 300., 50.)]);
    }

    #[test]
    fn vertical_splits_along_y_and_keeps_the_origin() {
        let out = arrange(Orientation::Vertical, Rect::new(10., 20., 100., 200.), &[1., 1.]);
        assert_eq!(out, vec![Rect::new(10., 20., 100., 100.), Rect::new(10., 120., 100., 100.)]);
    }

    #[test]
    fn children_tile_the_area_without_gaps_or_overlap() {
        let area = Rect::new(5., 7., 1000., 333.);
        let out = arrange(Orientation::Horizontal, area, &[1., 1., 1.]);
        let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        near(out[0].min_x(), area.min_x());
        near(out[2].max_x(), area.max_x());
        near(out[0].max_x(), out[1].min_x());
        near(out[1].max_x(), out[2].min_x());
    }

    #[test]
    fn one_child_gets_everything_and_no_children_get_nothing() {
        let area = Rect::new(0., 0., 80., 60.);
        assert_eq!(arrange(Orientation::Horizontal, area, &[2.]), vec![area]);
        assert!(arrange(Orientation::Horizontal, area, &[]).is_empty());
    }

    #[test]
    fn degenerate_weights_fall_back_to_an_even_split() {
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 100., 10.), &[0., 0.]);
        assert_eq!(out[0].size.w, 50.);
        let out = arrange(Orientation::Horizontal, Rect::new(0., 0., 100., 10.), &[f64::NAN, 1.]);
        assert_eq!(out[0].size.w, 50.);
    }

    #[test]
    fn it_declares_itself_a_weighted_non_overlapping_tree_system() {
        let hints = Tiles.hints();
        assert!(hints.in_tree && hints.weighted && !hints.overlapping);
        assert!(hints.alternates && hints.flattens);
        assert_eq!(Tiles.kind(), TILES);
    }

    #[test]
    fn stepping_moves_one_child_and_stops_at_the_ends() {
        let nav = |from, direction, orientation| Nav { len: 3, from, direction, orientation, state: None };
        let h = Orientation::Horizontal;
        assert_eq!(Tiles.step(&nav(1, Direction::Right, h)), Some(2));
        assert_eq!(Tiles.step(&nav(1, Direction::Left, h)), Some(0));
        assert_eq!(Tiles.step(&nav(2, Direction::Right, h)), None);
        assert_eq!(Tiles.step(&nav(0, Direction::Left, h)), None);
        assert_eq!(Tiles.step(&nav(1, Direction::Down, h)), None, "across the axis there is no neighbour");
        assert_eq!(Tiles.step(&nav(1, Direction::Down, Orientation::Vertical)), Some(2));
    }

    #[test]
    fn leaving_a_container_lands_beside_it_and_wrapping_goes_to_the_far_end() {
        let nav = |direction| Nav { len: 3, from: 1, direction, orientation: Orientation::Horizontal, state: None };
        assert_eq!(Tiles.place_beside(&nav(Direction::Right)), Some(2));
        assert_eq!(Tiles.place_beside(&nav(Direction::Left)), Some(1));
        assert_eq!(Tiles.place_beside(&nav(Direction::Up)), None);
        assert_eq!(Tiles.wrap_target(&nav(Direction::Right)), Some(0));
        assert_eq!(Tiles.wrap_target(&nav(Direction::Left)), Some(2));
        assert_eq!(Tiles.wrap_target(&nav(Direction::Down)), None);
    }
}
