//! Frame-interval statistics. All times are seconds unless a name says otherwise.

/// Never judge frames against a budget tighter than 60fps: on a 120Hz panel, a writer
/// that sustains 60fps still "holds 60fps".
pub const MIN_BUDGET: f64 = 1.0 / 60.0;
/// A frame is missed when its interval exceeds this multiple of the frame budget.
pub const MISS_FACTOR: f64 = 1.5;

/// One processed display-link tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameSample {
    pub index: u64,
    /// Display-link `timestamp` of this tick.
    pub timestamp: f64,
    /// Display-link nominal period for this tick (`targetTimestamp - timestamp`); 0 if unknown.
    pub nominal_period: f64,
    /// Delta to the previously *processed* tick (None for the first). Ticks dropped by
    /// latest-wins coalescing show up as an interval of ~2x or more the period.
    pub interval: Option<f64>,
    /// Wall time to complete all writes of this tick.
    pub write_total: f64,
    pub writes_ok: u32,
    pub writes_failed: u32,
    /// Writer-clock time at which all writes of this tick had completed.
    pub completed_at: f64,
    /// `completed_at` minus the tick's `target_timestamp`, mixing the writer clock and the
    /// CoreAnimation clock. The constant offset between the clocks is removed in
    /// `summarize` by subtracting the minimum over the run.
    pub lateness_raw: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub frames: usize,
    pub refresh_period: f64,
    pub budget: f64,
    pub interval_p50: f64,
    pub interval_p95: f64,
    pub interval_p99: f64,
    pub interval_max: f64,
    pub interval_mean: f64,
    pub missed: usize,
    pub missed_pct: f64,
    /// Ticks the display link delivered that the writer never saw (coalesced away), per
    /// interval `round(interval / budget) - 1`, summed.
    pub dropped_ticks: u64,
    /// `dropped / (processed intervals + dropped)`, in percent.
    pub dropped_pct: f64,
    /// Failed writes as a percentage of all attempted writes.
    pub failed_write_pct: f64,
    /// p95 of the delta between consecutive tick *completion* times.
    pub completion_interval_p95: f64,
    /// p95 of tick-target to write-completion lateness, relative to the best tick.
    pub lateness_p95: f64,
    pub writes_ok: u64,
    pub writes_failed: u64,
    pub writes_per_sec: f64,
    pub write_latency_mean: f64,
    pub write_latency_p95: f64,
    pub tick_write_p95: f64,
    pub duration: f64,
}

/// Nearest-rank percentile (`p` in 0..=100) of an ascending-sorted slice.
pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let p = p.clamp(0.0, 100.0);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

pub fn mean(v: &[f64]) -> Option<f64> {
    if v.is_empty() { None } else { Some(v.iter().sum::<f64>() / v.len() as f64) }
}

pub fn frame_budget(refresh_period: f64) -> f64 {
    if refresh_period.is_finite() && refresh_period > MIN_BUDGET { refresh_period } else { MIN_BUDGET }
}

pub fn is_missed(interval: f64, budget: f64) -> bool {
    interval > MISS_FACTOR * budget
}

fn sorted_copy(v: impl Iterator<Item = f64>) -> Vec<f64> {
    let mut v: Vec<f64> = v.collect();
    v.sort_by(f64::total_cmp);
    v
}

/// Ticks lost between two processed ticks `interval` apart, judged against `budget`.
pub fn dropped_in_interval(interval: f64, budget: f64) -> u64 {
    if !(interval.is_finite() && budget > 0.0) {
        return 0;
    }
    ((interval / budget).round() as i64 - 1).max(0) as u64
}

