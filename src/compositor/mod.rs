//! Wayland compositor and its command/event channels.
//!
//! Client buffers are copied on commit so later client writes cannot tear a
//! frame in transit to a terminal.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use calloop::{EventLoop, Interest, Mode as PollMode, PostAction, channel, generic::Generic};
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_dispatch2,
    desktop::{PopupKind, PopupManager, get_popup_toplevel_coords},
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorIcon, CursorImageStatus, PointerHandle},
        tablet::TabletSeatHandler,
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        Client, Display, DisplayHandle, ListeningSocket, Resource, Weak,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            Barrier, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SubsurfaceCachedState, SurfaceAttributes, add_blocker, add_pre_commit_hook,
            get_children, get_parent, with_states,
        },
        cursor_shape::CursorShapeManagerState,
        dmabuf::{
            DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier,
            get_dmabuf,
        },
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::PointerConstraintsHandler,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            wlr_data_control::{DataControlHandler, DataControlState},
        },
        shell::{
            kde::decoration::KdeDecorationState,
            xdg::{
                PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface,
                XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
                decoration::XdgDecorationState,
            },
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        viewporter::{ViewportCachedState, ViewporterState},
        xdg_activation::XdgActivationState,
    },
};

pub use crate::pixels::{MAX_SURFACE_PIXELS, MAX_SURFACE_SIDE};
use crate::protocol::{Input, Show, WindowInfo};

mod activation;
mod clipboard;
mod commands;
mod decoration;
mod dmabuf;
mod frame;
mod input;
mod render;
mod snapshot;

use dmabuf::DmabufBackend;
use frame::PaneState;
use render::{Renderer, surface_stack};
use snapshot::Snapshot;

