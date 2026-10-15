//! CSV and markdown formatting for benchmark output.

use crate::criterion::Criterion;
use crate::stats::{FrameSample, Summary, is_missed};

pub const CSV_HEADER: &str = "tick,timestamp_s,interval_ms,write_total_ms,writes_ok,writes_failed,missed";

/// Per-tick CSV. `budget` is the frame budget (see `stats::frame_budget`) used for the
/// `missed` column.
pub fn frames_csv(samples: &[FrameSample], budget: f64) -> String {
    let mut out = String::with_capacity(64 * (samples.len() + 1));
    out.push_str(CSV_HEADER);
    out.push('\n');
    for s in samples {
        let interval = s.interval.map(|i| format!("{:.3}", i * 1000.0)).unwrap_or_default();
        let missed = u8::from(s.interval.is_some_and(|i| is_missed(i, budget)));
        out.push_str(&format!(
            "{},{:.6},{},{:.3},{},{},{}\n",
            s.index,
            s.timestamp,
            interval,
            s.write_total * 1000.0,
            s.writes_ok,
            s.writes_failed,
            missed
        ));
    }
    out
}

pub struct CountResult {
    pub count: usize,
    pub summary: Option<Summary>,
    /// Why there is no summary (e.g. fewer windows than requested), or extra remarks.
    pub note: Option<String>,
    pub csv_path: Option<String>,
}

pub struct ReportMeta {
    pub app: String,
    pub motion: String,
    pub duration_secs: f64,
    pub generated: String,
}

