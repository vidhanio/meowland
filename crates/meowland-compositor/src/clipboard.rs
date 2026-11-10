//! Plain-text clipboard between Wayland applications and terminal panes.
//!
//! An application's clipboard text is copied to the pane showing it, and a
//! terminal's bracketed paste becomes the seat clipboard before the
//! application's paste shortcut runs.

use std::{
    collections::HashMap,
    io, mem,
    os::fd::{AsFd, OwnedFd},
    sync::Arc,
    time::Duration,
};

use calloop::{
    Interest, Mode, PostAction,
    generic::Generic,
    timer::{TimeoutAction, Timer},
};
use meowland_loop::Sources;
use meowland_protocol::{ServerToPane, clipboard::MAX_TEXT};
use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    io::Errno,
    pipe::{PipeFlags, pipe_with},
};
use smithay::wayland::selection::{
    SelectionSource, SelectionTarget,
    data_device::{request_data_device_client_selection, set_data_device_selection},
};

use super::{AttachedPane, Compositor, PaneId, input};

/// Plain-text representations, most useful first.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "TEXT",
    "STRING",
];

/// How long one transfer to or from an application may take.
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);
/// Most deliveries to applications in progress at once.
const MAX_DELIVERIES: usize = 16;
/// Bytes written to an application per readiness event.
const WRITE_BUDGET: usize = 64 * 1024;
/// Bytes read from an application per read call.
const READ_CHUNK: usize = 16 * 1024;
/// Read calls per readiness event.
const READS_PER_EVENT: usize = 4;

/// Text the compositor itself offers as the seat clipboard.
pub(crate) type Text = Arc<[u8]>;

/// The in-flight export of application text and deliveries of pasted text.
#[derive(Debug, Default)]
pub(crate) struct Clipboard {
    /// Incremented by every clipboard change, so a superseded export's
    /// callbacks recognize they are stale.
    generation: u64,
    export: Option<Sources<Compositor>>,
    deliveries: HashMap<u64, Sources<Compositor>>,
    next_delivery: u64,
}

impl Clipboard {
    fn cancel(&mut self) {
        self.generation += 1;
        self.export = None;
    }

    const fn take_export(&mut self, generation: u64) -> Option<Sources<Compositor>> {
        if self.generation == generation {
            self.export.take()
        } else {
            None
        }
    }
}

