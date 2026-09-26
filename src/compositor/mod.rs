//! The Wayland side of the server.
//!
//! One Smithay display runs on its own thread and talks to the rest of the
//! server over two channels: [`Command`] in, [`Event`] out.  Client buffers
//! are copied into owned snapshots the moment they are committed, so a client
//! that redraws into a buffer it still owns cannot tear a frame that is on its
//! way to a terminal.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_output,
    delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorIcon, CursorImageStatus, MotionEvent, PointerHandle,
        },
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle, ListeningSocket, Resource,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_shm, wl_surface::WlSurface},
        },
    },
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SubsurfaceCachedState, SurfaceAttributes, get_children, get_parent, with_states,
        },
        cursor_shape::CursorShapeManagerState,
        output::{OutputHandler, OutputManagerState},
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
                set_data_device_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
            XdgShellState, XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        tablet_manager::TabletSeatHandler,
        viewporter::{ViewportCachedState, ViewporterState},
    },
};

use crate::protocol::{Input, Show, WindowInfo, modifiers};

mod render;

use render::{
    extents, hit_test, popup_origin, render_frame, surface_stack, viewport_of, window_geometry,
};

/// Windows are composed at most this often.  A client that draws in a loop
/// cannot spin the compositor, and a pane that is slow costs its own frames.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);
/// A frame the pane has not acknowledged for this long is given up on: the
/// client is told it may draw again instead of waiting on a pane that may be
/// stuck writing to a terminal that stopped reading.  Long enough that a slow
/// terminal still gets its frames, short enough that a client never freezes.
/// The pane then stays behind until it acknowledges one, so a slow terminal
/// costs itself frames and nothing else.
const ACK_TIMEOUT: Duration = Duration::from_millis(500);
/// A window no pane shows still has to keep drawing, or it could never be
/// attached to.  It gets frame callbacks at the same rate.
const IDLE_INTERVAL: Duration = Duration::from_millis(16);
/// Largest surface side and pixel count the compositor will copy; the pane
/// protocol's size gate uses the same limits, so a pane can never be told a
/// size its clients' buffers would be refused at.
pub const MAX_SURFACE_SIDE: u32 = 8192;
pub const MAX_SURFACE_PIXELS: usize = 16_000_000;
/// Each modifier bit a stroke can carry, with the Linux input code of the key
/// that holds it down while the stroke is sent.
const MODIFIER_KEYS: [(u8, u16); 4] = [
    (modifiers::SHIFT, 42),
    (modifiers::CONTROL, 29),
    (modifiers::ALT, 56),
    (modifiers::SUPER, 125),
];

#[derive(Clone, Copy, Debug)]
pub enum Command {
    Attach {
        pane: u64,
        show: Show,
        width: u32,
        height: u32,
    },
    Detach {
        pane: u64,
    },
    Resize {
        pane: u64,
        width: u32,
        height: u32,
    },
    Input {
        pane: u64,
        event: Input,
    },
    Ack {
        pane: u64,
        /// Whether the pane drew the frame it is acknowledging.
        drawn: bool,
    },
    CloseAll,
    CloseShown {
        pane: u64,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    /// A window appeared, or its title or app id changed.
    WindowUp(WindowInfo),
    WindowDown(u64),
    /// The title a pane should show for the window it is displaying.
    Title {
        pane: u64,
        title: String,
    },
    /// The pointer shape a pane should show, or `None` for the terminal's own.
    Cursor {
        pane: u64,
        shape: Option<String>,
    },
    Focus(u64),
    Frame {
        pane: u64,
        width: u32,
        height: u32,
        /// The first row `rgb` covers.
        y: u32,
        rgb: Vec<u8>,
    },
    Release {
        pane: u64,
        reason: String,
    },
}

/// The channels and thread a running compositor is reached through.
pub type Handle = (
    mpsc::Sender<Command>,
    mpsc::Receiver<Event>,
    String,
    thread::JoinHandle<()>,
);

/// Start the compositor thread and wait until it is listening.
///
/// # Errors
/// Returns an error when the thread cannot be spawned, when the compositor
/// fails to build its display, keymap or Wayland socket, or when it does not
/// report readiness within two seconds.
pub fn spawn() -> crate::Result<Handle> {
    let (commands, rx) = mpsc::channel();
    let (tx, events) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let socket = format!("wayland-meowland-{}", std::process::id());
    let socket_for_thread = socket.clone();
    let handle = thread::Builder::new()
        .name("meowland-wayland".into())
        .spawn(move || run(socket_for_thread, rx, tx, ready_tx))?;
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Wayland compositor did not start",
            )
        })??;
    Ok((commands, events, socket, handle))
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}
impl ClientData for ClientState {
    fn initialized(&self, _id: ClientId) {}

    fn disconnected(&self, _id: ClientId, _reason: DisconnectReason) {}
}

/// A committed buffer, copied out of the client's memory at commit time.
#[derive(Debug)]
struct Snapshot {
    width: u32,
    height: u32,
    /// `r, g, b, a` per pixel, premultiplied as Wayland defines it.
    pixels: Vec<u8>,
    opaque: bool,
}

/// Where a pane is with the frame it was last given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrameState {
    /// Nothing in flight: the next frame can go out.
    Ready,
    /// A frame is with the pane until it acknowledges it.
    InFlight,
    /// The pane never took the frame it was given, so it is behind and nothing
    /// is rendered for it until it acknowledges one.
    Behind,
}

struct PaneState {
    window: u64,
    width: u32,
    height: u32,
    frame: FrameState,
    /// When the frame in flight was handed over, so one that is never
    /// acknowledged can be given up on instead of freezing the client that is
    /// waiting on it.
    sent: Instant,
    /// Acks for frames that were given up on, which say nothing about the
    /// frame that is in flight now.
    stale_acks: u32,
    /// The scene changed while that frame was in flight.
    dirty: bool,
    next_frame: Instant,
    /// The last frame this pane was sent, so an unchanged scene costs nothing.
    shown: Vec<u8>,
    /// The pane did not draw that frame, so `shown` is not what its terminal
    /// displays and the next frame has to be sent whatever it contains.
    shown_stale: bool,
}

impl PaneState {
    /// A pane showing `window`, or 0 for one still waiting for a window with
    /// pixels.
    fn new(window: u64, width: u32, height: u32) -> Self {
        Self {
            window,
            width,
            height,
            frame: FrameState::Ready,
            sent: Instant::now(),
            stale_acks: 0,
            dirty: false,
            next_frame: Instant::now(),
            shown: Vec::new(),
            shown_stale: false,
        }
    }
}

