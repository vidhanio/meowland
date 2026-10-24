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

use calloop::{
    EventLoop, Interest, LoopHandle, Mode as PollMode, PostAction, channel, generic::Generic,
};
use meowland_protocol::{Input, Show, WindowInfo};
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_dispatch2,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        dnd::DndGrabHandler,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorIcon, CursorImageStatus, PointerHandle},
        tablet::TabletSeatHandler,
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        Client, Display, DisplayHandle, ListeningSocket, Resource,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer, wl_surface::WlSurface},
    },
    utils::{Logical, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
            SurfaceAttributes, add_pre_commit_hook, get_children, get_parent, with_states,
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
            xdg::{ToplevelSurface, XdgShellState, decoration::XdgDecorationState},
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
        viewporter::{ViewportCachedState, ViewporterState},
        xdg_activation::XdgActivationState,
    },
};

mod activation;
mod attachment;
mod bell;
mod catalog;
mod clipboard;
mod commands;
mod decoration;
mod frame;
mod input;
mod readback;
mod shell;
mod sources;
mod toplevels;

use attachment::{Attachments, Binding, Pane, Policy, Selection};
pub use attachment::{PaneId, WindowId};
use catalog::WindowCatalog;
use meowland_render::{self as render, DmabufBackend, Renderer, Snapshot, surface_stack};

const FRAME_INTERVAL: Duration = Duration::from_nanos(16_666_667);

