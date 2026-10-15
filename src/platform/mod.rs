//! Thin macOS FFI layer. Everything here is intentionally logic-free: the behaviour lives
//! in the pure modules and is tested there with fakes. This layer is verified manually.

pub mod apps;
pub mod ax;
pub mod ax_restorer;
pub mod display_link;
pub mod guard;
pub mod screens;

use std::time::Instant;

use crate::geometry::Point;
use crate::runner::{Clock, WindowWriter, WriteError};

/// Monotonic wall clock for timing AX writes.
pub struct MonotonicClock(Instant);

impl MonotonicClock {
    pub fn new() -> Self {
        Self(Instant::now())
    }
}

impl Default for MonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> f64 {
        self.0.elapsed().as_secs_f64()
    }
}

/// Writes `AXPosition` of a fixed list of windows (cross-process IPC).
pub struct AxWriter {
    pub windows: Vec<ax::AxElement>,
}

impl WindowWriter for AxWriter {
    fn set_position(&mut self, window: usize, position: Point) -> Result<(), WriteError> {
        self.windows
            .get(window)
            .ok_or_else(|| WriteError(format!("no window {window}")))?
            .set_position(position)
            .map_err(WriteError)
    }
}

/// Sets the flag when dropped, including during a panic unwind, so the main thread's
/// run-loop pump can never wait forever on a dead worker.
pub struct DoneOnDrop(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Holds an `NSProcessInfo` activity assertion (App Nap / timer coalescing off) for its
/// lifetime. Not `Send`: keep it on the main thread.
pub struct Activity(objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>);

impl Activity {
    pub fn begin(reason: &str) -> Self {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        let opts = NSActivityOptions::UserInitiated | NSActivityOptions::LatencyCritical;
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(opts, &NSString::from_str(reason));
        Self(token)
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        unsafe { objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.0) };
    }
}
