//! Easing curves. Adapted from acsandmann/rift `actor/reactor/animation.rs`, Apache-2.0.

/// Ease-in-out-circ on `t` in `[0,1]` (clamped).
pub fn ease_in_out_circ(t: f64) -> f64 {
    let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
    if t < 0.5 {
        (1.0 - (1.0 - (2.0 * t).powi(2)).sqrt()) / 2.0
    } else {
        ((1.0 - (-2.0 * t + 2.0).powi(2)).sqrt() + 1.0) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(ease_in_out_circ(0.0), 0.0);
        assert_eq!(ease_in_out_circ(1.0), 1.0);
        assert!((ease_in_out_circ(0.5) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn clamps_out_of_range_and_nan_safe() {
        assert_eq!(ease_in_out_circ(-3.0), 0.0);
        assert_eq!(ease_in_out_circ(7.0), 1.0);
        assert!(ease_in_out_circ(f64::NAN).is_finite());
    }

    #[test]
    fn monotonic_non_decreasing() {
        let mut prev = -1.0;
        for i in 0..=1000 {
            let v = ease_in_out_circ(i as f64 / 1000.0);
            assert!(v >= prev, "not monotonic at {i}");
            assert!((0.0..=1.0).contains(&v));
            prev = v;
        }
    }

    #[test]
    fn point_symmetric_about_center() {
        for i in 0..=100 {
            let t = i as f64 / 100.0;
            let sum = ease_in_out_circ(t) + ease_in_out_circ(1.0 - t);
            assert!((sum - 1.0).abs() < 1e-12, "t={t} sum={sum}");
        }
    }

    #[test]
    fn slow_start_fast_middle() {
        assert!(ease_in_out_circ(0.1) < 0.1);
        assert!(ease_in_out_circ(0.9) > 0.9);
    }
}
