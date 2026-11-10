//! Wayland compositor driven by the host's calloop event loop.
//!
//! Client buffers are copied on commit so later client writes cannot tear a
//! frame in transit to a terminal.

use std::{collections::HashMap, time::Instant};

use meowland_core::{scale::UiScale, timing::RefreshRate};
use meowland_protocol::{ServerToPane, Shortcuts, WindowInfo};
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
mod bell;
mod buffer;
mod clipboard;
mod commands;
mod decoration;
#[cfg(test)]
mod fixture;
mod frame;
mod input;
mod keymap;
mod panes;
mod popup;
mod readback;
mod runtime;
mod scaling;
mod seat;
mod shell;
mod text;
mod window;

use meowland_panes::{PaneId, WindowCatalog, WindowId};

pub(crate) type AttachedPane = meowland_panes::AttachedPane<frame::PaneState>;
pub(crate) type Attachments = meowland_panes::Attachments<frame::PaneState>;
pub use commands::{CloseWindowError, Command};
/// A compositor projection of a host event loop.
pub type CompositorHandle = meowland_loop::Handle<Compositor>;

use meowland_render::{self as render, DmabufBackend, Renderer, Snapshot};
use meowland_settings::Settings;
use runtime::ClientState;
pub use runtime::Runtime;

/// An owned notification ready for delivery to one terminal pane.
#[derive(Debug)]
pub struct Event {
    /// Pane whose connection should receive this notification.
    pub pane: PaneId,
    /// Owned packet ready for encoding by the host transport.
    pub message: ServerToPane,
}

/// Whether an unmapped toplevel waits for the bufferless commit that starts
/// its remap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remap {
    Idle,
    /// The client removed its buffer; the next bufferless commit gets the
    /// configure that lets it map again.
    AwaitingInitialCommit,
}

struct Window {
    surface: ToplevelSurface,
    fullscreen: bool,
    remap: Remap,
    /// Earliest time the next frame callback may be sent.
    callback_due: Instant,
    /// Whether the client asked for a frame callback.
    callback_pending: bool,
}

/// Protocol globals and the Smithay state that keeps them alive.
struct Protocols {
    surfaces: CompositorState,
    shm: ShmState,
    dmabuf: DmabufState,
    xdg: XdgShellState,
    activation: XdgActivationState,
    launches: HashMap<String, activation::Launch>,
    _decoration: XdgDecorationState,
    kde_decoration: KdeDecorationState,
    _viewporter: ViewporterState,
    _fractional_scale: FractionalScaleManagerState,
    _single_pixel_buffer: SinglePixelBufferState,
    _output_manager: OutputManagerState,
    data_device: DataDeviceState,
    data_control: WlrDataControlState,
    ext_data_control: ExtDataControlState,
    primary_selection: PrimarySelectionState,
    _cursor_shape: CursorShapeManagerState,
    _text_input: TextInputManagerState,
}

/// Seat, keyboard, pointer, and popup-grab state.
struct Input {
    seats: SeatState<Compositor>,
    seat: Seat<Compositor>,
    keyboard: Option<KeyboardHandle<Compositor>>,
    keyboard_state: input::KeyboardState,
    shortcuts: Shortcuts,
    keymap: keymap::Keymap,
    pointer: PointerHandle<Compositor>,
    cursor: seat::CursorState,
    popup_grab: Option<PopupGrab<Compositor>>,
    /// Pane the most recent input came from, which shows the cursor.
    last_input_pane: Option<PaneId>,
}

/// Window map, surface index, and frame-callback pacing shared by windows.
struct Windows {
    by_id: HashMap<WindowId, Window>,
    id_by_surface: HashMap<WlSurface, WindowId>,
    surface_cache: buffer::SurfaceCache,
    catalog: WindowCatalog,
    popups: PopupManager,
    popups_dirty: bool,
}

/// Single-owner Wayland state, also available to host event sources.
pub struct Compositor {
    display: DisplayHandle,
    handle: CompositorHandle,
    protocols: Protocols,
    input: Input,
    windows: Windows,
    gpu: Option<DmabufBackend>,
    readbacks: readback::Readbacks,
    attachments: Attachments,
    clipboard: clipboard::Clipboard,
    bell: bell::Bell,
    scale: UiScale,
    refresh_rate: RefreshRate,
    output: Output,
    renderer: Renderer,
    events: Vec<Event>,
    /// Fixed virtual monitor size in logical coordinates.
    mode: Size<i32, Logical>,
    started: Instant,
    shutdown: bool,
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Compositor")
            .field("windows", &self.windows.by_id.len())
            .field("panes", &self.attachments.len())
            .field("shutdown", &self.shutdown)
            .finish_non_exhaustive()
    }
}

