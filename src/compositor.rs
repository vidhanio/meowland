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
//! # One window, terminal sized
//!
//! meowland runs one client, in one window, filling the terminal: no tiling, no
//! focus ring, no decorations, no other key bindings than the one that leaves.
//! The toplevels it knows about are therefore just "the newest one that has
//! drawn something is the one on screen" - which keeps dialogs and popups
//! working without a window manager in between.

use std::{collections::HashMap, os::unix::net::UnixStream, sync::Arc, time::Instant};

use smithay::{
    backend::input::{ButtonState, KeyState},
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_output,
    delegate_seat, delegate_shm, delegate_xdg_shell,
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
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
            XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
    keys, kitty,
    render::{Frame, Rect, Tiles},
    shm::Snapshot,
    tty::{Capabilities, Terminal},
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

/// Backdrop behind the window, for the moment before a client has drawn
/// anything.
const BACKDROP: [u8; 3] = [0x14, 0x16, 0x1b];

/// The compositor.
pub struct Meowland {
    // Protocol state.
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Self>,
    /// Kept alive so the `zxdg_output_manager_v1` global stays advertised;
    /// never queried.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    output_manager_state: OutputManagerState,
    data_device_state: DataDeviceState,
    /// Kept alive so clients can name a cursor shape instead of sending a
    /// cursor image.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    cursor_shape_state: CursorShapeManagerState,
    popup_manager: PopupManager,

    // Core.
    display_handle: DisplayHandle,
    socket_name: String,
    start: Instant,
    output: Output,
    keyboard: KeyboardHandle<Self>,
    pointer: PointerHandle<Self>,

    // Windows. There is no window management: whatever the client asks for, its toplevel fills the
    // terminal, and the newest one that has drawn is the one on screen.
    toplevels: Vec<ToplevelSurface>,
    presented: Option<usize>,

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
    scratch: Vec<u8>,
    needs_redraw: bool,
    /// Set by the quit binding and by the main loop's own reasons to stop.
    quitting: bool,
    cell: (u32, u32),
}

impl std::fmt::Debug for Meowland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meowland")
            .field("socket_name", &self.socket_name)
            .field("toplevels", &self.toplevels.len())
            .field("presented", &self.presented)
            .finish_non_exhaustive()
    }
}

impl Meowland {
    /// Advertise the initial state to clients.
    pub fn new(
        display: &DisplayHandle,
        socket_name: String,
        capabilities: &Capabilities,
    ) -> anyhow::Result<Self> {
        let compositor_state = CompositorState::new::<Self>(display);
        let shm_state = ShmState::new::<Self>(display, []);
        let xdg_shell_state = XdgShellState::new::<Self>(display);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(display);
        let data_device_state = DataDeviceState::new::<Self>(display);
        let cursor_shape_state = CursorShapeManagerState::new::<Self>(display);
        let mut seat_state = SeatState::new();

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
        let keyboard = seat.add_keyboard(
            XkbConfig {
                rules: "evdev",
                model: "pc105",
                layout: "us",
                variant: "",
                options: None,
            },
            250,
            30,
        )?;
        let pointer = seat.add_pointer();

        let frame = Frame::new(capabilities.pixels.0, capabilities.pixels.1);
        let tiles = Tiles::new(&frame, tile_size(capabilities.cell));

        Ok(Self {
            compositor_state,
            shm_state,
            xdg_shell_state,
            seat_state,
            output_manager_state,
            data_device_state,
            cursor_shape_state,
            popup_manager: PopupManager::default(),
            display_handle: display.clone(),
            socket_name,
            start: Instant::now(),
            output,
            keyboard,
            pointer,
            toplevels: Vec::new(),
            presented: None,
            pressed: Vec::new(),
            pointer_position: (0.0, 0.0).into(),
            cursor: CursorImageStatus::default_named(),
            pointer_shape: None,
            snapshots: HashMap::new(),
            frame,
            tiles,
            scratch: Vec::new(),
            needs_redraw: true,
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

    /// The surface the keyboard and pointer events are aimed at: the one on
    /// screen.
    fn presented_surface(&self) -> Option<WlSurface> {
        self.presented
            .and_then(|index| self.toplevels.get(index))
            .map(|toplevel| toplevel.wl_surface().clone())
    }

    /// Make the newest toplevel that has drawn something the one on screen.
    ///
    /// A client's dialogs and popup windows are toplevels too: the one it
    /// committed to last is the one on top, and when it goes away the
    /// previous one is still there, with its buffer intact.
    fn present_newest_window(&mut self) {
        let newest = self
            .toplevels
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, toplevel)| {
                self.snapshots
                    .contains_key(&toplevel.wl_surface().id())
                    .then_some(index)
            });
        if newest == self.presented {
            return;
        }
        self.presented = newest;
        let surface = self.presented_surface();
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, surface.clone(), SERIAL_COUNTER.next_serial());
        if let Some(surface) = surface {
            self.output.enter(&surface);
        }
        self.needs_redraw = true;
    }