struct Window {
    surface: ToplevelSurface,
    title: String,
    app_id: String,
    /// Set by the first commit that carries pixels.  A window that never drew
    /// must not capture a following pane.
    announced: bool,
    fullscreen: bool,
    /// Earliest time the window's next idle frame callback may be sent.
    callback_due: Instant,
}

struct State {
    display: DisplayHandle,
    compositor: CompositorState,
    shm: ShmState,
    xdg: XdgShellState,
    _viewporter: ViewporterState,
    _output_manager: OutputManagerState,
    data_device: DataDeviceState,
    _cursor_shape: CursorShapeManagerState,
    popups: PopupManager,
    seats: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: Option<KeyboardHandle<Self>>,
    pointer: Option<PointerHandle<Self>>,
    output: Output,
    windows: HashMap<u64, Window>,
    /// Toplevel surfaces only: subsurfaces and popups are found through the
    /// surface tree.
    ids: HashMap<WlSurface, u64>,
    snapshots: HashMap<WlSurface, Snapshot>,
    next_id: u64,
    /// The size the output is currently described as.
    mode: Size<i32, Logical>,
    /// The window the newest pane selection resolves to.
    newest: Option<u64>,
    panes: HashMap<u64, PaneState>,
    following: HashSet<u64>,
    /// Panes that have something new to draw.
    pending: HashSet<u64>,
    /// For each window, the pane that last interacted with it.  That pane
    /// decides the window's configured size while it is attached.
    deciding: HashMap<u64, u64>,
    focused: Option<u64>,
    /// The pane the pointer is over, which is the one that gets cursor shapes.
    cursor_pane: Option<u64>,
    cursor: Option<String>,
    /// The pane `cursor` was last sent to, so a shape that is unchanged but
    /// belongs to another pane is still sent.
    cursor_shape_pane: Option<u64>,
    /// The buffer a frame is composed into, kept so a pane that redraws does
    /// not allocate (and fault in) a frame's worth of memory every time.
    scratch: Vec<u8>,
    events: mpsc::Sender<Event>,
    started: Instant,
    shutdown: bool,
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client state")
            .compositor_state
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "`cached` borrows from the guard and must outlive `current`, so the two \
                  statements cannot be merged"
    )]
    fn commit(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);
        let assignment = with_states(surface, |states| {
            let mut cached = states.cached_state.get::<SurfaceAttributes>();
            let current = cached.current();
            // Damage is consumed here; leaving it would make it accumulate
            // across commits.
            current.damage.clear();
            current.buffer.take()
        });
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = self.pane_bound(surface);
                // The pixels of the snapshot this one replaces are the next
                // snapshot's storage: a client redrawing at a steady size then
                // never allocates (or faults in) a frame buffer again.  A
                // buffer that cannot be copied leaves the storage to the
                // snapshot already on screen, which stays there.
                let mut previous = self.snapshots.remove(surface);
                let mut reuse = previous
                    .as_mut()
                    .map_or_else(Vec::new, |old| std::mem::take(&mut old.pixels));
                let snapshot = with_buffer_contents(&buffer, |ptr, len, data| {
                    copy_buffer(ptr, len, &data, bound, &mut reuse)
                })
                .ok()
                .flatten();
                buffer.release();
                match snapshot {
                    Some(snapshot) => {
                        self.snapshots.insert(surface.clone(), snapshot);
                    }
                    None => {
                        if let Some(mut previous) = previous {
                            previous.pixels = reuse;
                            self.snapshots.insert(surface.clone(), previous);
                        }
                    }
                }
            }
            Some(BufferAssignment::Removed) => {
                self.snapshots.remove(surface);
            }
            None => {}
        }
        self.announce(surface);
        if let Some(window) = self.window_for_surface(surface) {
            self.touch(window);
        }
    }

    /// A destroyed surface takes its snapshot with it: `toplevel_destroyed`
    /// only sweeps surfaces its tree still leads back to, which never covers a
    /// surface that had no window role at all.
    fn destroyed(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
    }
}