impl Compositor {
    /// Copy an application's new clipboard text to the pane showing it.
    ///
    /// Smithay invokes this before it updates the seat selection, so the text
    /// is requested from an immediate timer rather than synchronously.
    pub(crate) fn copy_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
    ) {
        if target != SelectionTarget::Clipboard {
            return;
        }
        self.clipboard.cancel();
        let Some(mime) = source.and_then(|source| {
            let offered = source.mime_types();
            TEXT_MIMES
                .into_iter()
                .find(|mime| offered.iter().any(|offered| offered == mime))
        }) else {
            return;
        };
        let Some(pane) = self.selection_pane() else {
            return;
        };
        let generation = self.clipboard.generation;
        let mut timer = Sources::new(&self.handle);
        match self
            .handle
            .insert_source(Timer::immediate(), move |_, (), state| {
                if let Some(mut timer) = state.clipboard.take_export(generation) {
                    timer.timer = None;
                    if let Err(error) = state.export(generation, pane, mime) {
                        tracing::warn!(%error, mime, "Cannot read client clipboard");
                    }
                }
                TimeoutAction::Drop
            }) {
            Ok(token) => {
                timer.timer = Some(token);
                self.clipboard.export = Some(timer);
            }
            Err(error) => tracing::warn!(%error, "Cannot schedule clipboard export"),
        }
    }

    fn export(&mut self, generation: u64, pane: PaneId, mime: &str) -> io::Result<()> {
        let (read, write) = pipe_with(PipeFlags::CLOEXEC)?;
        nonblocking(&read)?;
        let mut sources = Sources::new(&self.handle);
        let mut bytes = Vec::new();
        sources.io = Some(self.handle.insert_source(
            Generic::new(read, Interest::READ, Mode::Level),
            move |_, fd, state| {
                let received = match receive(fd, &mut bytes) {
                    Ok(false) => return Ok(PostAction::Continue),
                    Ok(true) => Some(mem::take(&mut bytes)),
                    Err(error) => {
                        tracing::warn!(%error, "Abandoning client clipboard export");
                        None
                    }
                };
                if let Some(mut sources) = state.clipboard.take_export(generation) {
                    sources.io = None;
                    if let Some(bytes) = received.filter(|bytes| !bytes.is_empty())
                        && state.attachments.contains(pane)
                    {
                        state.emit(pane, ServerToPane::Copy(bytes));
                    }
                }
                Ok(PostAction::Remove)
            },
        )?);
        sources.timer = Some(self.handle.insert_source(
            Timer::from_duration(TRANSFER_TIMEOUT),
            move |_, (), state| {
                if let Some(mut sources) = state.clipboard.take_export(generation) {
                    sources.timer = None;
                    tracing::warn!("Client clipboard export timed out");
                }
                TimeoutAction::Drop
            },
        )?);
        self.clipboard.export = Some(sources);
        if let Err(error) =
            request_data_device_client_selection(&self.input.seat, mime.to_owned(), write)
        {
            self.clipboard.export = None;
            return Err(io::Error::other(error));
        }
        Ok(())
    }

    /// Make a terminal's bracketed paste the seat clipboard and paste it
    /// into the pane's window.
    pub(crate) fn paste(&mut self, pane: PaneId, text: String) {
        if text.len() > MAX_TEXT || !self.pane_has_window(pane) {
            return;
        }
        self.clipboard.cancel();
        set_data_device_selection(
            &self.display,
            &self.input.seat,
            TEXT_MIMES.map(str::to_owned).into(),
            Text::from(text.into_bytes()),
        );
        input::paste_action(self, pane);
    }

    /// Write pasted text to an application reading the seat clipboard.
    pub(crate) fn send_text(&mut self, fd: OwnedFd, text: &Text) {
        if self.clipboard.deliveries.len() >= MAX_DELIVERIES {
            return;
        }
        if let Err(error) = self.deliver(fd, Arc::clone(text)) {
            tracing::warn!(%error, "Cannot write client clipboard");
        }
    }

    fn deliver(&mut self, fd: OwnedFd, text: Text) -> io::Result<()> {
        nonblocking(&fd)?;
        let id = self.clipboard.next_delivery;
        self.clipboard.next_delivery += 1;
        let mut sources = Sources::new(&self.handle);
        let mut sent = 0;
        sources.io = Some(self.handle.insert_source(
            Generic::new(fd, Interest::WRITE, Mode::Level),
            move |_, fd, state| {
                let mut budget = WRITE_BUDGET;
                while sent < text.len() {
                    if budget == 0 {
                        return Ok(PostAction::Continue);
                    }
                    let end = (sent + budget).min(text.len());
                    match rustix::io::write(&*fd, &text[sent..end]) {
                        Ok(0) => break,
                        Ok(written) => {
                            sent += written;
                            budget -= written;
                        }
                        Err(Errno::INTR) => {}
                        Err(Errno::AGAIN) => return Ok(PostAction::Continue),
                        Err(error) => {
                            tracing::debug!(%error, "Client stopped reading the clipboard");
                            break;
                        }
                    }
                }
                if let Some(mut sources) = state.clipboard.deliveries.remove(&id) {
                    sources.io = None;
                }
                Ok(PostAction::Remove)
            },
        )?);
        sources.timer = Some(self.handle.insert_source(
            Timer::from_duration(TRANSFER_TIMEOUT),
            move |_, (), state| {
                if let Some(mut sources) = state.clipboard.deliveries.remove(&id) {
                    sources.timer = None;
                    tracing::warn!("Client clipboard delivery timed out");
                }
                TimeoutAction::Drop
            },
        )?);
        self.clipboard.deliveries.insert(id, sources);
        Ok(())
    }

    /// The pane that last routed input to the focused window.
    fn selection_pane(&self) -> Option<PaneId> {
        self.attachments.owner(self.windows.catalog.focused()?)
    }

    fn pane_has_window(&self, pane: PaneId) -> bool {
        self.attachments
            .get(pane)
            .and_then(AttachedPane::window)
            .is_some_and(|window| self.windows.catalog.drawable(window))
    }
}

