//! Wayland compositor driven by the host's calloop event loop.
//!
//! Client buffers are copied on commit so later client writes cannot tear a
//! frame in transit to a terminal.

use std::{collections::HashMap, time::Instant};

use calloop::LoopHandle;
use meowland_core::{scale::UiScale, timing::RefreshRate};
use meowland_protocol::{ServerToPane, WindowInfo};
use smithay::{
    delegate_dispatch2,
    desktop::{PopupGrab, PopupKind, PopupManager},
    input::{Seat, SeatState, keyboard::KeyboardHandle, pointer::PointerHandle},
    output::Output,
    reexports::wayland_server::{DisplayHandle, protocol::wl_surface::WlSurface},
    utils::{Logical, Size},
    wayland::{
        compositor::{CompositorState, get_parent},
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
mod settings;
mod shell;
mod sources;
mod text;
mod toplevels;
mod window;

use Compositor as State;
use attachment::{AttachedPane, Attachments, Binding, Policy, Selection};
pub use attachment::{PaneId, WindowId};
use catalog::WindowCatalog;
pub use commands::{CloseWindowError, Command};
use meowland_render::{self as render, DmabufBackend, Renderer, Snapshot};
use runtime::ClientState;
pub use runtime::Runtime;
pub use settings::{KeyboardSettings, Settings};

/// An owned notification ready for delivery to one terminal pane.
#[derive(Debug)]
pub struct Event {
    /// Pane whose connection should receive this notification.
    pub pane: PaneId,
    /// Owned packet ready for encoding by the host transport.
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
    refresh_rate: RefreshRate,
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
    keyboard_state: input::KeyboardState,
    pointer: Option<PointerHandle<Self>>,
    output: Output,
    windows: HashMap<WindowId, Window>,
    ids: HashMap<WlSurface, WindowId>,
    surface_cache: buffer::SurfaceCache,
    catalog: WindowCatalog,
    toplevels: toplevels::Toplevels,
    attachments: Attachments,
    /// Most recent input source, also used to route selection exports.
    last_input_pane: Option<PaneId>,
    cursor: seat::CursorState,
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
        settings: &Settings,
    ) -> Self {
        let scale = settings.scale;
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
        let pointer = Some(seat.add_pointer());
        let text_input = text::manager(&display_handle, &seat);
        let (output_manager, output, mode) =
            scaling::output(&display_handle, scale, settings.refresh_rate);
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
            refresh_rate: settings.refresh_rate,
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
            keyboard_state: input::KeyboardState::default(),
            pointer,
            output,
            windows: HashMap::new(),
            ids: HashMap::new(),
            surface_cache: buffer::SurfaceCache::default(),
            catalog: WindowCatalog::default(),
            toplevels: toplevels::Toplevels::new(&display_handle),
            attachments: Attachments::default(),
            last_input_pane: None,
            cursor: seat::CursorState::default(),
            renderer: Renderer::default(),
            events: Vec::new(),
            mode: Size::from((
                scale
                    .logical(u32::try_from(mode.size.w).expect("virtual output width is positive")),
                scale.logical(
                    u32::try_from(mode.size.h).expect("virtual output height is positive"),
                ),
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
                    self.scale
                        .logical(pane.presentation.size.width())
                        .unsigned_abs()
                        * 2,
                    self.scale
                        .logical(pane.presentation.size.height())
                        .unsigned_abs()
                        * 2,
                );
                Some(bound.map_or(pane, |bound| (bound.0.max(pane.0), bound.1.max(pane.1))))
            })
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        let mut surface = surface.clone();
        for _ in 0..64 {
            if let Some(id) = self
                .ids
                .get(&surface)
                .or_else(|| self.surface_cache.owners.get(&surface))
            {
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
        if let Some(entry) = self.windows.get(&window)
            && self.catalog.drawable(window)
        {
            entry.with_surfaces(|surface| self.output.enter(surface));
        }
    }

    fn leave_output(&self, window: WindowId) {
        if self.attachments.visible(window) && self.catalog.drawable(window) {
            return;
        }
        if let Some(entry) = self.windows.get(&window) {
            entry.with_surfaces(|surface| self.output.leave(surface));
        }
    }
}

delegate_dispatch2!(State);
