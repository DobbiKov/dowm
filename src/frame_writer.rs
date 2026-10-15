//! The frame write protocol and its app-clamp fallback, behind a seam.
//!
//! Some apps clamp a window's size when its position changes, so a frame is written
//! size, then position, then size again (AeroSpace `MacApp.swift:411-419`). An app can also
//! refuse or clamp the write outright (a minimum size, a position kept on-screen), so the
//! frame is read back afterwards; if it differs, the sequence is retried once and whatever
//! the app settled on is accepted. A switch never fails because one window would not
//! comply: the outcome is reported for the caller to log, nothing more.
//!
//! [`FrameIo`] is the seam: the app actor implements it over AX, tests over a fake app
//! that clamps.

use crate::geometry::{Point, Rect, Size};

/// How far a read-back frame may differ from the request, per coordinate, and still count
/// as written. AX reports whole pixels; this absorbs rounding.
pub const FRAME_TOLERANCE: f64 = 1.0;

/// One window's frame, as the protocol reads and writes it.
pub trait FrameIo {
    fn set_size(&mut self, size: Size) -> Result<(), String>;
    fn set_position(&mut self, position: Point) -> Result<(), String>;
    fn frame(&mut self) -> Result<Rect, String>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum WriteOutcome {
    /// The first attempt read back as requested.
    Exact,
    /// The first attempt did not take; the retry did.
    RetriedExact,
    /// The app refused or clamped the frame twice; `actual` is what it settled on.
    Clamped { requested: Rect, actual: Rect },
    /// The frame could not be read back, so nothing is known about the result.
    Unverified { error: String },
}

impl WriteOutcome {
    /// Whether the window ended up where it was asked to be.
    pub fn is_exact(&self) -> bool { matches!(self, Self::Exact | Self::RetriedExact) }
}

fn close(a: f64, b: f64) -> bool { (a - b).abs() <= FRAME_TOLERANCE }

fn matches(actual: Rect, requested: Rect) -> bool {
    close(actual.origin.x, requested.origin.x)
        && close(actual.origin.y, requested.origin.y)
        && close(actual.size.w, requested.size.w)
        && close(actual.size.h, requested.size.h)
}

/// One pass of the protocol. Write errors are not fatal: the read-back decides.
fn attempt<I: FrameIo>(io: &mut I, target: Rect) -> Result<Rect, String> {
    let first_error = [
        io.set_size(target.size),
        io.set_position(target.origin),
        io.set_size(target.size),
    ]
    .into_iter()
    .find_map(Result::err);
    io.frame().map_err(|e| first_error.unwrap_or(e))
}

/// Write `target` as size, position, size; verify; retry once; accept the result.
pub fn write_frame<I: FrameIo>(io: &mut I, target: Rect) -> WriteOutcome {
    let actual = match attempt(io, target) {
        Ok(actual) if matches(actual, target) => return WriteOutcome::Exact,
        Ok(_) => attempt(io, target),
        Err(_) => attempt(io, target),
    };
    match actual {
        Ok(actual) if matches(actual, target) => WriteOutcome::RetriedExact,
        Ok(actual) => WriteOutcome::Clamped { requested: target, actual },
        Err(error) => WriteOutcome::Unverified { error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Clone, Copy)]
    enum Call {
        Size(Size),
        Position(Point),
        Frame,
    }

    /// A fake app window with the misbehaviours real apps have.
    struct FakeApp {
        frame: Rect,
        calls: Vec<Call>,
        /// Sizes below this are raised to it on every size write.
        min_size: Size,
        /// After a position write the size is capped to this (the move-time clamp the
        /// trailing size write exists to undo).
        size_cap_on_move: Option<Size>,
        /// The first n position writes are ignored.
        ignore_positions: usize,
        /// Position writes land here instead (an app that keeps itself on-screen).
        position_override: Option<Point>,
        fail_writes: bool,
        fail_readback: bool,
    }

    impl FakeApp {
        fn new(frame: Rect) -> Self {
            FakeApp {
                frame,
                calls: Vec::new(),
                min_size: Size::new(0.0, 0.0),
                size_cap_on_move: None,
                ignore_positions: 0,
                position_override: None,
                fail_writes: false,
                fail_readback: false,
            }
        }

        fn writes(&self) -> Vec<Call> {
            self.calls.iter().copied().filter(|c| *c != Call::Frame).collect()
        }
    }

    impl FrameIo for FakeApp {
        fn set_size(&mut self, size: Size) -> Result<(), String> {
            self.calls.push(Call::Size(size));
            self.frame.size = Size::new(size.w.max(self.min_size.w), size.h.max(self.min_size.h));
            if self.fail_writes { Err("size refused".into()) } else { Ok(()) }
        }

        fn set_position(&mut self, position: Point) -> Result<(), String> {
            self.calls.push(Call::Position(position));
            if self.ignore_positions > 0 {
                self.ignore_positions -= 1;
            } else {
                self.frame.origin = self.position_override.unwrap_or(position);
                if let Some(cap) = self.size_cap_on_move {
                    self.frame.size = Size::new(self.frame.size.w.min(cap.w), self.frame.size.h.min(cap.h));
                }
            }
            if self.fail_writes { Err("position refused".into()) } else { Ok(()) }
        }

        fn frame(&mut self) -> Result<Rect, String> {
            self.calls.push(Call::Frame);
            if self.fail_readback { Err("no frame".into()) } else { Ok(self.frame) }
        }
    }

    const START: Rect = Rect::new(100.0, 100.0, 400.0, 300.0);
    const PARKED: Rect = Rect::new(-399.0, 1079.0, 400.0, 300.0);

    #[test]
    fn writes_size_then_position_then_size_and_verifies_once() {
        let mut app = FakeApp::new(START);
        assert_eq!(write_frame(&mut app, PARKED), WriteOutcome::Exact);
        assert_eq!(
            app.calls,
            vec![
                Call::Size(PARKED.size),
                Call::Position(PARKED.origin),
                Call::Size(PARKED.size),
                Call::Frame,
            ]
        );
        assert_eq!(app.frame, PARKED);
    }

    #[test]
    fn the_trailing_size_write_undoes_a_clamp_applied_by_the_move() {
        let mut app = FakeApp::new(START);
        app.size_cap_on_move = Some(Size::new(120.0, 90.0));
        let outcome = write_frame(&mut app, PARKED);
        assert_eq!(outcome, WriteOutcome::Exact, "size restored by the third write");
        assert_eq!(app.frame.size, PARKED.size);
        assert_eq!(app.writes().len(), 3, "no retry was needed");
    }

    #[test]
    fn a_write_the_app_ignored_is_retried_once() {
        let mut app = FakeApp::new(START);
        app.ignore_positions = 1;
        assert_eq!(write_frame(&mut app, PARKED), WriteOutcome::RetriedExact);
        assert_eq!(app.writes().len(), 6);
        assert_eq!(app.frame, PARKED);
    }

    #[test]
    fn a_frame_the_app_keeps_clamping_is_retried_once_then_accepted() {
        let mut app = FakeApp::new(START);
        app.position_override = Some(Point::new(-100.0, 700.0)); // keeps itself partly on-screen
        let outcome = write_frame(&mut app, PARKED);
        let actual = Rect { origin: Point::new(-100.0, 700.0), size: PARKED.size };
        assert_eq!(outcome, WriteOutcome::Clamped { requested: PARKED, actual });
        assert!(!outcome.is_exact());
        assert_eq!(app.writes().len(), 6, "exactly one retry");
        assert_eq!(app.calls.iter().filter(|c| **c == Call::Frame).count(), 2);
    }

    #[test]
    fn a_minimum_size_larger_than_requested_is_accepted_as_clamped() {
        let mut app = FakeApp::new(START);
        app.min_size = Size::new(500.0, 100.0);
        let target = Rect::new(10.0, 10.0, 200.0, 100.0);
        let WriteOutcome::Clamped { requested, actual } = write_frame(&mut app, target) else {
            panic!("expected a clamped outcome");
        };
        assert_eq!(requested, target);
        assert_eq!(actual.size, Size::new(500.0, 100.0));
        assert_eq!(actual.origin, target.origin);
    }

    #[test]
    fn rounding_within_the_tolerance_counts_as_written() {
        let mut app = FakeApp::new(START);
        app.position_override = Some(Point::new(PARKED.origin.x + 1.0, PARKED.origin.y - 1.0));
        assert_eq!(write_frame(&mut app, PARKED), WriteOutcome::Exact);
    }

    #[test]
    fn write_errors_alone_do_not_fail_the_write() {
        // Every write reports an error but the frame lands: the read-back decides.
        let mut app = FakeApp::new(START);
        app.fail_writes = true;
        assert_eq!(write_frame(&mut app, PARKED), WriteOutcome::Exact);
    }

    #[test]
    fn an_unreadable_frame_is_reported_not_raised() {
        let mut app = FakeApp::new(START);
        app.fail_readback = true;
        let outcome = write_frame(&mut app, PARKED);
        assert_eq!(outcome, WriteOutcome::Unverified { error: "no frame".into() });
        assert!(!outcome.is_exact());
        assert_eq!(app.writes().len(), 6, "still retried once");
    }

    #[test]
    fn a_write_error_is_the_reason_when_the_readback_also_fails() {
        let mut app = FakeApp::new(START);
        app.fail_writes = true;
        app.fail_readback = true;
        assert_eq!(
            write_frame(&mut app, PARKED),
            WriteOutcome::Unverified { error: "size refused".into() }
        );
    }
}
