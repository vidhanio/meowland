//! The Wayland side of meowland.
//!
//! The compositor deliberately has a small message interface.  Nothing in the
//! protocol dispatch path writes to a pane: a completed image is sent as an
//! owned `Vec`, so a slow presenter cannot stall Wayland clients.
#![allow(
    clippy::type_complexity,
    clippy::struct_field_names,
    clippy::needless_pass_by_value,
    clippy::collection_is_never_read,
    clippy::too_many_lines,
    clippy::manual_let_else,
    clippy::match_wildcard_for_single_variants,
    clippy::collapsible_if,
    clippy::redundant_clone,
    clippy::semicolon_if_nothing_returned,
    clippy::default_trait_access
)]

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    sync::mpsc,
    thread,
    time::Duration,
};

use crate::protocol::{Input, Show, WindowInfo};

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState, Keycode},
    delegate_compositor, delegate_output, delegate_seat, delegate_shm, delegate_xdg_shell,
    input::{
        Seat, SeatHandler, SeatState,
        keyboard::{FilterResult, KeyboardHandle},
        pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle},
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::wayland_server::{
        Client, Display, ListeningSocket,
        backend::{ClientData, ClientId, DisconnectReason},
        protocol::{wl_buffer, wl_shm, wl_surface::WlSurface},
    },
    utils::{SERIAL_COUNTER, Size},
    wayland::output::OutputHandler,
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
            with_states,
        },
        output::OutputManagerState,
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState, with_buffer_contents},
    },
};

#[derive(Clone, Debug)]
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
    },
    CloseAll,
    CloseShown {
        pane: u64,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum Event {
    WindowUp(WindowInfo),
    WindowDown(u64),
    Focus(u64),
    Frame {
        pane: u64,
        width: u32,
        height: u32,
        rgb: Vec<u8>,
    },
    Release {
        pane: u64,
        reason: String,
    },
}

pub fn spawn() -> std::io::Result<(
    mpsc::Sender<Command>,
    mpsc::Receiver<Event>,
    String,
    thread::JoinHandle<()>,
)> {
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

struct State {
    compositor: CompositorState,
    shm: ShmState,
    xdg: XdgShellState,
    seat_state: SeatState<Self>,
    seat: Seat<Self>,
    keyboard: Option<KeyboardHandle<Self>>,
    pointer: Option<PointerHandle<Self>>,
    _output_manager: OutputManagerState,
    output: Output,
    windows: HashMap<u64, ToplevelSurface>,
    metadata: HashMap<u64, (String, String)>,
    announced: HashSet<u64>,
    snapshots: HashMap<u64, (u32, u32, Vec<u8>)>,
    dirty: HashSet<u64>,
    ids: HashMap<WlSurface, u64>,
    next_id: u64,
    panes: HashMap<u64, (u64, u32, u32, bool)>,
    following: HashSet<u64>,
    focused: Option<u64>,
    events: mpsc::Sender<Event>,
    started: std::time::Instant,
    shutdown: bool,
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
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
    #[allow(unsafe_code)]
    fn commit(&mut self, surface: &WlSurface) {
        let id = match self.ids.get(surface).copied() {
            Some(id) => id,
            None => return,
        };
        let buffer = with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .take()
                .and_then(|b| match b {
                    smithay::wayland::compositor::BufferAssignment::NewBuffer(b) => Some(b),
                    _ => None,
                })
        });
        let Some(buffer) = buffer else { return };
        let snapshot = with_buffer_contents(&buffer, |ptr, len, data| {
            if !matches!(
                data.format,
                wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888
            ) || data.width <= 0
                || data.height <= 0
                || data.stride <= 0
            {
                return None;
            }
            let stride = data.stride as usize;
            let w = data.width as usize;
            let h = data.height as usize;
            if w > 8192
                || h > 8192
                || w.checked_mul(h).is_none_or(|pixels| pixels > 16_000_000)
                || w.checked_mul(4).is_none_or(|minimum| stride < minimum)
                || stride.checked_mul(h).is_none_or(|needed| len < needed)
            {
                return None;
            }
            let src = unsafe { std::slice::from_raw_parts(ptr, len) };
            let mut out = vec![0; w * h * 3];
            for y in 0..h {
                for x in 0..w {
                    let p = y * stride + x * 4;
                    let q = (y * w + x) * 3;
                    out[q] = src[p + 2];
                    out[q + 1] = src[p + 1];
                    out[q + 2] = src[p];
                }
            }
            Some((w as u32, h as u32, out))
        });
        buffer.release();
        let Ok(Some((width, height, rgb))) = snapshot else {
            return;
        };
        if self.announced.insert(id) {
            let (app_id, title) = self.metadata.get(&id).cloned().unwrap_or_default();
            let _ = self.events.send(Event::WindowUp(WindowInfo {
                id,
                app_id,
                title,
                active: true,
            }));
            let _ = self.events.send(Event::Focus(id));
            self.focused = Some(id);
            for pane in &self.following {
                if let Some(value) = self.panes.get_mut(pane) {
                    value.0 = id;
                }
            }
        }
        self.snapshots.insert(id, (width, height, rgb.clone()));
        for (&pane, value) in &mut self.panes {
            let (window, pw, ph, pending) = *value;
            if window != id {
                continue;
            }
            if pending {
                self.dirty.insert(pane);
            } else {
                let target_w = pw;
                let target_h = ph;
                let fitted = fit_rgb(&rgb, width, height, target_w, target_h);
                let _ = self.events.send(Event::Frame {
                    pane,
                    width: target_w,
                    height: target_h,
                    rgb: fitted,
                });
                value.3 = true;
            }
        }
    }
}

