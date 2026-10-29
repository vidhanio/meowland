//! Pane bindings, window catalog, and selection policy without Wayland.

mod attachment;
mod catalog;

pub use attachment::{AttachedPane, Attachments, PaneId, Policy, Presentation, Selection};
pub use catalog::WindowCatalog;
