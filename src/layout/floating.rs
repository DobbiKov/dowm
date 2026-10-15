//! `floating`: windows keep the frames they have. It is not a tree layout: no container
//! uses it; the workspace's floating set is arranged by it. A window with no known frame
//! is given the whole area.

use crate::geometry::Rect;
use crate::layout::{Arrange, Hints, LayoutSystem, Nav};
use crate::model::tree::LayoutKind;

pub const FLOATING: LayoutKind = LayoutKind::new("floating");

#[derive(Clone, Copy, Debug, Default)]
pub struct Floating;

impl LayoutSystem for Floating {
    fn kind(&self) -> LayoutKind { FLOATING }

    fn hints(&self) -> Hints { Hints { in_tree: false, overlapping: true, weighted: false, alternates: false, flattens: false } }

    fn arrange(&self, input: &Arrange<'_>) -> Vec<Rect> {
        input.children.iter().map(|c| c.frame.unwrap_or(input.area)).collect()
    }

    fn step(&self, _nav: &Nav<'_>) -> Option<usize> { None }

    fn place_beside(&self, _nav: &Nav<'_>) -> Option<usize> { None }

    fn wrap_target(&self, _nav: &Nav<'_>) -> Option<usize> { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Child;
    use crate::model::workspace::Direction;
    use crate::model::tree::Orientation;

    #[test]
    fn windows_keep_their_own_frames() {
        let (a, b) = (Rect::new(5., 6., 70., 80.), Rect::new(100., 100., 30., 30.));
        let children = [Child { weight: 1., frame: Some(a) }, Child { weight: 1., frame: Some(b) }];
        let input = Arrange {
            area: Rect::new(0., 0., 1000., 1000.),
            orientation: Orientation::Horizontal,
            children: &children,
            focused: 0,
            state: None,
        };
        assert_eq!(Floating.arrange(&input), vec![a, b]);
    }

    #[test]
    fn a_window_without_a_frame_gets_the_area() {
        let area = Rect::new(0., 0., 1000., 1000.);
        let children = [Child { weight: 1., frame: None }];
        let input = Arrange { area, orientation: Orientation::Vertical, children: &children, focused: 0, state: None };
        assert_eq!(Floating.arrange(&input), vec![area]);
    }

    #[test]
    fn it_is_not_a_tree_system_and_has_no_neighbours() {
        let hints = Floating.hints();
        assert!(!hints.in_tree && !hints.weighted);
        assert_eq!(Floating.kind(), FLOATING);
        for direction in [Direction::Left, Direction::Right, Direction::Up, Direction::Down] {
            let nav = Nav { len: 3, from: 1, direction, orientation: Orientation::Horizontal, state: None };
            assert_eq!((Floating.step(&nav), Floating.place_beside(&nav), Floating.wrap_target(&nav)), (None, None, None));
        }
    }
}
