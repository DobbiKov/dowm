//! Motion definitions evaluated in closed form at an elapsed time.

use crate::easing::ease_in_out_circ;
use crate::geometry::Point;

pub fn lerp(a: Point, b: Point, s: f64) -> Point {
    Point::new((1.0 - s) * a.x + s * b.x, (1.0 - s) * a.y + s * b.y)
}

/// One-shot eased move from `from` to `to`.
#[derive(Clone, Copy, Debug)]
pub struct Animation {
    pub from: Point,
    pub to: Point,
    pub duration: f64,
}

impl Animation {
    pub fn position_at(&self, elapsed: f64) -> Point {
        let t = if self.duration > 0.0 { elapsed / self.duration } else { 1.0 };
        lerp(self.from, self.to, ease_in_out_circ(t))
    }
    pub fn is_complete(&self, elapsed: f64) -> bool {
        elapsed >= self.duration
    }
}

/// Endless eased ping-pong between `home` and `away`; each leg lasts `half_period`.
/// Starts at `home` (no initial jump).
#[derive(Clone, Copy, Debug)]
pub struct Oscillation {
    pub home: Point,
    pub away: Point,
    pub half_period: f64,
}

impl Oscillation {
    pub fn position_at(&self, elapsed: f64) -> Point {
        if self.half_period <= 0.0 || elapsed <= 0.0 {
            return self.home;
        }
        let legs = elapsed / self.half_period;
        let leg = legs.floor();
        let s = ease_in_out_circ(legs - leg);
        if (leg as u64).is_multiple_of(2) {
            lerp(self.home, self.away, s)
        } else {
            lerp(self.away, self.home, s)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a() -> Animation {
        Animation { from: Point::new(-800.0, 1000.0), to: Point::new(100.0, 200.0), duration: 0.3 }
    }

    #[test]
    fn animation_endpoints_and_midpoint() {
        let an = a();
        assert_eq!(an.position_at(0.0), an.from);
        assert_eq!(an.position_at(0.3), an.to);
        let mid = an.position_at(0.15);
        assert!((mid.x - (-350.0)).abs() < 1e-9 && (mid.y - 600.0).abs() < 1e-9);
    }

    #[test]
    fn animation_clamps_outside_range() {
        let an = a();
        assert_eq!(an.position_at(-1.0), an.from);
        assert_eq!(an.position_at(5.0), an.to);
    }

    #[test]
    fn animation_completion() {
        let an = a();
        assert!(!an.is_complete(0.29));
        assert!(an.is_complete(0.3));
        assert!(an.is_complete(1.0));
    }

    #[test]
    fn zero_duration_is_instant() {
        let an = Animation { duration: 0.0, ..a() };
        assert_eq!(an.position_at(0.0), an.to);
        assert!(an.is_complete(0.0));
    }

    #[test]
    fn x_is_monotonic_along_animation() {
        let an = a();
        let mut prev = f64::MIN;
        for i in 0..=60 {
            let p = an.position_at(i as f64 * 0.005);
            assert!(p.x >= prev);
            prev = p.x;
        }
    }

    fn o() -> Oscillation {
        Oscillation { home: Point::new(0.0, 0.0), away: Point::new(100.0, 50.0), half_period: 1.0 }
    }

    #[test]
    fn oscillation_starts_home_reaches_away_and_returns() {
        let o = o();
        assert_eq!(o.position_at(0.0), o.home);
        let away = o.position_at(1.0);
        assert!((away.x - 100.0).abs() < 1e-9 && (away.y - 50.0).abs() < 1e-9);
        let back = o.position_at(2.0);
        assert!(back.x.abs() < 1e-9 && back.y.abs() < 1e-9);
    }

    #[test]
    fn oscillation_is_periodic_and_continuous() {
        let o = o();
        for i in 0..50 {
            let t = i as f64 * 0.137;
            let p = o.position_at(t);
            let q = o.position_at(t + 2.0);
            assert!((p.x - q.x).abs() < 1e-9);
            assert!((0.0..=100.0).contains(&(p.x + 1e-9)) && p.x <= 100.0 + 1e-9);
        }
    }

    #[test]
    fn oscillation_negative_time_and_degenerate_period() {
        let o = o();
        assert_eq!(o.position_at(-1.0), o.home);
        let d = Oscillation { half_period: 0.0, ..o };
        assert_eq!(d.position_at(3.0), d.home);
    }
}
