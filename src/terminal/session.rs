//! Readiness scheduling and lifecycle for one terminal attachment.

use std::{
    fs::File,
    io,
    os::unix::net::UnixStream,
    path::Path,
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec, poll},
    io::Errno,
};

use super::{
    Clipboard, FALLBACK_CELL, KEYBOARD_FLAGS, Keyboard, PanePresentation, PaneStats, TerminalGuard,
    cursor_bytes, input, output, pane_pixels, send_clipboard_actions, send_deferred, send_event,
    terminal_probe, title_bytes, window_pixels,
};
use crate::{
    Error, Result,
    protocol::{self, Hello, PaneToServer, ServerToPane, Show},
    signals,
};

#[derive(Clone, Copy)]
enum Lifecycle {
    Handshaking { deadline: Instant },
    Active,
    Closed,
}

enum Ready {
    Disconnected,
    Io {
        input: bool,
        output: bool,
        server: bool,
    },
}

pub(super) struct TerminalSession {
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
    mode: TerminalGuard,
}

impl TerminalSession {
    pub(super) fn connect(socket: &Path, show: Show) -> Result<Option<Self>> {
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
        let mut mode = TerminalGuard::enter()?;
        let (probe, shared) = terminal_probe(&interrupted)?;
        if interrupted.interrupted() {
            mode.restore()?;
            return Ok(None);
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
        let input = input::Reader::new()?;
        let wakeup = signals::Wakeup::new(&[
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGWINCH,
        ])?;
        mode.enable_mouse()?;
        if let Some(previous) = probe.clipboard {
            mode.enable_clipboard(previous)?;
        }
        let cell = probe.cell_width.zip(probe.cell_height);
        let (width, height) = pane_pixels(
            probe.pixel_width.zip(probe.pixel_height),
            window_pixels(),
            (cols, rows),
            cell.unwrap_or(FALLBACK_CELL),
        );
        let mut tx = stream.try_clone()?;
        protocol::send(
            &mut tx,
            &PaneToServer::Hello(Hello {
                width,
                height,
                show,
            }),
        )?;
        PaneStats::capabilities(&probe, width, height);
        Ok(Some(Self {
            stream,
            tx,
            reader: protocol::MessageReader::default(),
            input,
            stdout: output::open()?,
            keyboard: Keyboard::default(),
            clipboard: Clipboard::new(probe.clipboard.is_some())?,
            presentation: PanePresentation::new(cell, shared),
            cell,
            lifecycle: Lifecycle::Handshaking {
                deadline: Instant::now() + Duration::from_secs(1),
            },
            wakeup,
            interrupted,
            mode,
        }))
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
            Lifecycle::Active | Lifecycle::Closed => None,
        };
        handshake
            .into_iter()
            .chain(self.keyboard.deadline())
            .chain(self.clipboard.deadline())
            .min()
    }

    fn wait_ready(&self) -> io::Result<Ready> {
        let pending_output = self.presentation.pending.pending();
        let mut fds = [
            PollFd::new(&self.wakeup, PollFlags::IN),
            PollFd::new(
                &self.input.fd,
                PollFlags::IN | PollFlags::HUP | PollFlags::ERR,
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
                    self.lifecycle = Lifecycle::Active;
                    self.clipboard.refresh(false);
                }
            }
            ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                return self.close(Some(&reason));
            }
            ServerToPane::ClipboardWrite(data) => {
                self.clipboard.copy(data, &mut self.presentation.pending);
            }
            ServerToPane::ClipboardRead {
                request,
                offer,
                mime,
                deadline,
            } => self.clipboard.read(request, offer, mime, deadline),
            ServerToPane::ClipboardCancel { request } => self.clipboard.cancel(request),
            ServerToPane::Title(title) => self.presentation.pending.append(&title_bytes(&title)),
            ServerToPane::Cursor(shape) => self
                .presentation
                .pending
                .append(&cursor_bytes(shape.as_deref())),
            ServerToPane::Frame {
                width,
                height,
                y,
                rgb,
            } => self
                .presentation
                .draw(width, height, y, rgb, &mut self.tx)?,
        }
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
        self.lifecycle = Lifecycle::Closed;
        if let Some(reason) = reason {
            self.mode.restore_with_message(reason)?;
        } else {
            self.mode.restore()?;
        }
        Ok(())
    }
}
