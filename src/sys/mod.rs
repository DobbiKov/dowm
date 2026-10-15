//! Utilities for interfacing with OS-specific APIs.
//!
//! Ported from rift (Apache-2.0); see `NOTICE`. Native Spaces are not used by dowm:
//! `screen` enumerates displays only, and `skylight`/`window_server` neither declare
//! nor call space queries. (`skylight::KnownCGSEvent` still names rift's space
//! notification codes; they are numbers, not calls.)

use objc2_core_graphics::CGError;

// The `allow`s below cover upstream rift style in otherwise verbatim code, so diffs
// against rift stay small. New dowm code in these modules should not rely on them.
#[allow(clippy::unnecessary_lazy_evaluations)]
pub mod app;
#[allow(clippy::explicit_auto_deref, clippy::identity_op, clippy::missing_safety_doc)]
pub mod axuielement;
#[cfg(feature = "custom-event-loop")]
#[allow(clippy::items_after_test_module)]
mod cocoa;
#[allow(clippy::unnecessary_cast)]
pub mod dispatch;
pub mod display_link;
pub mod enhanced_ui;
pub mod event;
#[allow(clippy::collapsible_if, clippy::missing_safety_doc, clippy::too_many_arguments)]
pub mod event_tap;
#[allow(clippy::bool_assert_comparison)]
pub mod executor;
pub mod geometry;
// The `Arc`s here only feed CF retain/release; their contents stay on the run-loop
// thread and are `!Send` by construction.
#[allow(clippy::arc_with_non_send_sync, clippy::new_ret_no_self)]
pub mod observer;
pub mod power;
#[allow(clippy::field_reassign_with_default)]
pub mod process;
pub mod run_loop;
#[allow(clippy::collapsible_if, clippy::needless_return, clippy::result_unit_err)]
pub mod screen;
pub mod skylight;
// See `observer` for `arc_with_non_send_sync`.
#[allow(clippy::arc_with_non_send_sync, clippy::should_implement_trait)]
pub mod timer;
#[allow(
    clippy::needless_lifetimes,
    clippy::needless_return,
    clippy::redundant_pattern_matching,
    clippy::unnecessary_cast
)]
pub mod window_server;
pub mod window_notify;

#[inline(always)]
pub fn cg_ok(err: CGError) -> Result<(), CGError> {
    if err == CGError::Success {
        Ok(())
    } else {
        Err(err)
    }
}
