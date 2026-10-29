//! Wayland compositor driven by the host's calloop event loop.
//!
//! Client buffers are copied on commit so later client writes cannot tear a
//! frame in transit to a terminal.

use std::{collections::HashMap, sync::Mutex, time::Instant};

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
        compositor::{CompositorState, get_parent, with_states},
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
mod event_loop;
mod frame;
mod input;
mod keymap;
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

pub use meowland_panes::PaneId;
use meowland_panes::{Policy, Selection, WindowCatalog};

pub(crate) type AttachedPane = meowland_panes::AttachedPane<frame::PaneState>;
pub(crate) type Attachments = meowland_panes::Attachments<frame::PaneState>;
pub use commands::{CloseWindowError, Command};
pub use event_loop::CompositorHandle;
pub use meowland_protocol::WindowId;
use meowland_render::{self as render, DmabufBackend, Renderer, Snapshot};
use runtime::ClientState;
pub use runtime::Runtime;
pub use settings::{
    InvalidOutputSize, InvalidRepeatDelay, InvalidRepeatRate, KeyboardSettings, RepeatDelay,
    RepeatRate, Settings,
};

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

/// Protocol globals and the Smithay state that keeps them alive.
struct Protocols {
    surfaces: CompositorState,
    shm: ShmState,
    dmabuf: DmabufState,
    xdg: XdgShellState,
    activation: XdgActivationState,
    launches: HashMap<String, activation::Launch>,
    activation_deadline: Option<Instant>,
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
    toplevels: toplevels::Toplevels,
}

/// Seat, keyboard, pointer, and popup-grab state.
struct Input {
    seats: SeatState<Compositor>,
    seat: Seat<Compositor>,
    keyboard: Option<KeyboardHandle<Compositor>>,
    keyboard_state: input::KeyboardState,
    shortcuts: Shortcuts,
    keymap: keymap::Keymap,
    pointer: Option<PointerHandle<Compositor>>,
    cursor: seat::CursorState,
    popup_grab: Option<PopupGrab<Compositor>>,
    /// Most recent input source, also used to route selection exports.
    last_input_pane: Option<PaneId>,
}

/// Window map, surface index, and frame-callback pacing shared by windows.
struct Windows {
    by_id: HashMap<WindowId, Window>,
    ids: HashMap<WlSurface, WindowId>,
    callback_deadline: Option<Instant>,
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
    clipboard: clipboard::Bridge,
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

/// Cached toplevel ownership of one surface, stored in its data map.
///
/// Surface data maps are append-only, so the value is mutable in place and
/// can be cleared when the owning window goes away.
#[derive(Debug, Default)]
struct SurfaceOwner(Mutex<Option<WindowId>>);

fn cached_owner(surface: &WlSurface) -> Option<WindowId> {
    with_states(surface, |states| {
        let owner = states.data_map.get::<SurfaceOwner>()?;
        *owner.0.lock().expect("surface owner lock poisoned")
    })
}

fn cache_owner(surface: &WlSurface, window: WindowId) {
    with_states(surface, |states| {
        *states
            .data_map
            .get_or_insert(SurfaceOwner::default)
            .0
            .lock()
            .expect("surface owner lock poisoned") = Some(window);
    });
}

fn clear_owner(surface: &WlSurface) {
    with_states(surface, |states| {
        if let Some(owner) = states.data_map.get::<SurfaceOwner>() {
            *owner.0.lock().expect("surface owner lock poisoned") = None;
        }
    });
}

impl Compositor {
    fn new(display_handle: DisplayHandle, handle: CompositorHandle, settings: &Settings) -> Self {
        let scale = settings.scale;
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&display_handle, "meowland");
        let pointer = Some(seat.add_pointer());
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
            handle,
            protocols: Protocols {
                surfaces: CompositorState::new::<Self>(&display_handle),
                shm: ShmState::new::<Self>(&display_handle, vec![]),
                dmabuf,
                xdg: XdgShellState::new::<Self>(&display_handle),
                activation: XdgActivationState::new::<Self>(&display_handle),
                launches: HashMap::new(),
                activation_deadline: None,
                _decoration: XdgDecorationState::new::<Self>(&display_handle),
                kde_decoration: decoration::kde(&display_handle),
                _viewporter: ViewporterState::new::<Self>(&display_handle),
                _fractional_scale: FractionalScaleManagerState::new::<Self>(&display_handle),
                _single_pixel_buffer: SinglePixelBufferState::new::<Self>(&display_handle),
                _output_manager: output_manager,
                data_device: DataDeviceState::new::<Self>(&display_handle),
                data_control,
                ext_data_control,
                primary_selection,
                _cursor_shape: CursorShapeManagerState::new::<Self>(&display_handle),
                _text_input: text_input,
                toplevels: toplevels::Toplevels::new(&display_handle),
            },
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
                ids: HashMap::new(),
                callback_deadline: None,
                surface_cache: buffer::SurfaceCache::default(),
                catalog: WindowCatalog::default(),
                popups: PopupManager::default(),
                popups_dirty: false,
            },
            gpu,
            readbacks: readback::Readbacks::default(),
            attachments: Attachments::default(),
            clipboard: clipboard::Bridge::default(),
            bell: bell::Bell::new(&display_handle),
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