    /// Ask a toplevel to fill the terminal.
    fn maximize(&self, toplevel: &ToplevelSurface) {
        let size = (self.frame.width as i32, self.frame.height as i32);
        toplevel.with_pending_state(|state| {
            state.size = Some(size.into());
            state.states.set(xdg_state::MAXIMIZED);
            state.states.set(xdg_state::ACTIVATED);
        });
        let _ = toplevel.send_configure();
    }

    /// Fill the frame with the current state of the window and hand the changes
    /// to the terminal.
    pub fn present(&mut self, terminal: &mut Terminal) -> anyhow::Result<()> {
        self.compose();
        let dirty = self.tiles.diff(&self.frame);
        {
            let out = terminal.frame();
            kitty::begin_sync(out);
            for tile in &dirty {
                self.encode_tile(out, tile);
            }
            self.draw_pointer_shape(terminal);
            kitty::end_sync(terminal.frame());
        }
        terminal.present()?;

        self.needs_redraw = false;
        // Everything just composed is on screen, so the clients owning those
        // surfaces may start their next frame. Popups live outside the
        // parent's tree, so they are walked separately.
        let time = self.time();
        for toplevel in &self.toplevels {
            let surface = toplevel.wl_surface().clone();
            send_frame_callbacks(&surface, time);
            for (popup, _) in PopupManager::popups_for_surface(&surface) {
                send_frame_callbacks(popup.wl_surface(), time);
            }
        }
        Ok(())
    }

    /// Write one dirty tile to the terminal: address its first cell, transmit,
    /// place.
    fn encode_tile(&mut self, out: &mut Vec<u8>, tile: &Rect) {
        let (cell_width, cell_height) = self.cell;
        let columns = tile.width.div_ceil(cell_width.max(1)).max(1);
        let rows = tile.height.div_ceil(cell_height.max(1)).max(1);
        kitty::cursor_to(
            out,
            tile.x as u32 / cell_width.max(1),
            tile.y as u32 / cell_height.max(1),
        );
        // Terminals take whole images, so any partial repaint is a copy of the
        // tile's pixels into a contiguous scratch buffer first.
        let stride = self.frame.width as usize * 4;
        self.scratch.clear();
        self.scratch
            .reserve(tile.width as usize * tile.height as usize * 4);
        for row in 0..tile.height {
            let start = (tile.y as usize + row as usize) * stride + tile.x as usize * 4;
            self.scratch
                .extend_from_slice(&self.frame.pixels()[start..start + tile.width as usize * 4]);
        }
        let id = tile_id(
            &self.tiles,
            tile.x as u32 / self.tiles.size.0.max(1),
            tile.y as u32 / self.tiles.size.1.max(1),
        );
        kitty::transmit_and_place(
            out,
            id,
            &self.scratch,
            tile.width,
            tile.height,
            columns,
            rows,
        );
    }

