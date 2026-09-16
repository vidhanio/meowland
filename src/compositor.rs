//! Wayland protocol state, input routing, and frame composition.

use std::{
    collections::{HashMap, hash_map::Entry},
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

use evdev::KeyCode;
use smithay::{
    backend::{
        allocator::dmabuf::Dmabuf,
        input::{ButtonState, KeyState},
    },
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_output, delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle, Keycode, XkbConfig},
        pointer::{AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle},
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
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SubsurfaceCachedState, SurfaceAttributes, SurfaceData, TraversalAction, with_states,
            with_surface_tree_downward,
        },
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
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        viewporter::{ViewportCachedState, ViewporterState, ensure_viewport_valid},
    },
};

use crate::{
    Error,
    buffer::Snapshot,
    control,
    display::{self, Key, Pointer},
    keys, kitty,
    presenter::Presenter,
    render::{BYTES, Frame, Rect, Tiles},
    tty::Capabilities,
    types::{ImageId, PaneId, WindowId},
};

pub const REFRESH_MILLIHZ: i32 = 60_000;

const TILE_CELLS: (u32, u32) = (16, 8);

pub const BINDING_MODIFIER: crossterm::event::KeyModifiers = crossterm::event::KeyModifiers::ALT;

const BACKDROP: [u8; 3] = [0x14, 0x16, 0x1b];

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
    gpu: Option<crate::gpu::Renderer>,

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

    pub fn attach_view(&mut self, id: PaneId, show: display::Show, capabilities: &Capabilities) {
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

    fn resolve(&self, show: display::Show) -> (Option<WindowId>, bool) {
        match show {
            display::Show::Window(id) => (Some(id), false),
            display::Show::Newest => (self.windows.last().map(|window| window.id), true),
            display::Show::Focused => self.active_window().map_or_else(
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
            self.activate_index(window);
        }
        Some(index)
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

    /// Configure each window from its first attached pane.
    fn configure_windows(&self) {
        for (index, window) in self.windows.iter().enumerate() {
            let Some(pane) = self
                .views
                .iter()
                .find(|view| view.window == Some(window.id))
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
            presenter.raw(crate::presenter::pointer_shape_bytes(shape));
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
        presenter.raw(crate::tty::title(title.as_deref().unwrap_or_default()));
        self.views[pane].title = title;
    }

    /// Repaint one pane from the window it shows.
    ///
    /// Reading geometry needs `&self` and drawing needs the frame, so the
    /// drawing order is planned first and painted afterwards.
    fn compose(&mut self, pane: usize) {
        self.views[pane].frame.clear(BACKDROP);

        self.plan.clear();
        if let Some(window) = self.views[pane].index(&self.windows) {
            let surface = self.windows[window].surface.wl_surface().clone();
            if self.snapshots.contains_key(&surface.id()) {
                self.plan.push((surface.clone(), Point::from((0, 0))));
                let geometry = geometry_offset(&surface);
                for (popup, location) in PopupManager::popups_for_surface(&surface) {
                    self.plan.push((
                        popup.wl_surface().clone(),
                        geometry + location - popup.geometry().loc,
                    ));
                }
            }
        }

        let View { frame, .. } = &mut self.views[pane];
        for (surface, position) in &self.plan {
            draw_tree(frame, &self.snapshots, surface, *position);
        }
    }

    /// The surface under a point, in the coordinates of the window the pane
    /// shows.
    fn surface_at(
        &self,
        pane: usize,
        point: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<i32, Logical>)> {
        let window = self.views[pane].index(&self.windows)?;
        if !self.views[pane]
            .frame
            .bounds()
            .contains(point.x as i32, point.y as i32)
        {
            return None;
        }
        let surface = self.windows[window].surface.wl_surface().clone();
        let geometry = geometry_offset(&surface);
        for (popup, location) in PopupManager::popups_for_surface(&surface) {
            let origin = geometry + location - popup.geometry().loc;
            if let Some(under) = surface_under(&self.snapshots, popup.wl_surface(), point, origin) {
                return Some(under);
            }
        }
        surface_under(&self.snapshots, &surface, point, Point::from((0, 0)))
    }

    /// Handle a key the terminal reported.
    ///
    /// Terminals do not have to report key releases. So a press is treated as a
    /// whole keystroke: press the key, release it, and let the terminal's
    /// auto-repeat produce the repeats. A held key cannot otherwise be told
    /// from one that was never released, and the client would repeat it
    /// forever.
    ///
    /// The terminal reduces the key to a code and a shift first
    /// (`crate::display`), because it has the key codes and the keymap.
    /// Typing in a pane gives its window the keyboard, and the bindings act
    /// on that window.
    pub fn key(&mut self, pane: PaneId, key: Key) {
        use display::KeyKind;

        let modifiers = crossterm::event::KeyModifiers::from_bits_truncate(key.modifiers);
        self.sync_modifiers(modifiers);
        let stroke = keys::KeyStroke {
            code: key.code,
            shift: key.shift,
        };
        tracing::debug!(?key, code = stroke.code.code(), "key");

        // Modifier keys are *state* for everything typed while they are held,
        // so they follow the terminal's flags, not a keystroke.
        if key.modifier {
            match key.kind {
                KeyKind::Press => self.press_modifier(stroke.code),
                KeyKind::Repeat => {}
                KeyKind::Release => self.release_modifier(stroke.code),
            }
            return;
        }

        match key.kind {
            KeyKind::Press | KeyKind::Repeat => {
                if !self.binding(pane, modifiers, stroke.code) {
                    self.focus_pane(pane);
                    self.type_stroke(stroke);
                }
            }
            // The press already released this key.
            KeyKind::Release => {}
        }
    }

    /// Press a key and release it, holding shift while its symbol needs it.
    fn type_stroke(&mut self, stroke: keys::KeyStroke) {
        let synthesized_shift = stroke.shift && !self.is_pressed(keys::modifier::LEFT_SHIFT);
        if synthesized_shift {
            self.press_modifier(keys::modifier::LEFT_SHIFT);
        }
        self.forward_key(stroke.code, KeyState::Pressed);
        self.forward_key(stroke.code, KeyState::Released);
        if synthesized_shift {
            self.release_modifier(keys::modifier::LEFT_SHIFT);
        }
    }

    /// Handle compositor bindings before a key reaches a client.
    ///
    /// A binding acts on the pane the key was typed in, so it closes the window
    /// that pane shows.
    fn binding(
        &mut self,
        pane: PaneId,
        modifiers: crossterm::event::KeyModifiers,
        code: KeyCode,
    ) -> bool {
        use crossterm::event::KeyModifiers as M;
        if !modifiers.contains(BINDING_MODIFIER)
            || modifiers.contains(M::CONTROL)
            || modifiers.contains(M::SUPER)
        {
            return false;
        }
        // Linux input event codes, as the terminal reports them
        // (`keys::for_char`): `KEY_Q`.
        match code {
            KeyCode::KEY_Q => {
                // Close the window this pane shows, if it has one. Closing a
                // client ends its pane; a pane with nothing to
                // show releases its terminal instead.
                if self.pane_window(pane).is_some() {
                    self.close_window(pane);
                } else if let Some(index) = self.view(pane) {
                    self.views[index].detaching = true;
                }
            }
            _ => return false,
        }
        true
    }

    /// Type text into the focused client, one keystroke per character.
    ///
    /// Text arrives as characters and clients take key presses, so each
    /// character is matched to the stroke that produces it.
    pub fn paste(&mut self, text: &str) {
        for c in text.chars() {
            let stroke = match c {
                '\n' | '\r' => keys::for_key(crossterm::event::KeyCode::Enter),
                _ => keys::for_char(c),
            };
            if let Some(stroke) = stroke {
                self.type_stroke(stroke);
            } else {
                tracing::debug!(?c, "character has no key code in the advertised keymap");
            }
        }
    }

    fn sync_modifiers(&mut self, modifiers: crossterm::event::KeyModifiers) {
        use crossterm::event::KeyModifiers as M;
        for (flag, code) in [
            (M::SHIFT, keys::modifier::LEFT_SHIFT),
            (M::CONTROL, keys::modifier::LEFT_CTRL),
            (M::ALT, keys::modifier::LEFT_ALT),
            (M::SUPER, keys::modifier::LEFT_META),
        ] {
            if modifiers.contains(flag) {
                self.press_modifier(code);
            } else {
                self.release_modifier(code);
            }
        }
    }

    fn press_modifier(&mut self, code: KeyCode) {
        if !self.is_pressed(code) {
            self.forward_key(code, KeyState::Pressed);
        }
    }

    fn release_modifier(&mut self, code: KeyCode) {
        if self.is_pressed(code) {
            self.forward_key(code, KeyState::Released);
        }
    }

    fn is_pressed(&self, code: KeyCode) -> bool {
        self.pressed.contains(&code)
    }

    /// Hand a key to the focused client.
    ///
    /// The rest of this module counts keys as the terminal and `KEY_*` do
    /// (evdev); the seat counts as XKB does, eight codes further along, so
    /// the conversion happens here and nowhere else. Confusing the two is
    /// not a loud failure: it types the neighbouring key.
    fn forward_key(&mut self, code: KeyCode, state: KeyState) {
        tracing::debug!(code = code.code(), ?state, "forwarding key");
        if state == KeyState::Pressed {
            if !self.pressed.contains(&code) {
                self.pressed.push(code);
            }
        } else {
            self.pressed.retain(|pressed| *pressed != code);
        }
        let keyboard = self.keyboard.clone();
        let serial = SERIAL_COUNTER.next_serial();
        let time = self.time();
        keyboard.input(
            self,
            Keycode::from(u32::from(code.code()) + keys::XKB_OFFSET),
            state,
            serial,
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }

    /// Handle a mouse event in a pane, in the cells the terminal reports.
    pub fn pointer(&mut self, pane: PaneId, pointer: Pointer) {
        let Some(index) = self.view(pane) else {
            return;
        };
        match pointer {
            Pointer::Motion { column, row } => {
                let position = self.cell_position(index, column, row);
                self.pointer_motion(index, position);
            }
            Pointer::Button {
                column,
                row,
                button,
                pressed,
            } => {
                // A click goes to the window the pane shows, so give it the
                // keyboard.
                self.focus_pane(pane);
                let position = self.cell_position(index, column, row);
                self.pointer_motion(index, position);
                self.pointer_button(button, pressed);
            }
            Pointer::ScrollUp | Pointer::ScrollLeft => self.pointer_axis(index, -15.0),
            Pointer::ScrollDown | Pointer::ScrollRight => self.pointer_axis(index, 15.0),
        }
    }

    /// Where an event at a terminal cell happened, in the pixels that pane
    /// draws in.
    fn cell_position(&self, pane: usize, column: u16, row: u16) -> Point<f64, Logical> {
        let capabilities = &self.views[pane].capabilities;
        if capabilities.pixel_mouse {
            return (f64::from(column), f64::from(row)).into();
        }
        let (cell_width, cell_height) = capabilities.cell;
        (
            f64::mul_add(
                f64::from(column),
                f64::from(cell_width),
                f64::from(cell_width) / 2.0,
            ),
            f64::mul_add(
                f64::from(row),
                f64::from(cell_height),
                f64::from(cell_height) / 2.0,
            ),
        )
            .into()
    }

    fn pointer_motion(&mut self, pane: usize, position: Point<f64, Logical>) {
        self.pointer_position = position;
        // The seat takes the pointer position in *output* coordinates and the
        // origin of the focused surface, and subtracts the two to get
        // the client's position.
        let event = MotionEvent {
            location: position,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
        };
        let pointer = self.pointer.clone();
        if let Some((surface, origin)) = self.surface_at(pane, position) {
            pointer.motion(self, Some((surface, origin.to_f64())), &event);
        } else {
            pointer.motion(self, None, &event);
        }
        pointer.frame(self);
    }

    fn pointer_button(&mut self, button: KeyCode, pressed: bool) {
        let pointer = self.pointer.clone();
        let event = ButtonEvent {
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
            button: u32::from(button.code()),
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        };
        pointer.button(self, &event);
        pointer.frame(self);
    }

    fn pointer_axis(&mut self, pane: usize, vertical: f64) {
        let pointer = self.pointer.clone();
        if let Some((surface, origin)) = self.surface_at(pane, self.pointer_position) {
            let event = MotionEvent {
                location: self.pointer_position,
                serial: SERIAL_COUNTER.next_serial(),
                time: self.time(),
            };
            pointer.motion(self, Some((surface, origin.to_f64())), &event);
        }
        let frame =
            AxisFrame::new(self.time()).value(smithay::backend::input::Axis::Vertical, vertical);
        pointer.axis(self, frame);
        pointer.frame(self);
    }
}

fn output_mode(capabilities: &Capabilities) -> Mode {
    Mode {
        size: (capabilities.pixels.0 as i32, capabilities.pixels.1 as i32).into(),
        // A terminal has no refresh rate. Frame callbacks pace clients here.
        refresh: REFRESH_MILLIHZ,
    }
}

/// Where one tile goes on a pane's screen, and what it is called.
///
/// The name is a tile of that terminal's frame, so the same tile of the next
/// frame replaces the image the terminal has.
fn placement(tile: Rect, cell: (u32, u32), index: usize) -> kitty::Placement {
    let (cell_width, cell_height) = cell;
    kitty::Placement {
        id: ImageId::new(index as u32 + 1),
        width: tile.width,
        height: tile.height,
        cols: tile.width.div_ceil(cell_width.max(1)).max(1),
        rows: tile.height.div_ceil(cell_height.max(1)).max(1),
        cell: (
            tile.x as u32 / cell_width.max(1),
            tile.y as u32 / cell_height.max(1),
        ),
    }
}

const fn tile_size(cell: (u32, u32)) -> (u32, u32) {
    (cell.0 * TILE_CELLS.0, cell.1 * TILE_CELLS.1)
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

/// Where the toplevel's window geometry starts, relative to its surface origin.
///
/// Popup positions arrive relative to that rectangle.
fn geometry_offset(surface: &WlSurface) -> Point<i32, Logical> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
            .map_or_else(|| (0, 0).into(), |geometry| geometry.loc)
    })
}

/// Blend a surface tree into the frame, with the tree's root at `location`.
///
/// Both traversal closures receive the *parent's* accumulated location, so each
/// one adds its own offset.
fn draw_tree(
    frame: &mut Frame,
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    location: Point<i32, Logical>,
) {
    with_surface_tree_downward(
        surface,
        location,
        |_, states, location| TraversalAction::DoChildren(*location + subsurface_offset(states)),
        |surface, states, location| {
            draw_surface(
                frame,
                snapshots,
                surface,
                states,
                *location + subsurface_offset(states),
            );
        },
        |_, _, _| true,
    );
}

/// Blend one surface of a tree into the frame, from its committed copy.
fn draw_surface(
    frame: &mut Frame,
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    states: &SurfaceData,
    location: Point<i32, Logical>,
) {
    // A surface with nothing committed, or with a detached buffer, draws
    // nothing.
    let Some(snapshot) = snapshots.get(&surface.id()) else {
        return;
    };
    let viewport = *states.cached_state.get::<ViewportCachedState>().current();
    let scale = f64::from(snapshot.scale.max(1));
    let src = viewport.src.map_or_else(
        || Rect::new(0, 0, snapshot.width, snapshot.height),
        |src| {
            Rect::new(
                (src.loc.x * scale).floor() as i32,
                (src.loc.y * scale).floor() as i32,
                (src.size.w * scale).ceil() as u32,
                (src.size.h * scale).ceil() as u32,
            )
        },
    );
    let (width, height) = viewport.size().map_or_else(
        || snapshot.logical_size(),
        |size| (size.w.max(1), size.h.max(1)),
    );
    frame.draw(
        &snapshot.image(),
        src,
        Rect::new(location.x, location.y, width as u32, height as u32),
    );
}

fn subsurface_offset(states: &SurfaceData) -> Point<i32, Logical> {
    states
        .cached_state
        .get::<SubsurfaceCachedState>()
        .current()
        .location
}

/// The topmost surface of a tree that a point hits, in the client's
/// coordinates.
///
/// The point arrives in output coordinates and `origin` is a screen position. A
/// surface accepts input only inside its input region; with no region, the
/// whole surface accepts it.
fn surface_under(
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    point: Point<f64, Logical>,
    origin: Point<i32, Logical>,
) -> Option<(WlSurface, Point<i32, Logical>)> {
    use std::cell::RefCell;

    let found = RefCell::new(None);
    // Downward order is topmost first, the order a click must be matched in.
    with_surface_tree_downward(
        surface,
        origin,
        |_, states, location| TraversalAction::DoChildren(*location + subsurface_offset(states)),
        |surface, states, location| {
            if found.borrow().is_some() {
                return;
            }
            let location = *location + subsurface_offset(states);
            let Some(snapshot) = snapshots.get(&surface.id()) else {
                return;
            };
            let viewport = *states.cached_state.get::<ViewportCachedState>().current();
            let size = viewport.size().map_or_else(
                || snapshot.logical_size(),
                |size| (size.w.max(1), size.h.max(1)),
            );
            if accepts_input(states, point - location.to_f64(), size) {
                *found.borrow_mut() = Some((surface.clone(), location));
            }
        },
        |_, _, _| found.borrow().is_none(),
    );
    found.into_inner()
}

/// Whether a surface accepts input at a point, given in that surface's own
/// coordinates.
///
/// A client can cut its sensitive area out with `wl_surface.set_input_region`;
/// with no region the whole surface counts, as the protocol says.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the input region is borrowed out of the cached state, so the guard outlives the check"
)]
fn accepts_input(states: &SurfaceData, local: Point<f64, Logical>, size: (i32, i32)) -> bool {
    let mut state = states.cached_state.get::<SurfaceAttributes>();
    let attributes = state.current();
    attributes.input_region.as_ref().map_or_else(
        || {
            local.x >= 0.0
                && local.y >= 0.0
                && local.x < f64::from(size.0)
                && local.y < f64::from(size.1)
        },
        |region| region.contains((local.x.floor() as i32, local.y.floor() as i32)),
    )
}

