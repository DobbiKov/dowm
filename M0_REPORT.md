# dowm M0 report: AX write throughput under CADisplayLink animation

- Target app: `dowm-spike-target (fresh helper per window count)`
- Motion: continuous park <-> home cycles, 0.5 s per leg
- Duration per window count: 10.0 s
- Generated: unix time 1791983623 (INTERRUPTED: partial report, later counts not run)

**Max window count holding 60fps: 10**

Criterion: p95 frame interval <= 1.25 x frame budget (budget = max(display refresh period, 1/60 s), i.e. 20.83 ms at 60 Hz) AND dropped display ticks <= 5.0% (dropped = round(interval / budget) - 1 per interval) AND failed AX writes <= 1.0%; the p95 is the worse of the tick-to-tick and completion-to-completion intervals.

A count only qualifies if every smaller tested count also qualifies.

| Windows | Verdict | Frames | Refresh (ms) | Interval p50 (ms) | p95 (ms) | p99 (ms) | Completion p95 (ms) | Lateness p95 (ms) | Late frames % | Dropped ticks % | Failed writes % | AX writes/s (sustained at display rate) | Write mean (ms) | Write p95 (ms) | Tick writes p95 (ms) | Notes | CSV |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 10 | PASS | 600 | 16.67 | 16.67 | 16.67 | 16.67 | 19.48 | 3.33 | 0.0 | 0.00 | 0.00 | 600 | 0.14 | 0.22 | 3.64 |  | bench/frames_N10.csv |

Writer model: one thread performs the AX writes for all windows of the target app sequentially (as Rift does per app); the display link only publishes the latest tick. Late frames are intervals above 1.5 x budget; dropped ticks are display ticks the writer never saw (round(interval / budget) - 1 per interval, judged against max(refresh, 1/60 s)). Lateness is tick target to write completion relative to the best tick of the run.

## Step 3b: manual crash-safety check with real windows

The automated check (`tests/crash_safety.rs`) uses fake windows and a recording restorer.
It proves the signal and panic paths run and read the right set. It cannot prove the AX
restorer (`src/platform/ax_restorer.rs`) finds real windows. Check that by hand, using only
the helper's own windows, never your real apps.

dowm has no workspace commands bound yet (config and hotkeys are step 5), so the easiest
real-window check drives the M0 spike, which parks windows the same way and uses the same
`RestoreRegistry` idea:

1. `cargo build`
2. `target/debug/dowm-spike-target --windows 5` (prints `READY <pid>`; its 5 windows open).
3. In another terminal run the spike against that pid so it parks the windows (see
   `dowm-spike --help`); confirm the windows vanish into a bottom corner (a 1px sliver stays).
4. `kill -TERM <spike pid>`. Every helper window must come back on-screen within about a
   second. Repeat with `kill -INT`, `kill -HUP`, `kill -QUIT`.
5. A panic cannot be forced from outside; it is covered by `tests/crash_safety.rs`
   (fake windows). `kill -ABRT` is not caught and is not covered.
6. Not covered, by design: `kill -KILL` and a power loss. If windows are stranded, quit
   the helper (close its stdin or `kill <helper pid>`); its windows go with it.

For the daemon itself once commands exist (step 5): run `target/debug/dowm`, open
`dowm-spike-target`, switch workspaces so its windows park, then `kill -TERM` dowm; the
windows must return to their pre-park positions (AX restorer matches windows by their
parked position, within 3px). Also verify with two displays: the parked corner is the one
away from the neighbour, and a resolution change while parked still restores on-screen.