/// Summarize a run. `write_latencies` holds one entry per individual AX write.
/// Returns `None` when there are fewer than two ticks (no interval to measure).
pub fn summarize(samples: &[FrameSample], write_latencies: &[f64]) -> Option<Summary> {
    let intervals = sorted_copy(samples.iter().filter_map(|s| s.interval));
    let interval_p50 = percentile(&intervals, 50.0)?;
    let nominal = sorted_copy(samples.iter().map(|s| s.nominal_period).filter(|p| *p > 0.0));
    let refresh_period = percentile(&nominal, 50.0).unwrap_or(interval_p50);
    let budget = frame_budget(refresh_period);
    let missed = intervals.iter().filter(|i| is_missed(**i, budget)).count();
    let duration = (samples.last()?.timestamp - samples.first()?.timestamp).max(0.0);
    let writes_ok: u64 = samples.iter().map(|s| u64::from(s.writes_ok)).sum();
    let writes_failed: u64 = samples.iter().map(|s| u64::from(s.writes_failed)).sum();
    // The first tick's writes happen before the measured span starts, so a rate over
    // `duration` counts only the writes of the following ticks.
    let span_writes: u64 = samples.iter().skip(1).map(|s| u64::from(s.writes_ok)).sum();
    let dropped_ticks: u64 = intervals.iter().map(|i| dropped_in_interval(*i, budget)).sum();
    let attempted = writes_ok + writes_failed;
    let completion = sorted_copy(samples.windows(2).map(|w| w[1].completed_at - w[0].completed_at));
    let min_raw = samples.iter().map(|s| s.lateness_raw).fold(f64::INFINITY, f64::min);
    let lateness = sorted_copy(samples.iter().map(|s| s.lateness_raw - min_raw));
    let lat = sorted_copy(write_latencies.iter().copied());
    let tick_writes = sorted_copy(samples.iter().map(|s| s.write_total));
    Some(Summary {
        frames: intervals.len(),
        refresh_period,
        budget,
        interval_p50,
        interval_p95: percentile(&intervals, 95.0)?,
        interval_p99: percentile(&intervals, 99.0)?,
        interval_max: *intervals.last()?,
        interval_mean: mean(&intervals)?,
        missed,
        missed_pct: 100.0 * missed as f64 / intervals.len() as f64,
        dropped_ticks,
        dropped_pct: 100.0 * dropped_ticks as f64 / (intervals.len() as u64 + dropped_ticks) as f64,
        failed_write_pct: if attempted > 0 { 100.0 * writes_failed as f64 / attempted as f64 } else { 0.0 },
        completion_interval_p95: percentile(&completion, 95.0).unwrap_or(0.0),
        lateness_p95: percentile(&lateness, 95.0).unwrap_or(0.0),
        writes_ok,
        writes_failed,
        writes_per_sec: if duration > 0.0 { span_writes as f64 / duration } else { 0.0 },
        write_latency_mean: mean(&lat).unwrap_or(0.0),
        write_latency_p95: percentile(&lat, 95.0).unwrap_or(0.0),
        tick_write_p95: percentile(&tick_writes, 95.0).unwrap_or(0.0),
        duration,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<f64>) -> Vec<f64> {
        v.sort_by(f64::total_cmp);
        v
    }

    #[test]
    fn percentile_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&v, 50.0), Some(50.0));
        assert_eq!(percentile(&v, 95.0), Some(95.0));
        assert_eq!(percentile(&v, 99.0), Some(99.0));
        assert_eq!(percentile(&v, 100.0), Some(100.0));
        assert_eq!(percentile(&v, 0.0), Some(1.0));
    }

    #[test]
    fn percentile_edge_cases() {
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 99.0), Some(7.0));
        assert_eq!(percentile(&[1.0, 2.0], 50.0), Some(1.0));
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 150.0), Some(3.0)); // clamped
        assert_eq!(percentile(&[1.0, 2.0, 3.0], -5.0), Some(1.0));
    }

    #[test]
    fn mean_basic_and_empty() {
        assert_eq!(mean(&[1.0, 2.0, 3.0]), Some(2.0));
        assert_eq!(mean(&[]), None);
    }

    #[test]
    fn budget_never_tighter_than_60fps() {
        assert!((frame_budget(1.0 / 120.0) - MIN_BUDGET).abs() < 1e-12);
        assert!((frame_budget(1.0 / 60.0) - MIN_BUDGET).abs() < 1e-12);
        assert!((frame_budget(1.0 / 30.0) - 1.0 / 30.0).abs() < 1e-12);
        assert_eq!(frame_budget(0.0), MIN_BUDGET);
        assert_eq!(frame_budget(f64::NAN), MIN_BUDGET);
        assert_eq!(frame_budget(-1.0), MIN_BUDGET);
    }

    #[test]
    fn missed_detection_relative_to_budget() {
        let b = 1.0 / 60.0;
        assert!(!is_missed(b, b));
        assert!(!is_missed(b * 1.49, b));
        assert!(is_missed(b * 1.51, b));
        assert!(is_missed(b * 2.0, b));
    }

    /// Build `n` samples at a fixed `period`, with `write` seconds per tick of `writes` writes.
    fn steady(n: usize, period: f64, nominal: f64, write: f64, writes: u32) -> Vec<FrameSample> {
        (0..n)
            .map(|i| FrameSample {
                index: i as u64,
                timestamp: 100.0 + i as f64 * period,
                nominal_period: nominal,
                interval: if i == 0 { None } else { Some(period) },
                write_total: write,
                writes_ok: writes,
                writes_failed: 0,
                completed_at: i as f64 * period + write,
                lateness_raw: 7.0 + write,
            })
            .collect()
    }

    #[test]
    fn summarize_steady_60hz() {
        let s = steady(61, 1.0 / 60.0, 1.0 / 60.0, 0.004, 10);
        let lat = vec![0.0004; 610];
        let sum = summarize(&s, &lat).unwrap();
        assert_eq!(sum.frames, 60);
        assert!((sum.interval_p50 - 1.0 / 60.0).abs() < 1e-12);
        assert!((sum.interval_p99 - 1.0 / 60.0).abs() < 1e-12);
        assert_eq!(sum.missed, 0);
        assert_eq!(sum.missed_pct, 0.0);
        assert_eq!(sum.writes_ok, 610);
        assert!((sum.duration - 1.0).abs() < 1e-9);
        // 61 ticks span 60 intervals: the first tick's writes are not inside the span.
        assert!((sum.writes_per_sec - 600.0).abs() < 1e-6);
        assert!((sum.write_latency_mean - 0.0004).abs() < 1e-12);
        assert!((sum.tick_write_p95 - 0.004).abs() < 1e-12);
        assert!((sum.refresh_period - 1.0 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn summarize_counts_dropped_ticks_as_missed() {
        let mut s = steady(101, 1.0 / 60.0, 1.0 / 60.0, 0.004, 1);
        for i in [10usize, 20, 30, 40] {
            s[i].interval = Some(2.0 / 60.0);
        }
        let sum = summarize(&s, &[0.001]).unwrap();
        assert_eq!(sum.missed, 4);
        assert!((sum.missed_pct - 4.0).abs() < 1e-9);
        assert!((sum.interval_max - 2.0 / 60.0).abs() < 1e-12);
    }

    #[test]
    fn summarize_120hz_display_at_60fps_is_not_missed() {
        // link nominal 8.33ms but writer only keeps every other tick -> 16.67ms intervals
        let s = steady(121, 1.0 / 60.0, 1.0 / 120.0, 0.005, 1);
        let sum = summarize(&s, &[0.005]).unwrap();
        assert_eq!(sum.missed, 0);
        assert!((sum.refresh_period - 1.0 / 120.0).abs() < 1e-12);
        assert!((sum.budget - MIN_BUDGET).abs() < 1e-12);
    }

    #[test]
    fn summarize_falls_back_to_median_interval_without_nominal() {
        let s = steady(31, 1.0 / 60.0, 0.0, 0.001, 1);
        let sum = summarize(&s, &[]).unwrap();
        assert!((sum.refresh_period - 1.0 / 60.0).abs() < 1e-12);
        assert_eq!(sum.write_latency_mean, 0.0);
    }

    #[test]
    fn summarize_counts_failed_writes_separately() {
        let mut s = steady(10, 0.016, 0.016, 0.001, 5);
        s[3].writes_failed = 2;
        s[3].writes_ok = 3;
        let sum = summarize(&s, &[]).unwrap();
        assert_eq!(sum.writes_failed, 2);
        assert_eq!(sum.writes_ok, 48);
    }

    #[test]
    fn summarize_too_few_samples_is_none() {
        assert!(summarize(&[], &[]).is_none());
        assert!(summarize(&steady(1, 0.016, 0.016, 0.0, 0), &[]).is_none());
    }

    #[test]
    fn dropped_in_interval_rounds_to_whole_ticks() {
        let b = 1.0 / 60.0;
        assert_eq!(dropped_in_interval(b, b), 0);
        assert_eq!(dropped_in_interval(b * 1.4, b), 0);
        assert_eq!(dropped_in_interval(b * 1.6, b), 1);
        assert_eq!(dropped_in_interval(b * 3.0, b), 2);
        assert_eq!(dropped_in_interval(b * 0.5, b), 0); // never negative
        assert_eq!(dropped_in_interval(f64::NAN, b), 0);
    }

    #[test]
    fn summarize_counts_dropped_ticks_and_percentage() {
        let mut s = steady(101, 1.0 / 60.0, 1.0 / 60.0, 0.001, 1);
        s[10].interval = Some(2.0 / 60.0); // 1 dropped
        s[20].interval = Some(3.0 / 60.0); // 2 dropped
        s[30].interval = Some(1.4 / 60.0); // jitter only
        let sum = summarize(&s, &[]).unwrap();
        assert_eq!(sum.dropped_ticks, 3);
        // 100 processed intervals + 3 dropped = 103 expected ticks
        assert!((sum.dropped_pct - 100.0 * 3.0 / 103.0).abs() < 1e-9);
    }

    #[test]
    fn summarize_no_drops_on_steady_run_even_at_120hz_link() {
        let sum = summarize(&steady(121, 1.0 / 60.0, 1.0 / 120.0, 0.001, 1), &[]).unwrap();
        assert_eq!(sum.dropped_ticks, 0);
        assert_eq!(sum.dropped_pct, 0.0);
    }

    #[test]
    fn summarize_failed_write_percentage() {
        let mut s = steady(10, 0.016, 0.016, 0.001, 10);
        s[2].writes_failed = 5;
        s[2].writes_ok = 5;
        let sum = summarize(&s, &[]).unwrap();
        assert!((sum.failed_write_pct - 5.0).abs() < 1e-9); // 5 of 100
        let clean = summarize(&steady(10, 0.016, 0.016, 0.001, 10), &[]).unwrap();
        assert_eq!(clean.failed_write_pct, 0.0);
    }

    #[test]
    fn summarize_completion_interval_and_lateness() {
        let mut s = steady(11, 1.0 / 60.0, 1.0 / 60.0, 0.002, 1);
        // tick 5 finishes 30ms late; everything after is shifted accordingly
        for x in s.iter_mut().skip(5) {
            x.completed_at += 0.030;
        }
        s[5].lateness_raw += 0.030;
        s[6].lateness_raw += 0.030;
        let sum = summarize(&s, &[]).unwrap();
        assert!((sum.completion_interval_p95 - (1.0 / 60.0 + 0.030)).abs() < 1e-9);
        // offset (7.002 baseline) removed: p95 of lateness over 11 samples = the worst = 30ms
        assert!((sum.lateness_p95 - 0.030).abs() < 1e-9, "{}", sum.lateness_p95);
        let calm = summarize(&steady(11, 1.0 / 60.0, 1.0 / 60.0, 0.002, 1), &[]).unwrap();
        assert!(calm.lateness_p95.abs() < 1e-9);
        assert!((calm.completion_interval_p95 - 1.0 / 60.0).abs() < 1e-9);
    }

    #[test]
    fn sorted_helper_sanity() {
        assert_eq!(sorted(vec![3.0, 1.0, 2.0]), vec![1.0, 2.0, 3.0]);
    }
}