    /// Logical buffer limit: twice the widest and tallest attached
    /// panes, independently, to allow an in-flight resize. Without an attached
    /// pane only the global surface limits apply.
    fn pane_bound(&self, surface: &WlSurface) -> Option<(u32, u32)> {
        let window = self.window_for_surface(surface)?;
        let scale = self.scale;
        let mut bound: Option<(u32, u32)> = None;
        self.attachments.for_each_showing(window, |_, pane| {
            let pane = (
                scale.logical(pane.presentation.size.width()).unsigned_abs() * 2,
                scale
                    .logical(pane.presentation.size.height())
                    .unsigned_abs()
                    * 2,
            );
            bound = Some(bound.map_or(pane, |bound| (bound.0.max(pane.0), bound.1.max(pane.1))));
        });
        bound
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<WindowId> {
        if let Some(window) = self.windows.ids.get(surface) {
            return Some(*window);
        }
        if let Some(window) = cached_owner(surface) {
            return Some(window);
        }
        let mut current = surface.clone();
        for _ in 0..64 {
            let owner = self
                .windows
                .ids
                .get(&current)
                .copied()
                .or_else(|| cached_owner(&current));
            if let Some(window) = owner {
                cache_owner(surface, window);
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

    fn window_info(&self, window: WindowId) -> WindowInfo {
        self.windows.catalog.info(window)
    }

    fn touch(&mut self, window: WindowId) {
        self.attachments
            .for_each_showing_mut(window, |_, entry| entry.presentation.mark_dirty());
    }

    fn send_title(&mut self, pane: PaneId, window: WindowId) {
        let title = meowland_protocol::sanitize(self.windows.catalog.title(window));
        self.emit(pane, ServerToPane::Title(title));
    }

    fn send_titles(&mut self, window: WindowId) {
        let title = meowland_protocol::sanitize(self.windows.catalog.title(window));
        self.attachments.for_each_showing(window, |pane, _| {
            self.events.push(Event {
                pane,
                message: ServerToPane::Title(title.clone()),
            });
        });
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
            .filter(|&&id| self.windows.catalog.drawable(id))
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
        if let Some(entry) = self.windows.by_id.get(&window)
            && self.windows.catalog.drawable(window)
        {
            entry.with_surfaces(|surface| self.output.enter(surface));
        }
    }

    fn leave_output(&self, window: WindowId) {
        if self.attachments.visible(window) && self.windows.catalog.drawable(window) {
            return;
        }
        if let Some(entry) = self.windows.by_id.get(&window) {
            entry.with_surfaces(|surface| self.output.leave(surface));
        }
    }
}

delegate_dispatch2!(Compositor);