impl State {
    /// How large a buffer for this surface may be: at most twice the size of
    /// the panes showing its window, so a resize can be in flight, and
    /// unbounded when no pane shows it yet.  The two axes are taken
    /// independently: the tallest pane may not be the widest one.
    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)> {
        let window = self.window_for_surface(surface)?;
        self.panes
            .values()
            .filter(|pane| pane.window == window)
            .fold(None, |bound: Option<(u32, u32)>, pane| {
                let pane = (pane.width * 2, pane.height * 2);
                Some(bound.map_or(pane, |bound| (bound.0.max(pane.0), bound.1.max(pane.1))))
            })
    }

    /// The window a surface belongs to: the toplevel itself, one of its
    /// subsurfaces, or one of its popups.
    fn window_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        let mut surface = surface.clone();
        for _ in 0..64 {
            if let Some(id) = self.ids.get(&surface) {
                return Some(*id);
            }
            if let Some(popup) = self.popups.find_popup(&surface) {
                let parent = match popup {
                    PopupKind::Xdg(popup) => popup.get_parent_surface(),
                    PopupKind::InputMethod(_) => None,
                };
                surface = parent?;
                continue;
            }
            surface = get_parent(&surface)?;
        }
        None
    }

    /// A window exists for panes once it has pixels; before that it is a
    /// client that has not drawn and must not capture a following pane.
    fn announce(&mut self, surface: &WlSurface) {
        let Some(id) = self.ids.get(surface).copied() else {
            return;
        };
        if !self.snapshots.contains_key(surface) {
            return;
        }
        let Some(window) = self.windows.get_mut(&id) else {
            return;
        };
        let first = !window.announced;
        window.announced = true;
        if !first {
            return;
        }
        self.newest = Some(id);
        self.follow_panes(id);
        let info = self.window_info(id);
        let _ = self.events.send(Event::WindowUp(info));
        apply_window_state(self, id);
        self.enter_output(id);
        self.focus_window(id);
    }

    fn window_info(&self, window: u64) -> WindowInfo {
        let (app_id, title) = self
            .windows
            .get(&window)
            .map_or_default(|entry| (entry.app_id.clone(), entry.title.clone()));
        WindowInfo {
            id: window,
            app_id,
            title,
            active: self.focused == Some(window),
        }
    }

    /// The size a window is configured at: the pane the user last interacted
    /// with, the first pane showing it as a fallback, or the output's own size
    /// while no pane shows it.
    fn pane_size(&self, window: u64) -> Size<i32, Logical> {
        let deciding = self
            .deciding
            .get(&window)
            .and_then(|pane| self.panes.get(pane))
            .filter(|pane| pane.window == window);
        let first = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.window == window)
            .min_by_key(|(id, _)| *id)
            .map(|(_, pane)| pane);
        deciding.or(first).map_or(self.mode, |pane| {
            Size::from((pane.width as i32, pane.height as i32))
        })
    }

    fn shown(&self, window: u64) -> bool {
        self.panes.values().any(|pane| pane.window == window)
    }

    fn mark_dirty(&mut self, pane: u64) {
        let deferred = match self.panes.get_mut(&pane) {
            Some(state) => {
                // A frame is with the pane, or the pane never took the last
                // one: either way the scene change is remembered rather than
                // turned into a frame now.
                if state.frame != FrameState::Ready {
                    state.dirty = true;
                }
                state.frame != FrameState::Ready
            }
            None => return,
        };
        if !deferred {
            self.pending.insert(pane);
        }
    }

    /// The panes showing this window.
    fn panes_showing(&self, window: u64) -> Vec<u64> {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.window == window)
            .map(|(pane, _)| *pane)
            .collect()
    }

    /// Register a pane; while it is the lowest-numbered one, its size is the
    /// output mode every client sees.
    fn insert_pane(&mut self, pane: u64, entry: PaneState) {
        let (width, height) = (entry.width, entry.height);
        self.panes.insert(pane, entry);
        if self.panes.keys().min() == Some(&pane) {
            configure_output(self, width, height);
        }
    }

    /// Take a pane out of the compositor, wherever it is going: the window it
    /// showed is returned, and the output goes back to the panes that are
    /// left.  Everything keyed by a pane lives here so no removal path can
    /// leave a stale entry behind.
    fn remove_pane(&mut self, pane: u64) -> Option<u64> {
        let first = self.panes.keys().min() == Some(&pane);
        let window = self.panes.remove(&pane).map(|state| state.window);
        self.pending.remove(&pane);
        self.following.remove(&pane);
        self.deciding.retain(|_, deciding| *deciding != pane);
        if self.cursor_pane == Some(pane) {
            self.cursor_pane = None;
        }
        if self.cursor_shape_pane == Some(pane) {
            self.cursor_shape_pane = None;
        }
        if first && let Some((_, next)) = self.panes.iter().min_by_key(|(pane, _)| **pane) {
            configure_output(self, next.width, next.height);
        }
        window
    }

    /// What removing a pane means for the window it was showing: it is no
    /// longer drawn anywhere, and the focus moves to whichever pane is left.
    fn release_pane_window(&mut self, pane: u64) {
        let Some(window) = self.remove_pane(pane) else {
            return;
        };
        self.leave_output(window);
        if self.focused == Some(window)
            && let Some(next) = self
                .panes
                .iter()
                .min_by_key(|(pane, _)| **pane)
                .map(|(_, pane)| pane.window)
                .filter(|window| *window != 0)
        {
            self.focus_window(next);
        }
    }

    /// Point every follow-the-newest pane at `window`, with its title.
    fn follow_panes(&mut self, window: u64) {
        let following: Vec<u64> = self.following.iter().copied().collect();
        for pane in following {
            if let Some(state) = self.panes.get_mut(&pane) {
                state.window = window;
            }
            self.mark_dirty(pane);
            self.send_title(pane, window);
        }
    }

    /// Something the panes showing this window draw has changed.
    fn touch(&mut self, window: u64) {
        for pane in self.panes_showing(window) {
            self.mark_dirty(pane);
        }
    }

    fn send_title(&self, pane: u64, window: u64) {
        let title = self
            .windows
            .get(&window)
            .map_or_default(|entry| crate::protocol::sanitize(&entry.title));
        let _ = self.events.send(Event::Title { pane, title });
    }

    fn send_titles(&self, window: u64) {
        for pane in self.panes_showing(window) {
            self.send_title(pane, window);
        }
    }

    fn enter_output(&self, window: u64) {
        let Some(root) = self
            .windows
            .get(&window)
            .filter(|entry| entry.announced)
            .map(|entry| entry.surface.wl_surface().clone())
        else {
            return;
        };
        for (surface, _) in surface_stack(&root) {
            self.output.enter(&surface);
        }
    }

    fn leave_output(&self, window: u64) {
        if self.shown(window) {
            return;
        }
        let Some(root) = self
            .windows
            .get(&window)
            .map(|entry| entry.surface.wl_surface().clone())
        else {
            return;
        };
        for (surface, _) in surface_stack(&root) {
            self.output.leave(&surface);
        }
    }

    /// Focus is the compositor's; the active window's surface gets the
    /// keyboard and the window is told it is activated.
    fn focus_window(&mut self, window: u64) {
        if self.focused == Some(window) {
            return;
        }
        let previous = self.focused.replace(window);
        let surface = self
            .windows
            .get(&window)
            .map(|entry| entry.surface.wl_surface().clone());
        if let Some(keyboard) = self.keyboard.take() {
            keyboard.set_focus(self, surface, SERIAL_COUNTER.next_serial());
            self.keyboard = Some(keyboard);
        }
        if let Some(previous) = previous {
            apply_window_state(self, previous);
        }
        apply_window_state(self, window);
        let _ = self.events.send(Event::Focus(window));
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        let (app_id, title) = metadata(&surface);
        self.ids.insert(surface.wl_surface().clone(), id);
        self.windows.insert(
            id,
            Window {
                surface,
                title,
                app_id,
                announced: false,
                fullscreen: false,
                callback_due: Instant::now(),
            },
        );
        apply_window_state(self, id);
    }

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        if self
            .popups
            .track_popup(PopupKind::Xdg(surface.clone()))
            .is_err()
        {
            return;
        }
        place_popup(self, &surface, positioner);
        let _ = surface.send_configure();
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        place_popup(self, &surface, positioner);
        surface.send_repositioned(token);
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        // A click outside a popup dismisses it; nothing else is grabbed.
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        // Snapshots are keyed by surface, so they have to go while the tree
        // still leads back to this window.
        let stale: Vec<WlSurface> = self
            .snapshots
            .keys()
            .filter(|surface| self.window_for_surface(surface) == Some(id))
            .cloned()
            .collect();
        for surface in stale {
            self.snapshots.remove(&surface);
        }
        self.ids.remove(surface.wl_surface());
        for pane in self.panes_showing(id) {
            if self.following.contains(&pane) {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.window = 0;
                }
                self.mark_dirty(pane);
                // The title belonged to the window that just went away.
                self.send_title(pane, 0);
            } else {
                self.remove_pane(pane);
                let _ = self.events.send(Event::Release {
                    pane,
                    reason: "window closed".into(),
                });
            }
        }
        // With no pane showing the window any more, the surfaces that were
        // told they entered the output are told they left it.  The window
        // entry is what finds those surfaces, so this runs before it goes.
        self.leave_output(id);
        self.windows.remove(&id);
        self.deciding.remove(&id);
        if self.newest == Some(id) {
            self.newest = self
                .windows
                .iter()
                .filter(|(_, entry)| entry.announced)
                .map(|(id, _)| *id)
                .max();
            // A pane that follows the newest window follows the one that is
            // left, rather than sitting on an empty pane.
            if let Some(newest) = self.newest {
                self.follow_panes(newest);
            }
        }
        self.popups.cleanup();
        if self.focused == Some(id) {
            if let Some(next) = self.newest {
                self.focus_window(next);
            } else {
                self.focused = None;
                let _ = self.events.send(Event::Focus(0));
            }
        }
        let _ = self.events.send(Event::WindowDown(id));
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        let announced = match self.windows.get_mut(&id) {
            Some(window) => {
                window.fullscreen = true;
                window.announced
            }
            None => return,
        };
        // Fullscreen means "be the window on this screen": a pane that follows
        // the newest window switches to it, but only once the window has
        // pixels, or it would capture a pane it can only draw black into.
        if announced {
            self.follow_panes(id);
        }
        self.enter_output(id);
        self.focus_window(id);
        apply_window_state(self, id);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&id) {
            window.fullscreen = false;
        }
        apply_window_state(self, id);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.ids.get(surface.wl_surface()).copied() {
            apply_window_state(self, id);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        let (app_id, title) = metadata(&surface);
        if let Some(window) = self.windows.get_mut(&id) {
            window.title = title;
            window.app_id = app_id;
        }
        let info = self.window_info(id);
        let _ = self.events.send(Event::WindowUp(info));
        self.send_titles(id);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }
}