const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);
/// Transfers and implicit DMA-BUF fences still need periodic progress checks.
const TRANSFER_POLL_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Debug)]
pub enum Command {
    CreateActivationToken {
        reply: mpsc::Sender<String>,
    },
    CancelActivation {
        token: String,
    },
    Kill {
        show: Show,
        reply: mpsc::Sender<Result<(), String>>,
    },
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
        drawn: bool,
    },
    Paste {
        pane: u64,
        text: String,
    },
    ClipboardOffer {
        pane: u64,
        offer: u64,
        mimes: Vec<String>,
        paste: bool,
    },
    ClipboardReply {
        pane: u64,
        request: u64,
        data: Option<Vec<u8>>,
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
    ClipboardWrite {
        pane: u64,
        data: crate::clipboard::ClipboardData,
    },
    ClipboardRead {
        pane: u64,
        request: u64,
        offer: u64,
        mime: String,
    },
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

pub type Handle = (
    channel::Sender<Command>,
    mpsc::Receiver<Event>,
    String,
    thread::JoinHandle<()>,
);

/// Start the compositor thread and wait for its Wayland socket to be ready.
///
/// # Errors
/// Returns an error if the thread, display, keymap or socket cannot start, or
/// if readiness is not reported within two seconds.
pub fn spawn() -> crate::Result<Handle> {
    let (commands, rx) = channel::channel();
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

struct Window {
    surface: ToplevelSurface,
    title: String,
    app_id: String,
    /// A window without pixels must not capture a follow-the-newest pane.
    announced: bool,
    fullscreen: bool,
    callback_due: Instant,
}

type ViewportGeometry = (Option<Rectangle<f64, Logical>>, Option<Size<i32, Logical>>);

struct PendingImport {
    surface: Weak<WlSurface>,
    buffer: Dmabuf,
    barrier: Barrier,
}

struct State {
    display: DisplayHandle,
    compositor: CompositorState,
    shm: ShmState,
    dmabuf: DmabufState,
    gpu: Option<DmabufBackend>,
    pending_imports: Vec<PendingImport>,
    xdg: XdgShellState,
    activation: XdgActivationState,
    launches: HashMap<String, activation::Launch>,
    _decoration: XdgDecorationState,
    kde_decoration: KdeDecorationState,
    _viewporter: ViewporterState,
    _output_manager: OutputManagerState,
    data_device: DataDeviceState,
    data_control: DataControlState,
    clipboard: clipboard::Bridge,
    _cursor_shape: CursorShapeManagerState,
    popups: PopupManager,
    seats: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: Option<KeyboardHandle<Self>>,
    /// One terminal pane owns the seat's held keyboard state at a time.
    keyboard_pane: Option<u64>,
    keyboard_keys: HashSet<u16>,
    keyboard_known: bool,
    pointer: Option<PointerHandle<Self>>,
    output: Output,
    windows: HashMap<u64, Window>,
    ids: HashMap<WlSurface, u64>,
    snapshots: HashMap<WlSurface, Snapshot>,
    /// Cached to distinguish viewport changes from frame-only commits.
    viewport_geometry: HashMap<WlSurface, ViewportGeometry>,
    next_id: u64,
    newest: Option<u64>,
    panes: HashMap<u64, PaneState>,
    following: HashSet<u64>,
    /// Explicit selections waiting for an app ID or activation and pixels.
    pending_shows: HashMap<u64, Show>,
    /// For each window, the pane that last interacted with it.  That pane
    /// decides the window's configured size while it is attached.
    deciding: HashMap<u64, u64>,
    focused: Option<u64>,
    /// Most recent input pane, used for cursor shape events.
    cursor_pane: Option<u64>,
    cursor: Option<String>,
    /// The pane `cursor` was last sent to, so a shape that is unchanged but
    /// belongs to another pane is still sent.
    cursor_shape_pane: Option<u64>,
    renderer: Renderer,
    events: mpsc::Sender<Event>,
    /// Output mode, rewritten as panes attach and leave.
    mode: Size<i32, Logical>,
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

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.gpu.as_mut().is_some_and(|gpu| gpu.validate(&dmabuf)) {
            let _ = notifier.successful::<Self>();
        } else {
            notifier.failed();
        }
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

    fn new_surface(&mut self, surface: &WlSurface) {
        add_pre_commit_hook::<Self, _>(surface, |state, _, surface| {
            let buffer = with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                match &attributes.pending().buffer {
                    Some(BufferAssignment::NewBuffer(buffer)) => get_dmabuf(buffer).ok().cloned(),
                    _ => None,
                }
            });
            if let Some(buffer) = buffer
                && !dmabuf_ready(&buffer)
            {
                let barrier = Barrier::new(false);
                add_blocker(surface, barrier.clone());
                state.pending_imports.push(PendingImport {
                    surface: surface.downgrade(),
                    buffer,
                    barrier,
                });
            }
        });
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "`cached` borrows from the guard and must outlive `current`, so the two \
                  statements cannot be merged"
    )]
    fn commit(&mut self, surface: &WlSurface) {
        self.popups.commit(surface);
        let (assignment, damaged, viewport) = with_states(surface, |states| {
            let (assignment, damaged) = {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let current = cached.current();
                let damaged = !current.damage.is_empty();
                current.damage.clear();
                (current.buffer.take(), damaged)
            };
            let mut viewport = states.cached_state.get::<ViewportCachedState>();
            let current = viewport.current();
            (assignment, damaged, (current.src, current.dst))
        });
        let viewport_changed = if viewport.0.is_some() || viewport.1.is_some() {
            self.viewport_geometry.insert(surface.clone(), viewport) != Some(viewport)
        } else {
            self.viewport_geometry.remove(surface).is_some()
        };
        let visual_change = assignment.is_some()
            || damaged
            || viewport_changed
            || get_parent(surface).is_some()
            || !get_children(surface).is_empty()
            || self.popups.find_popup(surface).is_some()
            || PopupManager::popups_for_surface(surface).next().is_some();
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                let bound = self.pane_bound(surface);
                let mut previous = self.snapshots.remove(surface);
                let mut reuse = previous
                    .as_mut()
                    .map_or_else(Vec::new, |old| std::mem::take(&mut old.pixels));
                let mut release_safe = true;
                let snapshot = if let Ok(dmabuf) = get_dmabuf(&buffer) {
                    self.gpu.as_mut().and_then(|gpu| {
                        match gpu.snapshot(dmabuf, bound, &mut reuse) {
                            Ok(snapshot) => Some(snapshot),
                            Err(error) => {
                                release_safe = error.release_safe();
                                tracing::warn!(%error, release_safe, "DMA-BUF readback failed");
                                None
                            }
                        }
                    })
                } else {
                    with_buffer_contents(&buffer, |ptr, len, data| {
                        Snapshot::copy_shm(ptr, len, &data, bound, &mut reuse)
                    })
                    .ok()
                    .flatten()
                };
                if release_safe {
                    buffer.release();
                }
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
        if visual_change && let Some(window) = self.window_for_surface(surface) {
            self.touch(window);
        }
    }

    /// A destroyed surface may not belong to any toplevel, so remove its
    /// snapshot here rather than relying on toplevel cleanup.
    fn destroyed(&mut self, surface: &WlSurface) {
        self.snapshots.remove(surface);
        self.viewport_geometry.remove(surface);
        self.pending_imports
            .retain(|pending| pending.surface != surface.downgrade());
    }
}

