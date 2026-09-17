//! The compositor's Wayland state: its windows, their surfaces, and the panes
//! that show them.

mod compose;
mod input;
mod snapshot;

use std::{
    collections::HashMap,
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

use evdev::KeyCode;
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_output, delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorImageStatus, PointerHandle},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel::State as XdgState,
        wayland_server::{
            Client, DisplayHandle, Resource as _,
            backend::{ClientData, ClientId, DisconnectReason, ObjectId},
            protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, SERIAL_COUNTER, Serial, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        viewporter::ViewporterState,
    },
};

use self::compose::{placement, send_frame_callbacks, tile_size};
use crate::{
    Error, kitty,
    protocol::{
        PaneId, WindowId, control,
        pane::{Capabilities, Show},
    },
    render::{BYTES, Frame, Tiles},
    server::presenter::Presenter,
    wayland::buffer::Snapshot,
};

pub const REFRESH_MILLIHZ: i32 = 60_000;

const TILE_CELLS: (u32, u32) = (16, 8);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    pub tiles: usize,
    pub sent: usize,
    pub compose: Duration,
}

#[derive(Debug)]
struct PendingTiles {
    flags: Vec<bool>,
    count: usize,
}

impl PendingTiles {
    fn new(count: usize) -> Self {
        Self {
            flags: vec![false; count],
            count: 0,
        }
    }

    fn mark(&mut self, index: usize) {
        if !self.flags[index] {
            self.flags[index] = true;
            self.count += 1;
        }
    }

    const fn is_empty(&self) -> bool {
        self.count == 0
    }

    const fn len(&self) -> usize {
        self.count
    }

    fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.flags
            .iter()
            .enumerate()
            .filter_map(|(index, due)| due.then_some(index))
    }

    fn clear(&mut self) {
        self.flags.fill(false);
        self.count = 0;
    }
}

/// Per-pane rendering state.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each of these is an independent thing about one pane"
)]
#[derive(Debug)]
pub struct View {
    id: PaneId,
    capabilities: Capabilities,
    window: Option<WindowId>,
    follow: bool,
    frame: Frame,
    tiles: Tiles,
    pending: PendingTiles,
    dirty: Vec<usize>,
    scene_dirty: bool,
    escapes_dirty: bool,
    pointer_shape: Option<&'static str>,
    title: Option<String>,
    detaching: bool,
    done: bool,
}

impl View {
    fn new(id: PaneId, capabilities: &Capabilities) -> Self {
        let frame = Frame::new(capabilities.pixels.0, capabilities.pixels.1);
        let tiles = Tiles::new(&frame, tile_size(capabilities.cell));
        Self {
            id,
            capabilities: capabilities.clone(),
            window: None,
            follow: true,
            pending: PendingTiles::new(tiles.tile_count()),
            tiles,
            frame,
            dirty: Vec::new(),
            scene_dirty: true,
            escapes_dirty: false,
            pointer_shape: None,
            title: None,
            detaching: false,
            done: false,
        }
    }

    const fn should_present(&self, presenter_ready: bool) -> bool {
        self.scene_dirty || self.escapes_dirty || (!self.pending.is_empty() && presenter_ready)
    }

    fn index(&self, windows: &[Window]) -> Option<usize> {
        let id = self.window?;
        windows.iter().position(|window| window.id == id)
    }
}

#[derive(Debug)]
struct Window {
    id: WindowId,
    surface: ToplevelSurface,
    label: Option<String>,
    title: Option<String>,
    entered: bool,
    /// The pane whose interaction most recently selected this window.
    last_interacted: Option<PaneId>,
    /// Sent back in the configure state.
    fullscreen: bool,
}

pub struct Meowland {
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Self>,
    dmabuf_state: DmabufState,
    /// Kept alive so the render nodes stay described to clients; never read.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    dmabuf_global: Option<DmabufGlobal>,
    gpu: Option<crate::wayland::gpu::Renderer>,

    /// Kept alive so the `zxdg_output_manager_v1` global stays advertised.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    output_manager_state: OutputManagerState,
    data_device_state: DataDeviceState,
    /// Keeps the cursor-shape global alive.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    cursor_shape_state: CursorShapeManagerState,
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    viewporter_state: ViewporterState,
    popup_manager: PopupManager,

    display_handle: DisplayHandle,
    start: Instant,
    output: Output,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,

