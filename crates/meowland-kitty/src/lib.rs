//! Kitty graphics presentation with retained frame bands and optional
//! shared-memory transfers.

mod encoding;
mod presenter;

pub use presenter::{PresentOutcome, Presenter, query_frame_edits, query_shared};