#[derive(Clone, Debug)]
pub enum Command {
    CreateActivationToken {
        request: u64,
    },
    CancelActivation {
        token: String,
    },
    Kill {
        request: u64,
        show: Show,
        waiting: Arc<std::sync::atomic::AtomicBool>,
        deadline: Instant,
    },
    Attach {
        pane: PaneId,
        show: Show,
        width: u32,
        height: u32,
    },
    Detach {
        pane: PaneId,
    },
    Resize {
        pane: PaneId,
        width: u32,
        height: u32,
    },
    Input {
        pane: PaneId,
        event: Input,
    },
    Ack {
        pane: PaneId,
        drawn: bool,
    },
    Paste {
        pane: PaneId,
        text: String,
    },
    ClipboardOffer {
        pane: PaneId,
        offer: u64,
        mimes: Vec<String>,
        paste: bool,
    },
    ClipboardReply {
        pane: PaneId,
        request: u64,
        data: Option<Vec<u8>>,
    },
    CloseAll,
    CloseShown {
        pane: PaneId,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum Event {
    ActivationCreated {
        request: u64,
        token: String,
    },
    CloseRequested {
        request: u64,
        result: Result<(), String>,
    },
    /// A window appeared, or its title or app id changed.
    WindowUp(WindowInfo),
    WindowDown(WindowId),
    Title {
        pane: PaneId,
        title: String,
    },
    /// The pointer shape a pane should show, or `None` for the terminal's own.
    Cursor {
        pane: PaneId,
        shape: Option<String>,
    },
    Focus(Option<WindowId>),
    Bell {
        pane: PaneId,
    },
    ClipboardWrite {
        pane: PaneId,
        data: meowland_protocol::clipboard::ClipboardData,
    },
    ClipboardRead {
        pane: PaneId,
        request: u64,
        offer: u64,
        mime: String,
        deadline: meowland_protocol::clipboard::Deadline,
    },
    ClipboardCancel {
        pane: PaneId,
        request: u64,
    },
    Frame {
        pane: PaneId,
        width: u32,
        height: u32,
        /// The first row `rgb` covers.
        y: u32,
        rgb: Vec<u8>,
    },
    Release {
        pane: PaneId,
        reason: String,
    },
}

pub type Handle = (
    channel::Sender<Command>,
    tokio::sync::mpsc::UnboundedReceiver<Event>,
    String,
    thread::JoinHandle<()>,
);

/// Start the compositor thread and wait for its Wayland socket to be ready.
///
/// # Errors
/// Returns an error if the thread, display, keymap or socket cannot start, or
/// if readiness is not reported within two seconds.
pub fn spawn() -> meowland_core::Result<Handle> {
    let (commands, rx) = channel::channel();
    let (tx, events) = tokio::sync::mpsc::unbounded_channel();
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
    fullscreen: bool,
    callback_due: Instant,
    callback_pending: bool,
}

type ViewportGeometry = (Option<Rectangle<f64, Logical>>, Option<Size<i32, Logical>>);

struct State {
    display: DisplayHandle,
    handle: LoopHandle<'static, Self>,
    compositor: CompositorState,
    shm: ShmState,
    dmabuf: DmabufState,
    gpu: Option<DmabufBackend>,
    readbacks: readback::Readbacks,
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
    bell: bell::Bell,
    popups: PopupManager,
    seats: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: Option<KeyboardHandle<Self>>,
    /// One terminal pane owns the seat's held keyboard state at a time.
    keyboard_pane: Option<PaneId>,
    keyboard_keys: HashSet<u16>,
    keyboard_known: bool,
    pointer: Option<PointerHandle<Self>>,
    output: Output,
    windows: HashMap<WindowId, Window>,
    ids: HashMap<WlSurface, WindowId>,
    snapshots: HashMap<WlSurface, Snapshot>,
    /// Cached to distinguish viewport changes from frame-only commits.
    viewport_geometry: HashMap<WlSurface, ViewportGeometry>,
    catalog: WindowCatalog,
    toplevels: toplevels::Toplevels,
    attachments: Attachments,
    /// Most recent input pane, used for cursor shape events.
    cursor_pane: Option<PaneId>,
    cursor: Option<String>,
    /// Last delivered cursor, including its destination pane.
    sent_cursor: Option<(PaneId, Option<String>)>,
    renderer: Renderer,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
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
            let assignment = with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                match &attributes.pending().buffer {
                    Some(BufferAssignment::NewBuffer(buffer)) => {
                        Some(BufferAssignment::NewBuffer(buffer.clone()))
                    }
                    Some(BufferAssignment::Removed) => Some(BufferAssignment::Removed),
                    None => None,
                }
            });
            match assignment {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    if let Ok(dmabuf) = get_dmabuf(&buffer) {
                        readback::prepare(state, surface, &buffer, dmabuf.clone());
                    } else {
                        readback::clear(surface);
                    }
                }
                Some(BufferAssignment::Removed) => readback::clear(surface),
                None => {}
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
        let (assignment, damaged, viewport, callbacks) = with_states(surface, |states| {
            let (assignment, damaged, callbacks) = {
                let mut cached = states.cached_state.get::<SurfaceAttributes>();
                let current = cached.current();
                let damaged = !current.damage.is_empty();
                current.damage.clear();
                (
                    current.buffer.take(),
                    damaged,
                    !current.frame_callbacks.is_empty(),
                )
            };
            let mut viewport = states.cached_state.get::<ViewportCachedState>();
            let current = viewport.current();
            (assignment, damaged, (current.src, current.dst), callbacks)
        });
        if callbacks
            && let Some(window) = self
                .window_for_surface(surface)
                .and_then(|id| self.windows.get_mut(&id))
        {
            window.callback_pending = true;
        }
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
                let snapshot = if get_dmabuf(&buffer).is_ok() {
                    readback::snapshot(surface)
                } else {
                    with_buffer_contents(&buffer, |ptr, len, data| {
                        // SAFETY: Smithay keeps the mapped pool readable for
                        // this callback and supplies
                        // its pointer and length together.
                        unsafe { Snapshot::copy_shm(ptr, len, &data, bound, &mut previous) }
                    })
                    .ok()
                    .flatten()
                };
                buffer.release();
                if let Some(snapshot) = snapshot.or(previous) {
                    self.snapshots.insert(surface.clone(), snapshot);
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
        readback::destroyed(self, surface);
    }
}