fn metadata(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .map_or_default(|data| {
                let data = data.lock().unwrap();
                (
                    data.app_id.clone().unwrap_or_default(),
                    data.title.clone().unwrap_or_default(),
                )
            })
    })
}

/// Where a popup may be placed: the pane showing its window, in the
/// coordinates popup geometry is expressed in (the parent's window geometry).
fn popup_target(state: &State, popup: &PopupSurface) -> Rectangle<i32, Logical> {
    let geometry = popup
        .get_parent_surface()
        .and_then(|parent| {
            with_states(&parent, |states| {
                states
                    .cached_state
                    .get::<SurfaceCachedState>()
                    .current()
                    .geometry
            })
        })
        .unwrap_or_default();
    let size = state
        .window_for_surface(popup.wl_surface())
        .map_or(state.mode, |window| state.pane_size(window));
    Rectangle::new(
        (
            geometry.loc.x.saturating_neg(),
            geometry.loc.y.saturating_neg(),
        )
            .into(),
        size,
    )
}

/// Give a popup the geometry its positioner asks for, against the pane showing
/// its window.  Creating and re-positioning a popup both go through here, so
/// the two paths cannot place the same popup differently.
fn place_popup(state: &State, surface: &PopupSurface, positioner: PositionerState) {
    let target = popup_target(state, surface);
    surface.with_pending_state(|pending| {
        pending.geometry = positioner.get_unconstrained_geometry(target);
        pending.positioner = positioner;
    });
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seats
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(Resource::client);
        set_data_device_focus(&self.display, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let shape = match image {
            CursorImageStatus::Named(icon) => Some(cursor_name(icon).to_owned()),
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => None,
        };
        let pane = self.cursor_pane;
        if shape == self.cursor && pane == self.cursor_shape_pane {
            return;
        }
        self.cursor.clone_from(&shape);
        self.cursor_shape_pane = pane;
        let Some(pane) = pane else {
            return;
        };
        let _ = self.events.send(Event::Cursor { pane, shape });
    }
}

/// CSS cursor names, which is what the kitty pointer-shape protocol takes.
const fn cursor_name(icon: CursorIcon) -> &'static str {
    match icon {
        CursorIcon::ContextMenu => "context-menu",
        CursorIcon::Help => "help",
        CursorIcon::Pointer => "pointer",
        CursorIcon::Progress => "progress",
        CursorIcon::Wait => "wait",
        CursorIcon::Cell => "cell",
        CursorIcon::Crosshair => "crosshair",
        CursorIcon::Text => "text",
        CursorIcon::VerticalText => "vertical-text",
        CursorIcon::Alias => "alias",
        CursorIcon::Copy => "copy",
        CursorIcon::Move => "move",
        CursorIcon::NoDrop => "no-drop",
        CursorIcon::NotAllowed => "not-allowed",
        CursorIcon::Grab => "grab",
        CursorIcon::Grabbing => "grabbing",
        CursorIcon::EResize => "e-resize",
        CursorIcon::NResize => "n-resize",
        CursorIcon::NeResize => "ne-resize",
        CursorIcon::NwResize => "nw-resize",
        CursorIcon::SResize => "s-resize",
        CursorIcon::SeResize => "se-resize",
        CursorIcon::SwResize => "sw-resize",
        CursorIcon::WResize => "w-resize",
        CursorIcon::EwResize => "ew-resize",
        CursorIcon::NsResize => "ns-resize",
        CursorIcon::NeswResize => "nesw-resize",
        CursorIcon::NwseResize => "nwse-resize",
        CursorIcon::ColResize => "col-resize",
        CursorIcon::RowResize => "row-resize",
        CursorIcon::AllScroll => "all-scroll",
        CursorIcon::ZoomIn => "zoom-in",
        CursorIcon::ZoomOut => "zoom-out",
        CursorIcon::DndAsk => "dnd-ask",
        _ => "default",
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        _target: SelectionTarget,
        _source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device
    }
}

