//! Readiness scheduling and lifecycle for one terminal attachment.

use std::{
    fs::File,
    io::{self, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use meowland_core::{Error, Result, signals};
use meowland_protocol::{self as protocol, Hello, PaneToServer, ServerToPane, Show};
use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};

use super::{
    Clipboard, FALLBACK_CELL, KEYBOARD_FLAGS, Keyboard, PanePresentation, PaneStats, TerminalGuard,
    cursor_bytes, derived_cell, input, output, pane_pixels, send_clipboard_actions, send_deferred,
    send_event, terminal_probe, title_bytes, window_pixels,
};

enum Lifecycle {
    Handshaking { deadline: Instant },
    Waiting,
    Active(TerminalGuard),
    Closed,
}

#[derive(Default)]
struct PendingWindow {
    title: Option<String>,
    cursor: Option<String>,
    clipboard: Option<meowland_protocol::clipboard::ClipboardData>,
}

enum Ready {
    Disconnected,
    Io {
        input: bool,
        output: bool,
        server: bool,
    },
}

pub struct TerminalSession {
    stream: UnixStream,
    tx: UnixStream,
    reader: protocol::MessageReader,
    input: input::Reader,
    stdout: File,
    keyboard: Keyboard,
    clipboard: Clipboard,
    presentation: PanePresentation,
    cell: Option<(u16, u16)>,
    lifecycle: Lifecycle,
    wakeup: signals::Wakeup,
    interrupted: signals::TerminationFlag,
    window: PendingWindow,
}

impl TerminalSession {
    pub(super) fn connect(socket: &Path, show: Show) -> Result<Self> {
        let stream = UnixStream::connect(socket).map_err(|error| {
            Error::io(
                format!(
                    "could not reach the pane socket {} (is the server running?)",
                    socket.display()
                ),
                error,
            )
        })?;
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let interrupted = signals::termination_flag()?;
        let input = input::Reader::new()?;
        let wakeup = signals::Wakeup::new(&[
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGWINCH,
        ])?;
        let pixels = window_pixels();
        let cell = derived_cell(pixels, (cols, rows));
        let (width, height) =
            pane_pixels(None, pixels, (cols, rows), cell.unwrap_or(FALLBACK_CELL));
        let mut tx = stream.try_clone()?;
        protocol::send(
            &mut tx,
            &PaneToServer::Hello(Hello {
                width,
                height,
                show,
            }),
        )?;
        Ok(Self {
            stream,
            tx,
            reader: protocol::MessageReader::default(),
            input,
            stdout: output::open()?,
            keyboard: Keyboard::default(),
            clipboard: Clipboard::new(false)?,
            presentation: PanePresentation::new(cell, None),
            cell,
            lifecycle: Lifecycle::Handshaking {
                deadline: Instant::now() + Duration::from_secs(1),
            },
            wakeup,
            interrupted,
            window: PendingWindow::default(),
        })
    }

    pub(super) fn run(mut self) -> Result<()> {
        while !matches!(self.lifecycle, Lifecycle::Closed) {
            if self.interrupted.interrupted() {
                return self.close(None);
            }
            self.prepare_pending_work()?;
            let ready = match self.wait_ready() {
                Ok(ready) => ready,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            };
            if self.interrupted.interrupted() {
                continue;
            }
            let Ready::Io {
                input,
                output,
                server,
            } = ready
            else {
                return self.close(None);
            };
            if server {
                self.receive_server_message()?;
            }
            if matches!(self.lifecycle, Lifecycle::Closed) {
                break;
            }
            if output {
                self.presentation.drain(&mut self.stdout, &mut self.tx)?;
            }
            if input {
                self.input.read_ready()?;
            }
            self.handle_terminal_events()?;
        }
        Ok(())
    }

    fn prepare_pending_work(&mut self) -> Result<()> {
        if matches!(self.lifecycle, Lifecycle::Handshaking { deadline } if Instant::now() >= deadline)
        {
            return Err(Error::PaneHandshakeTimeout);
        }
        if self.keyboard.deferred_expired() {
            send_deferred(&mut self.keyboard, &mut self.tx)?;
        }
        self.clipboard.poll(&mut self.presentation.pending);
        send_clipboard_actions(&mut self.clipboard, &mut self.keyboard, &mut self.tx)
    }

    fn next_deadline(&self) -> Option<Instant> {
        let handshake = match self.lifecycle {
            Lifecycle::Handshaking { deadline } => Some(deadline),
            Lifecycle::Waiting | Lifecycle::Active(_) | Lifecycle::Closed => None,
        };
        handshake
            .into_iter()
            .chain(self.keyboard.deadline())
            .chain(self.clipboard.deadline())
            .min()
    }

    fn wait_ready(&self) -> io::Result<Ready> {
        let active = matches!(self.lifecycle, Lifecycle::Active(_));
        let pending_output = active && self.presentation.pending.pending();
        let mut fds = [
            PollFd::new(&self.wakeup, PollFlags::IN),
            PollFd::new(
                &self.input.fd,
                PollFlags::HUP
                    | PollFlags::ERR
                    | if active {
                        PollFlags::IN
                    } else {
                        PollFlags::empty()
                    },
            ),
            PollFd::new(
                &self.stdout,
                PollFlags::HUP
                    | PollFlags::ERR
                    | if pending_output {
                        PollFlags::OUT
                    } else {
                        PollFlags::empty()
                    },
            ),
            PollFd::new(
                &self.stream,
                PollFlags::IN | PollFlags::HUP | PollFlags::ERR,
            ),
        ];
        let timeout = if self.input.pending() {
            Some(Duration::ZERO)
        } else {
            self.next_deadline()
                .map(|deadline| deadline.saturating_duration_since(Instant::now()))
        }
        .map(|wait| Timespec {
            tv_sec: wait.as_secs() as i64,
            tv_nsec: wait.subsec_nanos().into(),
        });
        let watched = if pending_output {
            &mut fds[..3]
        } else {
            &mut fds[..]
        };
        poll(watched, timeout.as_ref()).map_err(|error| {
            if error == Errno::INTR {
                io::ErrorKind::Interrupted.into()
            } else {
                io::Error::from(error)
            }
        })?;
        if fds[0].revents().contains(PollFlags::IN) {
            self.wakeup.drain();
        }
        if [1, 2].into_iter().any(|index| {
            fds[index]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR)
        }) {
            return Ok(Ready::Disconnected);
        }
        Ok(Ready::Io {
            input: fds[1].revents().contains(PollFlags::IN),
            output: fds[2].revents().contains(PollFlags::OUT),
            server: !pending_output && !fds[3].revents().is_empty(),
        })
    }

    fn receive_server_message(&mut self) -> Result<()> {
        match self.reader.try_recv::<ServerToPane>(&self.stream) {
            Ok(Some(message)) => self.handle_server_message(message),
            Ok(None) => Ok(()),
            Err(_) => self.close(Some("server disconnected")),
        }
    }

    fn handle_server_message(&mut self, message: ServerToPane) -> Result<()> {
        match message {
            ServerToPane::HelloOk => {
                if matches!(self.lifecycle, Lifecycle::Handshaking { .. }) {
                    self.lifecycle = Lifecycle::Waiting;
                    writeln!(
                        io::stderr(),
                        "meowland: waiting for a window (Ctrl+C to cancel)"
                    )?;
                }
            }
            ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                return self.close(Some(&reason));
            }
            ServerToPane::ClipboardWrite(data) => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.clipboard.copy(data, &mut self.presentation.pending);
                } else {
                    self.window.clipboard = Some(data);
                }
            }
            ServerToPane::ClipboardRead {
                request,
                offer,
                mime,
                deadline,
            } => self.clipboard.read(request, offer, mime, deadline),
            ServerToPane::ClipboardCancel { request } => self.clipboard.cancel(request),
            ServerToPane::Title(title) => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.presentation.pending.append(&title_bytes(&title));
                } else {
                    self.window.title = Some(title);
                }
            }
            ServerToPane::Cursor(shape) => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.presentation
                        .pending
                        .append(&cursor_bytes(shape.as_deref()));
                } else {
                    self.window.cursor = shape;
                }
            }
            ServerToPane::Frame {
                width,
                height,
                y,
                rgb,
            } => {
                let starting = !matches!(self.lifecycle, Lifecycle::Active(_));
                if starting {
                    self.start_presentation()?;
                    if matches!(self.lifecycle, Lifecycle::Closed) {
                        return Ok(());
                    }
                }
                self.presentation
                    .draw(width, height, y, rgb, &mut self.tx)?;
                if starting {
                    let window = std::mem::take(&mut self.window);
                    if let Some(title) = window.title {
                        self.presentation.pending.append(&title_bytes(&title));
                    }
                    self.presentation
                        .pending
                        .append(&cursor_bytes(window.cursor.as_deref()));
                    if let Some(data) = window.clipboard {
                        self.clipboard.copy(data, &mut self.presentation.pending);
                    }
                }
            }
        }
        Ok(())
    }

    fn start_presentation(&mut self) -> Result<()> {
        let mut mode = TerminalGuard::enter()?;
        let (probe, shared) = terminal_probe(&self.interrupted)?;
        if self.interrupted.interrupted() {
            mode.restore()?;
            return self.close(None);
        }
        if !probe.graphics {
            return Err(Error::GraphicsUnsupported);
        }
        if !probe.sgr_pixels {
            return Err(Error::PixelMouseUnsupported);
        }
        if probe
            .keyboard_flags
            .is_none_or(|flags| !flags.contains(KEYBOARD_FLAGS))
        {
            return Err(Error::KeyboardUnsupported);
        }
        mode.enable_mouse()?;
        if let Some(previous) = probe.clipboard {
            mode.enable_clipboard(previous)?;
        }
        self.cell = probe.cell_width.zip(probe.cell_height).or(self.cell);
        let (width, height) = pane_pixels(
            probe.pixel_width.zip(probe.pixel_height),
            window_pixels(),
            crossterm::terminal::size().unwrap_or((80, 24)),
            self.cell.unwrap_or(FALLBACK_CELL),
        );
        self.presentation = PanePresentation::new(self.cell, shared);
        self.clipboard = Clipboard::new(probe.clipboard.is_some())?;
        let _ = self.clipboard.refresh(false);
        protocol::send(&mut self.tx, &PaneToServer::Resize { width, height })?;
        PaneStats::capabilities(&probe, width, height);
        self.lifecycle = Lifecycle::Active(mode);
        Ok(())
    }

    fn handle_terminal_events(&mut self) -> Result<()> {
        for _ in 0..64 {
            if self.interrupted.interrupted() {
                break;
            }
            let Some(event) = self.input.next() else {
                break;
            };
            if !matches!(self.lifecycle, Lifecycle::Active(_))
                && !matches!(event, input::Event::Resize)
            {
                continue;
            }
            send_event(
                event,
                &mut self.tx,
                &mut self.presentation.presenter,
                &mut self.cell,
                &mut self.clipboard,
                &mut self.keyboard,
            )?;
            send_clipboard_actions(&mut self.clipboard, &mut self.keyboard, &mut self.tx)?;
        }
        Ok(())
    }

    fn close(&mut self, reason: Option<&str>) -> Result<()> {
        if let Lifecycle::Active(mut mode) =
            std::mem::replace(&mut self.lifecycle, Lifecycle::Closed)
        {
            mode.restore()?;
        }
        if let Some(reason) = reason {
            let clean = protocol::sanitize_with_limit(reason, 512);
            writeln!(io::stdout(), "meowland: {clean}")?;
        }
        Ok(())
    }
}
