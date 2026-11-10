//! Kitty graphics presentation of server-composed frames.

mod encoding;
mod presenter;

pub(crate) use presenter::{FRAME_EDIT_PROBE_ID, Presenter, query_frame_edits};