    /// Tell the terminal which pointer shape the focused client asked for.
    ///
    /// The shape names come from the clients (`wp_cursor_shape_manager_v1`),
    /// the drawing is the terminal's: its pointer is a real pointer, and it
    /// keeps working while we are not drawing.
    fn draw_pointer_shape(&mut self, terminal: &mut Terminal) {
        let shape = match &self.cursor {
            CursorImageStatus::Named(icon) => Some(kitty::pointer_shape(*icon)),
            // A cursor sent as an image cannot be described to the terminal, and the compositor
            // does not draw one of its own, so the terminal's default pointer stands in.
            CursorImageStatus::Surface(_) | CursorImageStatus::Hidden => None,
        };
        if self.pointer_shape != shape {
            terminal.pointer_shape(shape);
            self.pointer_shape = shape;
        }
    }

    /// Repaint the window into the frame buffer.
    ///
    /// Reading geometry needs `&self` while drawing needs `&mut self.frame`, so
    /// the frame is planned in drawing order first and painted afterwards.
    fn compose(&mut self) {
        self.frame.clear(BACKDROP);

        let mut plan = Vec::new();
        if let Some(toplevel) = self.presented.and_then(|index| self.toplevels.get(index)) {
            let surface = toplevel.wl_surface().clone();
            if self.snapshots.contains_key(&surface.id()) {
                plan.push((surface.clone(), Point::from((0, 0))));
                // Popups live outside the parent's surface tree, at a position
                // the parent's commit computed for them,
                // relative to the window's geometry.
                let geometry = geometry_offset(&surface);
                for (popup, location) in PopupManager::popups_for_surface(&surface) {
                    // The manager hands out the popup's own surface position;
                    // the client's geometry is what its
                    // visible content starts at, so the difference is the
                    // content offset.
                    plan.push((
                        popup.wl_surface().clone(),
                        geometry + location - popup.geometry().loc,
                    ));
                }
            }
        }

        for (surface, position) in plan {
            draw_tree(&mut self.frame, &self.snapshots, &surface, position);
        }
    }

    /// The surface under a point in output coordinates.
    ///
    /// Only the presented window can be under the pointer: it is the only thing
    /// on screen.
    fn surface_at(&self, point: Point<f64, Logical>) -> Option<(WlSurface, Point<i32, Logical>)> {
        let toplevel = self.presented.and_then(|index| self.toplevels.get(index))?;
        let surface = toplevel.wl_surface();
        if !self.frame.bounds().contains(point.x as i32, point.y as i32) {
            return None;
        }
        let geometry = geometry_offset(surface);
        for (popup, location) in PopupManager::popups_for_surface(surface) {
            let origin = geometry + location - popup.geometry().loc;
            if let Some(under) = surface_under(popup.wl_surface(), point, origin) {
                return Some(under);
            }
        }
        surface_under(surface, point, Point::from((0, 0)))
    }