    // User-facing IDs are monotonic. Wayland object IDs are scoped to one client
    // connection and may be reused, so they are not stable handles.
    windows: Vec<Window>,
    active: Option<usize>,
    next_window_id: WindowId,

    pressed: Vec<KeyCode>,
    pointer_position: Point<f64, Logical>,
    cursor: CursorImageStatus,

    /// Attach order determines which pane sizes the output.
    views: Vec<View>,

    snapshots: HashMap<ObjectId, Snapshot>,
    plan: Vec<(WlSurface, Point<i32, Logical>)>,
}

impl std::fmt::Debug for Meowland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meowland")
            .field("windows", &self.windows.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl Meowland {
    pub fn new(
        display: &DisplayHandle,
        nodes: &[crate::dmabuf::RenderNode],
    ) -> Result<Self, Error> {
        let compositor_state = CompositorState::new::<Self>(display);
        let shm_state = ShmState::new::<Self>(display, []);
        let xdg_shell_state = XdgShellState::new::<Self>(display);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(display);
        let data_device_state = DataDeviceState::new::<Self>(display);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(display);
        let viewporter_state = ViewporterState::new::<Self>(display);
        let mut seat_state = SeatState::new();
        let mut dmabuf_state = DmabufState::new();

        // The output is described after the first terminal attaches: it is as
        // big as the pane that shows it. Until then windows wait for
        // their first configure.
        let output = Output::new(
            "meowland".into(),
            PhysicalProperties {
                // Reported as a ~96 DPI monitor, about what a terminal font is.
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "meowland".into(),
                model: "terminal".into(),
            },
        );
        let _global = output.create_global::<Self>(display);
        output.change_current_state(
            None,
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );

        let mut seat = seat_state.new_wl_seat(display, "meowland");
        // Match the key codes generated by `crate::keys`.
        let keyboard = seat
            .add_keyboard(
                XkbConfig {
                    rules: "evdev",
                    model: "pc105",
                    layout: "us",
                    variant: "",
                    options: None,
                },
                250,
                30,
            )
            .map_err(Error::Keymap)?;
        let pointer = seat.add_pointer();

        let (gpu, dmabuf_global) = match bring_up_renderer(nodes) {
            Some((renderer, node)) => {
                let global =
                    advertise_render_nodes(display, &mut dmabuf_state, Some((&renderer, &node)))?;
                (Some(renderer), global)
            }
            None => (None, None),
        };

        Ok(Self {
            compositor_state,
            shm_state,
            xdg_shell_state,
            seat_state,
            dmabuf_state,
            dmabuf_global,
            gpu,
            output_manager_state,
            data_device_state,
            cursor_shape_state,
            viewporter_state,
            popup_manager: PopupManager::default(),
            display_handle: display.clone(),
            start: Instant::now(),
            output,
            keyboard,
            pointer,
            windows: Vec::new(),
            active: None,
            next_window_id: WindowId::new(1),
            pressed: Vec::new(),
            pointer_position: (0.0, 0.0).into(),
            cursor: CursorImageStatus::default_named(),
            views: Vec::new(),
            snapshots: HashMap::new(),
            plan: Vec::new(),
        })
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn insert_client(&mut self, stream: UnixStream) -> std::io::Result<()> {
        self.display_handle
            .insert_client(stream, Arc::new(MeowlandClient::default()))?;
        Ok(())
    }

    fn active_surface(&self) -> Option<WlSurface> {
        self.active
            .and_then(|index| self.windows.get(index))
            .map(|window| window.surface.wl_surface().clone())
    }

    pub fn windows(&self) -> impl Iterator<Item = control::Window> + '_ {
        self.windows
            .iter()
            .enumerate()
            .map(|(index, window)| control::Window {
                id: window.id,
                label: window.label.clone().unwrap_or_default(),
                title: window.title.clone().unwrap_or_default(),
                active: self.active == Some(index),
            })
    }

    pub fn active_window(&self) -> Option<WindowId> {
        self.active.map(|index| self.windows[index].id)
    }

    fn activate_index(&mut self, index: usize) {
        if self.active == Some(index) || index >= self.windows.len() {
            return;
        }
        self.active = Some(index);
        let surface = self.active_surface();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
        self.configure_windows();
        // Every pane showing it draws again, because the state the client sees
        // changed.
        let id = self.windows[index].id;
        tracing::debug!(id = %id, "focused");
        for view in self.views_of(id) {
            view.scene_dirty = true;
        }
    }

    pub fn attach_view(&mut self, id: PaneId, show: Show, capabilities: &Capabilities) {
        let (window, follow) = self.resolve(show);
        let mut view = View::new(id, capabilities);
        view.window = window;
        view.follow = follow;
        if self.views.is_empty() {
            self.describe_output(capabilities);
        }
        tracing::info!(id = %id, ?window, follow, "pane attached");
        self.views.push(view);
        self.configure_windows();
        self.sync_outputs();
    }

    pub fn detach_view(&mut self, id: PaneId) {
        self.views.retain(|view| view.id != id);
        for window in &mut self.windows {
            if window.last_interacted == Some(id) {
                window.last_interacted = None;
            }
        }
        tracing::info!(id = %id, "pane gone");
        self.configure_windows();
        self.sync_outputs();
    }

    pub fn resize_view(&mut self, id: PaneId, capabilities: &Capabilities) {
        let Some(index) = self.view(id) else {
            return;
        };
        let first = index == 0;
        let view = &mut self.views[index];
        view.capabilities.clone_from(capabilities);
        view.frame
            .resize(capabilities.pixels.0, capabilities.pixels.1);
        view.tiles = Tiles::new(&view.frame, tile_size(capabilities.cell));
        view.pending = PendingTiles::new(view.tiles.tile_count());
        view.scene_dirty = true;
        if first {
            self.describe_output(capabilities);
        }
        self.configure_windows();
    }

    pub fn has_window(&self, id: WindowId) -> bool {
        self.windows.iter().any(|window| window.id == id)
    }

    fn view(&self, id: PaneId) -> Option<usize> {
        self.views.iter().position(|view| view.id == id)
    }

    fn views_of(&mut self, window: WindowId) -> impl Iterator<Item = &mut View> {
        self.views
            .iter_mut()
            .filter(move |view| view.window == Some(window))
    }

    fn resolve(&self, show: Show) -> (Option<WindowId>, bool) {
        match show {
            Show::Window(id) => (Some(id), false),
            Show::Newest => (self.windows.last().map(|window| window.id), true),
            Show::Focused => self.active_window().map_or_else(
                || (self.windows.last().map(|window| window.id), true),
                |id| (Some(id), false),
            ),
        }
    }

    fn describe_output(&self, capabilities: &Capabilities) {
        let mode = output_mode(capabilities);
        self.output.set_preferred(mode);
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
    }

    fn sync_outputs(&mut self) {
        for index in 0..self.windows.len() {
            let id = self.windows[index].id;
            let shown = self.views.iter().any(|view| view.window == Some(id));
            if shown == self.windows[index].entered {
                continue;
            }
            self.windows[index].entered = shown;
            let surface = self.windows[index].surface.wl_surface().clone();
            if shown {
                self.output.enter(&surface);
            } else {
                self.output.leave(&surface);
            }
        }
    }

    pub fn should_present_view(&self, id: PaneId, presenter_ready: bool) -> bool {
        self.view(id)
            .is_some_and(|index| self.views[index].should_present(presenter_ready))
    }

    pub fn take_detach_request(&mut self, id: PaneId) -> bool {
        let Some(index) = self.view(id) else {
            return false;
        };
        std::mem::replace(&mut self.views[index].detaching, false)
    }

    pub fn take_closed_views(&mut self) -> Vec<PaneId> {
        let mut closed = Vec::new();
        for view in &mut self.views {
            if std::mem::replace(&mut view.done, false) {
                closed.push(view.id);
            }
        }
        closed
    }

    fn index_of(&self, surface: &WlSurface) -> Option<usize> {
        self.windows
            .iter()
            .position(|window| window.surface.wl_surface() == surface)
    }

    /// Send `xdg_toplevel.close` to this pane's window.
    fn close_window(&self, pane: PaneId) {
        let Some(index) = self.pane_window(pane) else {
            return;
        };
        let window = &self.windows[index];
        tracing::info!(id = %window.id, pane = %pane, "asked to close");
        window.surface.send_close();
    }

    fn pane_window(&self, pane: PaneId) -> Option<usize> {
        let index = self.view(pane)?;
        self.views[index].index(&self.windows)
    }

    fn focus_pane(&mut self, pane: PaneId) -> Option<usize> {
        let index = self.view(pane)?;
        if let Some(window) = self.views[index].index(&self.windows) {
            let changed = self.windows[window].last_interacted != Some(pane);
            self.windows[window].last_interacted = Some(pane);
            if self.active == Some(window) {
                if changed {
                    self.configure_windows();
                }
            } else {
                self.activate_index(window);
            }
        }
        Some(index)
    }

    /// Record that a terminal pane was actively used.
    pub fn interact(&mut self, pane: PaneId) {
        let _ = self.focus_pane(pane);
    }

    fn relabel(&mut self, surface: &ToplevelSurface) {
        let (label, title) = window_names(surface);
        tracing::debug!(?label, ?title, "the client named its window");
        let Some(index) = self.index_of(surface.wl_surface()) else {
            return;
        };
        let window = &mut self.windows[index];
        let renamed = window.title != title;
        window.label = label.or_else(|| title.clone());
        window.title = title;
        if renamed {
            let id = window.id;
            for view in self.views_of(id) {
                view.escapes_dirty = true;
            }
        }
    }

    fn configured_view(
        views: &[View],
        window: WindowId,
        last_interacted: Option<PaneId>,
    ) -> Option<&View> {
        last_interacted
            .and_then(|id| {
                views
                    .iter()
                    .find(|view| view.id == id && view.window == Some(window))
            })
            .or_else(|| views.iter().find(|view| view.window == Some(window)))
    }

    /// Configure each window from the pane that most recently interacted with
    /// it, falling back to the first attached pane.
    fn configure_windows(&self) {
        for (index, window) in self.windows.iter().enumerate() {
            let Some(pane) = Self::configured_view(&self.views, window.id, window.last_interacted)
            else {
                continue;
            };
            let size = (
                pane.capabilities.pixels.0 as i32,
                pane.capabilities.pixels.1 as i32,
            );
            window.surface.with_pending_state(|state| {
                state.size = Some(size.into());
                state.states.set(XdgState::Maximized);
                if window.fullscreen {
                    state.states.set(XdgState::Fullscreen);
                } else {
                    state.states.unset(XdgState::Fullscreen);
                }
                if self.active == Some(index) {
                    state.states.set(XdgState::Activated);
                } else {
                    state.states.unset(XdgState::Activated);
                }
            });
            tracing::debug!(
                id = %window.id,
                pane = %pane.id,
                ?size,
                fullscreen = window.fullscreen,
                active = self.active == Some(index),
                "configured"
            );
            let _ = window.surface.send_configure();
        }
    }

    pub fn present_view(&mut self, id: PaneId, presenter: &mut Presenter) -> Cost {
        let Some(index) = self.view(id) else {
            return Cost::default();
        };
        let phase = Instant::now();
        if self.views[index].scene_dirty {
            self.compose(index);
            let View {
                tiles,
                frame,
                dirty,
                ..
            } = &mut self.views[index];
            tiles.diff(frame, dirty);
        } else {
            self.views[index].dirty.clear();
        }
        let mut cost = Cost {
            tiles: self.views[index].dirty.len(),
            compose: phase.elapsed(),
            ..Cost::default()
        };

        self.views[index].scene_dirty = false;
        self.views[index].escapes_dirty = false;
        self.draw_pointer_shape(index, presenter);
        self.name_terminal(index, presenter);
        self.hand_over(index, presenter, &mut cost);

        // Send callbacks even if the presenter dropped the frame.
        let time = self.time();
        if let Some(window) = self.views[index].index(&self.windows) {
            let surface = self.windows[window].surface.wl_surface().clone();
            send_frame_callbacks(&surface, time);
            for (popup, _) in PopupManager::popups_for_surface(&surface) {
                send_frame_callbacks(popup.wl_surface(), time);
            }
        }
        cost
    }

    fn hand_over(&mut self, pane: usize, presenter: &mut Presenter, cost: &mut Cost) {
        let view = &mut self.views[pane];
        for index in view.dirty.drain(..) {
            view.pending.mark(index);
        }
        if view.pending.is_empty() {
            return;
        }
        let Some(mut frame) = presenter.frame() else {
            return;
        };

        // The tiles are copied in the order they are listed, so the presenter
        // can cut them apart without knowing the frame layout.
        frame.pixels.clear();
        frame.tiles.clear();
        frame.tiles.reserve(view.pending.len());
        let stride = view.frame.width as usize * BYTES;
        for index in view.pending.indices() {
            let tile = view.tiles.tile(&view.frame, index);
            for row in 0..tile.height {
                let start = (tile.y as usize + row as usize) * stride + tile.x as usize * BYTES;
                frame.pixels.extend_from_slice(
                    &view.frame.pixels()[start..start + tile.width as usize * BYTES],
                );
            }
            frame
                .tiles
                .push(placement(tile, view.capabilities.cell, index));
        }

        cost.sent = frame.tiles.len();
        match presenter.present(frame) {
            Ok(()) => view.pending.clear(),
            // The tiles stay due, so the wait loses nothing.
            Err(frame) => presenter.recycle(frame),
        }
    }

    /// Tell the terminal which pointer shape the focused client asked for.
    ///
    /// The shape names come from the clients, through
    /// `wp_cursor_shape_manager_v1`, and the terminal draws the pointer
    /// itself.
    fn draw_pointer_shape(&mut self, pane: usize, presenter: &Presenter) {
        let shape = match &self.cursor {
            CursorImageStatus::Named(icon) => Some(kitty::pointer_shape(*icon)),
            // A cursor sent as an image cannot be described to the terminal, so the
            // terminal's default pointer stands in.
            CursorImageStatus::Surface(_) | CursorImageStatus::Hidden => None,
        };
        if self.views[pane].pointer_shape != shape {
            presenter.raw(crate::server::presenter::pointer_shape_bytes(shape));
            self.views[pane].pointer_shape = shape;
        }
    }

    /// Tell the terminal showing a pane's window what to call itself.
    ///
    /// The title is the client's, and the terminal title is the one place
    /// outside the frame where a client is visible.
    fn name_terminal(&mut self, pane: usize, presenter: &Presenter) {
        let title = self.views[pane]
            .index(&self.windows)
            .and_then(|window| self.windows[window].title.clone());
        if title.is_none() || self.views[pane].title == title {
            return;
        }
        presenter.raw(crate::kitty::title(title.as_deref().unwrap_or_default()));
        self.views[pane].title = title;
    }
}

fn output_mode(capabilities: &Capabilities) -> Mode {
    Mode {
        size: (capabilities.pixels.0 as i32, capabilities.pixels.1 as i32).into(),
        // A terminal has no refresh rate. Frame callbacks pace clients here.
        refresh: REFRESH_MILLIHZ,
    }
}

/// What a client calls its window: its app ID and its title.
///
/// This is read only to answer `list` and to name the terminal, so the window
/// keeps the names: reading them again would take a surface lock per keystroke.
fn window_names(surface: &ToplevelSurface) -> (Option<String>, Option<String>) {
    with_states(surface.wl_surface(), |states| {
        let named = || {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()?
                .lock()
                .ok()?;
            Some((attributes.app_id.clone(), attributes.title.clone()))
        };
        let (app_id, title) = named().unwrap_or_default();
        let named = |name: &Option<String>| {
            name.as_ref()
                .filter(|name| !name.trim().is_empty())
                .cloned()
        };
        (named(&app_id), named(&title))
    })
}

#[derive(Debug, Default)]
struct MeowlandClient {
    compositor_state: CompositorClientState,
}

impl ClientData for MeowlandClient {
    fn initialized(&self, _client: ClientId) {}

    fn disconnected(&self, _client: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for Meowland {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client that reaches this point was inserted here with this data
        // attached.
        &client
            .get_data::<MeowlandClient>()
            .expect("clients are always inserted with MeowlandClient data")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.popup_manager.commit(surface);

        // The compositor configures a popup before the client may map it, so
        // configure it now if the client committed first.
        if let Some(PopupKind::Xdg(popup)) = self.popup_manager.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(err) = popup.send_configure()
        {
            tracing::debug!(?err, "popup configure failed");
        }

        // Every surface that commits is copied and handed back, not only
        // toplevels: sub-surfaces, popups and cursors arrive here too.
        //
        // A window takes the screen when it first has pixels, and only if no
        // newer window has shown something. Nothing else moves it: a
        // first commit carries no buffer, it is how the client asks to
        // be configured; an unmap hides the window; and a redraw is the
        // window that was already showing.
        let drawn = self.snapshots.contains_key(&surface.id());
        self.snapshot(surface);
        let first_pixels = !drawn && self.snapshots.contains_key(&surface.id());
        // The panes showing this window draw again. A surface that is not a
        // toplevel is a sub-surface or a popup of a window that cannot
        // be named from here, so every pane draws again for those.
        let window = self.index_of(surface);
        let shown = window.map(|index| self.windows[index].id);
        for view in &mut self.views {
            if shown.is_none_or(|id| view.window == Some(id)) {
                view.scene_dirty = true;
            }
        }
        if first_pixels && let Some(index) = window {
            // A window with nothing on it is not shown, whatever the client
            // asked for. Some clients open windows they never draw
            // in, which must not leave a following pane without a
            // window.
            let id = self.windows[index].id;
            for view in &mut self.views {
                if view.follow {
                    view.window = Some(id);
                    view.scene_dirty = true;
                    tracing::debug!(id = %view.id, window = %id, "pane took the newest window");
                }
            }
            if self.views.iter().any(|view| view.window == Some(id)) {
                // The newest drawn window takes the keyboard, so the pane
                // showing it is the one that asked for it.
                self.configure_windows();
                self.sync_outputs();
                self.activate_index(index);
            }
        }
        tracing::debug!(id = ?surface.id(), "committed");
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.snapshots.remove(&surface.id());
    }
}

impl ShmHandler for Meowland {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl smithay::wayland::buffer::BufferHandler for Meowland {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl DmabufHandler for Meowland {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    /// Decide, before the client draws into it, whether meowland can read the
    /// buffer.
    ///
    /// A no is cheap, and the client still falls back to shared memory. A yes
    /// that turns out wrong is not: the client has stopped drawing into
    /// shared memory by then.
    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        match crate::wayland::buffer::dmabuf_readable(&dmabuf, self.gpu.as_mut()) {
            Ok(()) => {
                if let Err(err) = notifier.successful::<Self>() {
                    tracing::debug!(?err, "the client that offered a GPU buffer is gone");
                }
            }
            Err(reason) => {
                tracing::debug!(
                    reason = %reason,
                    "refusing a GPU buffer meowland cannot read"
                );
                notifier.failed();
            }
        }
    }
}

impl XdgShellHandler for Meowland {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_window_id;
        self.next_window_id = WindowId::new(self.next_window_id.into_inner().saturating_add(1));
        tracing::info!(id = %id, wayland_id = ?surface.wl_surface().id(), "new window");
        let (label, title) = window_names(&surface);
        self.windows.push(Window {
            id,
            surface,
            label: label.or_else(|| title.clone()),
            title,
            entered: false,
            last_interacted: None,
            fullscreen: false,
        });
        // A window that has not drawn yet is not shown. Panes that follow the
        // newest window take it when it first has pixels (`commit`),
        // which is also when it is configured.
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
        });
        if surface.is_initial_configure_sent() {
            surface.send_repositioned(0);
        } else if let Err(err) = surface.send_configure() {
            tracing::debug!(?err, "popup configure failed");
        }
        if let Err(err) = self.popup_manager.track_popup(PopupKind::Xdg(surface)) {
            tracing::debug!(?err, "could not track popup");
        }
    }

    fn grab(
        &mut self,
        surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
        // Popups get no keyboard grab, so the terminal's own shortcuts stay
        // usable.
        surface.send_popup_done();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
        });
        surface.send_repositioned(token);
    }

    fn maximize_request(&mut self, _surface: ToplevelSurface) {
        self.configure_windows();
    }

    fn unmaximize_request(&mut self, _surface: ToplevelSurface) {
        self.configure_windows();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let Some(index) = self.index_of(surface.wl_surface()) else {
            return;
        };
        tracing::info!(id = %self.windows[index].id, "fullscreen asked for");
        self.windows[index].fullscreen = true;
        // A request for the whole screen is a request to be the window on it,
        // which is what fullscreen means here. It brings a player that
        // starts fullscreen to the front.
        self.activate_index(index);
        self.configure_windows();
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(index) = self.index_of(surface.wl_surface()) else {
            return;
        };
        tracing::info!(id = %self.windows[index].id, "fullscreen given up");
        self.windows[index].fullscreen = false;
        self.configure_windows();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.output.leave(surface.wl_surface());
        let Some(removed) = self.index_of(surface.wl_surface()) else {
            return;
        };
        let was_active = self.active == Some(removed);
        let gone = self.windows[removed].id;
        self.windows.remove(removed);
        self.active = match self.active {
            None => None,
            Some(_) if self.windows.is_empty() => None,
            Some(active) if active > removed => Some(active - 1),
            Some(active) if was_active => Some(active.min(self.windows.len() - 1)),
            active => active,
        };
        // A pane that was given this window has nothing left to show, so it is
        // done. It goes when `take_closed_views` answers this, and its
        // terminal is released. A pane that follows the newest window
        // shows whatever is next.
        for view in &mut self.views {
            if view.window != Some(gone) {
                continue;
            }
            view.window = None;
            view.scene_dirty = true;
            if !view.follow {
                view.done = true;
            }
        }
        if was_active {
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, self.active_surface(), SERIAL_COUNTER.next_serial());
        }
        self.configure_windows();
        self.sync_outputs();
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        self.relabel(&surface);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.relabel(&surface);
    }
}