fn fit_rgb(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0; dw as usize * dh as usize * 3];
    let copy_w = sw.min(dw) as usize;
    let copy_h = sh.min(dh) as usize;
    for y in 0..copy_h {
        let from = y * sw as usize * 3;
        let to = y * dw as usize * 3;
        out[to..to + copy_w * 3].copy_from_slice(&src[from..from + copy_w * 3]);
    }
    out
}
impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm
    }
}
impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg
    }
    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let id = self.next_id;
        self.next_id += 1;
        let pane_size = self.following.iter().filter_map(|pane| self.panes.get(pane)).map(|pane| (pane.1, pane.2)).next();
        surface.with_pending_state(|s| {
            s.states.set(xdg_toplevel::State::Activated);
            if let Some((width, height)) = pane_size {
                s.size = Some((width as i32, height as i32).into());
            }
        });
        surface.send_configure();
        self.ids.insert(surface.wl_surface().clone(), id);
        self.windows.insert(id, surface.clone());
        self.metadata.insert(id, metadata(&surface));
    }
    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }
    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(id) = self.ids.remove(surface.wl_surface()) {
            self.windows.remove(&id);
            self.metadata.remove(&id);
            self.panes.retain(|pane, (window, _, _, _)| {
                if *window == id {
                    if self.following.contains(pane) {
                        *window = 0;
                        true
                    } else {
                        let _ = self.events.send(Event::Release {
                            pane: *pane,
                            reason: "window closed".into(),
                        });
                        false
                    }
                } else {
                    true
                }
            });
            self.announced.remove(&id);
            self.snapshots.remove(&id);
            if self.focused == Some(id) {
                self.focused = self.announced.iter().max().copied();
                if let Some(focused) = self.focused {
                    let _ = self.events.send(Event::Focus(focused));
                }
            }
            let _ = self.events.send(Event::WindowDown(id));
        }
    }
    fn title_changed(&mut self, surface: ToplevelSurface) {
        if let Some(&id) = self.ids.get(surface.wl_surface()) {
            let (app_id, title) = metadata(&surface);
            self.metadata.insert(id, (app_id.clone(), title.clone()));
            if self.announced.contains(&id) {
                let _ = self.events.send(Event::WindowUp(WindowInfo {
                    id,
                    app_id,
                    title,
                    active: self.focused == Some(id),
                }));
            }
        }
    }
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }
}

fn metadata(surface: &ToplevelSurface) -> (String, String) {
    with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .map(|data| {
                let data = data.lock().unwrap();
                (
                    data.app_id.clone().unwrap_or_default(),
                    data.title.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_default()
    })
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}
    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}
impl OutputHandler for State {}