impl ClientDndGrabHandler for State {}
impl ServerDndGrabHandler for State {}
impl TabletSeatHandler for State {}
impl OutputHandler for State {}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_viewporter!(State);
delegate_seat!(State);
delegate_output!(State);
delegate_data_device!(State);
delegate_cursor_shape!(State);

#[expect(
    clippy::needless_pass_by_value,
    reason = "the compositor thread's channels are handed over once, when the thread starts"
)]
fn run(
    name: String,
    rx: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    ready: mpsc::Sender<std::io::Result<()>>,
) {
    let Ok(mut display) = Display::<State>::new() else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not create Wayland display",
        )));
        return;
    };
    let display_handle = display.handle();
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
    let pointer = Some(seat.add_pointer());
    let mode = Mode {
        size: (1920, 1080).into(),
        refresh: 60_000,
    };
    let output_manager = OutputManagerState::new_with_xdg_output::<State>(&display_handle);
    let output = Output::new(
        "meowland".into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "meowland".into(),
            model: "terminal".into(),
        },
    );
    output.create_global::<State>(&display_handle);
    output.change_current_state(
        Some(mode),
        None,
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(mode);
    let mut state = State {
        display: display_handle.clone(),
        compositor: CompositorState::new::<State>(&display_handle),
        shm: ShmState::new::<State>(&display_handle, vec![]),
        xdg: XdgShellState::new::<State>(&display_handle),
        _viewporter: ViewporterState::new::<State>(&display_handle),
        _output_manager: output_manager,
        data_device: DataDeviceState::new::<State>(&display_handle),
        _cursor_shape: CursorShapeManagerState::new::<State>(&display_handle),
        popups: PopupManager::default(),
        seats: seat_state,
        seat,
        keyboard: None,
        pointer,
        output,
        windows: HashMap::new(),
        ids: HashMap::new(),
        snapshots: HashMap::new(),
        next_id: 1,
        newest: None,
        panes: HashMap::new(),
        following: HashSet::new(),
        pending: HashSet::new(),
        deciding: HashMap::new(),
        focused: None,
        cursor_pane: None,
        cursor: None,
        cursor_shape_pane: None,
        scratch: Vec::new(),
        events,
        mode: Size::from((mode.size.w, mode.size.h)),
        started: Instant::now(),
        shutdown: false,
    };
    let Ok(keyboard) = state.seat.add_keyboard(XkbConfig::default(), 25, 600) else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not compile the keyboard keymap",
        )));
        return;
    };
    state.keyboard = Some(keyboard);
    let Ok(listener) = ListeningSocket::bind(name) else {
        let _ = ready.send(Err(std::io::Error::other("could not bind Wayland socket")));
        return;
    };
    let _ = ready.send(Ok(()));
    while !state.shutdown {
        // Waiting on the channel rather than sleeping means a command is
        // handled the moment it arrives, while the frame work below still runs
        // at its own pace.
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(command) => handle_command(&mut state, command),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // Every sender is gone: the server that owns this thread has died.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(command) = rx.try_recv() {
            handle_command(&mut state, command);
        }
        // The backend owns accepted connections; the returned handle carries
        // nothing the compositor needs, so it is not kept.
        if let Ok(Some(stream)) = listener.accept() {
            let _ = state
                .display
                .insert_client(stream, Arc::new(ClientState::default()));
        }
        let _ = display.dispatch_clients(&mut state);
        let _ = display.flush_clients();
        dispatch_frames(&mut state);
        idle_callbacks(&mut state);
    }
}

fn handle_command(state: &mut State, command: Command) {
    match command {
        Command::Attach {
            pane,
            show,
            width,
            height,
        } => attach(state, pane, show, width, height),
        Command::Detach { pane } => state.release_pane_window(pane),
        Command::Resize {
            pane,
            width,
            height,
        } => {
            let Some(entry) = state.panes.get_mut(&pane) else {
                return;
            };
            entry.width = width;
            entry.height = height;
            let window = entry.window;
            state.mark_dirty(pane);
            if state.panes.keys().min() == Some(&pane) {
                configure_output(state, width, height);
            }
            apply_window_state(state, window);
        }
        Command::Input { pane, event } => pane_input(state, pane, &event),
        Command::Ack { pane, drawn } => {
            let Some(entry) = state.panes.get_mut(&pane) else {
                return;
            };
            // An ack for a frame that was given up on says nothing about the
            // one in flight now, so it only takes itself out of the count.
            let given_up = entry.stale_acks > 0;
            if given_up {
                entry.stale_acks -= 1;
            } else if entry.frame != FrameState::InFlight {
                // An ack that answers nothing is a duplicate or a stray:
                // acting on it would take a frame that is in flight for
                // delivered.
                return;
            }
            entry.frame = FrameState::Ready;
            // Whatever the pane was behind on, it is reading one more frame
            // now, so the scene it missed is due: everything drawn since then
            // was held in `dirty` rather than rendered.
            entry.shown_stale = !drawn;
            let dirty = std::mem::take(&mut entry.dirty);
            let window = entry.window;
            if dirty || given_up {
                state.mark_dirty(pane);
            }
            if window != 0 {
                // The pane has taken the frame, so the client may draw again.
                send_frame_callbacks(state, window);
            }
        }
        Command::CloseAll => {
            for window in state.windows.values() {
                window.surface.send_close();
            }
        }
        Command::CloseShown { pane } => match state.panes.get(&pane).map(|state| state.window) {
            Some(window) if window != 0 => {
                if let Some(window) = state.windows.get(&window) {
                    window.surface.send_close();
                }
            }
            _ => {
                let _ = state.events.send(Event::Release {
                    pane,
                    reason: "empty pane".into(),
                });
            }
        },
        Command::Shutdown => {
            for window in state.windows.values() {
                window.surface.send_close();
            }
            state.shutdown = true;
        }
    }
}

