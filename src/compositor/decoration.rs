//! Negotiate server-side decorations without drawing any compositor chrome.
//!
//! Clients supporting xdg-decoration then omit their own titlebars. Clients
//! that do not negotiate decorations may still draw client-side headerbars.

use smithay::{
    reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
    wayland::shell::xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
};

use super::State;

fn borderless(toplevel: &ToplevelSurface) {
    toplevel.with_pending_state(|pending| pending.decoration_mode = Some(Mode::ServerSide));
    // Mode requests require an xdg_surface.configure response even when our
    // preferred mode is unchanged, so use send_configure, not
    // send_pending_configure.
    toplevel.send_configure();
}

impl XdgDecorationHandler for State {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        borderless(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: Mode) {
        borderless(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        borderless(&toplevel);
    }
}
