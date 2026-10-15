//! Crash safety, end to end in a real process.
//!
//! Each test spawns this test binary again as a child (`crash_child`, selected by
//! environment variables). The child registers fake parked windows in a `SharedRegistry`,
//! installs the real termination handler with a restorer that appends every restore to a
//! file, and then ends in the way under test. The parent checks the log: every registered
//! window must have been restored to its on-screen target, and nothing else.
//!
//! No AX call is made and no real window is touched.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dowm::geometry::{Point, Size};
use dowm::restore::{ParkedHandle, SharedRegistry};
use dowm::termination::{self, Restorer, TerminationConfig, Terminator};
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

const MODE_ENV: &str = "DOWM_CRASH_CHILD";
const LOG_ENV: &str = "DOWM_CRASH_LOG";
const SCREEN: (f64, f64) = (1920.0, 1080.0);

/// Fake windows the child parks: (fake pid, restore target).
fn fake_windows() -> Vec<(i32, Point)> {
    vec![
        (2001, Point::new(100.0, 100.0)),
        (2002, Point::new(960.0, 540.0)),
        (2003, Point::new(0.0, 0.0)),
        (2004, Point::new(1500.0, 700.0)),
        (2005, Point::new(321.0, 654.0)),
    ]
}

/// A window parked and then shown again before the end: must never be restored.
const UNPARKED_PID: i32 = 2099;

/// Appends every restore to a file. Groups by pid, like the daemon's restorer.
struct FileRestorer {
    log: PathBuf,
    /// Restoring this pid never finishes (a hung app).
    hang_pid: Option<i32>,
    /// Restoring this pid panics (a bad window).
    panic_pid: Option<i32>,
}

impl Restorer<ParkedHandle> for FileRestorer {
    fn group(&self, handle: &ParkedHandle) -> i64 { i64::from(handle.pid) }

