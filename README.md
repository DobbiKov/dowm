# dowm

macOS tiling WM in Rust. See `PLAN.md`. This repo currently contains the **M0 spike**:
corner parking + CADisplayLink animation + AX write-throughput benchmark.

## Accessibility permission (required)

Both spike binaries drive other apps through the Accessibility API. Grant permission to the
terminal app you launch them from: System Settings > Privacy & Security > Accessibility,
enable Terminal / iTerm / WezTerm, then restart that terminal. Without it `dowm-spike` prints
a message and exits with status 2.

## Build and test

    cargo build --release
    cargo test
    cargo clippy --all-targets -- -D warnings

## M0: park demo (moves a real window of the app you name)

    ./target/release/dowm-spike park --app "TextEdit"          # name (case-insensitive) or pid
    ./target/release/dowm-spike park --app 1234 --duration-ms 300 --pause-ms 1000

Takes the app's first standard window, parks it just past the bottom-left corner of its
monitor's visible rect (AeroSpace-style: `visibleRect.bottomLeftCorner + (1,-1) - (w,0)`;
bottom-right if the left corner would land on a neighbouring display), pauses, then animates
it back with ease-in-out-circ driven by CADisplayLink, writing `AXPosition` each tick. Every
tick interval is printed and saved to `bench/park_frames.csv`.

## M0: benchmark (reproducible, uses a helper app with plain windows)

    ./target/release/dowm-spike bench --spawn-target --counts 10,20,40 --duration 10

Spawns `dowm-spike-target` (40 NSWindows, separate process so AX writes are cross-process),
animates N windows at once for each count, and writes `bench/frames_N10.csv`,
`bench/frames_N20.csv`, `bench/frames_N40.csv` and `M0_REPORT.md`, which states the max
window count holding 60fps and the criterion used. Options: `--motion park|wiggle`,
`--half-period <secs>`, `--out-dir`, `--report`. To benchmark an existing app instead:
`--app <name|pid>` (it must have at least N standard windows; it will be moved around).

Run the helper by itself with `./target/release/dowm-spike-target --windows 40` (it exits
when its stdin closes).

## 60fps criterion

Frame budget = max(display refresh period, 1/60 s). A count holds 60fps when all of:

- the worse of the tick-to-tick interval p95 and the completion-to-completion interval p95
  is <= 1.25 x budget (20.83 ms at 60 Hz);
- dropped display ticks <= 5% (per interval `round(interval / budget) - 1`, over processed
  intervals plus dropped);
- failed AX writes <= 1%.

"Late frames" (interval > 1.5 x budget) and tick-to-completion lateness are reported but
do not gate the verdict. The max count is the largest N where N and all smaller tested
counts hold. AX writes/s is the rate sustained at display rate, not a peak.

Run `bench --spawn-target` first: it uses throwaway windows. `park` and `bench --app` move
real windows; `park` takes the app's first standard window (AXWindows order).

## Crash safety

Every touched window's original position is registered before the first move. SIGINT,
SIGTERM, SIGHUP, panics and normal exit move windows back and re-enable
`AXEnhancedUserInterface`. If something goes wrong anyway (e.g. `kill -9`), windows sit
~1px inside a monitor corner; quit and reopen the app or use its Window menu to recover.
