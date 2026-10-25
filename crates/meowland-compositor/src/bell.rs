//! Application bell requests routed to one terminal pane.

use std::time::{Duration, Instant};

use smithay::{
    reexports::wayland_server::DisplayHandle,
    wayland::xdg_system_bell::{XdgSystemBellHandler, XdgSystemBellState},
};

use super::{ServerToPane, State, WlSurface};

const INTERVAL: Duration = Duration::from_millis(250);

pub struct Bell {
    _protocol: XdgSystemBellState,
    last: Option<Instant>,
}

impl Bell {
    pub fn new(display: &DisplayHandle) -> Self {
        Self {
            _protocol: XdgSystemBellState::new::<State>(display),
            last: None,
        }
    }
}

impl XdgSystemBellHandler for State {
    fn ring(&mut self, surface: Option<WlSurface>) {
        let window = match surface {
            Some(surface) => self.window_for_surface(&surface),
            None => self.catalog.focused(),
        };
        let Some(window) = window else {
            return;
        };
        let Some(pane) = self.attachments.owner(window) else {
            return;
        };
        let now = Instant::now();
        if self
            .bell
            .last
            .is_some_and(|last| now.duration_since(last) < INTERVAL)
        {
            return;
        }
        self.bell.last = Some(now);
        self.emit(pane, ServerToPane::Bell);
    }
}