/// Attach a pane to the window its selection resolves to.  A selection that
/// names a window which is not there is released with a reason, and one that
/// follows the newest window waits for the next window with pixels instead.
fn attach(state: &mut State, pane: u64, show: Show, width: u32, height: u32) {
    if matches!(show, Show::Newest) {
        state.following.insert(pane);
    }
    let id = match show {
        Show::Id(id) => id,
        Show::Newest => state.newest.unwrap_or(0),
        Show::Focused => state.focused.unwrap_or(0),
    };
    let announced = state
        .windows
        .get(&id)
        .is_some_and(|window| window.announced);
    if announced {
        state.insert_pane(pane, PaneState::new(id, width, height));
        state.enter_output(id);
        state.send_title(pane, id);
        state.mark_dirty(pane);
        apply_window_state(state, id);
        state.focus_window(id);
    } else if matches!(show, Show::Newest | Show::Focused) {
        // Nothing to show yet: wait for the next window with pixels.
        state.insert_pane(pane, PaneState::new(0, width, height));
        state.following.insert(pane);
    } else {
        let _ = state.events.send(Event::Release {
            pane,
            reason: "no such window".into(),
        });
    }
}

/// Hand a pane the newest frame, at most once per [`FRAME_INTERVAL`].
fn dispatch_frames(state: &mut State) {
    expire_frames(state);
    if state.pending.is_empty() {
        return;
    }
    let now = Instant::now();
    let due: Vec<u64> = state
        .pending
        .iter()
        .copied()
        .filter(|pane| {
            // A pane that is behind stays in `pending` until it acknowledges
            // one: the frame it wanted is not worth rendering yet.
            state
                .panes
                .get(pane)
                .is_some_and(|entry| entry.frame != FrameState::Behind && now >= entry.next_frame)
        })
        .collect();
    for pane in due {
        state.pending.remove(&pane);
        let Some((window, in_flight)) = state
            .panes
            .get(&pane)
            .map(|entry| (entry.window, entry.frame == FrameState::InFlight))
        else {
            continue;
        };
        if in_flight {
            if let Some(entry) = state.panes.get_mut(&pane) {
                entry.dirty = true;
            }
            continue;
        }
        if !render_frame(state, pane) {
            continue;
        }
        let (width, height, rows) = match state.panes.get_mut(&pane) {
            Some(entry) => {
                entry.next_frame = now + FRAME_INTERVAL;
                let stride = entry.width as usize * 3;
                // The pane is only sent the rows that changed; one that was
                // given a frame it did not draw is sent the whole of it again,
                // since what its terminal shows is not what `shown` holds.
                let rows = if entry.shown_stale {
                    Some(0..entry.height as usize)
                } else {
                    changed_rows(&entry.shown, &state.scratch, stride)
                };
                (entry.width, entry.height, rows)
            }
            None => continue,
        };
        match rows {
            Some(rows) => {
                let stride = width as usize * 3;
                let band = state.scratch[rows.start * stride..rows.end * stride].to_vec();
                if let Some(entry) = state.panes.get_mut(&pane) {
                    // Only the rows that go out have to be remembered.
                    entry.shown.resize(state.scratch.len(), 0);
                    entry.shown[rows.start * stride..rows.end * stride].copy_from_slice(&band);
                    entry.frame = FrameState::InFlight;
                    entry.sent = now;
                }
                let _ = state.events.send(Event::Frame {
                    pane,
                    width,
                    height,
                    y: u32::try_from(rows.start).unwrap_or(0),
                    rgb: band,
                });
            }
            None => {
                // The pane already holds these pixels; the client is free to
                // draw again without paying for a frame nobody would see.
                send_frame_callbacks(state, window);
            }
        }
    }
}

/// The rows of `buffer` that differ from `shown`, or `None` when the two are
/// the same picture.  `shown` of another size is a frame that has to be sent
/// whole.
fn changed_rows(shown: &[u8], buffer: &[u8], stride: usize) -> Option<Range<usize>> {
    let rows = buffer.len() / stride;
    if shown.len() != buffer.len() {
        return Some(0..rows);
    }
    let differs = |row: usize| {
        shown[row * stride..(row + 1) * stride] != buffer[row * stride..(row + 1) * stride]
    };
    let first = (0..rows).find(|row| differs(*row))?;
    // Searching from the end keeps a change near the top from scanning the
    // whole frame twice.
    let last = (first..rows)
        .rev()
        .find(|row| differs(*row))
        .unwrap_or(first);
    Some(first..last + 1)
}

/// Give up on frames no pane has acknowledged in [`ACK_TIMEOUT`].
///
/// A pane whose terminal has stopped reading sits in a write that never
/// finishes, and the client that is waiting for the frame callback behind it
/// would sit there with it: a frozen terminal would freeze the window too.
/// The frame is dropped, the client draws again, and the pane takes whichever
/// frame reaches it first.
fn expire_frames(state: &mut State) {
    let now = Instant::now();
    let expired: Vec<u64> = state
        .panes
        .iter()
        .filter(|(_, entry)| {
            entry.frame == FrameState::InFlight && now.duration_since(entry.sent) >= ACK_TIMEOUT
        })
        .map(|(pane, _)| *pane)
        .collect();
    for pane in expired {
        let Some(entry) = state.panes.get_mut(&pane) else {
            continue;
        };
        entry.stale_acks += 1;
        // The pane is behind: whatever it is shown next waits for its ack.
        // Rendering frame after frame for a terminal that cannot take them
        // only adds work to the slowest part of the pipeline, and the scene
        // that matters is the one rendered when the pane comes back.
        entry.frame = FrameState::Behind;
        entry.shown_stale = true;
        entry.dirty = true;
        let window = entry.window;
        if window != 0 {
            send_frame_callbacks(state, window);
        }
    }
}

/// Frame callbacks for windows no pane is showing: they still have to draw,
/// or there would be nothing to attach to.
fn idle_callbacks(state: &mut State) {
    let now = Instant::now();
    let due: Vec<u64> = state
        .windows
        .iter()
        .filter(|(id, entry)| {
            // A window a pane shows is paced by that pane's acknowledgements
            // once it has pixels; everything else draws on this clock.
            let paced = entry.announced && state.shown(**id);
            !paced && now >= entry.callback_due
        })
        .map(|(id, _)| *id)
        .collect();
    for window in due {
        // The check itself is the cost here, so a window that had nothing to
        // send waits for the idle clock rather than being looked at on every
        // pass of the loop.
        send_frame_callbacks(state, window);
        if let Some(entry) = state.windows.get_mut(&window) {
            entry.callback_due = now + IDLE_INTERVAL;
        }
    }
}

