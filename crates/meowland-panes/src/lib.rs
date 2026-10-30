//! Pane bindings, window catalog, and selection policy without Wayland.

mod attachment;
mod catalog;
mod selection;

pub use attachment::{AttachedPane, Attachments, PaneId, Presentation};
pub use catalog::WindowCatalog;
pub use selection::{Matcher, Selection, WindowId};
