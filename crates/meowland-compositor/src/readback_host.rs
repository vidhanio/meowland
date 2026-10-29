//! Compositor hooks for the DMA-BUF readback state machine.

use meowland_loop::Handle;
use meowland_readback::{Host, Readbacks};
use meowland_render::{DmabufBackend, Snapshot};
use smithay::reexports::wayland_server::{Client, protocol::wl_surface::WlSurface};

use super::{ClientState, Compositor};

impl Host for Compositor {
    fn handle(&self) -> &Handle<Self> {
        &self.handle
    }

    fn readbacks(&mut self) -> &mut Readbacks<Self> {
        &mut self.readbacks
    }

    fn gpu(&mut self) -> Option<&mut DmabufBackend> {
        self.gpu.as_mut()
    }

    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)> {
        Self::pane_bound(self, surface)
    }

    fn take_spare(&mut self, surface: &WlSurface) -> Option<Snapshot> {
        self.windows.surface_cache.spares.remove(surface)
    }

    fn return_spare(&mut self, surface: WlSurface, snapshot: Snapshot) {
        self.windows.surface_cache.spares.insert(surface, snapshot);
    }

    fn notify_client(&mut self, client: &Client) {
        if let Some(client_state) = client.get_data::<ClientState>() {
            let display = self.display.clone();
            client_state
                .compositor_state
                .blocker_cleared(self, &display);
        }
    }

    fn request_shutdown(&mut self) {
        self.shutdown = true;
    }
}
