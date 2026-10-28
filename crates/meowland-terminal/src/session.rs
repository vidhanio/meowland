//! Async scheduling and explicit lifecycle for one terminal attachment.

use std::{
    io::{self, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use calloop::{LoopHandle, io::Async, signals::Signal as UnixSignal};
use futures::FutureExt;
use meowland_core::{
    Error, Result,
    asynchronous::{Descriptor, sleep_until, when},
    future::yield_now,
    signals,
};
use meowland_input::{
    keyboard::Keyboard,
    {self as input},
};
use meowland_protocol::{
    self as protocol, FrameTransport, Hello, PaneAction, PaneToServer, ServerToPane, Show,
    asynchronous::{Outgoing, Reader},
    clipboard::Selection,
};
use meowland_terminal_clipboard::{Clipboard, Protocol as ClipboardProtocol, Refresh};
type Socket = Async<'static, UnixStream>;

use super::{
    input_gate::InputGate,
    output,
    presentation::PanePresentation,
    probe::terminal_probe,
    terminal::{self, FALLBACK_CELL, KEYBOARD_FLAGS, TerminalGuard, cursor_bytes, title_bytes},
};

mod events;

const EVENTS_PER_TURN: usize = 64;

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
    primary: Option<meowland_protocol::clipboard::ClipboardData>,
}

enum Ready {
    Terminate,
    Resize,
    Deadline,
    Server(io::Result<ServerToPane>),
    Output(Result<()>),
    Transmit(io::Result<()>),
    Input(io::Result<()>),
    BufferedInput,
}

pub(crate) struct TerminalSession {
    receiver: Reader<Socket>,
    writer: Socket,
    handle: LoopHandle<'static, ()>,
    tx: Outgoing,
    input: input::Reader,
    stdout: Descriptor,
    keyboard: Keyboard,
    input_gate: InputGate,
    clipboard: Clipboard,
    presentation: PanePresentation,
    cell: Option<(u16, u16)>,
    lifecycle: Lifecycle,
    termination: signals::Termination,
    resize: signals::Signal,
    window: PendingWindow,
}

impl TerminalSession {
    pub(crate) fn new(
        receiver: Reader<Socket>,
        writer: Socket,
        show: Show,
        handle: LoopHandle<'static, ()>,
    ) -> Result<Self> {
        let cells = terminal::cells();
        let pixels = terminal::window_pixels();
        let cell = terminal::derived_cell(pixels, cells);
        let (width, height) =
            terminal::pane_pixels(None, pixels, cells, cell.unwrap_or(FALLBACK_CELL));
        let termination = signals::Termination::new(&handle)?;
        let resize = signals::Signal::new(&handle, &[UnixSignal::SIGWINCH])?;
        let input = input::Reader::new(&handle)?;
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
            stdout: Descriptor::new(output::open()?.into(), &handle)?,
            handle,
            keyboard: Keyboard::default(),
            input_gate: InputGate::default(),
            clipboard: Clipboard::new(ClipboardProtocol::Osc52)?,
            presentation: PanePresentation::new(cell, FrameTransport::Inline),
            cell,
            lifecycle: Lifecycle::Handshaking {
                deadline: Instant::now() + protocol::REQUEST_TIMEOUT,
            },
            termination,
            resize,
            window: PendingWindow::default(),
        })
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    pub(crate) async fn run(mut self) -> Result<()> {
        while !matches!(self.lifecycle, Lifecycle::Closed) {
            self.prepare_pending_work()?;
            let active = matches!(self.lifecycle, Lifecycle::Active(_));
            let pending_output = active && self.presentation.pending();
            let pending_input = self.input.pending();
            let deadline = self.next_deadline();
            let pending_tx = self.tx.pending();
            let ready = futures::select! {
                _ = self.termination.recv().fuse() => Ready::Terminate,
                _ = self.resize.recv().fuse() => Ready::Resize,
                result = sleep_until(&self.handle, deadline).fuse() => { result?; Ready::Deadline },
                result = when(pending_output.then(|| self.presentation.drain(&mut self.stdout))).fuse() => Ready::Output(result),
                result = when((!pending_output && !pending_tx).then(|| self.receiver.read())).fuse() => Ready::Server(result),
                result = when(pending_tx.then(|| self.tx.write_ready(&mut self.writer))).fuse() => Ready::Transmit(result),
                result = when((active && !pending_input && !pending_tx).then(|| self.input.read_ready())).fuse() => Ready::Input(result),
                () = when((pending_input && !pending_tx).then(yield_now)).fuse() => Ready::BufferedInput,
            };
            match ready {
                Ready::Terminate => return self.close(None),
                Ready::Resize => self.send_event(input::Event::Resize)?,
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
            yield_now().await;
        }
        Ok(())
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn finish_after_write_error(&mut self, error: io::Error) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            futures::select! {
                _ = self.termination.recv().fuse() => return self.close(None),
                result = sleep_until(&self.handle, Some(deadline)).fuse() => { result?; return Err(error.into()); },
                result = self.receiver.read::<ServerToPane>().fuse() => match result {
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
        if self.input_gate.expired() {
            self.input_gate.flush(&mut self.tx)?;
        }
        self.presentation.advance(&mut self.tx)?;
        self.clipboard.advance(self.presentation.buffer());
        self.send_clipboard_actions()
    }

    fn next_deadline(&self) -> Option<Instant> {
        let handshake = match self.lifecycle {
            Lifecycle::Handshaking { deadline } => Some(deadline),
            Lifecycle::Waiting | Lifecycle::Active(_) | Lifecycle::Closed => None,
        };
        handshake
            .into_iter()
            .chain(self.input_gate.deadline())
            .chain(self.presentation.deadline())
            .chain(self.clipboard.deadline())
            .min()
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
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
                    self.presentation.append(b"\x07");
                }
            }
            ServerToPane::ClipboardWrite { selection, data } => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.clipboard
                        .copy(selection, data, self.presentation.buffer());
                } else if selection == Selection::Primary {
                    self.window.primary = Some(data);
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
                    self.presentation.append(&title_bytes(&title));
                } else {
                    self.window.title = Some(title);
                }
            }
            ServerToPane::Cursor(shape) => {
                if matches!(self.lifecycle, Lifecycle::Active(_)) {
                    self.presentation.append(&cursor_bytes(shape.as_deref()));
                } else {
                    self.window.cursor = shape;
                }
            }
            ServerToPane::SharedFrame(frame) => {
                self.presentation.draw_shared(frame, &mut self.tx)?;
            }
            ServerToPane::Frame(band) => {
                let starting = !matches!(self.lifecycle, Lifecycle::Active(_));
                if starting {
                    self.start_presentation().await?;
                    if matches!(self.lifecycle, Lifecycle::Closed) {
                        return Ok(());
                    }
                }
                self.presentation.draw(band, &mut self.tx)?;
                if starting {
                    self.apply_pending_window();
                }
            }
        }
        Ok(())
    }

    fn apply_pending_window(&mut self) {
        let PendingWindow {
            title,
            cursor,
            clipboard: clipboard_data,
            primary,
        } = std::mem::take(&mut self.window);
        if let Some(title) = title {
            self.presentation.append(&title_bytes(&title));
        }
        self.presentation.append(&cursor_bytes(cursor.as_deref()));
        for (selection, data) in [
            (Selection::Clipboard, clipboard_data),
            (Selection::Primary, primary),
        ] {
            if let Some(data) = data {
                self.clipboard
                    .copy(selection, data, self.presentation.buffer());
            }
        }
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn start_presentation(&mut self) -> Result<()> {
        let mut mode = TerminalGuard::enter()?;
        let Some(probe) = terminal_probe(
            &mut self.input,
            &mut self.stdout,
            &mut self.termination,
            &self.handle,
        )
        .await?
        else {
            mode.restore()?;
            return self.close(None);
        };
        if !probe.frame_edits {
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
        let (width, height) = terminal::pane_pixels(
            probe.pixel_width.zip(probe.pixel_height),
            terminal::window_pixels(),
            terminal::cells(),
            self.cell.unwrap_or(FALLBACK_CELL),
        );
        let transport = if probe.shared_memory {
            FrameTransport::Shared
        } else {
            FrameTransport::Inline
        };
        self.presentation = PanePresentation::new(self.cell, transport);
        self.tx
            .send(&PaneToServer::Action(PaneAction::FrameTransport(transport)))?;
        self.clipboard = Clipboard::new(if probe.clipboard.is_some() {
            ClipboardProtocol::Kitty
        } else {
            ClipboardProtocol::Osc52
        })?;
        let _ = self
            .clipboard
            .refresh(Selection::Clipboard, Refresh::Background);
        let _ = self
            .clipboard
            .refresh(Selection::Primary, Refresh::Background);
        self.tx
            .send(&PaneToServer::Action(PaneAction::Resize { width, height }))?;
        probe.log_capabilities(width, height);
        self.lifecycle = Lifecycle::Active(mode);
        Ok(())
    }

    fn handle_terminal_events(&mut self) -> Result<()> {
        for _ in 0..EVENTS_PER_TURN {
            let Some(event) = self.input.next_event() else {
                break;
            };
            if !matches!(self.lifecycle, Lifecycle::Active(_))
                && !matches!(event, input::Event::Resize)
            {
                continue;
            }
            self.send_event(event)?;
            self.send_clipboard_actions()?;
        }
        Ok(())
    }

    fn close(&mut self, reason: Option<&str>) -> Result<()> {
        let restored = match std::mem::replace(&mut self.lifecycle, Lifecycle::Closed) {
            Lifecycle::Active(mut mode) => mode.restore(),
            _ => Ok(()),
        };
        let printed = reason.map_or(Ok(()), |reason| {
            let clean = protocol::sanitize_with_limit(reason, 512);
            writeln!(io::stdout(), "meowland: {clean}")
        });
        restored?;
        printed?;
        Ok(())
    }
}
