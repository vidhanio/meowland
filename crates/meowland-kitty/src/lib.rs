//! Kitty graphics presentation with retained frame bands and optional
//! shared-memory transfers.

mod encoding;
mod presenter;

pub use presenter::{PROBE_ID, PresentOutcome, Presenter, SharedMemory, present_shared};