delegate_compositor!(State);
delegate_shm!(State);
delegate_xdg_shell!(State);
delegate_seat!(State);
delegate_output!(State);

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
    let dh = display.handle();
    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "meowland");
    let pointer = Some(seat.add_pointer());
    let output_manager = OutputManagerState::new();
    let output = Output::new(
        "meowland".into(),
        PhysicalProperties {
            size: (300, 200).into(),
            subpixel: Subpixel::Unknown,
            make: "meowland".into(),
            model: "terminal".into(),
        },
    );
    output.create_global::<State>(&dh);
    output.change_current_state(
        Some(Mode {
            size: (1920, 1080).into(),
            refresh: 60000,
        }),
        None,
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    output.set_preferred(Mode {
        size: (1920, 1080).into(),
        refresh: 60000,
    });
    let mut state = State {
        compositor: CompositorState::new::<State>(&dh),
        shm: ShmState::new::<State>(&dh, vec![]),
        xdg: XdgShellState::new::<State>(&dh),
        seat_state,
        seat,
        keyboard: None,
        pointer,
        _output_manager: output_manager,
        output,
        windows: HashMap::new(),
        metadata: HashMap::new(),
        announced: HashSet::new(),
        snapshots: HashMap::new(),
        dirty: HashSet::new(),
        ids: HashMap::new(),
        next_id: 1,
        panes: HashMap::new(),
        following: HashSet::new(),
        focused: None,
        events,
        started: std::time::Instant::now(),
        shutdown: false,
    };
    state.keyboard = state.seat.add_keyboard(Default::default(), 25, 600).ok();
    let Ok(listener) = ListeningSocket::bind(name) else {
        let _ = ready.send(Err(std::io::Error::other("could not bind Wayland socket")));
        return;
    };
    let _ = ready.send(Ok(()));
    let mut clients = Vec::new();
    while !state.shutdown {
        while let Ok(cmd) = rx.try_recv() {
            handle_command(&mut state, cmd);
        }
        if let Ok(Some(stream)) = listener.accept() {
            if let Ok(client) = display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                clients.push(client);
            }
        }
        let _ = display.dispatch_clients(&mut state);
        let _ = display.flush_clients();
        thread::sleep(Duration::from_millis(2));
    }
}