pub fn render_report(meta: &ReportMeta, results: &[CountResult], criterion: &Criterion) -> String {
    let ms = |s: f64| format!("{:.2}", s * 1000.0);
    let by_count: Vec<(usize, Option<Summary>)> = results.iter().map(|r| (r.count, r.summary.clone())).collect();
    let max = criterion.max_holding(&by_count);

    let mut o = String::new();
    o.push_str("# dowm M0 report: AX write throughput under CADisplayLink animation\n\n");
    o.push_str(&format!(
        "- Target app: `{}`\n- Motion: {}\n- Duration per window count: {:.1} s\n- Generated: {}\n\n",
        meta.app, meta.motion, meta.duration_secs, meta.generated
    ));
    match max {
        Some(n) => o.push_str(&format!("**Max window count holding 60fps: {n}**\n\n")),
        None => o.push_str("**Max window count holding 60fps: none of the tested counts**\n\n"),
    }
    o.push_str(&format!("Criterion: {}.\n\n", criterion.describe()));
    o.push_str("A count only qualifies if every smaller tested count also qualifies.\n\n");
    o.push_str("| Windows | Verdict | Frames | Refresh (ms) | Interval p50 (ms) | p95 (ms) | p99 (ms) | Completion p95 (ms) | Lateness p95 (ms) | Late frames % | Dropped ticks % | Failed writes % | AX writes/s (sustained at display rate) | Write mean (ms) | Write p95 (ms) | Tick writes p95 (ms) | Notes | CSV |\n");
    o.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
    for r in results {
        match &r.summary {
            Some(s) => {
                let verdict = if criterion.holds_60fps(s) { "PASS" } else { "FAIL" };
                let mut notes = Vec::new();
                if s.writes_failed > 0 {
                    notes.push(format!("{} failed writes", s.writes_failed));
                }
                if let Some(n) = &r.note {
                    notes.push(n.clone());
                }
                o.push_str(&format!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {:.1} | {:.2} | {:.2} | {:.0} | {:.2} | {:.2} | {} | {} | {} |\n",
                    r.count,
                    verdict,
                    s.frames,
                    ms(s.refresh_period),
                    ms(s.interval_p50),
                    ms(s.interval_p95),
                    ms(s.interval_p99),
                    ms(s.completion_interval_p95),
                    ms(s.lateness_p95),
                    s.missed_pct,
                    s.dropped_pct,
                    s.failed_write_pct,
                    s.writes_per_sec,
                    s.write_latency_mean * 1000.0,
                    s.write_latency_p95 * 1000.0,
                    ms(s.tick_write_p95),
                    notes.join("; "),
                    r.csv_path.as_deref().unwrap_or("")
                ));
            }
            None => o.push_str(&format!(
                "| {} | SKIPPED | | | | | | | | | | | | | | | {} | |\n",
                r.count,
                r.note.as_deref().unwrap_or("no data")
            )),
        }
    }
    o.push_str("\nWriter model: one thread performs the AX writes for all windows of the target app sequentially (as Rift does per app); the display link only publishes the latest tick. Late frames are intervals above 1.5 x budget; dropped ticks are display ticks the writer never saw (round(interval / budget) - 1 per interval, judged against max(refresh, 1/60 s)). Lateness is tick target to write completion relative to the best tick of the run.\n");
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{MIN_BUDGET, summarize};

    fn sample(i: u64, interval: Option<f64>) -> FrameSample {
        FrameSample {
            index: i,
            timestamp: 100.0 + i as f64 * MIN_BUDGET,
            nominal_period: MIN_BUDGET,
            interval,
            write_total: 0.0042,
            writes_ok: 10,
            writes_failed: 0,
            completed_at: i as f64 * MIN_BUDGET + 0.0042,
            lateness_raw: 3.0,
        }
    }

    #[test]
    fn csv_has_header_and_one_row_per_sample() {
        let csv = frames_csv(&[sample(0, None), sample(1, Some(MIN_BUDGET))], MIN_BUDGET);
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines[0], CSV_HEADER);
        assert_eq!(lines.len(), 3);
        assert!(csv.ends_with('\n'));
    }

    #[test]
    fn csv_first_row_has_empty_interval_and_zero_missed() {
        let csv = frames_csv(&[sample(0, None)], MIN_BUDGET);
        assert_eq!(csv.lines().nth(1).unwrap(), "0,100.000000,,4.200,10,0,0");
    }

    #[test]
    fn csv_flags_missed_frames() {
        let csv = frames_csv(&[sample(1, Some(MIN_BUDGET)), sample(2, Some(MIN_BUDGET * 2.0))], MIN_BUDGET);
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert!(rows[0].ends_with(",0"));
        assert!(rows[1].ends_with(",1"));
        assert!(rows[1].contains(",33.333,"));
    }

    #[test]
    fn csv_empty_is_header_only() {
        assert_eq!(frames_csv(&[], MIN_BUDGET), format!("{CSV_HEADER}\n"));
    }

    fn steady_summary() -> Summary {
        let s: Vec<FrameSample> = (0..61).map(|i| sample(i, if i == 0 { None } else { Some(MIN_BUDGET) })).collect();
        summarize(&s, &[0.0005; 10]).unwrap()
    }

    fn meta() -> ReportMeta {
        ReportMeta { app: "dowm-spike-target".into(), motion: "park".into(), duration_secs: 10.0, generated: "unix 1".into() }
    }

    #[test]
    fn report_states_max_count_and_criterion() {
        let r = vec![
            CountResult { count: 10, summary: Some(steady_summary()), note: None, csv_path: Some("bench/frames_N10.csv".into()) },
            CountResult { count: 20, summary: Some(steady_summary()), note: None, csv_path: None },
        ];
        let md = render_report(&meta(), &r, &Criterion::default());
        assert!(md.starts_with("# dowm M0 report"));
        assert!(md.contains("**Max window count holding 60fps: 20**"), "{md}");
        assert!(md.contains(&Criterion::default().describe()));
        assert!(md.contains("| 10 |") && md.contains("| 20 |"));
        assert!(md.contains("16.67"));
        assert!(md.contains("bench/frames_N10.csv"));
        assert!(md.contains("dowm-spike-target"));
    }

    #[test]
    fn report_says_none_when_nothing_holds() {
        let mut bad = steady_summary();
        bad.interval_p95 = MIN_BUDGET * 3.0;
        bad.missed_pct = 50.0;
        let r = vec![CountResult { count: 10, summary: Some(bad), note: None, csv_path: None }];
        let md = render_report(&meta(), &r, &Criterion::default());
        assert!(md.contains("**Max window count holding 60fps: none of the tested counts**"));
        assert!(md.contains("FAIL"));
        assert!(!md.contains("PASS"));
    }

    #[test]
    fn report_lists_skipped_counts_with_reason() {
        let r = vec![
            CountResult { count: 10, summary: Some(steady_summary()), note: None, csv_path: None },
            CountResult { count: 40, summary: None, note: Some("only 12 windows available".into()), csv_path: None },
        ];
        let md = render_report(&meta(), &r, &Criterion::default());
        assert!(md.contains("only 12 windows available"));
        assert!(md.contains("**Max window count holding 60fps: 10**"));
        assert!(md.contains("SKIPPED"));
    }

    #[test]
    fn report_shows_late_frames_dropped_ticks_and_completion_columns() {
        let mut s = steady_summary();
        s.missed_pct = 12.5;
        s.dropped_pct = 3.25;
        s.failed_write_pct = 0.5;
        s.lateness_p95 = 0.0042;
        s.completion_interval_p95 = 0.0171;
        let r = vec![CountResult { count: 10, summary: Some(s), note: None, csv_path: None }];
        let md = render_report(&meta(), &r, &Criterion::default());
        for needle in ["Late frames %", "Dropped ticks %", "Failed writes %", "Completion p95 (ms)", "Lateness p95 (ms)", "12.5", "3.25", "17.10", "4.20"] {
            assert!(md.contains(needle), "missing {needle}\n{md}");
        }
        assert!(md.contains("sustained at display rate"));
    }

    #[test]
    fn report_includes_write_metrics_and_failures() {
        let mut s = steady_summary();
        s.writes_failed = 7;
        let r = vec![CountResult { count: 10, summary: Some(s), note: None, csv_path: None }];
        let md = render_report(&meta(), &r, &Criterion::default());
        assert!(md.contains("AX writes/s (sustained at display rate)"));
        assert!(md.contains("0.50")); // mean per-write latency ms
        assert!(md.contains("7 failed"));
    }
}