impl Compositor {
    fn new(display_handle: DisplayHandle, handle: CompositorHandle, settings: &Settings) -> Self {
        let scale = settings.scale;
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
        let pointer = seat.add_pointer();
        let text_input = text::manager(&display_handle, &seat);
        let (output_manager, output, mode) = scaling::output(
            &display_handle,
            scale,
            settings.refresh_rate,
            settings.output_size,
        );
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
        let (protocols, bell) = Self::protocol_globals(
            &display_handle,
            gpu.as_ref(),
            output_manager,
            data_control,
            ext_data_control,
            primary_selection,
            text_input,
        );
        Self {
            handle,
            protocols,
            input: Input {
                seats: seat_state,
                seat,
                keyboard: None,
                keyboard_state: input::KeyboardState::default(),
                shortcuts: settings.shortcuts,
                keymap: keymap::Keymap::default(),
                pointer,
                cursor: seat::CursorState::default(),
                popup_grab: None,
                last_input_pane: None,
            },
            windows: Windows {
                by_id: HashMap::new(),
                id_by_surface: HashMap::new(),
                surface_cache: buffer::SurfaceCache::default(),
                catalog: WindowCatalog::default(),
                popups: PopupManager::default(),
                popups_dirty: false,
            },
            gpu,
            readbacks: readback::Readbacks::default(),
            attachments: Attachments::default(),
            clipboard: clipboard::Clipboard::default(),
            bell,
            scale,
            refresh_rate: settings.refresh_rate,
            output,
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

    /// Build the protocol globals in registration order, plus the bell.
    ///
    /// Registry announcement order follows registration order, so keep new
    /// globals below the existing ones when extending this constructor.
    fn protocol_globals(
        display_handle: &DisplayHandle,
        gpu: Option<&DmabufBackend>,
        output_manager: OutputManagerState,
        data_control: WlrDataControlState,
        ext_data_control: ExtDataControlState,
        primary_selection: PrimarySelectionState,
        text_input: TextInputManagerState,
    ) -> (Protocols, bell::Bell) {
        let mut dmabuf = DmabufState::new();
        if let Some(gpu) = gpu {
            match DmabufFeedbackBuilder::new(gpu.device(), gpu.formats().iter().copied()).build() {
                Ok(feedback) => {
                    dmabuf.create_global_with_default_feedback::<Self>(display_handle, &feedback);
                }
                Err(error) => tracing::warn!(%error, "DMA-BUF feedback initialization failed"),
            }
        }
        let surfaces = CompositorState::new::<Self>(display_handle);
        let shm = ShmState::new::<Self>(display_handle, vec![]);
        let xdg = XdgShellState::new::<Self>(display_handle);
        let activation = XdgActivationState::new::<Self>(display_handle);
        let decoration = XdgDecorationState::new::<Self>(display_handle);
        let kde_decoration = decoration::kde(display_handle);
        let viewporter = ViewporterState::new::<Self>(display_handle);
        let fractional_scale = FractionalScaleManagerState::new::<Self>(display_handle);
        let single_pixel_buffer = SinglePixelBufferState::new::<Self>(display_handle);
        let data_device = DataDeviceState::new::<Self>(display_handle);
        let cursor_shape = CursorShapeManagerState::new::<Self>(display_handle);
        let bell = bell::Bell::new(display_handle);
        (
            Protocols {
                surfaces,
                shm,
                dmabuf,
                xdg,
                activation,
                launches: HashMap::new(),
                _decoration: decoration,
                kde_decoration,
                _viewporter: viewporter,
                _fractional_scale: fractional_scale,
                _single_pixel_buffer: single_pixel_buffer,
                _output_manager: output_manager,
                data_device,
                data_control,
                ext_data_control,
                primary_selection,
                _cursor_shape: cursor_shape,
                _text_input: text_input,
            },
            bell,
        )
    }

    /// Logical buffer limit: twice the widest and tallest panes showing the
    /// window, independently, to allow an in-flight resize. Without a pane
    /// only the global surface limits apply.
    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)> {
        let window = self.window_for_surface(surface)?;
        self.attachments
            .showing(window)
            .iter()
            .filter_map(|&pane| self.attachments.get(pane))
            .map(|entry| {
                (
                    self.scale.logical(entry.state.size.width()).unsigned_abs() * 2,
                    self.scale.logical(entry.state.size.height()).unsigned_abs() * 2,
                )
            })
            .reduce(|bound, pane| (bound.0.max(pane.0), bound.1.max(pane.1)))
    }

    /// The toplevel a surface belongs to, walking its current subsurface and
    /// popup parents.
    fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        let mut current = surface.clone();
        for _ in 0..64 {
            if let Some(&window) = self.windows.id_by_surface.get(&current) {
                return Some(window);
            }
            current = match self.windows.popups.find_popup(&current) {
                Some(PopupKind::Xdg(popup)) => popup.get_parent_surface()?,
                Some(PopupKind::InputMethod(_)) => return None,
                None => get_parent(&current)?,
            };
        }
        None
    }

    fn send_title(&mut self, pane: PaneId, window: WindowId) {
        let title = meowland_protocol::sanitize(self.windows.catalog.title(window));
        self.emit(pane, ServerToPane::Title(title));
    }

    fn send_titles(&mut self, window: WindowId) {
        for pane in self.attachments.showing(window).to_vec() {
            self.send_title(pane, window);
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
            .by_id
            .keys()
            .copied()
            .filter(|&id| self.windows.catalog.drawable(id))
            .collect();
        windows.sort_unstable();
        windows
            .into_iter()
            .filter_map(|id| self.windows.catalog.info(id))
            .collect()
    }

    /// Whether the host should stop dispatching this compositor.
    #[must_use]
    pub const fn stopped(&self) -> bool {
        self.shutdown
    }

    fn enter_output(&self, window: WindowId) {
        if self.attachments.showing(window).is_empty() {
            return;
        }
        if let Some(entry) = self.windows.by_id.get(&window)
            && self.windows.catalog.drawable(window)
        {
            entry.with_surfaces(|surface, _| self.output.enter(surface));
        }
    }

    fn leave_output(&self, window: WindowId) {
        if !self.attachments.showing(window).is_empty() && self.windows.catalog.drawable(window) {
            return;
        }
        if let Some(entry) = self.windows.by_id.get(&window) {
            entry.with_surfaces(|surface, _| self.output.leave(surface));
        }
    }
}

delegate_dispatch2!(Compositor);