    fn restore(&self, windows: &[(ParkedHandle, Point)]) {
        let mut file =
            fs::OpenOptions::new().create(true).append(true).open(&self.log).expect("open log");
        for (handle, to) in windows {
            if Some(handle.pid) == self.hang_pid {
                std::thread::sleep(Duration::from_secs(60));
            }
            if Some(handle.pid) == self.panic_pid {
                panic!("bad window {}", handle.pid);
            }
            // One write per line: groups restore on parallel threads sharing the file.
            let line = format!("RESTORED {} {} {}\n", handle.pid, to.x, to.y);
            file.write_all(line.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
    }
}

/// The child half. Does nothing unless the parent set the environment.
#[test]
fn crash_child() {
    let Ok(mode) = std::env::var(MODE_ENV) else { return };
    let log = PathBuf::from(std::env::var(LOG_ENV).expect("log path"));

    let registry: Arc<SharedRegistry<ParkedHandle>> = Arc::new(SharedRegistry::new());
    let parked_at = Point::new(-399.0, 1079.0);
    for (pid, target) in fake_windows() {
        let handle = ParkedHandle {
            pid,
            window_id: Some(pid as u32),
            parked: parked_at,
            size: Size::new(400.0, 300.0),
        };
        registry.park(pid as u64, handle, target);
    }
    let handle = ParkedHandle {
        pid: UNPARKED_PID,
        window_id: None,
        parked: parked_at,
        size: Size::new(1.0, 1.0),
    };
    registry.park(UNPARKED_PID as u64, handle, Point::new(5.0, 5.0));
    registry.unpark(UNPARKED_PID as u64);

    let pid_env = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
    let restorer = FileRestorer {
        log,
        hang_pid: pid_env("DOWM_CRASH_HANG_PID"),
        panic_pid: pid_env("DOWM_CRASH_PANIC_PID"),
    };
    let config = TerminationConfig {
        settle: Duration::from_millis(10),
        backstop: Duration::from_secs(2),
        ..TerminationConfig::default()
    };
    if mode == "signal-stalled-stderr" {
        // Fill stderr until a write blocks while holding the stderr lock, like a terminal
        // that stopped draining. Anything that logs after this blocks with it.
        std::thread::spawn(|| {
            let chunk = [b'x'; 4096];
            loop {
                let _ = std::io::stderr().write_all(&chunk);
            }
        });
        std::thread::sleep(Duration::from_millis(300));
    }
    if mode == "signal-abort-hook" {
        // The daemon's hook: any panic ends the process. A restorer panic must not.
        std::panic::set_hook(Box::new(|_| std::process::abort()));
    }
    let terminator = Terminator::new(Arc::clone(&registry), Arc::new(restorer), config);
    let guard = termination::install(terminator).expect("install handlers");

    println!("READY");
    std::io::stdout().flush().unwrap();

    match mode.as_str() {
        "signal" | "signal-abort-hook" | "signal-stalled-stderr" => {
            std::thread::sleep(Duration::from_secs(30));
            std::process::exit(3); // the signal never came
        }
        "exit" => drop(guard),
        "panic-main" => {
            std::mem::forget(guard); // only the panic hook may do the restoring
            panic!("forced panic with windows parked");
        }
        "panic-holding-registry-lock" => {
            std::mem::forget(guard);
            let registry = Arc::clone(&registry);
            let panicked = std::thread::spawn(move || {
                // The panic hook runs while this thread still holds the writer lock.
                registry.update(|_| panic!("forced panic inside the registry's critical section"));
            })
            .join();
            assert!(panicked.is_err());
            std::process::exit(7); // skip destructors: the hook alone must have restored
        }
        other => panic!("unknown mode {other}"),
    }
}

struct Outcome {
    code: Option<i32>,
    restored: Vec<(i32, f64, f64)>,
    elapsed: Duration,
}

#[derive(Default)]
struct Child<'a> {
    signal: Option<&'a str>,
    /// Give the child a stderr whose reader is gone, like a closed terminal.
    broken_stderr: bool,
    /// Give the child a stderr pipe nobody reads, which the child fills until writes block.
    stalled_stderr: bool,
    env: Vec<(&'a str, &'a str)>,
}

fn run_child(mode: &str, name: &str, opts: Child<'_>) -> Outcome {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"));
    fs::create_dir_all(dir).unwrap();
    let log = dir.join(format!("crash_safety_{name}.log"));
    let _ = fs::remove_file(&log);

    let stderr = if opts.stalled_stderr {
        Stdio::piped()
    } else if opts.broken_stderr {
        let (reader, writer) = std::io::pipe().expect("pipe");
        drop(reader);
        Stdio::from(writer)
    } else {
        Stdio::null()
    };
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env(MODE_ENV, mode)
        .env(LOG_ENV, &log)
        .stdout(Stdio::piped())
        .stderr(stderr);
    for (key, value) in &opts.env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn child");
    let _unread_stderr = child.stderr.take(); // held open, never read

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let ready = lines.by_ref().map_while(Result::ok).any(|l| l.trim_end().ends_with("READY"));
    assert!(ready, "child exited before registering its windows");

    let signalled = Instant::now();
    if let Some(signal) = opts.signal {
        let status = Command::new("kill")
            .args(["-s", signal, &child.id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success());
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("child did not exit after {mode} {:?}", opts.signal);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let elapsed = signalled.elapsed();

    let text = fs::read_to_string(&log).unwrap_or_default();
    let restored = text
        .lines()
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(f[0], "RESTORED", "{line}");
            (f[1].parse().unwrap(), f[2].parse().unwrap(), f[3].parse().unwrap())
        })
        .collect();
    Outcome { code: status.code(), restored, elapsed }
}

fn signal(name: &str) -> Child<'_> { Child { signal: Some(name), ..Child::default() } }

/// Every registered window was restored to its registered, on-screen target, and nothing
/// that was unparked was.
fn assert_all_restored_on_screen(outcome: &Outcome) {
    for (pid, target) in fake_windows() {
        let mine: Vec<_> = outcome.restored.iter().filter(|(p, ..)| *p == pid).collect();
        assert!(!mine.is_empty(), "window {pid} was not restored: {:?}", outcome.restored);
        for &&(_, x, y) in &mine {
            assert_eq!((x, y), (target.x, target.y), "window {pid}");
            assert!((0.0..SCREEN.0).contains(&x) && (0.0..SCREEN.1).contains(&y), "off-screen");
        }
    }
    assert!(
        outcome.restored.iter().all(|(p, ..)| *p != UNPARKED_PID),
        "an unparked window was restored: {:?}",
        outcome.restored
    );
}

#[test]
fn every_termination_signal_restores_all_parked_windows_then_exits() {
    for (name, number) in [("TERM", SIGTERM), ("INT", SIGINT), ("HUP", SIGHUP), ("QUIT", SIGQUIT)] {
        let outcome = run_child("signal", &format!("sig_{name}"), signal(name));
        assert_eq!(outcome.code, Some(128 + number), "exit status after SIG{name}");
        assert_all_restored_on_screen(&outcome);
    }
}

#[test]
fn sighup_with_a_dead_terminal_still_restores_everything() {
    // Closing a terminal sends SIGHUP and leaves stderr unwritable. `eprintln!` would panic
    // there, before anything was restored.
    for name in ["HUP", "TERM"] {
        let opts = Child { broken_stderr: true, ..signal(name) };
        let outcome = run_child("signal", &format!("deadtty_{name}"), opts);
        assert!(outcome.code.is_some_and(|c| c >= 128), "exited by the handler: {:?}", outcome.code);
        assert_all_restored_on_screen(&outcome);
    }
}

#[test]
fn a_panic_with_a_dead_terminal_still_restores_everything() {
    let outcome =
        run_child("panic-main", "deadtty_panic", Child { broken_stderr: true, ..Child::default() });
    assert_ne!(outcome.code, Some(0));
    assert_all_restored_on_screen(&outcome);
}

#[test]
fn a_panic_on_the_main_thread_restores_all_parked_windows() {
    let outcome = run_child("panic-main", "panic_main", Child::default());
    assert_ne!(outcome.code, Some(0));
    assert_all_restored_on_screen(&outcome);
}

#[test]
fn a_panic_while_holding_the_registry_lock_restores_without_deadlocking() {
    let outcome = run_child("panic-holding-registry-lock", "panic_locked", Child::default());
    assert_eq!(outcome.code, Some(7));
    assert_all_restored_on_screen(&outcome);
}

#[test]
fn a_normal_exit_restores_all_parked_windows() {
    let outcome = run_child("exit", "normal_exit", Child::default());
    assert_eq!(outcome.code, Some(0));
    assert_all_restored_on_screen(&outcome);
}

#[test]
fn a_hung_app_does_not_stop_the_others_and_the_process_still_exits_in_time() {
    let opts = Child { env: vec![("DOWM_CRASH_HANG_PID", "2001")], ..signal("TERM") };
    let outcome = run_child("signal", "hung_app", opts);
    assert_eq!(outcome.code, Some(128 + SIGTERM));
    assert!(outcome.elapsed < Duration::from_secs(6), "took {:?}", outcome.elapsed);
    for (pid, target) in fake_windows().into_iter().filter(|(pid, _)| *pid != 2001) {
        assert!(
            outcome.restored.iter().any(|&(p, x, y)| p == pid && (x, y) == (target.x, target.y)),
            "window {pid} was not restored: {:?}",
            outcome.restored
        );
    }
    assert!(outcome.restored.iter().all(|(p, ..)| *p != 2001));
}

#[test]
fn a_restorer_panic_is_contained_even_when_the_previous_hook_aborts() {
    // The daemon chains an abort hook. A panic on a restore worker must neither run it nor
    // cost the other windows their restore.
    let opts = Child { env: vec![("DOWM_CRASH_PANIC_PID", "2003")], ..signal("TERM") };
    let outcome = run_child("signal-abort-hook", "restorer_panic", opts);
    assert_eq!(outcome.code, Some(128 + SIGTERM), "exited through the handler, not abort");
    for (pid, target) in fake_windows().into_iter().filter(|(pid, _)| *pid != 2003) {
        assert!(
            outcome.restored.iter().any(|&(p, x, y)| p == pid && (x, y) == (target.x, target.y)),
            "window {pid} was not restored: {:?}",
            outcome.restored
        );
    }
}

#[test]
fn a_stalled_stderr_cannot_hang_the_exit_after_the_restore() {
    // Nobody reads the child's stderr and it is full, so the log line after the restore
    // blocks. The watchdog must stay alive through that and end the process.
    let opts = Child { stalled_stderr: true, ..signal("TERM") };
    let outcome = run_child("signal-stalled-stderr", "stalled_stderr", opts);
    assert!(outcome.elapsed < Duration::from_secs(8), "took {:?}", outcome.elapsed);
    assert_eq!(outcome.code, Some(124), "ended by the watchdog");
    assert_all_restored_on_screen(&outcome);
}