    /// Re-read the terminal geometry after a resize and tell the client.
    pub fn resize(&mut self, capabilities: &Capabilities) {
        self.cell = capabilities.cell;
        self.frame
            .resize(capabilities.pixels.0, capabilities.pixels.1);
        self.tiles = Tiles::new(&self.frame, tile_size(capabilities.cell));
        let mode = output_mode(capabilities);
        self.output.set_preferred(mode);
        self.output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        for toplevel in &self.toplevels {
            self.maximize(toplevel);
        }
        self.needs_redraw = true;
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

    /// The one binding: `Alt+Q` leaves, killing the client with it.
    const fn binding(&mut self, modifiers: crossterm::event::KeyModifiers, code: u32) -> bool {
        use crossterm::event::KeyModifiers as M;
        if !modifiers.contains(BINDING_MODIFIER)
            || modifiers.contains(M::CONTROL)
            || modifiers.contains(M::SUPER)
        {
            return false;
        }
        if code != 16 {
            // Alt+Q is the only shortcut there is.
            return false;
        }
        self.quitting = true;
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
        self.needs_redraw = true;
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

    /// Whether anything changed that the terminal should be told about.
    pub const fn needs_frame(&self) -> bool {
        self.needs_redraw
    }
}

/// The mode the output advertises: the whole terminal, at the terminal's pixel
/// size.
fn output_mode(capabilities: &Capabilities) -> Mode {
    Mode {
        size: (capabilities.pixels.0 as i32, capabilities.pixels.1 as i32).into(),
        // A terminal has no refresh rate; frame callbacks are what pace clients here.
        refresh: 60_000,
    }
}

/// Tile size in pixels for a cell size.
const fn tile_size(cell: (u32, u32)) -> (u32, u32) {
    (cell.0 * TILE_CELLS.0, cell.1 * TILE_CELLS.1)
}

/// The image id of a tile: stable across frames, so a tile's image can be
/// replaced in place.
const fn tile_id(tiles: &Tiles, x: u32, y: u32) -> u32 {
    y * tiles.grid.0 + x + 1
}

/// xdg-shell state names, spelled out once.
mod xdg_state {
    use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;

    pub const ACTIVATED: xdg_toplevel::State = xdg_toplevel::State::Activated;
    pub const MAXIMIZED: xdg_toplevel::State = xdg_toplevel::State::Maximized;
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
    location: Point<i32, Logical>,
) {
    // A surface with nothing committed (or a detached buffer) draws nothing.
    let Some(snapshot) = snapshots.get(&surface.id()) else {
        return;
    };
    let (width, height) = snapshot.logical_size();
    frame.draw(
        &snapshot.image(),
        Rect::new(0, 0, snapshot.width, snapshot.height),
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
            if accepts_input(states, point - location.to_f64()) {
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
fn accepts_input(states: &SurfaceData, local: Point<f64, Logical>) -> bool {
    let mut state = states.cached_state.get::<SurfaceAttributes>();
    let attributes = state.current();
    attributes.input_region.as_ref().map_or_else(
        || local.x >= 0.0 && local.y >= 0.0,
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
        self.snapshot(surface);
        self.needs_redraw = true;
        tracing::debug!(id = ?surface.id(), "committed");

        if self
            .toplevels
            .iter()
            .any(|toplevel| toplevel.wl_surface() == surface)
        {
            self.present_newest_window();
        }
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
                (state.current().buffer.take(), state.current().buffer_scale)
            };
            buffer.map(|buffer| (buffer, scale))
        });
        match committed {
            Some((BufferAssignment::NewBuffer(buffer), scale)) => {
                match crate::shm::snapshot(&buffer, scale, (limit.width, limit.height)) {
                    Some(snapshot) => {
                        self.snapshots.insert(surface.id(), snapshot);
                    }
                    None => {
                        tracing::debug!(format = ?buffer, "buffer is not one we can composite");
                    }
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

impl XdgShellHandler for Meowland {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        tracing::debug!(id = ?surface.wl_surface().id(), "new toplevel");
        // Whatever the client asked for, it gets the terminal - that is the
        // whole window policy.
        self.maximize(&surface);
        self.toplevels.push(surface);
        self.needs_redraw = true;
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
        // There is only one size: the terminal's.
        self.maximize(&surface);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        self.maximize(&surface);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        self.maximize(&surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.toplevels
            .retain(|toplevel| toplevel.wl_surface() != surface.wl_surface());
        self.present_newest_window();
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {}
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
        self.needs_redraw = true;
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

delegate_compositor!(Meowland);
delegate_shm!(Meowland);
delegate_xdg_shell!(Meowland);
delegate_output!(Meowland);
delegate_seat!(Meowland);
delegate_data_device!(Meowland);
delegate_cursor_shape!(Meowland);
