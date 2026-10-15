//! Helper process for `dowm-spike bench --spawn-target`: opens N plain NSWindows so AX
//! writes are cross-process but never touch the user's real apps.
//!
//! Protocol: prints `READY <pid>` on stdout once the windows exist. Exits when stdin
//! reaches EOF (i.e. when the parent bench process exits or crashes).

#[cfg(target_os = "macos")]
fn main() {
    use std::io::{Read, Write};

    use clap::Parser;
    use objc2::MainThreadMarker;
    use objc2::MainThreadOnly;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    #[derive(Parser)]
    #[command(about = "Opens N plain windows for the dowm M0 benchmark")]
    struct Args {
        #[arg(long, default_value_t = 40)]
        windows: usize,
    }
    let args = Args::parse();

    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Resizable;
    let mut keep = Vec::new();
    for i in 0..args.windows {
        let rect = NSRect::new(
            NSPoint::new(80.0 + (i % 10) as f64 * 36.0, 160.0 + (i / 10) as f64 * 36.0),
            NSSize::new(320.0, 200.0),
        );
        let w = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { w.setReleasedWhenClosed(false) };
        w.setTitle(&NSString::from_str(&format!("dowm-target-{i}")));
        w.makeKeyAndOrderFront(None);
        keep.push(w);
    }
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    std::thread::spawn(|| {
        let mut buf = [0u8; 64];
        let mut stdin = std::io::stdin();
        while matches!(stdin.read(&mut buf), Ok(n) if n > 0) {}
        std::process::exit(0);
    });

    // AX requests are serviced by the main run loop: finish launching and spin it once so
    // the parent's first AXWindows query doesn't race us into kAXErrorCannotComplete.
    app.finishLaunching();
    objc2_foundation::NSRunLoop::currentRunLoop()
        .runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.3));

    println!("READY {}", std::process::id());
    let _ = std::io::stdout().flush();
    app.run();
    drop(keep);
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
