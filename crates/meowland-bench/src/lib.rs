//! End-to-end harness for Meowland: an isolated `meowland server`, a pane
//! process on a pseudo-terminal, the `meowland-workload` Wayland SHM fixture,
//! and a headless Kitty-graphics endpoint that decodes and validates every
//! frame the pane writes.
//!
//! The workload and the endpoint share [`pattern`] and [`control`], so the
//! pixels and timestamps a frame is produced with are exactly those it is
//! validated and measured against.

pub mod binaries;
pub mod control;
pub mod endpoint;
pub mod pattern;
pub mod session;
