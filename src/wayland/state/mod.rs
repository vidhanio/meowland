//! The compositor's Wayland state: its windows, their surfaces, and the panes
//! that show them.

mod compose;
mod input;
mod snapshot;

use std::{collections::HashMap, os::unix::net::UnixStream, sync::Arc, time::Instant};

use calloop::channel::Sender;
use evdev::KeyCode;
use smithay::{
    backend::allocator::dmabuf::Dmabuf,
    delegate_compositor, delegate_cursor_shape, delegate_data_device, delegate_dmabuf,
    delegate_output, delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_shell,
    desktop::{PopupKind, PopupManager},
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{KeyboardHandle, XkbConfig},
        pointer::{CursorImageStatus, PointerHandle},
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
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
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
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        viewporter::ViewporterState,
    },
};

use self::compose::{is_drawn, send_frame_callbacks};
use crate::{
    Error, kitty,
    protocol::{
        PaneId, WindowId,
        pane::{Capabilities, Show},
    },
    render::Frame,
    wayland::{buffer::Snapshot, message::Event},
};

pub const REFRESH_MILLIHZ: i32 = 60_000;

/// Per-pane rendering state.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each of these is an independent thing about one pane"
)]
#[derive(Debug)]
struct View {
    id: PaneId,
    capabilities: Capabilities,
    window: Option<WindowId>,
    follow: bool,
    /// The frame this pane draws into, while the presenter is not holding it.
    frame: Option<Frame>,
    /// The frame no longer matches the capabilities.
    stale_layout: bool,
    scene_dirty: bool,
    /// The terminal is owed the title or the pointer shape it was last told of,
    /// or both.
    escapes_dirty: bool,
    /// What this pane's terminal was last told, so that it is told again only
    /// when that changes.
    pointer_shape: Option<&'static str>,
    title: Option<String>,
    /// This pane has nothing left to show, and the server has been told.
    leaving: bool,
}

impl View {
    fn new(id: PaneId, capabilities: &Capabilities) -> Self {
        let frame = Frame::new(capabilities.pixels.0, capabilities.pixels.1);
        Self {
            id,
            capabilities: capabilities.clone(),
            window: None,
            follow: true,
            frame: Some(frame),
            stale_layout: false,
            scene_dirty: true,
            escapes_dirty: true,
            pointer_shape: None,
            title: None,
            leaving: false,
        }
    }

    /// Whether this pane has something to do now: something to tell its
    /// terminal, or a scene to draw and a frame to draw it in.
    const fn is_due(&self) -> bool {
        !self.leaving && (self.escapes_dirty || (self.scene_dirty && self.frame.is_some()))
    }

    /// Bring the frame back in step with the capabilities.
    fn relayout(&mut self) {
        let Some(frame) = &mut self.frame else {
            return;
        };
        if !self.stale_layout {
            return;
        }
        frame.resize(self.capabilities.pixels.0, self.capabilities.pixels.1);
        self.stale_layout = false;
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
    /// The pane whose interaction most recently selected this window.
    last_interacted: Option<PaneId>,
    /// Sent back in the configure state.
    fullscreen: bool,
}

pub struct Compositor {
    /// The `wl_compositor` global: the state every surface of a client is made
    /// from.
    surfaces: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    seat_state: SeatState<Self>,
    dmabuf_state: DmabufState,
    /// Kept alive so the render nodes stay described to clients; never read.
    #[expect(dead_code, reason = "the state object is what keeps the global alive")]
    dmabuf_global: Option<DmabufGlobal>,
    gpu: Option<crate::wayland::gpu::Renderer>,

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
    cursor: CursorImageStatus,

    /// Attach order determines which pane sizes the output.
    views: Vec<View>,

    snapshots: HashMap<ObjectId, Snapshot>,
    plan: Vec<(WlSurface, Point<i32, Logical>)>,

    /// What the server is told.
    events: Sender<Event>,
}

impl std::fmt::Debug for Compositor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Compositor")
            .field("windows", &self.windows.len())
            .field("active", &self.active)
            .finish_non_exhaustive()
    }
}

