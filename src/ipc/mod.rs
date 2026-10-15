//! Daemon-side IPC: the Unix-socket server, its event bus and the secure socket setup.
//! The wire format lives in the `dowm-protocol` crate.

pub mod backend;
pub mod bus;
pub mod server;
pub mod socket;

pub use backend::ReactorBackend;
pub use bus::EventBus;
pub use server::{Backend, spawn};

#[cfg(test)]
mod e2e_tests;
