//! Reactor-owned state shared with other actors: displays, windows, workspaces, and
//! the per-workspace tiling trees (`tree`, kept in step with membership by `tree_store`).

pub mod display;
pub mod tree;
pub mod tree_store;
pub mod tx_store;
pub mod visibility;
pub mod window_store;
pub mod workspace;
