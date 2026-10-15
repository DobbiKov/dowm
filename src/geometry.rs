//! Pure geometry: Cocoa (bottom-left origin) <-> AX (top-left origin) conversion,
//! monitor selection and AeroSpace-style corner parking.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

impl Size {
    pub const fn new(w: f64, h: f64) -> Self {
        Self { w, h }
    }
}

impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { origin: Point::new(x, y), size: Size::new(w, h) }
    }
    pub fn min_x(&self) -> f64 {
        self.origin.x
    }
    pub fn max_x(&self) -> f64 {
        self.origin.x + self.size.w
    }
    pub fn min_y(&self) -> f64 {
        self.origin.y
    }
    pub fn max_y(&self) -> f64 {
        self.origin.y + self.size.h
    }
    pub fn center(&self) -> Point {
        Point::new(self.origin.x + self.size.w / 2.0, self.origin.y + self.size.h / 2.0)
    }
    pub fn intersection_area(&self, other: &Rect) -> f64 {
        let w = self.max_x().min(other.max_x()) - self.min_x().max(other.min_x());
        let h = self.max_y().min(other.max_y()) - self.min_y().max(other.min_y());
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }
}

/// A display, with rects in **Cocoa** global coordinates (origin bottom-left of the
/// primary display, y up) exactly as `NSScreen.frame` / `visibleFrame` report them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub id: u32,
    pub frame: Rect,
    pub visible: Rect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    BottomLeft,
    BottomRight,
}

/// Flip a rect between Cocoa and AX global coordinates. The flip is an involution:
/// `y' = primary_height - (y + h)`. Height of the *primary* display is the pivot.
pub fn flip_rect(r: Rect, primary_height: f64) -> Rect {
    Rect::new(r.origin.x, primary_height - (r.origin.y + r.size.h), r.size.w, r.size.h)
}

/// Index of the monitor a window (AX coords) lives on: greatest overlap with the
/// monitor's frame, else nearest center. `None` only for an empty monitor list.
pub fn monitor_for_window(monitors: &[Monitor], window_ax: Rect, primary_height: f64) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, m) in monitors.iter().enumerate() {
        let area = flip_rect(m.frame, primary_height).intersection_area(&window_ax);
        if area > 0.0 && best.is_none_or(|(_, a)| area > a) {
            best = Some((i, area));
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }
    let c = window_ax.center();
    let dist = |m: &Monitor| {
        let mc = flip_rect(m.frame, primary_height).center();
        (mc.x - c.x).powi(2) + (mc.y - c.y).powi(2)
    };
    monitors
        .iter()
        .enumerate()
        .min_by(|a, b| dist(a.1).total_cmp(&dist(b.1)))
        .map(|(i, _)| i)
}

/// Where to park a window of `size` so it sits just past a bottom corner of
/// `monitors[monitor]`'s visible rect (AX coordinates returned). Prefers the bottom-left
/// corner; uses the bottom-right one when the parked window would overlap another display
/// more there. Adapted from AeroSpace `MacWindow.swift` (`bottomLeftCorner + (1,-1) + (-w,0)`).
pub fn park_position(monitors: &[Monitor], monitor: usize, size: Size, primary_height: f64) -> (Point, Corner) {
    let visible = flip_rect(monitors[monitor].visible, primary_height);
    // AX coordinates: bottom edge is max_y. One pixel of the window stays on its monitor.
    let bl = Point::new(visible.min_x() + 1.0 - size.w, visible.max_y() - 1.0);
    let br = Point::new(visible.max_x() - 1.0, visible.max_y() - 1.0);
    let overlap_with_others = |origin: Point| -> f64 {
        let parked = Rect { origin, size };
        monitors
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != monitor)
            .map(|(_, m)| flip_rect(m.frame, primary_height).intersection_area(&parked))
            .sum()
    };
    if overlap_with_others(br) < overlap_with_others(bl) {
        (br, Corner::BottomRight)
    } else {
        (bl, Corner::BottomLeft)
    }
}

/// Window position relative to its monitor's AX frame, as fractions (survives monitor resize).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proportional {
    pub fx: f64,
    pub fy: f64,
}

