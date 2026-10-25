//! Wayland compositor driven by the host's calloop event loop.
//!
//! Client buffers are copied on commit so later client writes cannot tear a
//! frame in transit to a terminal.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

use calloop::LoopHandle;
use meowland_core::scale::UiScale;
use meowland_protocol::{
    Input, ServerToPane, Show, WindowInfo, clipboard::Selection as ClipboardSelection,
};
use smithay::{
    delegate_dispatch2,
    desktop::{PopupGrab, PopupKind, PopupManager},
    input::{Seat, SeatState, keyboard::KeyboardHandle, pointer::PointerHandle},
    output::Output,
    reexports::wayland_server::{
        DisplayHandle, backend::ClientId, protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Serial, Size},
    wayland::{
        compositor::{CompositorState, SurfaceAttributes, get_parent, with_states},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufFeedbackBuilder, DmabufState},
        fractional_scale::FractionalScaleManagerState,
        output::OutputManagerState,
        selection::{
            data_device::DataDeviceState,
            ext_data_control::DataControlState as ExtDataControlState,
            primary_selection::PrimarySelectionState,
            wlr_data_control::DataControlState as WlrDataControlState,
        },
        shell::{
            kde::decoration::KdeDecorationState,
            xdg::{ToplevelSurface, XdgShellState, decoration::XdgDecorationState},
        },
        shm::ShmState,
        single_pixel_buffer::SinglePixelBufferState,
        text_input::TextInputManagerState,
        viewporter::ViewporterState,
        xdg_activation::XdgActivationState,
    },
};

mod activation;
mod attachment;
mod bell;
mod buffer;
mod catalog;
mod clipboard;
mod commands;
mod decoration;
mod frame;
mod input;
mod panes;
mod popup;
mod readback;
mod runtime;
mod scaling;
mod seat;
mod shell;
mod sources;
mod text;
mod toplevels;

use Compositor as State;
use attachment::{Attachments, Binding, Pane, Policy, Selection};
pub use attachment::{PaneId, WindowId};
use catalog::WindowCatalog;
use meowland_render::{self as render, DmabufBackend, Renderer, Snapshot, surface_stack};
use runtime::ClientState;
pub use runtime::Runtime;

const FRAME_INTERVAL: Duration = Duration::from_micros(8_334);

#[derive(Clone, Debug)]
pub enum Command {
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
        selection: ClipboardSelection,
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

/// An owned notification ready for delivery to one terminal pane.
#[derive(Debug)]
pub struct Event {
    pub pane: PaneId,
    pub message: ServerToPane,
}

struct Window {
    surface: ToplevelSurface,
    fullscreen: bool,
    callback_due: Instant,
    callback_pending: bool,
}

/// Single-owner Wayland state, also available to host event sources.
pub struct Compositor {
    display: DisplayHandle,
    handle: LoopHandle<'static, Self>,
    surfaces: CompositorState,
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
    _fractional_scale: FractionalScaleManagerState,
    scale: UiScale,
    _single_pixel_buffer: SinglePixelBufferState,
    _output_manager: OutputManagerState,
    data_device: DataDeviceState,
    data_control: WlrDataControlState,
    ext_data_control: ExtDataControlState,
    primary_selection: PrimarySelectionState,
    clipboard: clipboard::Bridge,
    _cursor_shape: CursorShapeManagerState,
    bell: bell::Bell,
    _text_input: TextInputManagerState,
    popups: PopupManager,
    popup_grab: Option<PopupGrab<Self>>,
    seats: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: Option<KeyboardHandle<Self>>,
    /// One terminal pane owns the seat's held keyboard state at a time.
    keyboard_pane: Option<PaneId>,
    keyboard_keys: HashSet<u16>,
    keyboard_known: bool,
    recent_serials: VecDeque<(Serial, ClientId)>,
    pointer: Option<PointerHandle<Self>>,
    output: Output,
    windows: HashMap<WindowId, Window>,
    ids: HashMap<WlSurface, WindowId>,
    snapshots: HashMap<WlSurface, Snapshot>,
    /// Cached to distinguish geometry changes from frame-only commits.
    surface_geometry: HashMap<WlSurface, buffer::Geometry>,
    catalog: WindowCatalog,
    toplevels: toplevels::Toplevels,
    attachments: Attachments,
    /// Most recent input pane, used for cursor shape events.
    cursor_pane: Option<PaneId>,
    cursor: Option<String>,
    /// Last delivered cursor, including its destination pane.
    sent_cursor: Option<(PaneId, Option<String>)>,
    renderer: Renderer,
    events: Vec<Event>,
    /// Output mode, rewritten as panes attach and leave.
    mode: Size<i32, Logical>,
    started: Instant,
    shutdown: bool,
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Compositor")
            .field("windows", &self.windows.len())
            .field("panes", &self.attachments.panes.len())
            .field("shutdown", &self.shutdown)
            .finish_non_exhaustive()
    }
}

