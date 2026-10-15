//! The "holds 60fps" criterion and max-window-count selection.

use crate::stats::Summary;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Criterion {
    /// The worse of the tick-interval p95 and the completion-interval p95 must be at most
    /// `p95_factor * budget`.
    pub p95_factor: f64,
    /// At most this percentage of display ticks may be dropped (never seen by the writer).
    pub max_dropped_pct: f64,
    /// At most this percentage of AX writes may fail.
    pub max_failed_write_pct: f64,
}

impl Default for Criterion {
    fn default() -> Self {
        Self { p95_factor: 1.25, max_dropped_pct: 5.0, max_failed_write_pct: 1.0 }
    }
}

impl Criterion {
    pub fn holds_60fps(&self, s: &Summary) -> bool {
        // Tiny epsilon so exact-boundary values are not lost to float rounding.
        s.interval_p95.max(s.completion_interval_p95) <= self.p95_factor * s.budget + 1e-9
            && s.dropped_pct <= self.max_dropped_pct + 1e-9
            && s.failed_write_pct <= self.max_failed_write_pct + 1e-9
    }

    /// Largest tested count `N` such that `N` and every smaller tested count hold 60fps.
    /// `results` need not be sorted. Counts without a summary (skipped) are ignored.
    pub fn max_holding(&self, results: &[(usize, Option<Summary>)]) -> Option<usize> {
        let mut measured: Vec<(usize, &Summary)> =
            results.iter().filter_map(|(n, s)| s.as_ref().map(|s| (*n, s))).collect();
        measured.sort_by_key(|(n, _)| *n);
        measured.iter().take_while(|(_, s)| self.holds_60fps(s)).last().map(|(n, _)| *n)
    }

    pub fn describe(&self) -> String {
        format!(
            "p95 frame interval <= {:.2} x frame budget (budget = max(display refresh period, 1/60 s), i.e. {:.2} ms at 60 Hz) AND dropped display ticks <= {:.1}% (dropped = round(interval / budget) - 1 per interval) AND failed AX writes <= {:.1}%; the p95 is the worse of the tick-to-tick and completion-to-completion intervals",
            self.p95_factor,
            self.p95_factor * 1000.0 / 60.0,
            self.max_dropped_pct,
            self.max_failed_write_pct
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::MIN_BUDGET;

    fn summary(p95: f64, dropped_pct: f64) -> Summary {
        Summary {
            frames: 600,
            refresh_period: MIN_BUDGET,
            budget: MIN_BUDGET,
            interval_p50: MIN_BUDGET,
            interval_p95: p95,
            interval_p99: p95,
            interval_max: p95,
            interval_mean: MIN_BUDGET,
            missed: 0,
            missed_pct: 0.0,
            dropped_ticks: 0,
            dropped_pct,
            failed_write_pct: 0.0,
            completion_interval_p95: p95,
            lateness_p95: 0.0,
            writes_ok: 1,
            writes_failed: 0,
            writes_per_sec: 1.0,
            write_latency_mean: 0.0,
            write_latency_p95: 0.0,
            tick_write_p95: 0.0,
            duration: 10.0,
        }
    }

    #[test]
    fn healthy_run_holds() {
        assert!(Criterion::default().holds_60fps(&summary(MIN_BUDGET, 0.0)));
    }

    #[test]
    fn p95_boundary() {
        let c = Criterion::default();
        assert!(c.holds_60fps(&summary(MIN_BUDGET * 1.25, 0.0)));
        assert!(!c.holds_60fps(&summary(MIN_BUDGET * 1.26, 0.0)));
    }

    #[test]
    fn slow_completion_interval_fails_even_with_good_tick_interval() {
        let c = Criterion::default();
        let mut s = summary(MIN_BUDGET, 0.0);
        s.completion_interval_p95 = MIN_BUDGET * 1.26;
        assert!(!c.holds_60fps(&s));
        s.completion_interval_p95 = MIN_BUDGET * 1.25;
        assert!(c.holds_60fps(&s));
    }

    #[test]
    fn failed_write_rate_boundary() {
        let c = Criterion::default();
        let mut s = summary(MIN_BUDGET, 0.0);
        s.failed_write_pct = 1.0;
        assert!(c.holds_60fps(&s));
        s.failed_write_pct = 1.5;
        assert!(!c.holds_60fps(&s));
    }

    #[test]
    fn missed_late_frames_alone_do_not_fail() {
        // late frames are reported, but only dropped ticks gate the verdict
        let mut s = summary(MIN_BUDGET, 0.0);
        s.missed_pct = 30.0;
        assert!(Criterion::default().holds_60fps(&s));
    }

    #[test]
    fn missed_boundary() {
        let c = Criterion::default();
        assert!(c.holds_60fps(&summary(MIN_BUDGET, 5.0)));
        assert!(!c.holds_60fps(&summary(MIN_BUDGET, 5.1)));
    }

    #[test]
    fn slow_display_budget_scales() {
        // 30Hz display: budget 33.3ms; a 33ms p95 is fine.
        let mut s = summary(1.0 / 30.0, 0.0);
        s.budget = 1.0 / 30.0;
        assert!(Criterion::default().holds_60fps(&s));
    }

    #[test]
    fn max_holding_picks_largest_passing_prefix() {
        let c = Criterion::default();
        let good = || Some(summary(MIN_BUDGET, 0.0));
        let bad = || Some(summary(MIN_BUDGET * 3.0, 40.0));
        assert_eq!(c.max_holding(&[(10, good()), (20, good()), (40, bad())]), Some(20));
        assert_eq!(c.max_holding(&[(10, good()), (20, good()), (40, good())]), Some(40));
        assert_eq!(c.max_holding(&[(10, bad()), (20, good()), (40, good())]), None);
    }

    #[test]
    fn max_holding_requires_all_smaller_counts_to_pass_and_sorts_input() {
        let c = Criterion::default();
        let good = || Some(summary(MIN_BUDGET, 0.0));
        let bad = || Some(summary(MIN_BUDGET * 3.0, 40.0));
        // 20 fails, 40 passes by luck -> still 10
        assert_eq!(c.max_holding(&[(40, good()), (10, good()), (20, bad())]), Some(10));
    }

    #[test]
    fn max_holding_ignores_skipped_and_handles_empty() {
        let c = Criterion::default();
        assert_eq!(c.max_holding(&[]), None);
        assert_eq!(c.max_holding(&[(10, Some(summary(MIN_BUDGET, 0.0))), (20, None)]), Some(10));
        assert_eq!(c.max_holding(&[(10, None)]), None);
    }

    #[test]
    fn describe_mentions_thresholds() {
        let d = Criterion::default().describe();
        assert!(d.contains("1.25") && d.contains("5.0%") && d.contains("1.0%") && d.contains("20.83"));
    }
}
