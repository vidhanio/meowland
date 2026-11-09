//! Kitty graphics presentation with retained frame bands and optional
//! shared-memory transfers.

mod encoding;
mod presenter;

pub use presenter::{
    FRAME_EDIT_PROBE_ID, PresentOutcome, Presenter, SCREEN_ID, SHARED_PROBE_ID, query_frame_edits,
    query_shared,
};