impl OutputHandler for Meowland {}

impl SeatHandler for Meowland {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor = image;
        // Every pane is owed the new shape, so each one draws it again.
        for view in &mut self.views {
            view.escapes_dirty = true;
        }
    }
}

/// The cursor-shape protocol requires this. meowland has no tablets, so the
/// default is enough.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Meowland {}

impl SelectionHandler for Meowland {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for Meowland {}

impl ServerDndGrabHandler for Meowland {}

impl DataDeviceHandler for Meowland {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

/// Bring up a renderer on the first render node that has one.
///
/// Without a renderer, a buffer the CPU cannot map cannot be read, and a client
/// that hands one over has nothing to show. So this decides whether clients are
/// offered GPU buffers, and on which device.
fn bring_up_renderer(
    nodes: &[crate::dmabuf::RenderNode],
) -> Option<(crate::wayland::gpu::Renderer, crate::dmabuf::RenderNode)> {
    for node in nodes {
        match crate::wayland::gpu::Renderer::new(&node.path) {
            Ok(renderer) => return Some((renderer, node.clone())),
            Err(err) => {
                tracing::info!(?err, path = %node.path.display(), "no renderer on this render node");
            }
        }
    }
    None
}

/// Offer clients the device that reads their GPU buffers back, if a renderer
/// was found.
///
/// A client that renders on the GPU keeps doing so only if it is told where to
/// put the memory, and can hand it over only if meowland can read it back. So
/// this advertises the formats the renderer takes and the device it is on. See
/// [`crate::dmabuf`] for what keeps that offer honest.
fn advertise_render_nodes(
    display: &DisplayHandle,
    state: &mut DmabufState,
    gpu: Option<(&crate::wayland::gpu::Renderer, &crate::dmabuf::RenderNode)>,
) -> Result<Option<DmabufGlobal>, Error> {
    let Some((gpu, node)) = gpu else {
        return Ok(None);
    };
    let formats = gpu.formats();
    if formats.is_empty() {
        tracing::info!("the renderer takes no layouts meowland can composite");
        return Ok(None);
    }
    tracing::info!(
        path = %node.path.display(),
        device = node.device.into_inner(),
        formats = formats.len(),
        "offering GPU buffers to clients"
    );
    let feedback = DmabufFeedbackBuilder::new(node.device.into_inner(), formats).build()?;
    Ok(Some(state.create_global_with_default_feedback::<Meowland>(
        display, &feedback,
    )))
}

delegate_compositor!(Meowland);
delegate_shm!(Meowland);
delegate_dmabuf!(Meowland);
delegate_xdg_shell!(Meowland);
delegate_output!(Meowland);
delegate_seat!(Meowland);
delegate_data_device!(Meowland);
delegate_cursor_shape!(Meowland);
delegate_viewporter!(Meowland);

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(pixels: (u32, u32)) -> Capabilities {
        Capabilities {
            cell: (10, 20),
            cells: (pixels.0 / 10, pixels.1 / 20),
            pixels,
            terminal: None,
            graphics: true,
            keyboard: true,
            pixel_mouse: true,
            shared_memory: false,
        }
    }

    #[test]
    fn configured_view_prefers_the_most_recently_interacted_pane() {
        let window = WindowId::new(1);
        let mut first = View::new(PaneId::new(1), &capabilities((800, 600)));
        first.window = Some(window);
        let mut second = View::new(PaneId::new(2), &capabilities((1200, 900)));
        second.window = Some(window);
        let views = vec![first, second];

        assert_eq!(
            Meowland::configured_view(&views, window, Some(PaneId::new(2))).map(|view| view.id),
            Some(PaneId::new(2))
        );
        assert_eq!(
            Meowland::configured_view(&views, window, Some(PaneId::new(3))).map(|view| view.id),
            Some(PaneId::new(1))
        );
    }
}