/// Send the frame callbacks the surfaces of a window are waiting for.  Returns
/// whether any client was told it may draw.
fn send_frame_callbacks(state: &State, window: u64) -> bool {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface().clone())
    else {
        return false;
    };
    let time = state.started.elapsed().as_millis() as u32;
    let mut sent = false;
    for (surface, _) in surface_stack(&root) {
        with_states(&surface, |states| {
            for callback in states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .frame_callbacks
                .drain(..)
            {
                callback.done(time);
                sent = true;
            }
        });
    }
    sent
}

fn pane_input(state: &mut State, pane: u64, event: &Input) {
    let Some(window) = state.panes.get(&pane).map(|pane| pane.window) else {
        return;
    };
    if window == 0 {
        return;
    }
    state.cursor_pane = Some(pane);
    if state.deciding.insert(window, pane) != Some(pane) {
        // The window's size follows the pane the user is actually using.
        apply_window_state(state, window);
    }
    match *event {
        Input::Key {
            code,
            pressed,
            modifiers,
        } => {
            state.focus_window(window);
            if let Some(keyboard) = state.keyboard.take() {
                inject_key(&keyboard, state, code, pressed, modifiers);
                state.keyboard = Some(keyboard);
            }
        }
        Input::Pointer {
            x,
            y,
            button,
            pressed,
            scroll,
        } => {
            if pressed && button.is_some() && !point_in_popups(state, window, x, y) {
                dismiss_popups(state, window);
            }
            let hit = hit_test(state, window, x, y);
            let location = Point::from((x, y));
            if let Some(pointer) = state.pointer.take() {
                pointer.motion(
                    state,
                    hit,
                    &MotionEvent {
                        location,
                        serial: SERIAL_COUNTER.next_serial(),
                        time: 0,
                    },
                );
                if let Some(button) = button {
                    pointer.button(
                        state,
                        &ButtonEvent {
                            button: match button {
                                0 => 0x110,
                                1 => 0x111,
                                2 => 0x112,
                                _ => 0x113,
                            },
                            state: if pressed {
                                ButtonState::Pressed
                            } else {
                                ButtonState::Released
                            },
                            serial: SERIAL_COUNTER.next_serial(),
                            time: 0,
                        },
                    );
                }
                if scroll != 0 {
                    pointer.axis(
                        state,
                        AxisFrame::new(0)
                            .source(AxisSource::Wheel)
                            .value(Axis::Vertical, -f64::from(scroll))
                            .v120(Axis::Vertical, -i32::from(scroll.signum()) * 120),
                    );
                }
                pointer.frame(state);
                state.pointer = Some(pointer);
            }
        }
    }
}

/// A press outside every open popup dismisses them, which is what a popup
/// grab asks for.
fn point_in_popups(state: &State, window: u64, x: f64, y: f64) -> bool {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface().clone())
    else {
        return false;
    };
    let geometry = window_geometry(&root);
    PopupManager::popups_for_surface(&root).any(|(popup, location)| {
        let surface = popup.wl_surface();
        let Some(snapshot) = state.snapshots.get(surface) else {
            return false;
        };
        let (_, dst) = extents(snapshot, viewport_of(surface));
        let origin = popup_origin(geometry, location, &popup);
        let left = f64::from(origin.x);
        let top = f64::from(origin.y);
        x >= left && y >= top && x < left + f64::from(dst.w) && y < top + f64::from(dst.h)
    })
}

fn dismiss_popups(state: &mut State, window: u64) {
    let Some(root) = state
        .windows
        .get(&window)
        .map(|entry| entry.surface.wl_surface().clone())
    else {
        return;
    };
    let popups: Vec<PopupKind> = PopupManager::popups_for_surface(&root)
        .map(|(popup, _)| popup)
        .collect();
    if popups.is_empty() {
        return;
    }
    for popup in popups.into_iter().rev() {
        if let PopupKind::Xdg(popup) = &popup {
            popup.send_popup_done();
        }
        let _ = PopupManager::dismiss_popup(&root, &popup);
    }
    state.touch(window);
}

fn inject_key(
    keyboard: &KeyboardHandle<State>,
    state: &mut State,
    code: u16,
    pressed: bool,
    modifiers: u8,
) {
    // The modifier keys are pressed before the stroke and released after it,
    // in the reverse order, so the shift a symbol needs is held while it is
    // sent.
    let emit = |code: u16, pressed: bool, state: &mut State| {
        keyboard.input(
            state,
            Keycode::from(u32::from(code) + 8),
            if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            },
            SERIAL_COUNTER.next_serial(),
            0,
            |_, _, _| FilterResult::<()>::Forward,
        );
    };
    if pressed {
        for (bit, key) in MODIFIER_KEYS {
            if modifiers & bit != 0 {
                emit(key, true, state);
            }
        }
    }
    emit(code, pressed, state);
    if !pressed {
        for (bit, key) in MODIFIER_KEYS.into_iter().rev() {
            if modifiers & bit != 0 {
                emit(key, false, state);
            }
        }
    }
}

/// Configure a window at the size of the pane that decides it, maximized, and
/// activated or fullscreen when it is.
fn apply_window_state(state: &State, window: u64) {
    let Some(entry) = state.windows.get(&window) else {
        return;
    };
    let surface = entry.surface.clone();
    let fullscreen = entry.fullscreen;
    let activated = state.focused == Some(window);
    let size = state.pane_size(window);
    surface.with_pending_state(|pending| {
        pending.size = Some(size);
        pending.states.set(xdg_toplevel::State::Maximized);
        if activated {
            pending.states.set(xdg_toplevel::State::Activated);
        } else {
            pending.states.unset(xdg_toplevel::State::Activated);
        }
        if fullscreen {
            pending.states.set(xdg_toplevel::State::Fullscreen);
        } else {
            pending.states.unset(xdg_toplevel::State::Fullscreen);
        }
    });
    surface.send_configure();
}

/// The output is the first pane: its size is the mode every client sees.
fn configure_output(state: &mut State, width: u32, height: u32) {
    let mode = Mode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    };
    state.mode = Size::from((width as i32, height as i32));
    state
        .output
        .change_current_state(Some(mode), None, None, None);
    state.output.set_preferred(mode);
}