fn handle_command(state: &mut State, command: Command) {
    match command {
        Command::Attach {
            pane,
            show,
            width,
            height,
        } => {
            if matches!(show, Show::Newest) {
                state.following.insert(pane);
            }
            let id = match show {
                Show::Id(id) => id,
                Show::Newest => state.announced.iter().max().copied().unwrap_or(0),
                Show::Focused => state.focused.unwrap_or(0),
            };
            if state.windows.contains_key(&id) {
                state.panes.insert(pane, (id, width, height, false));
                if state.panes.keys().min() == Some(&pane) {
                    configure_output(state, width, height);
                }
                if let Some(&(sw, sh, ref rgb)) = state.snapshots.get(&id) {
                    let _ = state.events.send(Event::Frame {
                        pane,
                        width,
                        height,
                        rgb: fit_rgb(rgb, sw, sh, width, height),
                    });
                    if let Some(v) = state.panes.get_mut(&pane) {
                        v.3 = true;
                    }
                }
                if let Some(surface) = state.windows.get(&id) {
                    surface.with_pending_state(|pending| {
                        pending.size = Some(Size::from((width as i32, height as i32)))
                    });
                    surface.send_configure();
                }
            } else if matches!(show, Show::Newest | Show::Focused) {
                state.panes.insert(pane, (0, width, height, false));
                if state.panes.keys().min() == Some(&pane) {
                    configure_output(state, width, height);
                }
                state.following.insert(pane);
            } else {
                let _ = state.events.send(Event::Release {
                    pane,
                    reason: "no such window".into(),
                });
            }
        }
        Command::Detach { pane } => {
            let first = state.panes.keys().min() == Some(&pane);
            state.panes.remove(&pane);
            state.dirty.remove(&pane);
            state.following.remove(&pane);
            if first
                && let Some((_, width, height, _)) = state
                    .panes
                    .iter()
                    .min_by_key(|(id, _)| *id)
                    .map(|(_, value)| *value)
            {
                configure_output(state, width, height);
            }
        }
        Command::Resize {
            pane,
            width,
            height,
        } => {
            if let Some(v) = state.panes.get_mut(&pane) {
                v.1 = width;
                v.2 = height;
                if v.3 {
                    state.dirty.insert(pane);
                } else if let Some(&(sw, sh, ref rgb)) = state.snapshots.get(&v.0) {
                    let _ = state.events.send(Event::Frame {
                        pane,
                        width,
                        height,
                        rgb: fit_rgb(rgb, sw, sh, width, height),
                    });
                    v.3 = true;
                }
                if let Some(surface) = state.windows.get(&v.0) {
                    surface.with_pending_state(|pending| {
                        pending.size = Some(Size::from((width as i32, height as i32)))
                    });
                    surface.send_configure();
                }
            }
            if state.panes.keys().min() == Some(&pane) {
                configure_output(state, width, height);
            }
        }
        Command::Ack { pane } => {
            if let Some(surface) = state
                .panes
                .get(&pane)
                .and_then(|value| state.windows.get(&value.0))
                .map(|window| window.wl_surface().clone())
            {
                with_states(&surface, |states| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(state.started.elapsed().as_millis() as u32);
                    }
                });
            }
            if let Some(v) = state.panes.get_mut(&pane) {
                v.3 = false;
                if state.dirty.remove(&pane) {
                    if let Some(&(sw, sh, ref rgb)) = state.snapshots.get(&v.0) {
                        let (width, height) = (v.1, v.2);
                        let _ = state.events.send(Event::Frame {
                            pane,
                            width,
                            height,
                            rgb: fit_rgb(rgb, sw, sh, width, height),
                        });
                        v.3 = true;
                    }
                }
            }
        }
        Command::Input { pane, event } => {
            if let Some(&(window, _, _, _)) = state.panes.get(&pane) {
                if let Some(surface) = state.windows.get(&window).map(|w| w.wl_surface().clone()) {
                    if state.focused != Some(window) {
                        state.focused = Some(window);
                        let _ = state.events.send(Event::Focus(window));
                    }
                    if let Some(keyboard) = state.keyboard.take() {
                        keyboard.set_focus(state, Some(surface.clone()), 0.into());
                        if let Input::Key {
                            code,
                            pressed,
                            modifiers,
                        } = event
                        {
                            inject_key(&keyboard, state, code, pressed, modifiers);
                        }
                        state.keyboard = Some(keyboard);
                    }
                    if let Some(pointer) = state.pointer.take() {
                        if let Input::Pointer {
                            x,
                            y,
                            button,
                            pressed,
                            scroll,
                        } = event
                        {
                            let location = smithay::utils::Point::from((x, y));
                            pointer.motion(
                                state,
                                Some((surface.clone(), location)),
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
                        }
                        state.pointer = Some(pointer);
                    }
                }
            }
        }
        Command::CloseAll => {
            for surface in state.windows.values() {
                surface.send_close();
            }
        }
        Command::CloseShown { pane } => {
            match state.panes.get(&pane).map(|v| v.0).filter(|id| *id != 0) {
                Some(window) => {
                    if let Some(surface) = state.windows.get(&window) {
                        surface.send_close();
                    }
                }
                None => {
                    let _ = state.events.send(Event::Release {
                        pane,
                        reason: "empty pane".into(),
                    });
                }
            }
        }
        Command::Shutdown => {
            for surface in state.windows.values() {
                surface.send_close();
            }
            state.shutdown = true;
        }
    }
}

fn inject_key(
    keyboard: &KeyboardHandle<State>,
    state: &mut State,
    code: u16,
    pressed: bool,
    modifiers: u8,
) {
    const MODIFIER_KEYS: [(u8, u16); 4] = [(1, 42), (2, 29), (4, 56), (8, 125)];
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

fn configure_output(state: &State, width: u32, height: u32) {
    let mode = Mode {
        size: (width as i32, height as i32).into(),
        refresh: 60_000,
    };
    state
        .output
        .change_current_state(Some(mode), None, None, None);
    state.output.set_preferred(mode);
}
