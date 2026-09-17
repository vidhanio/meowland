//! What the server and the compositor say to each other.
//!
//! The compositor runs on a thread of its own and holds everything Wayland: the
//! clients, their surfaces, and the frames those add up to. The server holds
//! everything that outlives a terminal: the panes attached to it, the control
//! socket, and the client processes it started. Neither reaches into the other;
//! they send each other these.
//!
//! A pane's frame goes round: the compositor draws into the frame while it has
//! it, and sends it with the tiles that changed; the server hands it to that
//! pane's presenter, which puts the tiles on the terminal and gives the frame
//! back once the terminal has them. So a pane has one frame, and a scene that
//! changes while it is out waits for it: frames are dropped, but the tiles due
//! are not.

use crate::{
    protocol::{
        PaneId, WindowId,
        pane::{Capabilities, Input, Show},
    },
    render::{Frame, Tile},
};

/// What the server tells the compositor.
#[derive(Debug)]
pub enum Command {
    /// Show a window, in a pane that has just attached.
    Attach {
        pane: PaneId,
        show: Show,
        capabilities: Capabilities,
    },
    /// Stop showing anything in this pane: it is on its way out.
    Detach { pane: PaneId },
    /// The terminal this pane is in now reports these capabilities.
    Resized {
        pane: PaneId,
        capabilities: Capabilities,
    },
    /// What the user did in this pane.
    Input { pane: PaneId, input: Input },
    /// Ask every window to close, the way a window manager does: the client is
    /// told its window is wanted gone, and it decides what to do about it. This
    /// is how the server stops, so that a client exits on its own terms, with
    /// the children of its own.
    Close,
    /// A frame the presenter is done with, and its tile list, to be used again.
    Frame {
        pane: PaneId,
        frame: Frame,
        tiles: Vec<Tile>,
    },
}

/// What the compositor tells the server.
#[derive(Debug)]
pub enum Event {
    /// A window exists under these names, or has been renamed.
    Named {
        window: WindowId,
        label: Option<String>,
        title: Option<String>,
    },
    /// The window that has the keyboard changed.
    Focused { window: Option<WindowId> },
    /// A window is gone.
    Closed { window: WindowId },
    /// This pane has nothing left to show, so its terminal is released. The
    /// reason, if there is one, is shown to whoever is at that terminal.
    PaneDone {
        pane: PaneId,
        reason: Option<String>,
    },
    /// What the terminal in this pane should call itself.
    Title { pane: PaneId, title: String },
    /// The pointer shape the terminal in this pane should draw.
    Pointer {
        pane: PaneId,
        shape: Option<&'static str>,
    },
    /// A frame for this pane, and the tiles that changed in it.
    Frame {
        pane: PaneId,
        frame: Frame,
        tiles: Vec<Tile>,
    },
}