impl State {
    fn new(
        display_handle: DisplayHandle,
        handle: LoopHandle<'static, Self>,
        events: tokio::sync::mpsc::UnboundedSender<Event>,
    ) -> Self {
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
            handle,
            readbacks: readback::Readbacks::default(),
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
            bell: bell::Bell::new(&display_handle),
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
            catalog: WindowCatalog::default(),
            toplevels: toplevels::Toplevels::new(&display_handle),
            attachments: Attachments::default(),
            cursor_pane: None,
            cursor: None,
            sent_cursor: None,
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
        self.attachments
            .panes
            .values()
            .filter(|pane| pane.window() == Some(window))
            .fold(None, |bound: Option<(u32, u32)>, pane| {
                let pane = (
                    pane.presentation.size.width() * 2,
                    pane.presentation.size.height() * 2,
                );
                Some(bound.map_or(pane, |bound| (bound.0.max(pane.0), bound.1.max(pane.1))))
            })
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
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
        if !self.catalog.announce(id) {
            return;
        }
        self.follow_panes(id);
        self.resolve_pending_panes();
        let info = self.window_info(id);
        self.toplevels.announce(id, &info.title, &info.app_id);
        let _ = self.events.send(Event::WindowUp(info));
        self.apply_window_state(id);
        self.enter_output(id);
        self.focus_window(id);
    }

    fn window_info(&self, window: WindowId) -> WindowInfo {
        self.catalog.info(window)
    }

    /// The last-used pane decides the window's size; otherwise use the first
    /// attached pane, then the output mode.
    fn pane_size(&self, window: WindowId) -> Size<i32, Logical> {
        self.attachments.size(window).map_or(self.mode, |size| {
            Size::from((size.width() as i32, size.height() as i32))
        })
    }

    fn panes_showing(&self, window: WindowId) -> Vec<PaneId> {
        self.attachments.showing(window)
    }

    /// The lowest-numbered pane sets the output mode seen by clients.
    fn insert_pane(&mut self, pane: PaneId, entry: Pane) {
        let size = entry.presentation.size;
        self.attachments.panes.insert(pane, entry);
        if self.attachments.first() == Some(pane) {
            self.configure_output(size);
        }
    }

    /// Remove every pane-keyed entry, restoring the remaining output mode.
    fn remove_pane(&mut self, pane: PaneId) -> Option<WindowId> {
        if self.keyboard_pane == Some(pane) {
            input::reset_keyboard(self);
        }
        let first = self.attachments.first() == Some(pane);
        let removed = self.attachments.remove(pane);
        let window = removed.as_ref().and_then(Pane::window);
        self.clipboard.detach(pane);
        if let Some(Pane {
            binding: Binding::Waiting(Selection::Activation(token)),
            ..
        }) = removed
        {
            self.forget_activation(&token);
        }
        if self.cursor_pane == Some(pane) {
            self.cursor_pane = None;
        }
        if self
            .sent_cursor
            .as_ref()
            .is_some_and(|(sent, _)| *sent == pane)
        {
            self.sent_cursor = None;
        }
        if first && let Some(next) = self.attachments.first() {
            self.configure_output(self.attachments.panes[&next].presentation.size);
        }
        window
    }

    /// Move focus to another attached pane's window when this pane is released.
    fn release_pane_window(&mut self, pane: PaneId) {
        let Some(window) = self.remove_pane(pane) else {
            return;
        };
        self.leave_output(window);
        if self.catalog.focused() == Some(window)
            && let Some(next) = self
                .attachments
                .first()
                .and_then(|id| self.attachments.panes[&id].window())
        {
            self.focus_window(next);
        }
    }

    fn follow_panes(&mut self, window: WindowId) {
        let following = self.attachments.following();
        if self
            .keyboard_pane
            .is_some_and(|pane| following.contains(&pane))
        {
            input::reset_keyboard(self);
        }
        for pane in following {
            if let Some(state) = self.attachments.panes.get_mut(&pane) {
                state.show(window);
            }
            self.send_title(pane, window);
            self.send_pane_cursor(pane);
        }
        self.resume_callbacks(window);
    }