/// Send the frame callbacks a surface tree waits for, and clear them.
///
/// A client driven by animation draws one frame per callback, so this paces it.
fn send_frame_callbacks(surface: &WlSurface, time: u32) {
    with_surface_tree_downward(
        surface,
        (),
        |_, _, ()| TraversalAction::DoChildren(()),
        |_, states, ()| {
            let mut state = states.cached_state.get::<SurfaceAttributes>();
            for callback in state.current().frame_callbacks.drain(..) {
                callback.done(time);
            }
        },
        |_, _, ()| true,
    );
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

/// Take a copy of the buffer a surface committed, and hand it straight back.
///
/// This is the moment client memory is read: from here on the surface is
/// composited from [`Meowland::snapshots`], so a client that reuses its buffer
/// cannot tear a frame.
impl Meowland {
    /// The biggest screen any pane has, which bounds the copy of a client
    /// buffer: nothing bigger can be shown.
    ///
    /// With no pane attached there is no bound, and the copy is the buffer's
    /// own size.
    fn snapshot_limit(&self) -> Option<(u32, u32)> {
        let (width, height) = self.views.iter().fold((0, 0), |largest, view| {
            let bounds = view.frame.bounds();
            (largest.0.max(bounds.width), largest.1.max(bounds.height))
        });
        (width > 0 && height > 0).then_some((width, height))
    }

    fn snapshot(&mut self, surface: &WlSurface) {
        let limit = self.snapshot_limit();

        let committed = with_states(surface, |states| {
            // The cached state is borrowed only for the reads that need it, so
            // the copy below holds no lock.
            let (buffer, scale) = {
                let mut state = states.cached_state.get::<SurfaceAttributes>();
                let current = state.current();
                let buffer = current.buffer.take();
                let scale = current.buffer_scale;
                // A snapshot copies the whole buffer, so all accumulated damage
                // is consumed. Left here it would make Smithay
                // keep every damage rectangle across later
                // commits.
                current.damage.clear();
                current.buffer_delta = None;
                drop(state);
                (buffer, scale)
            };
            buffer.map(|buffer| (buffer, scale))
        });
        match committed {
            Some((BufferAssignment::NewBuffer(buffer), scale)) => {
                let copied = match self.snapshots.entry(surface.id()) {
                    Entry::Occupied(mut entry) => crate::buffer::snapshot(
                        &buffer,
                        scale,
                        limit,
                        entry.get_mut(),
                        self.gpu.as_mut(),
                    ),
                    Entry::Vacant(entry) => {
                        let mut snapshot = Snapshot::empty();
                        let copied = crate::buffer::snapshot(
                            &buffer,
                            scale,
                            limit,
                            &mut snapshot,
                            self.gpu.as_mut(),
                        );
                        if copied {
                            entry.insert(snapshot);
                        }
                        copied
                    }
                };
                if !copied {
                    tracing::debug!(format = ?buffer, "buffer is not one we can composite");
                } else if let Some(snapshot) = self.snapshots.get(&surface.id()) {
                    let size = snapshot.logical_size();
                    with_states(surface, |states| {
                        ensure_viewport_valid(states, size.into());
                    });
                }
                buffer.release();
            }
            Some((BufferAssignment::Removed, _)) => {
                self.snapshots.remove(&surface.id());
            }
            None => {}
        }
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
        match crate::buffer::dmabuf_readable(&dmabuf, self.gpu.as_mut()) {
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
) -> Option<(crate::gpu::Renderer, crate::dmabuf::RenderNode)> {
    for node in nodes {
        match crate::gpu::Renderer::new(&node.path) {
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
    gpu: Option<(&crate::gpu::Renderer, &crate::dmabuf::RenderNode)>,
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
