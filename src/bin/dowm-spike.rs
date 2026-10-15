//! dowm M0 spike: park a window at its monitor's bottom corner and animate it back with
//! CADisplayLink; benchmark sustained cross-process AX write throughput.

#[cfg(target_os = "macos")]
mod app {
    use std::io::{BufRead, BufReader};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use clap::{Args, Parser, Subcommand, ValueEnum};
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    use objc2_foundation::{NSDate, NSRunLoop};

    use dowm::anim::{Animation, Oscillation};
    use dowm::appspec::{AppSpec, parse_counts};
    use dowm::criterion::Criterion;
    use dowm::geometry::{
        Point, Rect, Size, flip_rect, monitor_for_window, park_position, position_mismatch, restore_proportional,
        save_proportional,
    };
    use dowm::platform::ax::{self, AxElement};
    use dowm::platform::screens::Screens;
    use dowm::platform::{Activity, AxWriter, DoneOnDrop, MonotonicClock, apps, display_link, guard, screens};
    use dowm::report::{CountResult, ReportMeta, frames_csv, render_report};
    use dowm::runner::{run_animation, run_bench};
    use dowm::stats::{MIN_BUDGET, summarize};

    #[derive(Parser)]
    #[command(name = "dowm-spike", about = "dowm M0 spike: corner parking + CADisplayLink animation + AX write benchmark")]
    struct Cli {
        #[command(subcommand)]
        cmd: Cmd,
    }

    #[derive(Subcommand)]
    enum Cmd {
        /// Park one window of an app at its monitor's bottom corner, then animate it back.
        Park(ParkArgs),
        /// Benchmark sustained AX write throughput at several window counts.
        Bench(BenchArgs),
    }

    #[derive(Args)]
    struct ParkArgs {
        /// App name (case-insensitive, substring ok) or pid.
        #[arg(long)]
        app: String,
        /// Animation length in milliseconds.
        #[arg(long, default_value_t = 300)]
        duration_ms: u64,
        /// Pause while parked, in milliseconds.
        #[arg(long, default_value_t = 1000)]
        pause_ms: u64,
        /// Directory for the per-tick CSV.
        #[arg(long, default_value = "bench")]
        out_dir: PathBuf,
    }

    #[derive(Clone, Copy, ValueEnum)]
    enum MotionKind {
        /// Each window continuously cycles between its home frame and its parked corner.
        Park,
        /// Each window oscillates 80px to the right and back.
        Wiggle,
    }

    #[derive(Args)]
    struct BenchArgs {
        /// App name or pid (omit with --spawn-target).
        #[arg(long)]
        app: Option<String>,
        /// Launch the dowm-spike-target helper (plain NSWindows) and benchmark against it.
        #[arg(long)]
        spawn_target: bool,
        /// Comma-separated window counts.
        #[arg(long, default_value = "10,20,40")]
        counts: String,
        /// Seconds of animation per window count.
        #[arg(long, default_value_t = 10.0)]
        duration: f64,
        #[arg(long, value_enum, default_value_t = MotionKind::Park)]
        motion: MotionKind,
        /// Seconds per leg of the oscillation.
        #[arg(long, default_value_t = 0.5)]
        half_period: f64,
        #[arg(long, default_value = "bench")]
        out_dir: PathBuf,
        #[arg(long, default_value = "M0_REPORT.md")]
        report: PathBuf,
    }

    static CHILD: Mutex<Option<Child>> = Mutex::new(None);