impl Compositor {
    pub fn new(
        display: &DisplayHandle,
        nodes: &[crate::dmabuf::RenderNode],
        events: Sender<Event>,
    ) -> Result<Self, Error> {
        let surfaces = CompositorState::new::<Self>(display);
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
            surfaces,
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
            cursor: CursorImageStatus::default_named(),
            views: Vec::new(),
            snapshots: HashMap::new(),
            plan: Vec::new(),
            events,
        })
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    pub fn insert_client(&mut self, stream: UnixStream) -> std::io::Result<()> {
        self.display_handle
            .insert_client(stream, Arc::new(ClientState::default()))?;
        Ok(())
    }

    fn active_surface(&self) -> Option<WlSurface> {
        self.active
            .and_then(|index| self.windows.get(index))
            .map(|window| window.surface.wl_surface().clone())
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
        let _ = self.events.send(Event::Focused { window: Some(id) });
    }

    pub fn attach_view(&mut self, id: PaneId, show: Show, capabilities: &Capabilities) {
        let (window, follow) = self.resolve(show);
        let mut view = View::new(id, capabilities);
        // The server checks a window asked for by ID before it sends this, but
        // a pane attaches one message later: the window may have closed since,
        // and a view holding a window that is gone would never be released by
        // `toplevel_destroyed`.
        view.window = window.filter(|id| self.has_window(*id));
        view.follow = follow;
        let gone = view.window.is_none() && !follow;
        if self.views.is_empty() {
            self.describe_output(capabilities);
        }
        tracing::info!(id = %id, ?window, follow, "pane attached");
        self.views.push(view);
        self.configure_windows();
        self.sync_outputs();
        let pane = self.views.len() - 1;
        self.mark_escapes(pane);
        if gone {
            // Only a window asked for by ID can be missing: the other ways of
            // choosing one fall back to following the newest window.
            self.leave(pane, Some("no window has that ID".to_owned()));
        }
    }

    /// Ask every window to close, the way a window manager does.
    ///
    /// This is a request, not a command: the client decides what closing means
    /// for it, and a client that would rather ask its user first may. So this
    /// is what the server sends when it is stopping, and the signals that
    /// follow are for whatever is left.
    pub fn close_windows(&self) {
        tracing::info!(windows = self.windows.len(), "asked every window to close");
        for window in &self.windows {
            tracing::debug!(id = %window.id, "asked to close");
            window.surface.send_close();
        }
    }

    /// Whether the compositor has this window.
    fn has_window(&self, window: WindowId) -> bool {
        self.windows.iter().any(|known| known.id == window)
    }

    pub fn detach_view(&mut self, id: PaneId) {
        self.views.retain(|view| view.id != id);
        for window in &mut self.windows {
            if window.last_interacted == Some(id) {
                window.last_interacted = None;
            }
        }
        tracing::info!(id = %id, "pane gone");
        self.configure_windows();
        self.sync_outputs();
    }

    /// The terminal this pane is in now reports these capabilities.
    ///
    /// The frame itself is resized when the presenter gives it back, if it is
    /// out: a frame already on its way to the terminal is at the old size.
    pub fn resize_view(&mut self, id: PaneId, capabilities: &Capabilities) {
        let Some(index) = self.view(id) else {
            return;
        };
        let first = index == 0;
        self.views[index].capabilities.clone_from(capabilities);
        self.views[index].stale_layout = true;
        self.views[index].scene_dirty = true;
        if first {
            self.describe_output(capabilities);
        }
        self.configure_windows();
    }

    fn view(&self, id: PaneId) -> Option<usize> {
        self.views.iter().position(|view| view.id == id)
    }

    fn views_of(&mut self, window: WindowId) -> impl Iterator<Item = &mut View> {
        self.views
            .iter_mut()
            .filter(move |view| view.window == Some(window))
    }

    fn resolve(&self, show: Show) -> (Option<WindowId>, bool) {
        match show {
            Show::Window(id) => (Some(id), false),
            Show::Newest => (self.windows.last().map(|window| window.id), true),
            Show::Focused => self.active_window().map_or_else(
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

    /// Tell every window where it is drawn, now that the panes may have moved.
    ///
    /// The output keeps the set of surfaces it has entered, so a window that
    /// was not moved is not told again.
    fn sync_outputs(&self) {
        for index in 0..self.windows.len() {
            let id = self.windows[index].id;
            let shown = self.views.iter().any(|view| view.window == Some(id));
            let surface = self.windows[index].surface.wl_surface().clone();
            if shown {
                self.output.enter(&surface);
            } else {
                self.output.leave(&surface);
            }
        }
    }

    /// Whether a pane has something to draw and a frame to draw it in.
    pub fn any_due(&self) -> bool {
        self.views.iter().any(View::is_due)
    }

    /// Draw a frame for every pane that is due, and hand it to the server.
    ///
    /// A pane has one frame, and it is out while the presenter has it: what the
    /// pane shows is drawn when the frame comes back, so a scene that changes
    /// meanwhile waits rather than queues.
    pub fn present(&mut self) -> usize {
        let mut sent = 0;
        for index in 0..self.views.len() {
            if !self.views[index].is_due() {
                continue;
            }
            // Escapes go out once per frame at most, so a client that changes
            // its cursor on every mouse move cannot queue them up faster than
            // the terminal reads them.
            self.announce(index);
            self.views[index].relayout();
            if !self.views[index].scene_dirty {
                continue;
            }
            let pane = self.views[index].id;
            // The presenter may still have the frame: what is owed the
            // terminal goes anyway, and the scene waits for the frame.
            let Some(mut frame) = self.views[index].frame.take() else {
                continue;
            };
            self.draw(index, &mut frame);
            self.views[index].scene_dirty = false;
            // The client is told it may draw again whether or not anything of
            // its showing on the screen changed: a client that waited for this
            // callback would otherwise never draw again.
            self.frame_callbacks(index);
            sent += 1;
            tracing::trace!(pane = %pane, "composed a frame");
            let _ = self.events.send(Event::Frame { pane, frame });
        }
        sent
    }

    /// Drop what the popup manager remembers about popups that are gone.
    ///
    /// It keeps a tree per window that ever had one, and holds dead handles in
    /// it until this is called, so it is called once per frame.
    pub fn cleanup_popups(&mut self) {
        self.popup_manager.cleanup();
    }

    /// A frame the presenter is done with, to draw the next one into.
    pub fn recycle(&mut self, pane: PaneId, frame: Frame) {
        let Some(index) = self.view(pane) else {
            return;
        };
        self.views[index].frame = Some(frame);
        self.views[index].relayout();
    }

    /// Send the frame callbacks the window this pane shows waits for.
    ///
    /// A client driven by animation draws one frame per callback, so this paces
    /// it: it is told when a frame of its window is on its way to a terminal.
    fn frame_callbacks(&self, pane: usize) {
        let time = self.time();
        let Some(window) = self.views[pane].index(&self.windows) else {
            return;
        };
        let surface = self.windows[window].surface.wl_surface().clone();
        send_frame_callbacks(&surface, time);
        for (popup, _) in PopupManager::popups_for_surface(&surface) {
            send_frame_callbacks(popup.wl_surface(), time);
        }
    }

    /// Note that this pane's terminal is owed the title or the pointer shape.
    ///
    /// Called wherever one of them can have changed, and not per frame: what
    /// the terminal is owed is one escape, however often it changed.
    fn mark_escapes(&mut self, pane: usize) {
        if self.views[pane].escapes_dirty {
            return;
        }
        let wanted = self.wanted_escapes(pane);
        let view = &self.views[pane];
        if view.pointer_shape != wanted.0 || view.title.as_ref() != wanted.1.as_ref() {
            self.views[pane].escapes_dirty = true;
        }
    }

    /// What this pane's terminal should be told: the pointer shape to draw and
    /// what to call itself.
    ///
    /// The shape names come from the clients, through
    /// `wp_cursor_shape_manager_v1`, and the terminal draws the pointer itself.
    /// A cursor sent as an image cannot be described to the terminal, so the
    /// terminal's default pointer stands in. A window that has no title leaves
    /// the terminal's own title alone.
    fn wanted_escapes(&self, pane: usize) -> (Option<&'static str>, Option<String>) {
        let shape = match &self.cursor {
            CursorImageStatus::Named(icon) => Some(kitty::pointer_shape(*icon)),
            CursorImageStatus::Surface(_) | CursorImageStatus::Hidden => None,
        };
        let title = self.views[pane]
            .index(&self.windows)
            .and_then(|window| self.windows[window].title.clone());
        (shape, title)
    }

    /// Tell the terminal showing this pane what it is owed.
    fn announce(&mut self, pane: usize) {
        if !std::mem::replace(&mut self.views[pane].escapes_dirty, false) {
            return;
        }
        let pane_id = self.views[pane].id;
        let (shape, title) = self.wanted_escapes(pane);
        if self.views[pane].pointer_shape != shape {
            self.views[pane].pointer_shape = shape;
            let _ = self.events.send(Event::Pointer {
                pane: pane_id,
                shape,
            });
        }
        // A window with no title leaves the terminal's own title alone, but
        // what it is not told is recorded all the same: otherwise it would be
        // looked at again on every later change.
        let told = std::mem::replace(&mut self.views[pane].title, title.clone());
        if let Some(title) = title
            && told.as_ref() != Some(&title)
        {
            let _ = self.events.send(Event::Title {
                pane: pane_id,
                title,
            });
        }
    }

    /// This pane has nothing left to show, so its terminal is released.
    fn leave(&mut self, pane: usize, reason: Option<String>) {
        if std::mem::replace(&mut self.views[pane].leaving, true) {
            return;
        }
        let _ = self.events.send(Event::PaneDone {
            pane: self.views[pane].id,
            reason,
        });
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

    /// Give the keyboard to the window this pane shows, and remember that the
    /// pane was the one in use.
    pub fn interact(&mut self, pane: PaneId) {
        let Some(index) = self.view(pane) else {
            return;
        };
        if let Some(window) = self.views[index].index(&self.windows) {
            let changed = self.windows[window].last_interacted != Some(pane);
            self.windows[window].last_interacted = Some(pane);
            if self.active == Some(window) {
                if changed {
                    self.configure_windows();
                }
            } else {
                self.activate_index(window);
            }
        }
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
        let id = window.id;
        let (label, title) = (window.label.clone(), window.title.clone());
        let _ = self.events.send(Event::Named {
            window: id,
            label,
            title,
        });
        if renamed {
            for index in 0..self.views.len() {
                if self.views[index].window == Some(id) {
                    self.mark_escapes(index);
                }
            }
        }
    }

    fn configured_view(
        views: &[View],
        window: WindowId,
        last_interacted: Option<PaneId>,
    ) -> Option<&View> {
        last_interacted
            .and_then(|id| {
                views
                    .iter()
                    .find(|view| view.id == id && view.window == Some(window))
            })
            .or_else(|| views.iter().find(|view| view.window == Some(window)))
    }

    /// Configure each window from the pane that most recently interacted with
    /// it, falling back to the first attached pane.
    fn configure_windows(&self) {
        for (index, window) in self.windows.iter().enumerate() {
            let Some(pane) = Self::configured_view(&self.views, window.id, window.last_interacted)
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
                pane = %pane.id,
                ?size,
                fullscreen = window.fullscreen,
                active = self.active == Some(index),
                "configured"
            );
            let _ = window.surface.send_configure();
        }
    }
}

fn output_mode(capabilities: &Capabilities) -> Mode {
    Mode {
        size: (capabilities.pixels.0 as i32, capabilities.pixels.1 as i32).into(),
        // A terminal has no refresh rate. Frame callbacks pace clients here.
        refresh: REFRESH_MILLIHZ,
    }
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

#[derive(Debug, Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client: ClientId) {}

    fn disconnected(&self, _client: ClientId, _reason: DisconnectReason) {}
}

impl CompositorHandler for Compositor {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.surfaces
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client that reaches this point was inserted here with this data
        // attached.
        &client
            .get_data::<ClientState>()
            .expect("clients are always inserted with ClientState data")
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
        let first_pixels = self.snapshot(surface);
        // The panes that draw this surface draw again. A surface that is not a
        // toplevel is a sub-surface, a popup or a cursor: the first two are
        // drawn only by the panes whose window holds them, and a cursor by
        // none. So what a pane shows is what says whether it draws again.
        let window = self.index_of(surface);
        let shown = window.map(|index| self.windows[index].id);
        for index in 0..self.views.len() {
            let drawn_by = match shown {
                Some(id) => self.views[index].window == Some(id),
                None => self.views[index]
                    .index(&self.windows)
                    .is_some_and(|at| is_drawn(self.windows[at].surface.wl_surface(), surface)),
            };
            if drawn_by {
                self.views[index].scene_dirty = true;
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
                for pane in 0..self.views.len() {
                    if self.views[pane].window == Some(id) {
                        self.mark_escapes(pane);
                    }
                }
            }
        }
        tracing::debug!(id = ?surface.id(), "committed");
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        self.snapshots.remove(&surface.id());
    }
}

impl ShmHandler for Compositor {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl smithay::wayland::buffer::BufferHandler for Compositor {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl DmabufHandler for Compositor {
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
        match crate::wayland::buffer::dmabuf_readable(&dmabuf, self.gpu.as_mut()) {
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

impl XdgShellHandler for Compositor {
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
            last_interacted: None,
            fullscreen: false,
        });
        let (label, title) = (
            self.windows.last().and_then(|w| w.label.clone()),
            self.windows.last().and_then(|w| w.title.clone()),
        );
        let _ = self.events.send(Event::Named {
            window: id,
            label,
            title,
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
        let _ = self.events.send(Event::Closed { window: gone });
        // A pane that was given this window has nothing left to show, so its
        // terminal is released. A pane that follows the newest window shows
        // whatever is next.
        for index in 0..self.views.len() {
            if self.views[index].window != Some(gone) {
                continue;
            }
            self.views[index].window = None;
            self.views[index].scene_dirty = true;
            let follow = self.views[index].follow;
            if !follow {
                self.leave(index, None);
            }
        }
        if was_active {
            let keyboard = self.keyboard.clone();
            keyboard.set_focus(self, self.active_surface(), SERIAL_COUNTER.next_serial());
        }
        let active = self.active.map(|index| self.windows[index].id);
        let _ = self.events.send(Event::Focused { window: active });
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

impl OutputHandler for Compositor {}

impl SeatHandler for Compositor {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor = image;
        // Every pane's terminal is owed the new shape.
        for index in 0..self.views.len() {
            self.mark_escapes(index);
        }
    }
}

/// The cursor-shape protocol requires this. meowland has no tablets, so the
/// default is enough.
impl smithay::wayland::tablet_manager::TabletSeatHandler for Compositor {}

impl SelectionHandler for Compositor {
    type SelectionUserData = ();
}

impl ClientDndGrabHandler for Compositor {}

impl ServerDndGrabHandler for Compositor {}

impl DataDeviceHandler for Compositor {
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
) -> Option<(crate::wayland::gpu::Renderer, crate::dmabuf::RenderNode)> {
    for node in nodes {
        match crate::wayland::gpu::Renderer::new(&node.path) {
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
    gpu: Option<(&crate::wayland::gpu::Renderer, &crate::dmabuf::RenderNode)>,
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
    Ok(Some(
        state.create_global_with_default_feedback::<Compositor>(display, &feedback),
    ))
}

delegate_compositor!(Compositor);
delegate_shm!(Compositor);
delegate_dmabuf!(Compositor);
delegate_xdg_shell!(Compositor);
delegate_output!(Compositor);
delegate_seat!(Compositor);
delegate_data_device!(Compositor);
delegate_cursor_shape!(Compositor);
delegate_viewporter!(Compositor);

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities(pixels: (u32, u32)) -> Capabilities {
        Capabilities {
            cell: (10, 20),
            cells: (pixels.0 / 10, pixels.1 / 20),
            pixels,
            terminal: None,
            graphics: true,
            keyboard: true,
            pixel_mouse: true,
            shared_memory: false,
            patches: true,
        }
    }

    #[test]
    fn a_pane_that_asked_for_a_window_that_is_gone_is_released() {
        // The server checks the ID before it sends the attach, but the window
        // may close in between: a pane left holding it would show nothing
        // forever, because nothing is left to tell it that the window went.
        let display =
            smithay::reexports::wayland_server::Display::<Compositor>::new().expect("display");
        let (events, announced) = calloop::channel::channel();
        let mut state = Compositor::new(&display.handle(), &[], events).expect("compositor");
        let pane = PaneId::new(1);
        state.attach_view(
            pane,
            Show::Window(WindowId::new(7)),
            &capabilities((800, 600)),
        );

        let released = std::iter::from_fn(|| announced.try_recv().ok())
            .any(|event| matches!(event, Event::PaneDone { pane: done, reason: Some(_) } if done == pane));
        assert!(released, "the pane is released with a reason to show");

        // Following the newest window is not the same thing: there is nothing
        // to show yet, but something may still come.
        let (events, announced) = calloop::channel::channel();
        let mut state = Compositor::new(&display.handle(), &[], events).expect("compositor");
        let pane = PaneId::new(2);
        state.attach_view(pane, Show::Newest, &capabilities((800, 600)));
        assert!(
            std::iter::from_fn(|| announced.try_recv().ok())
                .all(|event| !matches!(event, Event::PaneDone { .. })),
            "a pane waiting for a window is not released"
        );
    }

    #[test]
    fn a_pane_whose_frame_is_out_is_still_told_what_its_terminal_owes() {
        // The frame is in the presenter's hands and the pane is owed an escape:
        // the escape goes out, the frame stays out, and nothing is drawn.
        let display =
            smithay::reexports::wayland_server::Display::<Compositor>::new().expect("display");
        let (events, announced) = calloop::channel::channel();
        let mut state = Compositor::new(&display.handle(), &[], events).expect("compositor");
        let pane = PaneId::new(1);
        state.attach_view(pane, Show::Newest, &capabilities((800, 600)));
        assert!(state.present() > 0, "the first frame draws the backdrop");
        assert!(
            state.views[0].frame.is_none(),
            "the presenter has the frame"
        );

        // Standing in for a cursor or title change while the frame is out.
        state.views[0].escapes_dirty = true;
        assert_eq!(state.present(), 0, "nothing to draw without a frame");
        assert!(state.views[0].frame.is_none(), "the frame is still out");
        assert!(
            !state.views[0].escapes_dirty,
            "what the terminal is owed went out"
        );
        let _ = announced;
    }

    #[test]
    fn configured_view_prefers_the_most_recently_interacted_pane() {
        let window = WindowId::new(1);
        let mut first = View::new(PaneId::new(1), &capabilities((800, 600)));
        first.window = Some(window);
        let mut second = View::new(PaneId::new(2), &capabilities((1200, 900)));
        second.window = Some(window);
        let views = vec![first, second];

        assert_eq!(
            Compositor::configured_view(&views, window, Some(PaneId::new(2))).map(|view| view.id),
            Some(PaneId::new(2))
        );
        assert_eq!(
            Compositor::configured_view(&views, window, Some(PaneId::new(3))).map(|view| view.id),
            Some(PaneId::new(1))
        );
    }
}
