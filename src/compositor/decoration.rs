//! Negotiate server-side decorations without drawing any compositor chrome.
//!
//! XDG decoration clients and GTK clients using KDE decoration negotiation
//! omit their ordinary titlebars. Explicit app headerbars may remain.

use smithay::{
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode,
        wayland_protocols_misc::server_decoration::server::{
            org_kde_kwin_server_decoration::{Mode as KdeMode, OrgKdeKwinServerDecoration},
            org_kde_kwin_server_decoration_manager::Mode as KdeDefaultMode,
        },
        wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    },
    wayland::shell::{
        kde::decoration::{KdeDecorationHandler, KdeDecorationState},
        xdg::{ToplevelSurface, decoration::XdgDecorationHandler},
    },
};

use super::State;

pub(super) fn kde(display: &DisplayHandle) -> KdeDecorationState {
    KdeDecorationState::new::<State>(display, KdeDefaultMode::Server)
}

impl KdeDecorationHandler for State {
    fn kde_decoration_state(&self) -> &KdeDecorationState {
        &self.kde_decoration
    }

    fn new_decoration(&mut self, _surface: &WlSurface, decoration: &OrgKdeKwinServerDecoration) {
        decoration.mode(KdeMode::Server);
    }
    // The protocol's default request handler acknowledges explicit client
    // preferences, including undecorated popups and app-owned headerbars.
}

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