/// Read what is available, reporting whether the writer finished.
fn receive(fd: &impl AsFd, bytes: &mut Vec<u8>) -> io::Result<bool> {
    let mut chunk = [0; READ_CHUNK];
    for _ in 0..READS_PER_EVENT {
        match rustix::io::read(fd, &mut chunk) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                if bytes.len() + read > MAX_TEXT {
                    return Err(io::ErrorKind::FileTooLarge.into());
                }
                bytes.extend_from_slice(&chunk[..read]);
            }
            Err(Errno::AGAIN) => return Ok(false),
            Err(Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use meowland_core::pixels::FrameSize;
    use meowland_protocol::{Follow, PaneAction, Selector, Target};

    use super::*;
    use crate::{
        Command, Event,
        fixture::{Fixture, Toplevel},
    };

    fn shown(fixture: &mut Fixture) -> (Toplevel, PaneId) {
        let toplevel = fixture.toplevel("app");
        fixture.present(&toplevel.surface, 4, 4, [0, 0, 0, 255]);
        let pane = PaneId::from(1);
        fixture.state.focus_pane(
            pane,
            Target::Selector(Selector::AppId("app".into())),
            Follow::Current,
            FrameSize::new(64, 64).expect("test size"),
        );
        fixture.roundtrip();
        (toplevel, pane)
    }

    fn settle(fixture: &mut Fixture) {
        for _ in 0..4 {
            fixture.roundtrip();
        }
    }

    fn copies(fixture: &mut Fixture, pane: PaneId) -> Vec<Vec<u8>> {
        settle(fixture);
        fixture
            .state
            .drain_events()
            .filter_map(|event| match event {
                Event {
                    pane: target,
                    message: ServerToPane::Copy(bytes),
                } if target == pane => Some(bytes),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_application_copy_reaches_the_pane_as_its_preferred_text() {
        let mut fixture = Fixture::new();
        let (_toplevel, pane) = shown(&mut fixture);
        let text = "copied ✓\n".as_bytes();

        let source = fixture.copy(&["text/html", "STRING", "text/plain"]);
        settle(&mut fixture);
        let requested: Vec<_> = fixture
            .app
            .sends
            .iter()
            .map(|(_, mime, _)| mime.as_str())
            .collect();
        assert_eq!(requested, ["text/plain"]);
        fixture.answer(&source, text).expect("write copied text");

        assert_eq!(copies(&mut fixture, pane), [text]);
    }

    #[test]
    fn a_replaced_selection_delivers_only_the_newer_text() {
        let mut fixture = Fixture::new();
        let (_toplevel, pane) = shown(&mut fixture);
        let old = fixture.copy(&["text/plain;charset=utf-8"]);
        settle(&mut fixture);

        let new = fixture.copy(&["text/plain;charset=utf-8"]);
        settle(&mut fixture);
        let _ = fixture.answer(&old, b"old");
        fixture.answer(&new, b"new").expect("write newer text");

        assert_eq!(copies(&mut fixture, pane), [b"new"]);
    }

    #[test]
    fn a_pane_paste_is_readable_from_the_seat_clipboard_in_full() {
        let mut fixture = Fixture::new();
        let (_toplevel, pane) = shown(&mut fixture);
        let text: String = "pasted ✓ ".repeat(40_000);
        assert!(text.len() > 4 * WRITE_BUDGET && text.len() <= MAX_TEXT);

        fixture.state.handle_command(Command::Pane {
            pane,
            action: PaneAction::Paste(text.clone()),
        });
        fixture.roundtrip();

        for mime in TEXT_MIMES {
            assert_eq!(fixture.read_selection(mime), text.as_bytes(), "{mime}");
        }
    }
}