    fn kill_child() {
        if let Some(mut c) = CHILD.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn run() -> Result<(), String> {
        let cli = Cli::parse();
        let mtm = setup()?;
        // No App Nap / timer coalescing while we are measuring frame pacing.
        let _activity = Activity::begin("dowm M0 spike: frame-paced AX writes");
        let result = match cli.cmd {
            Cmd::Park(a) => park(mtm, a),
            Cmd::Bench(a) => bench(mtm, a),
        };
        // Normal path: the writer thread has been joined, so nothing can re-park a window.
        guard::restore_all();
        kill_child();
        let sig = guard::SIGNAL.load(Ordering::Acquire);
        if sig != 0 {
            std::process::exit(128 + sig);
        }
        result
    }

    fn setup() -> Result<MainThreadMarker, String> {
        if !ax::is_trusted() {
            return Err("Accessibility permission is not granted to this process.\n\
                 Open System Settings > Privacy & Security > Accessibility, enable your terminal app\n\
                 (Terminal / iTerm / WezTerm / ...), then re-run. A restart of the terminal may be needed."
                .into());
        }
        let mtm = MainThreadMarker::new().ok_or("must run on the main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        guard::install_handlers(kill_child);
        Ok(mtm)
    }

    /// Pump the main run loop (where the display link fires) until `done` flips or a stop
    /// is requested. With no run-loop sources (timer fallback) `runUntilDate` returns
    /// immediately, so back off instead of spinning.
    fn pump_until(done: &AtomicBool) {
        let rl = NSRunLoop::mainRunLoop();
        while !done.load(Ordering::Acquire) && !guard::stopping() {
            let t0 = std::time::Instant::now();
            rl.runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.05));
            if t0.elapsed() < Duration::from_millis(10) {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    struct Win {
        el: AxElement,
        home: Point,
        size: Size,
        monitor: usize,
    }

    /// Standard windows with readable geometry. Windows that cannot be read are reported
    /// (with the AX error) and skipped rather than silently dropped.
    fn standard_windows(app: &AxElement) -> Result<Vec<(AxElement, Point, Size)>, String> {
        let mut out = Vec::new();
        for (i, w) in app.windows()?.into_iter().enumerate() {
            match w.is_standard_window() {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    eprintln!("skipping window {i}: {e}");
                    continue;
                }
            }
            match (w.position(), w.size()) {
                (Ok(p), Ok(s)) => out.push((w, p, s)),
                (p, s) => eprintln!(
                    "skipping window {i}: {}",
                    [p.err(), s.err()].into_iter().flatten().collect::<Vec<_>>().join("; ")
                ),
            }
        }
        Ok(out)
    }

    fn describe_windows(screens: &Screens, found: Vec<(AxElement, Point, Size)>) -> Result<Vec<Win>, String> {
        found
            .into_iter()
            .map(|(el, home, size)| {
                let monitor = monitor_for_window(&screens.monitors, Rect { origin: home, size }, screens.primary_height)
                    .ok_or("no displays found")?;
                Ok(Win { el, home, size, monitor })
            })
            .collect()
    }

    fn write_file(path: &Path, contents: &str) -> Result<(), String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()))
    }

    fn park(mtm: MainThreadMarker, a: ParkArgs) -> Result<(), String> {
        let spec = AppSpec::parse(&a.app)?;
        let (pid, name) = apps::resolve(&spec)?;
        let app = AxElement::application(pid);
        let scr = screens::read(mtm);
        let found = standard_windows(&app)?;
        let wins = describe_windows(&scr, found)?;
        let win = wins.into_iter().next().ok_or_else(|| format!("{name} (pid {pid}) has no standard AX window"))?;

        let h = scr.primary_height;
        let mon = scr.monitors[win.monitor];
        let (parked, corner) = park_position(&scr.monitors, win.monitor, win.size, h);
        let mon_ax = flip_rect(mon.frame, h);
        let saved = save_proportional(win.home, mon_ax);
        let restore_to = restore_proportional(saved, mon_ax);
        println!("app: {name} (pid {pid}), title: {:?}", win.el.title());
        println!("monitor {} frame(AX)={:?} primary_height={h}", win.monitor, mon_ax);
        println!("original: {:?} size {:?}", win.home, win.size);
        println!("parking at {corner:?}: {parked:?}; restore target {restore_to:?}");

        guard::register_window(0, win.el.clone(), win.home);
        match app.disable_enhanced_ui() {
            Ok(true) => {
                guard::register_enhanced_ui(app.clone());
                println!("AXEnhancedUserInterface disabled for the burst (restored afterwards)");
            }
            Ok(false) => {}
            Err(e) => eprintln!("warning: could not disable AXEnhancedUserInterface: {e}"),
        }
        win.el.set_position(parked)?;
        std::thread::sleep(Duration::from_millis(a.pause_ms));
        match win.el.position() {
            Ok(actual) if position_mismatch(actual, parked, 2.0) => eprintln!(
                "warning: after parking the window reports {actual:?}, not {parked:?}; the app or macOS clamped the move"
            ),
            Ok(_) => {}
            Err(e) => eprintln!("warning: cannot read back parked position: {e}"),
        }
        if guard::stopping() {
            return Err("interrupted".into());
        }

        let screen = scr.screen(win.monitor).ok_or("screen vanished")?;
        let (link, ticks) = display_link::start(mtm, screen)?;
        println!("tick source: {}", link.kind);

        let anim = Animation { from: parked, to: restore_to, duration: a.duration_ms as f64 / 1000.0 };
        let done = Arc::new(AtomicBool::new(false));
        let done_guard = DoneOnDrop(done.clone());
        let el = win.el.clone();
        let worker = std::thread::spawn(move || {
            let _done = done_guard; // flips `done` even if this thread panics
            let mut ticks = ticks;
            let mut writer = AxWriter { windows: vec![el] };
            run_animation(&mut ticks, &mut writer, &MonotonicClock::new(), &anim)
        });
        pump_until(&done);
        if guard::stopping() {
            drop(link);
            let _ = worker.join();
            return Err("interrupted".into());
        }
        let out = worker.join().map_err(|_| "writer thread panicked")?;
        drop(link);

        println!("tick,timestamp_s,interval_ms,write_ms,ok,failed");
        for s in &out.samples {
            println!(
                "{},{:.6},{},{:.3},{},{}",
                s.index,
                s.timestamp,
                s.interval.map(|i| format!("{:.3}", i * 1000.0)).unwrap_or_default(),
                s.write_total * 1000.0,
                s.writes_ok,
                s.writes_failed
            );
        }
        match summarize(&out.samples, &out.write_latencies) {
            Some(s) => {
                println!(
                    "{} ticks, interval p50 {:.2} ms p95 {:.2} ms p99 {:.2} ms, late {:.1}%, dropped {:.1}%, failed writes {:.1}%, refresh {:.2} ms",
                    s.frames + 1,
                    s.interval_p50 * 1e3,
                    s.interval_p95 * 1e3,
                    s.interval_p99 * 1e3,
                    s.missed_pct,
                    s.dropped_pct,
                    s.failed_write_pct,
                    s.refresh_period * 1e3
                );
                write_file(&a.out_dir.join("park_frames.csv"), &frames_csv(&out.samples, s.budget))?;
            }
            None => println!("too few ticks to summarize"),
        }
        println!("final position: {:?} (target {:?})", win.el.position(), restore_to);
        Ok(())
    }

    fn spawn_target(count: usize) -> Result<(i32, String), String> {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("dowm-spike-target");
        if !exe.exists() {
            return Err(format!("{} not found; run `cargo build --release` first", exe.display()));
        }
        let mut child = Command::new(&exe)
            .process_group(0) // Ctrl-C must not kill the helper before we restore its windows
            .args(["--windows", &count.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("spawn {}: {e}", exe.display()))?;
        let stdout = child.stdout.take().ok_or("no child stdout")?;
        *CHILD.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let line = rx.recv_timeout(Duration::from_secs(15)).map_err(|_| "target did not report READY in 15 s")?;
        let pid: i32 = line
            .trim()
            .strip_prefix("READY ")
            .and_then(|p| p.parse().ok())
            .ok_or_else(|| format!("unexpected target output: {line:?}"))?;
        Ok((pid, "dowm-spike-target".into()))
    }

    fn bench(mtm: MainThreadMarker, a: BenchArgs) -> Result<(), String> {
        let counts = parse_counts(&a.counts)?;
        if !(a.duration > 0.0 && a.half_period > 0.0) {
            return Err("--duration and --half-period must be > 0".into());
        }
        // With --app the same app serves every round; with --spawn-target each round gets a
        // fresh helper with exactly N windows, so only the windows under test are on screen.
        let fixed = if a.spawn_target {
            None
        } else {
            let spec = AppSpec::parse(a.app.as_deref().ok_or("pass --app <name|pid> or --spawn-target")?)?;
            Some(apps::resolve(&spec)?)
        };

        let mut results = Vec::new();
        let mut target = String::new();
        let mut interrupted = false;
        for &n in &counts {
            if guard::stopping() {
                interrupted = true;
                break;
            }
            let (pid, name) = match &fixed {
                Some(app) => app.clone(),
                None => spawn_target(n)?,
            };
            target = format!("{name} (pid {pid})");
            let round = bench_round(mtm, &a, pid, &name, n);
            // Put this round's windows back and forget them before the next round (the
            // helper is about to go away, and an --app's window order may change).
            guard::restore_all();
            guard::forget_windows();
            if a.spawn_target {
                kill_child();
            }
            match round {
                Ok(r) => results.push(r),
                Err(_) if guard::stopping() => {
                    interrupted = true;
                    break;
                }
                Err(e) => results.push(CountResult { count: n, summary: None, note: Some(e), csv_path: None }),
            }
        }
        if results.is_empty() {
            return Err(if interrupted { "interrupted before any round finished".into() } else { "no rounds ran".into() });
        }

        let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let meta = ReportMeta {
            app: if a.spawn_target { "dowm-spike-target (fresh helper per window count)".into() } else { target },
            motion: match a.motion {
                MotionKind::Park => format!("continuous park <-> home cycles, {} s per leg", a.half_period),
                MotionKind::Wiggle => format!("80 px horizontal wiggle, {} s per leg", a.half_period),
            },
            duration_secs: a.duration,
            generated: if interrupted {
                format!("unix time {secs} (INTERRUPTED: partial report, later counts not run)")
            } else {
                format!("unix time {secs}")
            },
        };
        let md = render_report(&meta, &results, &Criterion::default());
        write_file(&a.report, &md)?;
        println!("{md}");
        println!("report written to {}", a.report.display());
        if interrupted {
            return Err("interrupted (partial report written)".into());
        }
        Ok(())
    }

    /// One benchmark round: animate the first `n` standard windows of `pid` for `a.duration`.
    fn bench_round(mtm: MainThreadMarker, a: &BenchArgs, pid: i32, name: &str, n: usize) -> Result<CountResult, String> {
        let app = AxElement::application(pid);

        // A freshly spawned helper answers kAXErrorCannotComplete until its run loop is
        // servicing AX, and its windows appear shortly after — retry both until the deadline.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut found = standard_windows(&app);
        while a.spawn_target
            && found.as_ref().map_or(true, |f| f.len() < n)
            && std::time::Instant::now() < deadline
            && !guard::stopping()
        {
            std::thread::sleep(Duration::from_millis(200));
            found = standard_windows(&app);
        }
        let mut found = found?;
        found.truncate(n);
        let scr = screens::read(mtm);
        let wins = describe_windows(&scr, found)?;
        eprintln!("{name} (pid {pid}): {} usable standard windows (requested {n})", wins.len());
        if wins.len() < n {
            return Ok(CountResult {
                count: n,
                summary: None,
                note: Some(format!("only {} windows available", wins.len())),
                csv_path: None,
            });
        }

        let h = scr.primary_height;
        for (i, w) in wins.iter().enumerate() {
            guard::register_window(i as u64, w.el.clone(), w.home);
        }
        match app.disable_enhanced_ui() {
            Ok(true) => {
                guard::register_enhanced_ui(app.clone());
                eprintln!("AXEnhancedUserInterface disabled for the burst (restored afterwards)");
            }
            Ok(false) => {}
            Err(e) => eprintln!("warning: could not disable AXEnhancedUserInterface: {e}"),
        }
        let motions: Vec<Oscillation> = wins
            .iter()
            .map(|w| {
                let away = match a.motion {
                    MotionKind::Park => park_position(&scr.monitors, w.monitor, w.size, h).0,
                    MotionKind::Wiggle => Point::new(w.home.x + 80.0, w.home.y),
                };
                Oscillation { home: w.home, away, half_period: a.half_period }
            })
            .collect();

        let screen = scr.screen(wins[0].monitor).ok_or("screen vanished")?;
        let (link, ticks) = display_link::start(mtm, screen)?;
        eprintln!("tick source: {}", link.kind);

        let elements: Vec<AxElement> = wins.iter().map(|w| w.el.clone()).collect();
        let (duration, out_dir) = (a.duration, a.out_dir.clone());
        let done = Arc::new(AtomicBool::new(false));
        let done_guard = DoneOnDrop(done.clone());
        let worker = std::thread::spawn(move || -> Result<CountResult, String> {
            let _done = done_guard; // flips `done` even if this thread panics
            let mut ticks = ticks;
            let clock = MonotonicClock::new();
            let mut writer = AxWriter { windows: elements };
            // Let the app settle after discovery and drop stale ticks.
            std::thread::sleep(Duration::from_millis(400));
            ticks.flush();
            eprintln!("benchmarking {n} windows for {duration} s ...");
            let run = run_bench(&mut ticks, &mut writer, &clock, &motions, duration);
            if guard::stopping() {
                return Err("interrupted".into());
            }
            let summary = summarize(&run.samples, &run.write_latencies);
            let csv_path = out_dir.join(format!("frames_N{n}.csv"));
            let budget = summary.as_ref().map(|s| s.budget).unwrap_or(MIN_BUDGET);
            write_file(&csv_path, &frames_csv(&run.samples, budget))?;
            Ok(CountResult {
                count: n,
                note: summary.is_none().then(|| "too few ticks recorded".to_string()),
                summary,
                csv_path: Some(csv_path.display().to_string()),
            })
        });
        pump_until(&done);
        drop(link);
        worker.join().map_err(|_| "bench thread panicked")?
    }
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(e) = app::run() {
        eprintln!("dowm-spike: {e}");
        std::process::exit(2);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