impl State {
    fn new(display_handle: DisplayHandle, events: mpsc::Sender<Event>) -> Self {
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
        let pointer = Some(seat.add_pointer());
        let mode = Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        let output_manager = OutputManagerState::new_with_xdg_output::<Self>(&display_handle);
        let output = Output::new(
            "meowland".into(),
            PhysicalProperties {
                size: (300, 200).into(),
                subpixel: Subpixel::Unknown,
                make: "meowland".into(),
                model: "terminal".into(),
                serial_number: String::new(),
            },
        );
        output.create_global::<Self>(&display_handle);
        output.change_current_state(
            Some(mode),
            None,
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        let gpu = DmabufBackend::discover();
        let mut dmabuf = DmabufState::new();
        if let Some(gpu) = &gpu {
            match DmabufFeedbackBuilder::new(gpu.device(), gpu.formats().iter().copied()).build() {
                Ok(feedback) => {
                    dmabuf.create_global_with_default_feedback::<Self>(&display_handle, &feedback);
                }
                Err(error) => tracing::warn!(%error, "DMA-BUF feedback initialization failed"),
            }
        }
        Self {
            compositor: CompositorState::new::<Self>(&display_handle),
            shm: ShmState::new::<Self>(&display_handle, vec![]),
            dmabuf,
            gpu,
            pending_imports: Vec::new(),
            xdg: XdgShellState::new::<Self>(&display_handle),
            activation: XdgActivationState::new::<Self>(&display_handle),
            launches: HashMap::new(),
            _decoration: XdgDecorationState::new::<Self>(&display_handle),
            kde_decoration: decoration::kde(&display_handle),
            _viewporter: ViewporterState::new::<Self>(&display_handle),
            _output_manager: output_manager,
            data_device: DataDeviceState::new::<Self>(&display_handle),
            data_control: DataControlState::new::<Self, _>(&display_handle, None, |_| true),
            clipboard: clipboard::Bridge::default(),
            _cursor_shape: CursorShapeManagerState::new::<Self>(&display_handle),
            popups: PopupManager::default(),
            seats: seat_state,
            seat,
            keyboard: None,
            keyboard_pane: None,
            keyboard_keys: HashSet::new(),
            keyboard_known: false,
            pointer,
            output,
            windows: HashMap::new(),
            ids: HashMap::new(),
            snapshots: HashMap::new(),
            viewport_geometry: HashMap::new(),
            next_id: 1,
            newest: None,
            panes: HashMap::new(),
            following: HashSet::new(),
            pending_shows: HashMap::new(),
            deciding: HashMap::new(),
            focused: None,
            cursor_pane: None,
            cursor: None,
            cursor_shape_pane: None,
            renderer: Renderer::default(),
            events,
            mode: Size::from((mode.size.w, mode.size.h)),
            started: Instant::now(),
            shutdown: false,
            display: display_handle,
        }
    }

    /// Pane-relative buffer limit: twice the widest and tallest attached
    /// panes, independently, to allow an in-flight resize. Without an attached
    /// pane only the global surface limits apply.
    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)> {
        let window = self.window_for_surface(surface)?;
        self.panes
            .values()
            .filter(|pane| pane.window == window)
            .fold(None, |bound: Option<(u32, u32)>, pane| {
                let pane = (pane.size.width() * 2, pane.size.height() * 2);
                Some(bound.map_or(pane, |bound| (bound.0.max(pane.0), bound.1.max(pane.1))))
            })
    }

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
        self.resolve_pending_panes();
        let info = self.window_info(id);
        let _ = self.events.send(Event::WindowUp(info));
        self.apply_window_state(id);
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

    /// The last-used pane decides the window's size; otherwise use the first
    /// attached pane, then the output mode.
    fn pane_size(&self, window: u64) -> Size<i32, Logical> {
        let selected = self
            .deciding
            .get(&window)
            .and_then(|pane| self.panes.get(pane))
            .filter(|pane| pane.window == window)
            .or_else(|| {
                self.panes
                    .iter()
                    .filter(|(_, pane)| pane.window == window)
                    .min_by_key(|(id, _)| *id)
                    .map(|(_, pane)| pane)
            });
        selected.map_or(self.mode, |pane| {
            Size::from((pane.size.width() as i32, pane.size.height() as i32))
        })
    }

