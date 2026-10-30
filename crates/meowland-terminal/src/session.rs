//! Async scheduling and explicit lifecycle for one terminal attachment.

use std::{
    future::{Future, poll_fn},
    io::{self, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use calloop::{LoopHandle, io::Async, signals::Signal as UnixSignal};
use futures::{FutureExt, io::AsyncWriteExt};
use meowland_async::{
    asynchronous::{Descriptor, sleep_until, when},
    future::yield_now,
    signals,
};
use meowland_input::{
    keyboard::Keyboard,
    {self as input},
};
use meowland_protocol::{
    self as protocol, Follow, FrameTransport, Hello, PaneAction, PaneToServer, ServerToPane,
    Shortcuts, Target,
    asynchronous::{Outgoing, Reader},
    clipboard::{Selection, Transport},
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
use crate::{Error, Result};

mod events;

const EVENTS_PER_TURN: usize = 64;

/// Turns the session loop may complete without yielding to the executor.
///
/// Most turns finish without waiting: a writable terminal, buffered input, or a
/// queued frame is ready immediately, and each turn already does bounded work
/// (up to [`EVENTS_PER_TURN`] input events and one 64 KiB write). Yielding
/// after every turn cost an executor round trip per event, so instead the loop
/// yields once a budget of productive turns is spent. A turn that actually
/// waited resets the budget because the executor already got control while the
/// loop was pending.
const TURNS_PER_YIELD: usize = 16;

/// Longest shutdown waits for queued terminal output before restoring modes.
///
/// A terminal that stops accepting bytes (flow control, a suspended emulator,
/// or a closed window) would otherwise keep the pane alive forever, because the
/// restore sequence shares the queue with the frames still ahead of it.
const CLOSE_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

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
    Deadline(io::Result<()>),
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
    shortcuts: Shortcuts,
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
        target: Target,
        follow: Follow,
        handle: LoopHandle<'static, ()>,
    ) -> Result<Self> {
        let cells = terminal::cells();
        let pixels = terminal::window_pixels();
        let cell = terminal::derived_cell(pixels, cells);
        let (width, height) =
            terminal::pane_pixels(None, pixels, cells, cell.unwrap_or(FALLBACK_CELL));
        let size = terminal::frame_size(width, height)?;
        let termination = signals::Termination::new(&handle)?;
        let resize = signals::Signal::new(&handle, &[UnixSignal::SIGWINCH])?;
        let input = input::Reader::new(&handle)?;
        let mut tx = Outgoing::default();
        tx.send(&PaneToServer::Hello(Hello {
            size,
            target,
            follow,
        }))?;
        Ok(Self {
            receiver,
            writer,
            tx,
            input,
            stdout: Descriptor::new(output::open()?.into(), &handle)?,
            handle,
            keyboard: Keyboard::default(),
            shortcuts: Shortcuts::default(),
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
        let mut turns = TURNS_PER_YIELD;
        while !matches!(self.lifecycle, Lifecycle::Closed) {
            self.prepare_pending_work()?;
            if let Err(error) = self.input.check_recovery() {
                return self.finish_after_input_error(error).await;
            }
            let active = matches!(self.lifecycle, Lifecycle::Active(_));
            let pending_output = active && self.presentation.pending();
            let pending_input = self.input.pending();
            let deadline = self.next_deadline();
            let pending_tx = self.tx.pending();
            let (ready, waited) = scheduled(async {
                futures::select! {
                    _ = self.termination.recv().fuse() => Ready::Terminate,
                    _ = self.resize.recv().fuse() => Ready::Resize,
                    result = sleep_until(&self.handle, deadline).fuse() => Ready::Deadline(result),
                    result = when(pending_output.then(|| self.presentation.drain(&mut self.stdout))).fuse() => Ready::Output(result),
                    result = when((!pending_output && !pending_tx).then(|| self.receiver.read())).fuse() => Ready::Server(result),
                    result = when(pending_tx.then(|| self.tx.write_ready(&mut self.writer))).fuse() => Ready::Transmit(result),
                    result = when((active && !pending_input && !pending_tx).then(|| self.input.read_ready())).fuse() => Ready::Input(result),
                    () = when((pending_input && !pending_tx).then(yield_now)).fuse() => Ready::BufferedInput,
                }
            })
            .await;
            match ready {
                Ready::Input(Err(error)) if error.kind() == io::ErrorKind::InvalidData => {
                    return self.finish_after_input_error(error).await;
                }
                Ready::Terminate | Ready::Input(Err(_)) => return self.close(None).await,
                Ready::Resize => self.send_event(input::Event::Resize)?,
                Ready::Server(result) => self.handle_server_message(result?).await?,
                Ready::Output(result) => {
                    result?;
                    self.presentation.finish(&mut self.tx)?;
                }
                Ready::Transmit(Err(error)) => return self.finish_after_write_error(error).await,
                Ready::Deadline(result) => result?,
                Ready::Input(Ok(())) | Ready::Transmit(Ok(())) | Ready::BufferedInput => {}
            }
            if !matches!(self.lifecycle, Lifecycle::Closed) && !self.tx.pending() {
                self.handle_terminal_events()?;
            }
            turns = if waited {
                TURNS_PER_YIELD
            } else {
                turns.saturating_sub(1)
            };
            if turns == 0 {
                turns = TURNS_PER_YIELD;
                yield_now().await;
            }
        }
        Ok(())
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn finish_after_input_error(&mut self, error: io::Error) -> Result<()> {
        self.close(None).await?;
        Err(error.into())
    }

    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn finish_after_write_error(&mut self, error: io::Error) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            futures::select! {
                _ = self.termination.recv().fuse() => return self.close(None).await,
                result = sleep_until(&self.handle, Some(deadline)).fuse() => { result?; return Err(error.into()); },
                result = self.receiver.read::<ServerToPane>().fuse() => match result {
                    Ok(ServerToPane::Release(reason) | ServerToPane::Reject(reason)) => return self.close(Some(&reason)).await,
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
            .chain(self.input.recovery_deadline())
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
            ServerToPane::HelloOk { shortcuts } => {
                if matches!(self.lifecycle, Lifecycle::Handshaking { .. }) {
                    self.shortcuts = shortcuts;
                    self.lifecycle = Lifecycle::Waiting;
                    writeln!(
                        io::stderr(),
                        "meowland: waiting for a window (Ctrl+C to cancel)"
                    )?;
                }
            }
            ServerToPane::Reject(reason) | ServerToPane::Release(reason) => {
                return self.close(Some(&reason)).await;
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
        let mut mode = TerminalGuard::enter(self.presentation.buffer())?;
        self.flush_output().await?;
        let Some(probe) = terminal_probe(
            &mut self.input,
            &mut self.stdout,
            &mut self.termination,
            &self.handle,
        )
        .await?
        else {
            let raw = mode.restore_into(self.presentation.buffer());
            self.close(None).await?;
            raw?;
            return Ok(());
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
        self.cell = probe.cell.or(self.cell);
        let (width, height) = terminal::pane_pixels(
            probe.pixels,
            terminal::window_pixels(),
            terminal::cells(),
            self.cell.unwrap_or(FALLBACK_CELL),
        );
        let size = terminal::frame_size(width, height)?;
        let transport = if probe.shared_memory {
            FrameTransport::Shared
        } else {
            FrameTransport::Inline
        };
        self.presentation = PanePresentation::new(self.cell, transport);
        mode.enable_mouse(self.presentation.buffer());
        if let Some(previous) = probe.clipboard {
            mode.enable_clipboard(previous, self.presentation.buffer());
        }
        self.flush_output().await?;
        self.tx
            .send(&PaneToServer::Action(PaneAction::FrameTransport(transport)))?;
        let protocol = if probe.clipboard.is_some() {
            ClipboardProtocol::Kitty
        } else {
            ClipboardProtocol::Osc52
        };
        self.tx
            .send(&PaneToServer::Action(PaneAction::ClipboardTransport(
                clipboard_transport(protocol),
            )))?;
        self.clipboard = Clipboard::new(protocol)?;
        let _ = self
            .clipboard
            .refresh(Selection::Clipboard, Refresh::Background);
        let _ = self
            .clipboard
            .refresh(Selection::Primary, Refresh::Background);
        self.tx
            .send(&PaneToServer::Action(PaneAction::Resize { size }))?;
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

    /// Restore terminal modes after a bounded drain, then report a reason.
    ///
    /// The restore sequence is appended to the same ordered queue as frames, so
    /// a half-written frame can never interleave with it. If the terminal stops
    /// accepting output, queued frames are discarded and the restore sequence
    /// is written directly, best effort, so shutdown always finishes.
    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn close(&mut self, reason: Option<&str>) -> Result<()> {
        let mut restore = Vec::new();
        let mut failure = if let Lifecycle::Active(mut mode) =
            std::mem::replace(&mut self.lifecycle, Lifecycle::Closed)
        {
            mode.restore_into(&mut restore).err().map(Error::from)
        } else {
            None
        };
        if let Some(reason) = reason {
            let clean = protocol::sanitize_with_limit(reason, terminal::MAX_TEXT_CHARS);
            restore.extend_from_slice(format!("meowland: {clean}\n").as_bytes());
        }
        self.presentation.append(&restore);
        let drained = self.drain_until(Instant::now() + CLOSE_DRAIN_TIMEOUT).await;
        let complete = match drained {
            Ok(complete) => complete,
            Err(error) => {
                failure = failure.or(Some(error));
                false
            }
        };
        if !complete {
            tracing::warn!("queued terminal output did not drain before shutdown");
            self.presentation.discard();
            output::write_best_effort(&restore);
        }
        failure.map_or(Ok(()), Err)
    }

    /// Write queued bytes until the deadline, reporting whether the queue
    /// emptied.
    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn drain_until(&mut self, deadline: Instant) -> Result<bool> {
        while self.presentation.pending() && Instant::now() < deadline {
            futures::select! {
                result = self.presentation.drain(&mut self.stdout).fuse() => result?,
                result = sleep_until(&self.handle, Some(deadline)).fuse() => result?,
            }
        }
        Ok(!self.presentation.pending())
    }

    /// Write every queued terminal byte in order before continuing.
    ///
    /// Startup uses this so mode changes reach the terminal before probing and
    /// before the first frame shares their queue.
    #[expect(
        clippy::future_not_send,
        reason = "the terminal session runs on calloop's local executor"
    )]
    async fn flush_output(&mut self) -> Result<()> {
        while self.presentation.pending() {
            self.presentation.drain(&mut self.stdout).await?;
        }
        self.stdout.flush().await?;
        Ok(())
    }
}

/// Report the probed clipboard protocol as the transport hint for the server.
const fn clipboard_transport(protocol: ClipboardProtocol) -> Transport {
    match protocol {
        ClipboardProtocol::Kitty => Transport::Kitty,
        ClipboardProtocol::Osc52 => Transport::Osc52,
    }
}

/// Await one scheduling turn, reporting whether it had to wait for readiness.
///
/// A turn that never returned `Pending` kept the executor to itself, so the
/// caller counts it against its budget. A turn that waited already handed
/// control to other tasks and does not.
async fn scheduled<F: Future>(future: F) -> (F::Output, bool) {
    let mut future = std::pin::pin!(future);
    let mut waited = false;
    let output = poll_fn(|context| {
        let poll = future.as_mut().poll(context);
        waited |= poll.is_pending();
        poll
    })
    .await;
    (output, waited)
}
