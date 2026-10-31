//! Pane bindings, window catalog, and selection policy without Wayland.

mod attachment;
mod catalog;
mod window;

pub use attachment::{AttachedPane, Attachments, PaneId, Presentation};
pub use catalog::WindowCatalog;
pub use window::WindowId;