    fn mark_dirty(&mut self, pane: u64) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.mark_dirty();
        }
    }

    fn panes_showing(&self, window: u64) -> Vec<u64> {
        self.panes
            .iter()
            .filter(|(_, pane)| pane.window == window)
            .map(|(pane, _)| *pane)
            .collect()
    }

    /// The lowest-numbered pane sets the output mode seen by clients.
    fn insert_pane(&mut self, pane: u64, entry: PaneState) {
        let size = entry.size;
        self.panes.insert(pane, entry);
        if self.panes.keys().min() == Some(&pane) {
            self.configure_output(size);
        }
    }

    /// Remove every pane-keyed entry, restoring the remaining output mode.
    fn remove_pane(&mut self, pane: u64) -> Option<u64> {
        if self.keyboard_pane == Some(pane) {
            input::reset_keyboard(self);
        }
        let first = self.panes.keys().min() == Some(&pane);
        let window = self.panes.remove(&pane).map(|state| state.window);
        self.clipboard.detach(pane);
        self.following.remove(&pane);
        if let Some(Show::Activation(token)) = self.pending_shows.remove(&pane) {
            self.forget_activation(&token);
        }
        self.deciding.retain(|_, deciding| *deciding != pane);
        if self.cursor_pane == Some(pane) {
            self.cursor_pane = None;
        }
        if self.cursor_shape_pane == Some(pane) {
            self.cursor_shape_pane = None;
        }
        if first && let Some((_, next)) = self.panes.iter().min_by_key(|(pane, _)| **pane) {
            self.configure_output(next.size);
        }
        window
    }

    /// Move focus to another attached pane's window when this pane is released.
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

    fn follow_panes(&mut self, window: u64) {
        if self
            .keyboard_pane
            .is_some_and(|pane| self.following.contains(&pane))
        {
            input::reset_keyboard(self);
        }
        for &pane in &self.following {
            if let Some(state) = self.panes.get_mut(&pane) {
                state.window = window;
                state.mark_dirty();
            }
            self.send_title(pane, window);
        }
    }

    fn touch(&mut self, window: u64) {
        for entry in self.panes.values_mut() {
            if entry.window != window {
                continue;
            }
            entry.mark_dirty();
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
        for (&pane, entry) in &self.panes {
            if entry.window == window {
                self.send_title(pane, window);
            }
        }
    }

    fn enter_output(&self, window: u64) {
        let Some(root) = self
            .windows
            .get(&window)
            .filter(|entry| entry.announced)
            .map(|entry| entry.surface.wl_surface())
        else {
            return;
        };
        for (surface, _) in surface_stack(root) {
            self.output.enter(&surface);
        }
    }

    fn leave_output(&self, window: u64) {
        if self.panes.values().any(|pane| pane.window == window) {
            return;
        }
        let Some(root) = self
            .windows
            .get(&window)
            .map(|entry| entry.surface.wl_surface())
        else {
            return;
        };
        for (surface, _) in surface_stack(root) {
            self.output.leave(&surface);
        }
    }

    fn focus_window(&mut self, window: u64) {
        if self.focused == Some(window) {
            return;
        }
        input::reset_keyboard(self);
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
            self.apply_window_state(previous);
        }
        self.apply_window_state(window);
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
        self.apply_window_state(id);
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
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
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
        self.cancel_window_activations(id);
        if self
            .keyboard_pane
            .is_some_and(|pane| self.panes.get(&pane).is_some_and(|p| p.window == id))
        {
            input::reset_keyboard(self);
        }
        for pane in self.panes_showing(id) {
            if self.following.contains(&pane) {
                if let Some(state) = self.panes.get_mut(&pane) {
                    state.window = 0;
                }
                self.mark_dirty(pane);
                self.send_title(pane, 0);
            } else {
                self.remove_pane(pane);
                let _ = self.events.send(Event::Release {
                    pane,
                    reason: "window closed".into(),
                });
            }
        }
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
        if announced {
            self.follow_panes(id);
        }
        self.enter_output(id);
        self.focus_window(id);
        self.apply_window_state(id);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.ids.get(surface.wl_surface()).copied() else {
            return;
        };
        if let Some(window) = self.windows.get_mut(&id) {
            window.fullscreen = false;
        }
        self.apply_window_state(id);
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.ids.get(surface.wl_surface()).copied() {
            self.apply_window_state(id);
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
        self.resolve_pending_panes();
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
    let window = state.window_for_surface(popup.wl_surface());
    let geometry = window
        .and_then(|window| state.windows.get(&window))
        .map_or_else(
            || (0, 0).into(),
            |window| render::window_geometry(window.surface.wl_surface()),
        );
    let parent = get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
    let size = window.map_or(state.mode, |window| state.pane_size(window));
    Rectangle::new(
        (
            geometry.x.saturating_add(parent.x).saturating_neg(),
            geometry.y.saturating_add(parent.y).saturating_neg(),
        )
            .into(),
        size,
    )
}

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
    type SelectionUserData = clipboard::Source;

    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if target == SelectionTarget::Clipboard {
            clipboard::selection(self, source);
        }
    }

    fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        source: &Self::SelectionUserData,
    ) {
        if target == SelectionTarget::Clipboard {
            clipboard::send(self, fd, &mime, source);
        }
    }
}