pub fn save_proportional(origin: Point, monitor_ax: Rect) -> Proportional {
    let frac = |v: f64, min: f64, len: f64| if len > 0.0 { (v - min) / len } else { 0.0 };
    Proportional {
        fx: frac(origin.x, monitor_ax.min_x(), monitor_ax.size.w),
        fy: frac(origin.y, monitor_ax.min_y(), monitor_ax.size.h),
    }
}

pub fn restore_proportional(p: Proportional, monitor_ax: Rect) -> Point {
    Point::new(
        monitor_ax.min_x() + p.fx * monitor_ax.size.w,
        monitor_ax.min_y() + p.fy * monitor_ax.size.h,
    )
}

/// True when `actual` differs from `target` by more than `tolerance` px on either axis
/// (used to detect apps that clamp or refuse an AXPosition write).
pub fn position_mismatch(actual: Point, target: Point, tolerance: f64) -> bool {
    (actual.x - target.x).abs() > tolerance || (actual.y - target.y).abs() > tolerance
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_mismatch_tolerance() {
        let t = Point::new(-799.0, 1009.0);
        assert!(!position_mismatch(t, t, 2.0));
        assert!(!position_mismatch(Point::new(-797.0, 1011.0), t, 2.0));
        assert!(position_mismatch(Point::new(-796.9, 1009.0), t, 2.0));
        assert!(position_mismatch(Point::new(-799.0, 900.0), t, 2.0)); // clamped to visible area
        assert!(position_mismatch(Point::new(f64::NAN, 0.0), t, 2.0));
    }

    const H: f64 = 1080.0;

    fn primary() -> Monitor {
        Monitor {
            id: 1,
            frame: Rect::new(0.0, 0.0, 1920.0, 1080.0),
            // menu bar 25 on top, dock 70 on bottom (Cocoa: y up)
            visible: Rect::new(0.0, 70.0, 1920.0, 985.0),
        }
    }

    #[test]
    fn rect_edges_and_center() {
        let r = Rect::new(10.0, 20.0, 100.0, 50.0);
        assert_eq!((r.min_x(), r.max_x(), r.min_y(), r.max_y()), (10.0, 110.0, 20.0, 70.0));
        assert_eq!(r.center(), Point::new(60.0, 45.0));
    }

    #[test]
    fn intersection_area_cases() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        assert_eq!(a.intersection_area(&Rect::new(5.0, 5.0, 10.0, 10.0)), 25.0);
        assert_eq!(a.intersection_area(&Rect::new(10.0, 0.0, 5.0, 5.0)), 0.0); // touching
        assert_eq!(a.intersection_area(&Rect::new(20.0, 20.0, 5.0, 5.0)), 0.0);
        assert_eq!(a.intersection_area(&a), 100.0);
    }

    #[test]
    fn flip_is_involution_and_correct() {
        // Cocoa window at bottom-left of a 1080-high primary: y=0,h=100 -> AX top y=980
        let r = Rect::new(5.0, 0.0, 200.0, 100.0);
        let ax = flip_rect(r, H);
        assert_eq!(ax, Rect::new(5.0, 980.0, 200.0, 100.0));
        assert_eq!(flip_rect(ax, H), r);
    }

    #[test]
    fn flip_menubar_top_edge() {
        // Cocoa rect touching the top of the primary (y+h == H) -> AX y == 0
        let r = Rect::new(0.0, 1000.0, 50.0, 80.0);
        assert_eq!(flip_rect(r, H).origin.y, 0.0);
    }

    #[test]
    fn park_single_monitor_bottom_left() {
        let (p, c) = park_position(&[primary()], 0, Size::new(800.0, 600.0), H);
        assert_eq!(c, Corner::BottomLeft);
        // visible bottom in Cocoa y=70 -> AX y = 1080-70 = 1010. x = 0 + 1 - 800.
        assert_eq!(p, Point::new(-799.0, 1009.0));
    }

    #[test]
    fn park_offset_scales_with_window_size() {
        let (a, _) = park_position(&[primary()], 0, Size::new(100.0, 100.0), H);
        let (b, _) = park_position(&[primary()], 0, Size::new(300.0, 100.0), H);
        assert_eq!(a.x - b.x, 200.0);
        assert_eq!(a.y, b.y);
    }

    #[test]
    fn park_negative_origin_monitor_on_left() {
        // secondary 1280x1024 to the left of primary, bottoms aligned (Cocoa y=0).
        let left = Monitor {
            id: 2,
            frame: Rect::new(-1280.0, 0.0, 1280.0, 1024.0),
            visible: Rect::new(-1280.0, 0.0, 1280.0, 1000.0),
        };
        let mons = [primary(), left];
        // parking on the left monitor: BL corner at x=-1280 is free (nothing further left).
        let (p, c) = park_position(&mons, 1, Size::new(400.0, 300.0), H);
        assert_eq!(c, Corner::BottomLeft);
        assert_eq!(p.x, -1280.0 + 1.0 - 400.0);
        // visible bottom Cocoa y=0 -> AX y = 1080 - 0 = 1080 (primary-height pivot)
        assert_eq!(p.y, 1079.0);
    }

    #[test]
    fn park_prefers_bottom_right_when_left_neighbour_exists() {
        // Primary has a monitor directly to its left: BL parking would land on it.
        let left = Monitor {
            id: 2,
            frame: Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
            visible: Rect::new(-1920.0, 0.0, 1920.0, 1080.0),
        };
        let mons = [primary(), left];
        let (p, c) = park_position(&mons, 0, Size::new(400.0, 300.0), H);
        assert_eq!(c, Corner::BottomRight);
        assert_eq!(p.x, 1920.0 - 1.0);
        assert_eq!(p.y, 1010.0 - 1.0);
    }

    #[test]
    fn park_multi_monitor_above_secondary() {
        // Secondary stacked ABOVE primary: AX y negative. Bottom corner of the secondary
        // sits on top of primary, so both corners overlap; BL wins the tie.
        let top = Monitor {
            id: 2,
            frame: Rect::new(0.0, 1080.0, 1920.0, 1080.0),
            visible: Rect::new(0.0, 1080.0, 1920.0, 1055.0),
        };
        let mons = [primary(), top];
        let (_, c) = park_position(&mons, 1, Size::new(400.0, 300.0), H);
        assert_eq!(c, Corner::BottomLeft);
    }

    #[test]
    fn monitor_for_window_by_overlap() {
        let right = Monitor {
            id: 2,
            frame: Rect::new(1920.0, 0.0, 1920.0, 1080.0),
            visible: Rect::new(1920.0, 0.0, 1920.0, 1080.0),
        };
        let mons = [primary(), right];
        // AX rect mostly on the right monitor
        let w = Rect::new(1800.0, 100.0, 600.0, 400.0);
        assert_eq!(monitor_for_window(&mons, w, H), Some(1));
        let w = Rect::new(100.0, 100.0, 600.0, 400.0);
        assert_eq!(monitor_for_window(&mons, w, H), Some(0));
    }

    #[test]
    fn monitor_for_window_offscreen_uses_nearest_and_empty_is_none() {
        let mons = [primary()];
        assert_eq!(monitor_for_window(&mons, Rect::new(-5000.0, -5000.0, 10.0, 10.0), H), Some(0));
        assert_eq!(monitor_for_window(&[], Rect::new(0.0, 0.0, 1.0, 1.0), H), None);
    }

    #[test]
    fn proportional_roundtrip_same_and_resized_monitor() {
        let mon = Rect::new(0.0, 0.0, 1000.0, 500.0);
        let p = save_proportional(Point::new(250.0, 100.0), mon);
        assert_eq!(p, Proportional { fx: 0.25, fy: 0.2 });
        assert_eq!(restore_proportional(p, mon), Point::new(250.0, 100.0));
        let bigger = Rect::new(0.0, 0.0, 2000.0, 1000.0);
        assert_eq!(restore_proportional(p, bigger), Point::new(500.0, 200.0));
    }

    #[test]
    fn proportional_respects_monitor_origin() {
        let mon = Rect::new(-1000.0, 0.0, 1000.0, 500.0);
        let p = save_proportional(Point::new(-750.0, 50.0), mon);
        assert_eq!(p, Proportional { fx: 0.25, fy: 0.1 });
        assert_eq!(restore_proportional(p, mon), Point::new(-750.0, 50.0));
    }

    #[test]
    fn proportional_zero_sized_monitor_does_not_nan() {
        let p = save_proportional(Point::new(5.0, 5.0), Rect::new(0.0, 0.0, 0.0, 0.0));
        assert!(p.fx.is_finite() && p.fy.is_finite());
    }
}
