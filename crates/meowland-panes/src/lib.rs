//! Pane bindings, window catalog, and selection policy without Wayland.

mod attachment;
mod catalog;
mod pane;
mod window;

pub use attachment::{AttachedPane, Attachments};
pub use catalog::WindowCatalog;
pub use pane::PaneId;
pub use window::WindowId;
