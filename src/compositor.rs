//! The Wayland server: protocol globals, input routing and presentation.
//!
//! Wayland's clients own their pixels and their keyboard interpretation; a
//! compositor's job is to move buffers between them and the display, and to
//! route input. Here the display is a terminal (see [`crate::kitty`] and
//! [`crate::tty`]) and the "GPU" is a `Vec<u8>` (see [`crate::render`]).
//!
//! The pieces smithay provides are the protocol front end and the input state
//! machines; the backend is ours: buffers arrive as shared memory, get blended
//! into a frame buffer every frame, and the frame is diffed tile by tile before
//! being encoded as terminal graphics.
//!
//! # One window per view
//!
//! Every toplevel is an independent, terminal-sized view. Exactly one view is
//! visible and receives input; `Alt+Tab` cycles between them. Popups remain
//! attached to their parent view.

use std::{
    collections::{HashMap, hash_map::Entry},
    os::unix::net::UnixStream,
    sync::Arc,
    time::{Duration, Instant},
};

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
    reexports::wayland_server::{
        Client, DisplayHandle, Resource as _,
        backend::{ClientData, ClientId, DisconnectReason, ObjectId},
        protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
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
    buffer::Snapshot,
    keys, kitty,
    presenter::Presenter,
    render::{BYTES, Frame, Rect, Tiles},
    tty::Capabilities,
};

/// Tile size in character cells. Terminals update images whole, so this is the
/// resolution of a partial repaint: bigger tiles mean fewer images, smaller
/// tiles mean less re-sent data.
const TILE_CELLS: (u32, u32) = (16, 8);

/// The modifier the quit shortcut hangs off.
///
/// It has to survive two levels of nesting: the host compositor and the
/// terminal both take keystrokes before meowland ever sees them. `Super` is
/// what window managers grab and `Ctrl` is what terminals grab, which leaves
/// `Alt`.
pub const BINDING_MODIFIER: crossterm::event::KeyModifiers = crossterm::event::KeyModifiers::ALT;

/// Backdrop behind client surfaces.
const BACKDROP: [u8; 3] = [0x14, 0x16, 0x1b];

/// What one presented frame cost *this* thread, which is the thread input
/// waits on.
///
/// The presenter's own time - compressing and writing - is reported by the
/// presenter, since none of it happens here any more.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cost {
    /// Tiles that changed, out of the grid the frame is divided into.
    pub tiles: usize,
    /// Tiles handed to the presenter, which is none of them if it was busy.
    pub sent: usize,
    /// Composing the frame buffer and finding what changed.
    pub compose: Duration,
}

/// Tiles whose newest pixels have not yet been accepted by the presenter.
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

