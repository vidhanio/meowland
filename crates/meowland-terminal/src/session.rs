//! Async scheduling and explicit lifecycle for one terminal attachment.

use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

use meowland_core::{Error, Result, asynchronous::Descriptor, signals};
use meowland_protocol::{
    self as protocol, Hello, PaneToServer, ServerToPane, Show,
    asynchronous::{Outgoing, Reader},
};
use tokio::{
    io::AsyncReadExt,
    net::unix::{OwnedReadHalf, OwnedWriteHalf},
};

use super::{
    Clipboard, ClipboardProtocol, FALLBACK_CELL, KEYBOARD_FLAGS, Keyboard, PanePresentation,
    PaneStats, TerminalGuard, cursor_bytes, derived_cell, input, output, pane_pixels,
    send_clipboard_actions, send_deferred, send_event, terminal_probe, title_bytes, window_pixels,
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
    Signal(io::Result<usize>),
    Deadline,
    Server(io::Result<ServerToPane>),
    Output(Result<()>),
    Transmit(io::Result<()>),
    Input(io::Result<()>),
    BufferedInput,
}

pub struct TerminalSession {
    receiver: Reader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    tx: Outgoing,
    input: input::Reader,
    stdout: Descriptor,
    keyboard: Keyboard,
    clipboard: Clipboard,
    presentation: PanePresentation,
    cell: Option<(u16, u16)>,
    lifecycle: Lifecycle,
    wakeup: Descriptor,
    _signals: signals::Wakeup,
    interrupted: signals::TerminationFlag,
    window: PendingWindow,
}

impl TerminalSession {
    pub fn new(
        receiver: Reader<OwnedReadHalf>,
        writer: OwnedWriteHalf,
        show: Show,
    ) -> Result<Self> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        let interrupted = signals::termination_flag()?;
        let input = input::Reader::new()?;
        let signals = signals::Wakeup::new(&[
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
            signal_hook::consts::SIGWINCH,
        ])?;
        let wakeup = Descriptor::new(rustix::io::dup(&signals).map_err(io::Error::from)?)?;
        let pixels = window_pixels();
        let cell = derived_cell(pixels, (cols, rows));
        let (width, height) =
            pane_pixels(None, pixels, (cols, rows), cell.unwrap_or(FALLBACK_CELL));
        let mut tx = Outgoing::default();
        tx.send(&PaneToServer::Hello(Hello {
            width,
            height,
            show,
        }))?;
        Ok(Self {
            receiver,
            writer,
            tx,
            input,
            stdout: Descriptor::new(output::open()?.into())?,
            keyboard: Keyboard::default(),
            clipboard: Clipboard::new(ClipboardProtocol::Osc52)?,
            presentation: PanePresentation::new(cell, None),
            cell,
            lifecycle: Lifecycle::Handshaking {
                deadline: Instant::now() + Duration::from_secs(1),
            },
            wakeup,
            _signals: signals,
            interrupted,
            window: PendingWindow::default(),
        })
    }

    pub async fn run(mut self) -> Result<()> {
        while !matches!(self.lifecycle, Lifecycle::Closed) {
            if self.interrupted.interrupted() {
                return self.close(None);
            }
            self.prepare_pending_work()?;
            let active = matches!(self.lifecycle, Lifecycle::Active(_));
            let pending_output = active && self.presentation.pending.pending();
            let pending_input = self.input.pending();
            let deadline = self.next_deadline();
            let mut signal = [0; 256];
            let ready = tokio::select! {
                result = self.wakeup.read(&mut signal) => Ready::Signal(result),
                () = wait_deadline(deadline) => Ready::Deadline,
                result = self.presentation.drain(&mut self.stdout), if pending_output => Ready::Output(result),
                result = self.receiver.read(), if !pending_output && !self.tx.pending() => Ready::Server(result),
                result = self.tx.write_ready(&mut self.writer), if self.tx.pending() => Ready::Transmit(result),
                result = self.input.read_ready(), if active && !pending_input && !self.tx.pending() => Ready::Input(result),
                () = tokio::task::yield_now(), if pending_input && !self.tx.pending() => Ready::BufferedInput,
            };
            if self.interrupted.interrupted() {
                continue;
            }
            match ready {
                Ready::Signal(result) => {
                    if result? == 0 {
                        return self.close(None);
                    }
                }
                Ready::Server(result) => self.handle_server_message(result?).await?,
                Ready::Output(result) => {
                    result?;
                    self.presentation.finish(&mut self.tx)?;
                }
                Ready::Transmit(Err(error)) => return self.finish_after_write_error(error).await,
                Ready::Input(result) => {
                    if result.is_err() {
                        return self.close(None);
                    }
                }
                Ready::Transmit(Ok(())) | Ready::Deadline | Ready::BufferedInput => {}
            }
            if !matches!(self.lifecycle, Lifecycle::Closed) && !self.tx.pending() {
                self.handle_terminal_events()?;
            }
        }
        Ok(())
    }

    async fn finish_after_write_error(&mut self, error: io::Error) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if self.interrupted.interrupted() {
                return self.close(None);
            }
            let mut signal = [0; 256];
            tokio::select! {
                () = tokio::time::sleep_until(deadline.into()) => return Err(error.into()),
                result = self.wakeup.read(&mut signal) => { result?; }
                result = self.receiver.read() => match result {
                    Ok(ServerToPane::Release(reason) | ServerToPane::Reject(reason)) => return self.close(Some(&reason)),
                    Err(_) => return Err(error.into()),
                    Ok(_) => {}
                }
            }
        }
    }

    fn prepare_pending_work(&mut self) -> Result<()> {
        if matches!(self.lifecycle, Lifecycle::Handshaking { deadline } if Instant::now() >= deadline)
        {
            return Err(Error::PaneHandshakeTimeout);
        }
        if self.keyboard.deferred_expired() {
            send_deferred(&mut self.keyboard, &mut self.tx)?;
        }
        self.clipboard.advance(self.presentation.pending.buffer());
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

    async fn handle_server_message(&mut self, message: ServerToPane) -> Result<()> {
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
            ServerToPane::Bell => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.presentation.pending.append(b"\x07");
                }
            }
            ServerToPane::ClipboardWrite(data) => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.clipboard
                        .copy(data, self.presentation.pending.buffer());
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
                    self.start_presentation().await?;
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
                        self.clipboard
                            .copy(data, self.presentation.pending.buffer());
                    }
                }
            }
        }
        Ok(())
    }

    async fn start_presentation(&mut self) -> Result<()> {
        let mut mode = TerminalGuard::enter()?;
        let (probe, shared) = terminal_probe(
            &mut self.input,
            &mut self.stdout,
            &mut self.wakeup,
            &self.interrupted,
        )
        .await?;
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
        self.clipboard = Clipboard::new(if probe.clipboard.is_some() {
            ClipboardProtocol::Kitty
        } else {
            ClipboardProtocol::Osc52
        })?;
        let _ = self.clipboard.refresh(false);
        self.tx.send(&PaneToServer::Resize { width, height })?;
        PaneStats::capabilities(&probe, width, height);
        self.lifecycle = Lifecycle::Active(mode);
        Ok(())
    }

    fn handle_terminal_events(&mut self) -> Result<()> {
        for _ in 0..64 {
            if self.interrupted.interrupted() {
                break;
            }
            let Some(event) = self.input.next_event() else {
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

async fn wait_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        tokio::time::sleep_until(deadline.into()).await;
    } else {
        std::future::pending::<()>().await;
    }
}