    fn touch(&mut self, window: WindowId) {
        for entry in self.attachments.panes.values_mut() {
            if entry.window() == Some(window) {
                entry.presentation.mark_dirty();
            }
        }
    }

    fn send_pane_cursor(&mut self, pane: PaneId) {
        let window = self.attachments.panes.get(&pane).and_then(Pane::window);
        let focused = self
            .pointer
            .as_ref()
            .and_then(PointerHandle::current_focus)
            .and_then(|surface| self.window_for_surface(&surface));
        let shape = if window.is_some() && window == focused {
            self.cursor.clone()
        } else {
            None
        };
        self.send_cursor(pane, shape);
    }

    fn send_cursor(&mut self, pane: PaneId, shape: Option<String>) {
        let cursor = (pane, shape);
        if self.sent_cursor.as_ref() == Some(&cursor) {
            return;
        }
        let _ = self.events.send(Event::Cursor {
            pane,
            shape: cursor.1.clone(),
        });
        self.sent_cursor = Some(cursor);
    }

    fn send_title(&self, pane: PaneId, window: WindowId) {
        let title = meowland_protocol::sanitize(self.catalog.title(window));
        let _ = self.events.send(Event::Title { pane, title });
    }

    fn send_titles(&self, window: WindowId) {
        for (&pane, entry) in &self.attachments.panes {
            if entry.window() == Some(window) {
                self.send_title(pane, window);
            }
        }
    }

    fn enter_output(&self, window: WindowId) {
        let Some(root) = self
            .windows
            .get(&window)
            .filter(|_| self.catalog.drawable(window))
            .map(|entry| entry.surface.wl_surface())
        else {
            return;
        };
        for (surface, _) in surface_stack(root) {
            self.output.enter(&surface);
        }
    }

    fn leave_output(&self, window: WindowId) {
        if self
            .attachments
            .panes
            .values()
            .any(|pane| pane.window() == Some(window))
        {
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

    fn focus_window(&mut self, window: WindowId) {
        if self.catalog.focused() == Some(window) {
            return;
        }
        input::reset_keyboard(self);
        let previous = self.catalog.focus(Some(window));
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
        let _ = self.events.send(Event::Focus(Some(window)));
    }
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

    /// During pointer motion, delivery waits until the caller restores the
    /// pointer handle so the destination can be checked against its focus.
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let shape = match image {
            CursorImageStatus::Named(icon) => Some(cursor_name(icon).to_owned()),
            CursorImageStatus::Hidden | CursorImageStatus::Surface(_) => None,
        };
        self.cursor = shape;
        if self.pointer.is_some()
            && let Some(pane) = self.cursor_pane
        {
            self.send_pane_cursor(pane);
        }
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
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    ready: mpsc::Sender<std::io::Result<()>>,
) {
    let Ok(mut display) = Display::<State>::new() else {
        let _ = ready.send(Err(std::io::Error::other(
            "could not create Wayland display",
        )));
        return;
    };
    let mut event_loop = match EventLoop::<State>::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(std::io::Error::other(error)));
            return;
        }
    };
    let mut state = State::new(display.handle(), event_loop.handle(), events);
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
    let setup = (|| -> std::io::Result<()> {
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
        Ok(())
    })();
    if let Err(error) = setup {
        let _ = ready.send(Err(error));
        return;
    }
    let _ = ready.send(Ok(()));
    while !state.shutdown {
        if let Err(error) = event_loop.dispatch(state.next_wakeup(Instant::now()), &mut state) {
            tracing::warn!(%error, "compositor readiness loop stopped");
            break;
        }
        let _ = display.dispatch_clients(&mut state);
        state.expire_activation_tokens();
        state.frame_callbacks();
        state.dispatch_frames();
        let _ = display.flush_clients();
    }
}