impl State {
    fn new(
        display_handle: DisplayHandle,
        handle: LoopHandle<'static, Self>,
        scale: UiScale,
    ) -> Self {
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
        let pointer = Some(seat.add_pointer());
        let text_input = text::manager(&display_handle, &seat);
        let (output_manager, output, mode) = scaling::output(&display_handle, scale);
        let primary_selection = PrimarySelectionState::new::<Self>(&display_handle);
        let data_control =
            WlrDataControlState::new::<Self, _>(&display_handle, Some(&primary_selection), |_| {
                true
            });
        let ext_data_control =
            ExtDataControlState::new::<Self, _>(&display_handle, Some(&primary_selection), |_| {
                true
            });
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
            surfaces: CompositorState::new::<Self>(&display_handle),
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
            _fractional_scale: FractionalScaleManagerState::new::<Self>(&display_handle),
            scale,
            _single_pixel_buffer: SinglePixelBufferState::new::<Self>(&display_handle),
            _output_manager: output_manager,
            data_device: DataDeviceState::new::<Self>(&display_handle),
            data_control,
            ext_data_control,
            primary_selection,
            clipboard: clipboard::Bridge::default(),
            _cursor_shape: CursorShapeManagerState::new::<Self>(&display_handle),
            bell: bell::Bell::new(&display_handle),
            _text_input: text_input,
            popups: PopupManager::default(),
            popup_grab: None,
            seats: seat_state,
            seat,
            keyboard: None,
            keyboard_pane: None,
            keyboard_keys: HashSet::new(),
            keyboard_known: false,
            recent_serials: VecDeque::new(),
            pointer,
            output,
            windows: HashMap::new(),
            ids: HashMap::new(),
            snapshots: HashMap::new(),
            surface_geometry: HashMap::new(),
            catalog: WindowCatalog::default(),
            toplevels: toplevels::Toplevels::new(&display_handle),
            attachments: Attachments::default(),
            cursor_pane: None,
            cursor: None,
            sent_cursor: None,
            renderer: Renderer::default(),
            events: Vec::new(),
            mode: Size::from((
                scale.logical(mode.size.w as u32),
                scale.logical(mode.size.h as u32),
            )),
            started: Instant::now(),
            shutdown: false,
            display: display_handle,
        }
    }

    /// Logical buffer limit: twice the widest and tallest attached
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
                    self.scale.logical(pane.presentation.size.width()) as u32 * 2,
                    self.scale.logical(pane.presentation.size.height()) as u32 * 2,
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
            self.toplevels.remove(id);
            return;
        }
        if !self.toplevels.contains(id) {
            let info = self.window_info(id);
            self.toplevels.announce(id, &info.title, &info.app_id);
        }
        if !self.catalog.announce(id) {
            return;
        }
        self.follow_panes(id);
        self.resolve_pending_panes();
        self.apply_window_state(id);
        self.enter_output(id);
        self.focus_window(id);
    }

    fn window_info(&self, window: WindowId) -> WindowInfo {
        self.catalog.info(window)
    }

    fn touch(&mut self, window: WindowId) {
        for entry in self.attachments.panes.values_mut() {
            if entry.window() == Some(window) {
                entry.presentation.mark_dirty();
            }
        }
    }

    fn send_title(&mut self, pane: PaneId, window: WindowId) {
        let title = meowland_protocol::sanitize(self.catalog.title(window));
        self.emit(pane, ServerToPane::Title(title));
    }

    fn send_titles(&mut self, window: WindowId) {
        let title = meowland_protocol::sanitize(self.catalog.title(window));
        for (&pane, entry) in &self.attachments.panes {
            if entry.window() == Some(window) {
                self.events.push(Event {
                    pane,
                    message: ServerToPane::Title(title.clone()),
                });
            }
        }
    }

    fn emit(&mut self, pane: PaneId, message: ServerToPane) {
        self.events.push(Event { pane, message });
    }

    /// Drain notifications while retaining the queue's allocation.
    pub fn drain_events(&mut self) -> impl Iterator<Item = Event> + '_ {
        self.events.drain(..)
    }

    /// List mapped windows directly from the compositor's catalog.
    #[must_use]
    pub fn window_list(&self) -> Vec<WindowInfo> {
        let mut windows: Vec<_> = self
            .windows
            .keys()
            .filter(|&&id| self.catalog.drawable(id))
            .map(|&id| self.window_info(id))
            .collect();
        windows.sort_unstable_by_key(|window| window.id);
        windows
    }

    /// Whether the host should stop dispatching this compositor.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.shutdown
    }

    fn enter_output(&self, window: WindowId) {
        if !self.attachments.visible(window) {
            return;
        }
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
        if self.attachments.visible(window) {
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
}

delegate_dispatch2!(State);
