//! dowm library.
//!
//! - M0 spike: pure logic (geometry, easing, stats, reporting, loop drivers) plus a
//!   thin macOS FFI layer in `platform`.
//! - M1 engine, ported from rift (Apache-2.0, see `NOTICE`): the OS layer in `sys`,
//!   the per-app actor and presenter in `actor`.
//! - M3 tiling: the per-workspace tree in `model::tree` / `model::tree_store`, and the
//!   `LayoutSystem` trait with its three systems (tiles, accordion, floating) and the
//!   tiling commands in `layout`. Pure logic; the reactor wires it in.
//! - M4 config: AeroSpace-compatible TOML (`config`), hotkeys and binding modes,
//!   `on-window-detected` rules, callbacks, gaps. Pure logic; the reactor applies it
//!   and the platform actors in `actor::input` carry out its effects.

pub mod anim;
pub mod cli;
pub mod appspec;
pub mod criterion;
pub mod easing;
pub mod frame_writer;
pub mod geometry;
#[cfg(target_os = "macos")]
pub mod ipc;
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
pub mod config;
#[cfg(target_os = "macos")]
pub mod layout;
#[cfg(target_os = "macos")]
pub mod model;
#[cfg(target_os = "macos")]
pub mod sys;

