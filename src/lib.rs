//! dowm library.
//!
//! - M0 spike: pure logic (geometry, easing, stats, reporting, loop drivers) plus a
//!   thin macOS FFI layer in `platform`.
//! - M1 engine, ported from rift (Apache-2.0, see `NOTICE`): the OS layer in `sys`,
//!   the per-app actor and presenter in `actor`.

pub mod anim;
pub mod appspec;
pub mod criterion;
pub mod easing;
pub mod frame_writer;
pub mod geometry;
pub mod parking;
pub mod report;
pub mod restore;
pub mod runner;
pub mod stats;
pub mod termination;

#[cfg(target_os = "macos")]
pub mod platform;

#[cfg(target_os = "macos")]
pub mod actor;
#[cfg(target_os = "macos")]
pub mod common;
#[cfg(target_os = "macos")]
pub mod model;
#[cfg(target_os = "macos")]
pub mod sys;