/// Copy a committed shm buffer into an owned snapshot, in `r, g, b, a` order.
///
/// `pixels` is the storage of the snapshot this one replaces, or empty; it is
/// resized and overwritten whole, and left with the caller (emptied) when the
/// buffer cannot be used.
#[expect(
    unsafe_code,
    reason = "the shm pool is only reachable as a raw pointer, so reading it takes one \
              bounded, documented slice"
)]
fn copy_buffer(
    ptr: *const u8,
    len: usize,
    data: &smithay::wayland::shm::BufferData,
    bound: Option<(u32, u32)>,
    pixels: &mut Vec<u8>,
) -> Option<Snapshot> {
    if !matches!(
        data.format,
        wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
    ) || data.width <= 0
        || data.height <= 0
        || data.stride <= 0
    {
        return None;
    }
    let width = data.width as u32;
    let height = data.height as u32;
    let stride = data.stride as usize;
    let offset = data.offset as usize;
    if width > MAX_SURFACE_SIDE
        || height > MAX_SURFACE_SIDE
        || width as usize * height as usize > MAX_SURFACE_PIXELS
        || stride < width as usize * 4
    {
        return None;
    }
    if let Some((max_width, max_height)) = bound
        && (width > max_width || height > max_height)
    {
        return None;
    }
    if offset.checked_add(stride.checked_mul(height as usize)?)? > len {
        return None;
    }
    // SAFETY: `with_buffer_contents` passes the pool's mapping base and its
    // length, and the checks above establish `offset + stride * height <= len`,
    // so every index read below is inside the mapping.  The pool is shared with
    // the client and could be written at any time, so the slice is never held:
    // the pixels are copied out here and the reference ends with this function.
    let source = unsafe { std::slice::from_raw_parts(ptr, len) };
    let row_bytes = width as usize * 4;
    let needed = row_bytes * height as usize;
    pixels.resize(needed, 0);
    // A surface that once drew large must not pin that memory for as long as
    // it lives: reuse is worth an allocation, not a page's worth of them.
    if pixels.capacity() >= needed.saturating_mul(4).max(1 << 20) {
        pixels.shrink_to_fit();
    }
    let xrgb = data.format == wl_shm::Format::Xrgb8888;
    let mut opaque = true;
    for y in 0..height as usize {
        let from = offset + y * stride;
        let source_row = source[from..from + row_bytes].as_chunks::<4>().0;
        let to = y * row_bytes;
        let row = pixels[to..to + row_bytes].as_chunks_mut::<4>().0;
        for (pixel, out_pixel) in source_row.iter().zip(row) {
            // The pool holds `b, g, r, a` in memory order, which is the word
            // `a << 24 | r << 16 | g << 8 | b`; the snapshot keeps
            // `r, g, b, a`, the same word with its outer two bytes swapped.
            let word = u32::from_le_bytes(*pixel);
            let alpha = if xrgb { 255 } else { word >> 24 };
            *out_pixel = ((word & 0xFF00_FF00)
                | ((word & 0x00FF_0000) >> 16)
                | ((word & 0x0000_00FF) << 16)
                | (alpha << 24))
                .to_le_bytes();
            opaque &= alpha == 255;
        }
    }
    Some(Snapshot {
        width,
        height,
        pixels: std::mem::take(pixels),
        opaque,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band a pane is sent is the rows that differ from the frame it was
    /// sent before, so a change near one edge does not cost a whole frame.
    #[test]
    fn changed_rows_are_the_rows_that_differ() {
        const STRIDE: usize = 9;
        let shown = vec![0u8; 4 * STRIDE];
        let mut buffer = shown.clone();
        assert_eq!(changed_rows(&shown, &buffer, STRIDE), None, "identical");

        buffer[0..3].copy_from_slice(&[1, 1, 1]);
        assert_eq!(changed_rows(&shown, &buffer, STRIDE), Some(0..1), "row 0");

        buffer[0..3].copy_from_slice(&[0, 0, 0]);
        buffer[3 * STRIDE..3 * STRIDE + 3].copy_from_slice(&[1, 1, 1]);
        assert_eq!(changed_rows(&shown, &buffer, STRIDE), Some(3..4), "row 3");

        buffer[0..3].copy_from_slice(&[1, 1, 1]);
        assert_eq!(
            changed_rows(&shown, &buffer, STRIDE),
            Some(0..4),
            "rows 0 and 3 span the frame"
        );

        // A frame of another size cannot be compared row for row.
        assert_eq!(
            changed_rows(&shown, &buffer[..2 * STRIDE], STRIDE),
            Some(0..2)
        );
    }

    /// `Argb8888` holds `b, g, r, a` in memory and the snapshot holds
    /// `r, g, b, a`; `Xrgb8888` ignores the fourth byte and is always opaque.
    /// The stride is wider than the row, so padding must be neither read into
    /// the snapshot nor written from the previous one.
    #[test]
    fn snapshot_swizzles_both_shm_formats() {
        const STRIDE: usize = 12;
        let raw: [u8; 24] = [
            3, 4, 5, 0x40, 6, 7, 8, 0xff, 0xaa, 0xaa, 0xaa, 0xaa, //
            9, 10, 11, 0xff, 12, 13, 14, 0x7f, 0xbb, 0xbb, 0xbb, 0xbb,
        ];
        let data = |format| smithay::wayland::shm::BufferData {
            format,
            width: 2,
            height: 2,
            stride: STRIDE as i32,
            offset: 0,
        };
        let copy = |format, reuse: Vec<u8>| {
            let mut reuse = reuse;
            copy_buffer(raw.as_ptr(), raw.len(), &data(format), None, &mut reuse)
                .expect("a 2x2 buffer")
        };

        let argb = copy(wl_shm::Format::Argb8888, Vec::new());
        assert_eq!(
            argb.pixels,
            [
                5, 4, 3, 0x40, 8, 7, 6, 0xff, 11, 10, 9, 0xff, 14, 13, 12, 0x7f
            ]
        );
        assert!(!argb.opaque, "an alpha byte below 255 is not opaque");

        let xrgb = copy(wl_shm::Format::Xrgb8888, Vec::new());
        assert_eq!(
            xrgb.pixels,
            [
                5, 4, 3, 0xff, 8, 7, 6, 0xff, 11, 10, 9, 0xff, 14, 13, 12, 0xff
            ]
        );
        assert!(xrgb.opaque, "Xrgb8888 has no alpha byte");

        // A second copy into the first one's storage must overwrite it whole.
        let mut reused = argb.pixels.clone();
        reused.fill(0xcc);
        let again = copy(wl_shm::Format::Argb8888, reused);
        assert_eq!(again.pixels, argb.pixels);
    }
}