impl DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut DataControlState {
        &mut self.data_control
    }
}

impl PointerConstraintsHandler for State {}
impl DndGrabHandler for State {}
impl WaylandDndGrabHandler for State {}
impl TabletSeatHandler for State {
    type ToolFocus = WlSurface;
}
impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device
    }
}

impl OutputHandler for State {}

delegate_dispatch2!(State);

#[expect(
    clippy::needless_pass_by_value,
    reason = "the compositor thread's channels are handed over once, when the thread starts"
)]
fn run(
    name: String,
    rx: channel::Channel<Command>,
    events: mpsc::Sender<Event>,
    ready: mpsc::Sender<std::io::Result<()>>,
) {
    let Ok(mut display) = Display::<State>::new() else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not create Wayland display",
        )));
        return;
    };
    let mut state = State::new(display.handle(), events);
    let keymap = XkbConfig {
        layout: "us",
        ..XkbConfig::default()
    };
    let Ok(keyboard) = state.seat.add_keyboard(keymap, 600, 25) else {
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
    let setup = (|| -> std::io::Result<_> {
        let event_loop = EventLoop::<State>::try_new().map_err(std::io::Error::other)?;
        let handle = event_loop.handle();
        handle
            .insert_source(rx, |event, (), state| match event {
                channel::Event::Msg(command) => state.handle_command(command),
                channel::Event::Closed => state.shutdown = true,
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        handle
            .insert_source(
                Generic::new(listener, Interest::READ, PollMode::Level),
                |_, listener, state| {
                    while let Some(stream) = listener.accept()? {
                        let _ = state
                            .display
                            .insert_client(stream, Arc::new(ClientState::default()));
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(std::io::Error::other)?;
        handle
            .insert_source(
                Generic::new(rustix::io::dup(&display)?, Interest::READ, PollMode::Level),
                |_, _, _| Ok(PostAction::Continue),
            )
            .map_err(std::io::Error::other)?;
        Ok(event_loop)
    })();
    let mut event_loop = match setup {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    while !state.shutdown {
        if let Err(error) = event_loop.dispatch(state.next_wakeup(Instant::now()), &mut state) {
            tracing::warn!(%error, "compositor readiness loop stopped");
            break;
        }
        let _ = display.dispatch_clients(&mut state);
        poll_imports(&mut state);
        clipboard::poll(&mut state);
        state.expire_activation_tokens();
        state.frame_callbacks();
        state.dispatch_frames();
        let _ = display.flush_clients();
    }
}

fn dmabuf_ready(buffer: &Dmabuf) -> bool {
    buffer.handles().all(|fd| {
        let mut descriptors = [rustix::event::PollFd::new(
            &fd,
            rustix::event::PollFlags::IN,
        )];
        rustix::event::poll(
            &mut descriptors,
            Some(&rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
        )
        .is_ok_and(|count| {
            count != 0
                && descriptors[0].revents().intersects(
                    rustix::event::PollFlags::IN
                        | rustix::event::PollFlags::ERR
                        | rustix::event::PollFlags::HUP,
                )
        })
    })
}

fn poll_imports(state: &mut State) {
    let mut index = 0;
    while index < state.pending_imports.len() {
        let pending = &state.pending_imports[index];
        let Ok(surface) = pending.surface.upgrade() else {
            state.pending_imports.swap_remove(index);
            continue;
        };
        if !dmabuf_ready(&pending.buffer) {
            index += 1;
            continue;
        }
        state.pending_imports.swap_remove(index).barrier.signal();
        if let Some(client) = surface.client()
            && let Some(client_state) = client.get_data::<ClientState>()
        {
            let display = state.display.clone();
            client_state
                .compositor_state
                .blocker_cleared(state, &display);
        }
    }
}