/// Why meowland could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The keymap clients are given could not be built.
    #[error("could not build the keymap clients are given")]
    Keymap(#[source] smithay::input::keyboard::Error),
    /// The render nodes could not be described to clients.
    #[error(transparent)]
    RenderNodes(#[from] crate::dmabuf::Error),
    /// The GPU buffers clients may hand over could not be described.
    #[error("could not describe the GPU buffers clients may hand over")]
    Feedback(#[source] std::io::Error),
}

#[derive(Debug)]
struct Window {
    /// The ID this window is known by outside the compositor, which is what
    /// `attach` takes and what the shell completes.
    id: u64,
    surface: ToplevelSurface,
    /// What the client calls itself: its app ID, or its title without one.
    label: Option<String>,
}

/// A window exposed through the local control socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u64,
    pub label: String,
    pub active: bool,
}

/// The compositor.
pub struct Meowland {
    // Protocol state.
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Self>,
    dmabuf_state: DmabufState,
    /// Kept alive so that the render node clients may allocate on stays
    /// described to them; never read once it has been created.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    dmabuf_global: Option<DmabufGlobal>,
    /// The renderer a client's GPU buffers are brought back through, when
    /// there is a device to have one on.
    gpu: Option<crate::gpu::Renderer>,
    /// Which tiles have changed and have not reached the terminal yet, one flag
    /// per cell of the tile grid, because the presenter was busy with the frame
    /// before them.
    /// A set rather than a queue: a tile that changes again while it waits is
    /// still one tile to send, and it carries its newest pixels when it goes.
    /// If it were a queue, a client at 60 Hz and a terminal that cannot
    /// keep up would grow it without bound, and every handover would cost
    /// more than the one before it until the screen stopped moving.
    pending: PendingTiles,
    /// Kept alive so the `zxdg_output_manager_v1` global stays advertised;
    /// never queried.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    output_manager_state: OutputManagerState,
    data_device_state: DataDeviceState,
    /// Kept alive so clients can name a cursor shape instead of sending a
    /// cursor image.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    cursor_shape_state: CursorShapeManagerState,
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    viewporter_state: ViewporterState,
    popup_manager: PopupManager,

    // Core.
    display_handle: DisplayHandle,
    start: Instant,
    output: Output,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,

    // User-facing IDs are monotonic. Wayland object IDs are scoped to one
    // client connection and may be reused, so they are not stable handles.
    windows: Vec<Window>,
    active: Option<usize>,
    next_window_id: u64,

    // Input.
    /// Keys we currently consider pressed, so modifier state can be diffed.
    pressed: Vec<u32>,
    pointer_position: Point<f64, Logical>,
    cursor: CursorImageStatus,
    pointer_shape: Option<&'static str>,

    // Presentation.
    /// Pixels of the last committed buffer of every surface, the thing we
    /// actually composite.
    snapshots: HashMap<ObjectId, Snapshot>,
    frame: Frame,
    tiles: Tiles,
    dirty: Vec<Rect>,
    plan: Vec<(WlSurface, Point<i32, Logical>)>,
    /// The client scene changed and must be composited again.
    scene_dirty: bool,
    pointer_dirty: bool,
    /// Set by the quit binding and by the main loop's own reasons to stop.
    quitting: bool,
    cell: (u32, u32),
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
    /// Advertise the initial state to clients.
    pub fn new(
        display: &DisplayHandle,
        capabilities: &Capabilities,
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

        let output = Output::new(
            "meowland".into(),
            PhysicalProperties {
                // Reported as a ~96 DPI monitor, which is what a terminal font roughly is.
                size: (
                    (capabilities.pixels.0 * 2646 / 10_000) as i32,
                    (capabilities.pixels.1 * 2646 / 10_000) as i32,
                )
                    .into(),
                subpixel: Subpixel::Unknown,
                make: "meowland".into(),
                model: "terminal".into(),
            },
        );
        let _global = output.create_global::<Self>(display);
        let mode = output_mode(capabilities);
        output.set_preferred(mode);
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );

        let mut seat = seat_state.new_wl_seat(display, "meowland");
        // The keymap clients are given. It has to match [`crate::keys`], which
        // translates the characters the terminal hands us back into key
        // codes of *this* layout.
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

        let frame = Frame::new(capabilities.pixels.0, capabilities.pixels.1);
        let tiles = Tiles::new(&frame, tile_size(capabilities.cell));
        let grid_tiles = tiles.tile_count();

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
            pending: PendingTiles::new(grid_tiles),
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
            next_window_id: 1,
            pressed: Vec::new(),
            pointer_position: (0.0, 0.0).into(),
            cursor: CursorImageStatus::default_named(),
            pointer_shape: None,
            snapshots: HashMap::new(),
            frame,
            tiles,
            dirty: Vec::new(),
            plan: Vec::new(),
            scene_dirty: false,
            pointer_dirty: false,
            quitting: false,
            cell: capabilities.cell,
        })
    }

    /// Milliseconds since startup, the clock Wayland events are timestamped
    /// with.
    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    /// Adopt a newly connected client.
    pub fn insert_client(&mut self, stream: UnixStream) -> std::io::Result<()> {
        self.display_handle
            .insert_client(stream, Arc::new(MeowlandClient::default()))?;
        Ok(())
    }

    /// Whether the quit binding has been used.
    pub const fn quitting(&self) -> bool {
        self.quitting
    }

    /// Whether a toplevel has committed pixels that can be displayed.
    ///
    /// This is what the terminal waits for before it is taken over: with
    /// nothing to draw, taking it over would wipe the screen for a frame of
    /// backdrop and then put the user's shell back.
    pub fn window_ready(&self) -> bool {
        self.windows.iter().any(|window| {
            self.snapshots
                .contains_key(&window.surface.wl_surface().id())
        })
    }

    fn active_surface(&self) -> Option<WlSurface> {
        self.active
            .and_then(|index| self.windows.get(index))
            .map(|window| window.surface.wl_surface().clone())
    }

    /// Return the stable IDs currently accepted by `attach`.
    pub fn windows(&self) -> impl Iterator<Item = WindowInfo> + '_ {
        self.windows
            .iter()
            .enumerate()
            .map(|(index, window)| WindowInfo {
                id: window.id,
                label: window.label.clone().unwrap_or_default(),
                active: self.active == Some(index),
            })
    }

    /// Select a view by its server-assigned ID.
    pub fn activate(&mut self, id: u64) -> bool {
        let Some(index) = self.windows.iter().position(|window| window.id == id) else {
            return false;
        };
        self.activate_index(index);
        true
    }

    fn activate_index(&mut self, index: usize) {
        if self.active == Some(index) || index >= self.windows.len() {
            return;
        }
        if let Some(surface) = self.active_surface() {
            self.output.leave(&surface);
        }
        self.active = Some(index);
        let surface = self.active_surface();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface.clone(), SERIAL_COUNTER.next_serial());
        if let Some(surface) = surface {
            self.output.enter(&surface);
        }
        self.configure_windows();
        self.scene_dirty = true;
    }

    fn cycle_window(&mut self) {
        if !self.windows.is_empty() {
            self.activate_index(
                self.active
                    .map_or(0, |index| (index + 1) % self.windows.len()),
            );
        }
    }

    /// Re-read what the client calls this window, after it said so.
    fn relabel(&mut self, surface: &ToplevelSurface) {
        let label = window_label(surface);
        if let Some(window) = self
            .windows
            .iter_mut()
            .find(|window| window.surface.wl_surface() == surface.wl_surface())
        {
            window.label = label;
        }
    }

    /// Give every toplevel the terminal size and update its activation state.
    fn configure_windows(&self) {
        let size = (self.frame.width as i32, self.frame.height as i32);
        for (index, window) in self.windows.iter().enumerate() {
            window.surface.with_pending_state(|state| {
                state.size = Some(size.into());
                state.states.set(xdg_state::MAXIMIZED);
                if self.active == Some(index) {
                    state.states.set(xdg_state::ACTIVATED);
                } else {
                    state.states.unset(xdg_state::ACTIVATED);
                }
            });
            let _ = window.surface.send_configure();
        }
    }

    /// Fill the frame with the current state of the window and hand what
    /// changed to the presenter.
    ///
    /// Nothing here waits for the terminal: composing is this thread's work and
    /// sending is the presenter's, so a keystroke is never behind a frame. What
    /// the presenter is too busy to take stays due, and the next frame carries
    /// it - which is what makes dropping a frame safe.
    pub fn present(&mut self, presenter: &mut Presenter) -> Cost {
        let phase = Instant::now();
        if self.scene_dirty {
            self.compose();
            self.tiles.diff(&self.frame, &mut self.dirty);
        } else {
            self.dirty.clear();
        }
        let mut cost = Cost {
            tiles: self.dirty.len(),
            compose: phase.elapsed(),
            ..Cost::default()
        };

        // Everything composed is on screen as far as the clients are concerned;
        // what the terminal has yet to receive is the presenter's business.
        self.scene_dirty = false;
        self.pointer_dirty = false;
        self.draw_pointer_shape(presenter);
        self.hand_over(presenter, &mut cost);

        // Whatever became of the frame, the clients that drew it are owed a
        // callback: without one a client that paces itself by them waits
        // forever, and the screen stops moving.
        let time = self.time();
        for window in &self.windows {
            let surface = window.surface.wl_surface().clone();
            send_frame_callbacks(&surface, time);
            for (popup, _) in PopupManager::popups_for_surface(&surface) {
                send_frame_callbacks(popup.wl_surface(), time);
            }
        }
        cost
    }

    /// Copy the tiles that are due into a buffer and pass them on, unless the
    /// presenter is still busy with the frame before them - in which case they
    /// stay due and the next frame carries them.
    fn hand_over(&mut self, presenter: &mut Presenter, cost: &mut Cost) {
        for tile in self.dirty.drain(..) {
            let index = self.tiles.index(tile);
            self.pending.mark(index);
        }
        if self.pending.is_empty() {
            return;
        }
        let Some(mut frame) = presenter.frame() else {
            return;
        };

        // The tiles are copied out of the frame in the order they are listed,
        // so the presenter can cut them apart again without knowing the frame.
        frame.pixels.clear();
        frame.tiles.clear();
        frame.tiles.reserve(self.pending.len());
        let stride = self.frame.width as usize * BYTES;
        for index in self.pending.indices() {
            let tile = self.tiles.tile(&self.frame, index);
            for row in 0..tile.height {
                let start = (tile.y as usize + row as usize) * stride + tile.x as usize * BYTES;
                frame.pixels.extend_from_slice(
                    &self.frame.pixels()[start..start + tile.width as usize * BYTES],
                );
            }
            frame.tiles.push(self.placement(tile));
        }

        cost.sent = frame.tiles.len();
        match presenter.present(frame) {
            Ok(()) => {
                self.pending.clear();
            }
            // Not taken: the tiles stay due, so nothing is lost by the wait.
            Err(frame) => presenter.recycle(frame),
        }
    }

    /// Where one tile goes and what it is called.
    fn placement(&self, tile: Rect) -> kitty::Placement {
        let (cell_width, cell_height) = self.cell;
        let columns = tile.width.div_ceil(cell_width.max(1)).max(1);
        let rows = tile.height.div_ceil(cell_height.max(1)).max(1);
        kitty::Placement {
            id: self.tiles.index(tile) as u32 + 1,
            width: tile.width,
            height: tile.height,
            cols: columns,
            rows,
            cell: (
                tile.x as u32 / cell_width.max(1),
                tile.y as u32 / cell_height.max(1),
            ),
        }
    }

    /// Tell the terminal which pointer shape the focused client asked for.
    ///
    /// The shape names come from the clients (`wp_cursor_shape_manager_v1`),
    /// the drawing is the terminal's: its pointer is a real pointer, and it
    /// keeps working while we are not drawing.
    fn draw_pointer_shape(&mut self, presenter: &Presenter) {
        let shape = match &self.cursor {
            CursorImageStatus::Named(icon) => Some(kitty::pointer_shape(*icon)),
            // A cursor sent as an image cannot be described to the terminal, and the compositor
            // does not draw one of its own, so the terminal's default pointer stands in.
            CursorImageStatus::Surface(_) | CursorImageStatus::Hidden => None,
        };
        if self.pointer_shape != shape {
            presenter.raw(crate::presenter::pointer_shape_bytes(shape));
            self.pointer_shape = shape;
        }
    }

    /// Repaint the active view into the frame buffer.
    ///
    /// Reading geometry needs `&self` while drawing needs `&mut self.frame`, so
    /// the frame is planned in drawing order first and painted afterwards.
    fn compose(&mut self) {
        self.frame.clear(BACKDROP);

        self.plan.clear();
        if let Some(surface) = self.active_surface()
            && self.snapshots.contains_key(&surface.id())
        {
            self.plan.push((surface.clone(), Point::from((0, 0))));
            let geometry = geometry_offset(&surface);
            for (popup, location) in PopupManager::popups_for_surface(&surface) {
                self.plan.push((
                    popup.wl_surface().clone(),
                    geometry + location - popup.geometry().loc,
                ));
            }
        }

        for (surface, position) in &self.plan {
            draw_tree(&mut self.frame, &self.snapshots, surface, *position);
        }
    }

    /// The surface under a point in output coordinates.
    fn surface_at(&self, point: Point<f64, Logical>) -> Option<(WlSurface, Point<i32, Logical>)> {
        if !self.frame.bounds().contains(point.x as i32, point.y as i32) {
            return None;
        }
        let surface = self.active_surface()?;
        let geometry = geometry_offset(&surface);
        for (popup, location) in PopupManager::popups_for_surface(&surface) {
            let origin = geometry + location - popup.geometry().loc;
            if let Some(under) = surface_under(&self.snapshots, popup.wl_surface(), point, origin) {
                return Some(under);
            }
        }
        surface_under(&self.snapshots, &surface, point, Point::from((0, 0)))
    }

    /// Re-read the terminal geometry after a resize and tell the client.
    pub fn resize(&mut self, capabilities: &Capabilities) {
        self.cell = capabilities.cell;
        self.frame
            .resize(capabilities.pixels.0, capabilities.pixels.1);
        self.tiles = Tiles::new(&self.frame, tile_size(capabilities.cell));
        self.pending = PendingTiles::new(self.tiles.tile_count());
        let mode = output_mode(capabilities);
        self.output.set_preferred(mode);
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        self.configure_windows();
        self.scene_dirty = true;
    }

    // ---------------------------------------------------------------- input

    /// Handle a key the terminal reported.
    ///
    /// Terminals are not required to report key releases - multiplexers and
    /// every legacy encoding do not - so a key press is treated as a whole
    /// keystroke: press it, release it, and let the terminal's own
    /// auto-repeat produce the repeats. Holding a key down is otherwise
    /// indistinguishable from a key that was never let go, and the client would
    /// repeat it forever.
    pub fn key(&mut self, event: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyEventKind};

        self.sync_modifiers(event.modifiers);
        let Some(stroke) = keys::for_key(event.code) else {
            if let KeyCode::Char(c) = event.code {
                tracing::debug!(?c, "character has no key code in the advertised keymap");
            }
            return;
        };
        tracing::debug!(?event, code = stroke.code, "key");

        // Modifier keys are the exception: they are *state* for everything
        // typed while they are held, so they follow the terminal's
        // modifier flags rather than a keystroke.
        if matches!(event.code, KeyCode::Modifier(_)) {
            match event.kind {
                KeyEventKind::Press => self.press_modifier(stroke.code),
                KeyEventKind::Repeat => {}
                KeyEventKind::Release => self.release_modifier(stroke.code),
            }
            return;
        }

        match event.kind {
            KeyEventKind::Press | KeyEventKind::Repeat => {
                if !self.binding(event.modifiers, stroke.code) {
                    self.type_stroke(stroke);
                }
            }
            // The press released this key already; there is nothing left to let go of.
            KeyEventKind::Release => {}
        }
    }

    /// Press a key and let it go, holding shift for as long as its symbol needs
    /// it.
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

    /// Handle compositor bindings before a key reaches the active client.
    fn binding(&mut self, modifiers: crossterm::event::KeyModifiers, code: u32) -> bool {
        use crossterm::event::KeyModifiers as M;
        if !modifiers.contains(BINDING_MODIFIER)
            || modifiers.contains(M::CONTROL)
            || modifiers.contains(M::SUPER)
        {
            return false;
        }
        match code {
            15 => self.cycle_window(),
            16 => self.quitting = true,
            _ => return false,
        }
        true
    }

    /// Type text into the focused client, one keystroke per character.
    ///
    /// Text arrives as characters (from the terminal's paste), while clients
    /// want key presses, so each character is matched to the stroke that
    /// produces it.
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

    /// Send the modifier presses and releases the terminal's flags imply.
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

    /// Press a modifier key if it is not down yet.
    fn press_modifier(&mut self, code: u32) {
        if !self.is_pressed(code) {
            self.forward_key(code, KeyState::Pressed);
        }
    }

    /// Release a modifier key if it is down.
    fn release_modifier(&mut self, code: u32) {
        if self.is_pressed(code) {
            self.forward_key(code, KeyState::Released);
        }
    }

    fn is_pressed(&self, code: u32) -> bool {
        self.pressed.contains(&code)
    }

    /// Hand a key to the focused client.
    ///
    /// Everything in this module counts keys the way the terminal and `KEY_*`
    /// do (evdev); the seat counts the way XKB does, eight codes further
    /// along, so the conversion happens here and only here. Mixing the two
    /// namespaces up is not a loud failure: it types the neighbouring key.
    fn forward_key(&mut self, code: u32, state: KeyState) {
        tracing::debug!(code, ?state, "forwarding key");
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
            Keycode::from(code + keys::XKB_OFFSET),
            state,
            serial,
            time,
            |_, _, _| FilterResult::<()>::Forward,
        );
    }

    /// Handle pointer motion.
    pub fn pointer_motion(&mut self, position: Point<f64, Logical>) {
        self.pointer_position = position;
        // The seat wants the pointer's position in *output* coordinates and the
        // focused surface's origin beside it: it subtracts the two to
        // get the position the client is told about.
        let event = MotionEvent {
            location: position,
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
        };
        let pointer = self.pointer.clone();
        if let Some((surface, origin)) = self.surface_at(position) {
            pointer.motion(self, Some((surface, origin.to_f64())), &event);
        } else {
            pointer.motion(self, None, &event);
        }
        pointer.frame(self);
    }

    /// Handle a mouse button.
    pub fn pointer_button(&mut self, button: u32, pressed: bool) {
        let pointer = self.pointer.clone();
        let event = ButtonEvent {
            serial: SERIAL_COUNTER.next_serial(),
            time: self.time(),
            button,
            state: if pressed {
                ButtonState::Pressed
            } else {
                ButtonState::Released
            },
        };
        pointer.button(self, &event);
        pointer.frame(self);
    }

    /// Handle a scroll wheel tick.
    pub fn pointer_axis(&mut self, vertical: f64) {
        let pointer = self.pointer.clone();
        if let Some((surface, origin)) = self.surface_at(self.pointer_position) {
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

    /// Whether the main loop should present now. A due frame waits without
    /// polling until the presenter reports that its reusable frame is ready.
    ///
    /// This is asked only once the terminal is ours, which `main` decides; on
    /// its own this says whether anything has changed. It must not wait for the
    /// active window to have pixels: cycling to a window that has not drawn yet
    /// still has to produce a frame, because that frame is what carries the
    /// callback the client draws its first one for.
    pub const fn should_present(&self, presenter_ready: bool) -> bool {
        self.scene_dirty || self.pointer_dirty || (!self.pending.is_empty() && presenter_ready)
    }
}

/// The mode the output advertises: the whole terminal, at the terminal's pixel
/// size.
fn output_mode(capabilities: &Capabilities) -> Mode {
    Mode {
        size: (capabilities.pixels.0 as i32, capabilities.pixels.1 as i32).into(),
        // A terminal has no refresh rate; frame callbacks are what pace clients here.
        refresh: crate::REFRESH_MILLIHZ,
    }
}

/// Tile size in pixels for a cell size.
const fn tile_size(cell: (u32, u32)) -> (u32, u32) {
    (cell.0 * TILE_CELLS.0, cell.1 * TILE_CELLS.1)
}

/// xdg-shell state names, spelled out once.
mod xdg_state {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

    pub const ACTIVATED: xdg_toplevel::State = xdg_toplevel::State::Activated;
    pub const MAXIMIZED: xdg_toplevel::State = xdg_toplevel::State::Maximized;
}

/// What a client calls its window: the app ID, which is the name a shell would
/// have started it by, or the title when the client has no app ID.
///
/// This is only ever read to answer `list`, so it is kept in the window rather
/// than re-read: the label is what a shell offers beside a window ID, and a
/// title is not worth a lock on the surface for every keystroke of a
/// completion.
fn window_label(surface: &ToplevelSurface) -> Option<String> {
    with_states(surface.wl_surface(), |states| {
        let label = {
            let attributes = states
                .data_map
                .get::<XdgToplevelSurfaceData>()?
                .lock()
                .ok()?;
            attributes
                .app_id
                .as_ref()
                .or(attributes.title.as_ref())?
                .clone()
        };
        (!label.trim().is_empty()).then_some(label)
    })
}

/// Where the toplevel's window geometry starts, relative to its surface origin.
///
/// Popup positions are handed to us relative to that rectangle, so this is what
/// turns a popup offset into a position on screen.
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
/// Sub-surfaces are drawn as part of their parent, shifted by their own
/// position. Both traversal closures are handed the *parent's* accumulated
/// location, so each one adds its own offset.
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

/// Blend one surface of a tree into the frame, from the copy taken when it was
/// committed.
fn draw_surface(
    frame: &mut Frame,
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    states: &SurfaceData,
    location: Point<i32, Logical>,
) {
    // A surface with nothing committed (or a detached buffer) draws nothing.
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

/// Where a sub-surface sits relative to its parent (zero for a toplevel).
fn subsurface_offset(states: &SurfaceData) -> Point<i32, Logical> {
    states
        .cached_state
        .get::<SubsurfaceCachedState>()
        .current()
        .location
}

/// The topmost surface of a tree that a point hits, in the coordinates its
/// client expects.
///
/// The point is in output coordinates and `origin` is where the tree's root
/// sits on screen. A surface only accepts input inside its input region;
/// without one set, the whole surface does.
fn surface_under(
    snapshots: &HashMap<ObjectId, Snapshot>,
    surface: &WlSurface,
    point: Point<f64, Logical>,
    origin: Point<i32, Logical>,
) -> Option<(WlSurface, Point<i32, Logical>)> {
    use std::cell::RefCell;

    let found = RefCell::new(None);
    // Downward order is topmost first, which is exactly the order a click has
    // to be matched in.
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
/// A client can carve its sensitive area out with
/// `wl_surface.set_input_region`; without a region the whole surface counts, as
/// the protocol says.
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

/// Send the frame callbacks a surface tree is waiting for, and clear them.
///
/// A client that is animation-driven draws one frame per callback, so this is
/// what paces every client inside the compositor.
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

/// Per-client data, as required by the Wayland backend.
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
        // Every client that reaches this point was inserted by us, with this
        // data attached.
        &client
            .get_data::<MeowlandClient>()
            .expect("clients are always inserted with MeowlandClient data")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        self.popup_manager.commit(surface);

        // A popup is configured by the compositor before the client may map it;
        // if the client committed before we got around to it, do it
        // now.
        if let Some(PopupKind::Xdg(popup)) = self.popup_manager.find_popup(surface)
            && !popup.is_initial_configure_sent()
            && let Err(err) = popup.send_configure()
        {
            tracing::debug!(?err, "popup configure failed");
        }

        // Every surface that commits has to be copied and handed back, not just
        // the toplevels: sub-surfaces, popups and the client's cursor
        // all arrive here too.
        //
        // A window takes the screen when it first has pixels to put on it, and
        // only if no newer window has already shown something. Nothing else
        // moves it: a toplevel's first commit carries no buffer - it is how the
        // client asks to be configured - a client that later unmaps one is
        // hiding a window rather than closing it, and a window that redraws is
        // the window that was already showing. The window leaves the list when
        // it is destroyed, and is chosen again by `attach` or the cycle binding
        // when the user says so.
        let drawn = self.snapshots.contains_key(&surface.id());
        self.snapshot(surface);
        self.scene_dirty = true;
        if !drawn
            && self.snapshots.contains_key(&surface.id())
            && let Some(index) = self
                .windows
                .iter()
                .position(|window| window.surface.wl_surface() == surface)
            && self.active.is_none_or(|active| active < index)
        {
            // Whatever the client asked for, a window with nothing on it is not
            // worth looking at - and some clients open windows they never draw
            // in at all, which must not leave the screen without the one that
            // is drawing.
            self.activate_index(index);
        }
        tracing::debug!(id = ?surface.id(), "committed");
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.snapshots.remove(&surface.id());
    }
}

/// Take a copy of whatever the surface just committed, and hand the buffer
/// straight back.
///
/// This is the moment client memory is read: from here on the surface is
/// composited from [`Meowland::snapshots`], so a client reusing its buffer
/// cannot tear a frame, and a client that is waiting for its buffer to come
/// back is not kept waiting.
impl Meowland {
    fn snapshot(&mut self, surface: &WlSurface) {
        let limit = self.frame.bounds();
        let committed = with_states(surface, |states| {
            // The cached state is only borrowed for the few reads that need it:
            // the copy below happens with no surface lock held.
            let (buffer, scale) = {
                let mut state = states.cached_state.get::<SurfaceAttributes>();
                let current = state.current();
                let buffer = current.buffer.take();
                let scale = current.buffer_scale;
                // A snapshot copies the complete buffer, so all accumulated
                // damage has been consumed. Leaving it here would make Smithay
                // retain every damage rectangle across future commits.
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
                        (limit.width, limit.height),
                        entry.get_mut(),
                        self.gpu.as_mut(),
                    ),
                    Entry::Vacant(entry) => {
                        let mut snapshot = Snapshot::empty();
                        let copied = crate::buffer::snapshot(
                            &buffer,
                            scale,
                            (limit.width, limit.height),
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

    /// Decide, before the client draws into it, whether a GPU buffer is one
    /// meowland will be able to read when it is committed.
    ///
    /// Answering no is cheap and the client can still fall back to shared
    /// memory; answering yes and finding out later is not, because by then the
    /// client has stopped drawing into shared memory.
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
        self.next_window_id = self.next_window_id.saturating_add(1);
        tracing::info!(id, wayland_id = ?surface.wl_surface().id(), "new window");
        let label = window_label(&surface);
        self.windows.push(Window { id, surface, label });
        self.activate_index(self.windows.len() - 1);
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
        // Popups are not given keyboard grabs: the terminal's own shortcuts
        // stay usable.
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

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        let _ = surface;
        self.configure_windows();
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let _ = surface;
        self.configure_windows();
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let _ = surface;
        self.configure_windows();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.output.leave(surface.wl_surface());
        let Some(removed) = self
            .windows
            .iter()
            .position(|window| window.surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        let was_active = self.active == Some(removed);
        self.windows.remove(removed);
        self.active = match self.active {
            None => None,
            Some(_) if self.windows.is_empty() => None,
            Some(active) if active > removed => Some(active - 1),
            Some(active) if was_active => Some(active.min(self.windows.len() - 1)),
            active => active,
        };
        if was_active {
            let surface = self.active_surface();
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, surface.clone(), SERIAL_COUNTER.next_serial());
            if let Some(surface) = surface {
                self.output.enter(&surface);
            }
        }
        self.configure_windows();
        self.scene_dirty = true;
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
        self.pointer_dirty = true;
    }
}

/// The cursor-shape protocol asks for this: meowland has no tablets, so the
/// default (which ignores tool images) is all it needs.
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

/// Bring up a renderer on the first render node that has one, and say which
/// node that was.
///
/// Without a renderer there is no way to read a buffer the CPU cannot map, and
/// a client that hands one over would have nothing to show, so this is what
/// decides whether clients are offered GPU buffers at all - and it decides on
/// which device, since a device no renderer can be built on is one whose
/// buffers could not be read back.
fn bring_up_renderer(
    nodes: &[crate::dmabuf::RenderNode],
) -> Option<(crate::gpu::Renderer, crate::dmabuf::RenderNode)> {
    for node in nodes {
        match crate::gpu::Renderer::new(&node.path) {
            Ok(renderer) => return Some((renderer, node.clone())),
            Err(err) => tracing::info!(?err, "no renderer on this render node"),
        }
    }
    None
}

/// Offer clients the device their GPU buffers are read back through, if one was
/// asked for.
///
/// A client that renders on the GPU only keeps doing so if it is told where to
/// put the memory, and can only hand that memory over if meowland can read it
/// back - so what is advertised here is what the renderer takes, and the device
/// it is on, and nothing else. See [`crate::dmabuf`] for what keeps that offer
/// honest.
///
/// Not offering it is not an error: the global is simply never advertised, and
/// clients draw into shared memory.
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
        device = node.device,
        formats = formats.len(),
        "offering GPU buffers to clients"
    );
    let feedback = DmabufFeedbackBuilder::new(node.device, formats)
        .build()
        .map_err(Error::Feedback)?;
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
