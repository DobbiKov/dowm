//! Tick-driven animation and benchmark loops, generic over the tick source, the AX writer
//! and the clock so they can be tested with fakes. FFI implementations live in `platform`.

use crate::anim::{Animation, Oscillation};
use crate::geometry::Point;
use crate::stats::FrameSample;

/// A display-link tick, in CoreAnimation media-time seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tick {
    pub timestamp: f64,
    pub target_timestamp: f64,
}

/// Blocking source of display ticks. Latest-wins: a slow consumer simply sees later ticks.
/// `None` means the source has stopped.
pub trait TickSource {
    fn next_tick(&mut self) -> Option<Tick>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError(pub String);

/// Writes `AXPosition` only. Size is deliberately not part of this interface.
pub trait WindowWriter {
    fn set_position(&mut self, window: usize, position: Point) -> Result<(), WriteError>;
}

/// Monotonic seconds, used to time individual writes.
pub trait Clock {
    fn now(&self) -> f64;
}

#[derive(Debug, Default, Clone)]
pub struct RunOutput {
    pub samples: Vec<FrameSample>,
    /// One entry per individual write (successful or not).
    pub write_latencies: Vec<f64>,
}

/// Core loop. `position(window, elapsed)` gives the target; `finished(elapsed)` is checked
/// before writing (`stop_before`) or after (`!stop_before`).
fn drive<T, W, C>(
    ticks: &mut T,
    writer: &mut W,
    clock: &C,
    windows: usize,
    mut position: impl FnMut(usize, f64) -> Point,
    mut finished: impl FnMut(f64) -> bool,
    stop_before: bool,
) -> RunOutput
where
    T: TickSource,
    W: WindowWriter,
    C: Clock,
{
    let mut out = RunOutput::default();
    let mut first_target: Option<f64> = None;
    let mut prev_ts: Option<f64> = None;
    while let Some(tick) = ticks.next_tick() {
        let base = *first_target.get_or_insert(tick.target_timestamp);
        let elapsed = tick.target_timestamp - base;
        if stop_before && finished(elapsed) {
            break;
        }
        let start = clock.now();
        let (mut ok, mut failed) = (0u32, 0u32);
        for w in 0..windows {
            let t0 = clock.now();
            match writer.set_position(w, position(w, elapsed)) {
                Ok(()) => ok += 1,
                Err(_) => failed += 1,
            }
            out.write_latencies.push(clock.now() - t0);
        }
        let completed_at = clock.now();
        out.samples.push(FrameSample {
            index: out.samples.len() as u64,
            timestamp: tick.timestamp,
            nominal_period: (tick.target_timestamp - tick.timestamp).max(0.0),
            interval: prev_ts.map(|p| tick.timestamp - p),
            write_total: completed_at - start,
            writes_ok: ok,
            writes_failed: failed,
            completed_at,
            lateness_raw: completed_at - tick.target_timestamp,
        });
        prev_ts = Some(tick.timestamp);
        if !stop_before && finished(elapsed) {
            break;
        }
    }
    out
}

/// Animate a single window (writer window index 0) along `anim`, one `AXPosition` write per
/// tick. The final write lands exactly on `anim.to`.
pub fn run_animation<T, W, C>(ticks: &mut T, writer: &mut W, clock: &C, anim: &Animation) -> RunOutput
where
    T: TickSource,
    W: WindowWriter,
    C: Clock,
{
    drive(ticks, writer, clock, 1, |_, e| anim.position_at(e), |e| anim.is_complete(e), false)
}

/// Oscillate `motions.len()` windows simultaneously for `duration` seconds of tick time.
pub fn run_bench<T, W, C>(ticks: &mut T, writer: &mut W, clock: &C, motions: &[Oscillation], duration: f64) -> RunOutput
where
    T: TickSource,
    W: WindowWriter,
    C: Clock,
{
    drive(ticks, writer, clock, motions.len(), |w, e| motions[w].position_at(e), |e| e > duration, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    struct FakeClock(Rc<Cell<f64>>);
    impl Clock for FakeClock {
        fn now(&self) -> f64 {
            self.0.get()
        }
    }

    struct FakeTicks {
        queue: VecDeque<Tick>,
    }
    impl TickSource for FakeTicks {
        fn next_tick(&mut self) -> Option<Tick> {
            self.queue.pop_front()
        }
    }

    fn ticks_at(period: f64, n: usize) -> FakeTicks {
        FakeTicks {
            queue: (0..n)
                .map(|i| Tick { timestamp: 50.0 + i as f64 * period, target_timestamp: 50.0 + (i + 1) as f64 * period })
                .collect(),
        }
    }

    struct FakeWriter {
        clock: Rc<Cell<f64>>,
        latency: f64,
        log: Rc<RefCell<Vec<(usize, Point)>>>,
        fail_window: Option<usize>,
    }
    impl WindowWriter for FakeWriter {
        fn set_position(&mut self, window: usize, p: Point) -> Result<(), WriteError> {
            self.clock.set(self.clock.get() + self.latency);
            self.log.borrow_mut().push((window, p));
            if self.fail_window == Some(window) { Err(WriteError("boom".into())) } else { Ok(()) }
        }
    }

    type Log = Rc<RefCell<Vec<(usize, Point)>>>;
    fn rig(latency: f64, fail_window: Option<usize>) -> (FakeClock, FakeWriter, Log) {
        let c = Rc::new(Cell::new(0.0));
        let log: Log = Rc::default();
        (FakeClock(c.clone()), FakeWriter { clock: c, latency, log: log.clone(), fail_window }, log)
    }

    const P: f64 = 1.0 / 60.0;

    fn anim() -> Animation {
        Animation { from: Point::new(-799.0, 1009.0), to: Point::new(100.0, 200.0), duration: 0.3 }
    }

    #[test]
    fn animation_starts_at_from_ends_exactly_at_to_and_stops() {
        let (clock, mut w, log) = rig(0.0, None);
        let mut ticks = ticks_at(P, 120);
        let out = run_animation(&mut ticks, &mut w, &clock, &anim());
        let log = log.borrow();
        assert_eq!(log.first().unwrap().1, anim().from);
        assert_eq!(log.last().unwrap().1, anim().to);
        // stopped early: far fewer than the 120 ticks offered
        assert!(out.samples.len() <= 20 && out.samples.len() >= 18, "{}", out.samples.len());
        assert!(!ticks.queue.is_empty());
    }

    #[test]
    fn animation_writes_one_write_per_tick_monotonic_x() {
        let (clock, mut w, log) = rig(0.0, None);
        let out = run_animation(&mut ticks_at(P, 120), &mut w, &clock, &anim());
        assert_eq!(log.borrow().len(), out.samples.len());
        let xs: Vec<f64> = log.borrow().iter().map(|(_, p)| p.x).collect();
        assert!(xs.windows(2).all(|p| p[1] >= p[0]));
    }

    #[test]
    fn intervals_are_timestamp_deltas_and_first_is_none() {
        let (clock, mut w, _) = rig(0.0, None);
        let out = run_animation(&mut ticks_at(P, 120), &mut w, &clock, &anim());
        assert_eq!(out.samples[0].interval, None);
        for s in &out.samples[1..] {
            assert!((s.interval.unwrap() - P).abs() < 1e-9);
        }
        assert!((out.samples[0].nominal_period - P).abs() < 1e-9);
    }

    #[test]
    fn dropped_ticks_show_up_as_double_interval() {
        let (clock, mut w, _) = rig(0.0, None);
        let mut ticks = ticks_at(P, 30);
        ticks.queue.remove(5); // simulated coalesced tick
        let motions = [Oscillation { home: Point::new(0.0, 0.0), away: Point::new(10.0, 0.0), half_period: 1.0 }];
        let out = run_bench(&mut ticks, &mut w, &clock, &motions, 10.0);
        let doubled = out.samples.iter().filter(|s| s.interval.is_some_and(|i| (i - 2.0 * P).abs() < 1e-9)).count();
        assert_eq!(doubled, 1);
    }

    fn motions(n: usize) -> Vec<Oscillation> {
        (0..n)
            .map(|i| Oscillation { home: Point::new(i as f64, 0.0), away: Point::new(i as f64 + 100.0, 0.0), half_period: 0.5 })
            .collect()
    }

    #[test]
    fn bench_writes_every_window_each_tick_in_order() {
        let (clock, mut w, log) = rig(0.0, None);
        let out = run_bench(&mut ticks_at(P, 10), &mut w, &clock, &motions(3), 100.0);
        assert_eq!(out.samples.len(), 10);
        assert_eq!(log.borrow().len(), 30);
        let windows: Vec<usize> = log.borrow().iter().take(6).map(|(w, _)| *w).collect();
        assert_eq!(windows, vec![0, 1, 2, 0, 1, 2]);
        assert!(out.samples.iter().all(|s| s.writes_ok == 3 && s.writes_failed == 0));
        assert_eq!(out.write_latencies.len(), 30);
    }

    #[test]
    fn bench_respects_duration() {
        let (clock, mut w, _) = rig(0.0, None);
        let out = run_bench(&mut ticks_at(P, 600), &mut w, &clock, &motions(2), 1.0);
        let span = out.samples.last().unwrap().timestamp - out.samples[0].timestamp;
        assert!(span <= 1.0 + 1e-9 && span > 0.9, "span {span}");
    }

    #[test]
    fn bench_records_write_latency_and_tick_total() {
        let (clock, mut w, _) = rig(0.002, None);
        let out = run_bench(&mut ticks_at(P, 5), &mut w, &clock, &motions(3), 100.0);
        assert!(out.write_latencies.iter().all(|l| (l - 0.002).abs() < 1e-9));
        assert!(out.samples.iter().all(|s| (s.write_total - 0.006).abs() < 1e-9));
    }

    #[test]
    fn bench_records_completion_time_and_raw_lateness() {
        let (clock, mut w, _) = rig(0.002, None);
        let out = run_bench(&mut ticks_at(P, 3), &mut w, &clock, &motions(3), 100.0);
        // the fake clock only advances inside writes: 3 x 2ms per tick
        for (i, s) in out.samples.iter().enumerate() {
            assert!((s.completed_at - 0.006 * (i + 1) as f64).abs() < 1e-9);
        }
        let first_target = 50.0 + P;
        assert!((out.samples[0].lateness_raw - (0.006 - first_target)).abs() < 1e-9);
        let d = out.samples[1].lateness_raw - out.samples[0].lateness_raw;
        assert!((d - (0.006 - P)).abs() < 1e-9);
    }

    #[test]
    fn bench_counts_failures_and_keeps_going() {
        let (clock, mut w, _) = rig(0.0, Some(1));
        let out = run_bench(&mut ticks_at(P, 5), &mut w, &clock, &motions(3), 100.0);
        assert_eq!(out.samples.len(), 5);
        assert!(out.samples.iter().all(|s| s.writes_ok == 2 && s.writes_failed == 1));
    }

    #[test]
    fn empty_tick_source_and_zero_windows() {
        let (clock, mut w, log) = rig(0.0, None);
        let out = run_bench(&mut ticks_at(P, 0), &mut w, &clock, &motions(3), 1.0);
        assert!(out.samples.is_empty());
        let out = run_bench(&mut ticks_at(P, 3), &mut w, &clock, &[], 1.0);
        assert_eq!(out.samples.len(), 3);
        assert!(log.borrow().is_empty());
    }
}
