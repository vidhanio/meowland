//! Pane frame presentation and acknowledgement.
//!
//! The server owns every composed pixel. The pane only encodes what it
//! receives and asks for a keyframe whenever the terminal no longer holds a
//! whole image the next band could edit.

use std::time::{Duration, Instant};

use futures::io::AsyncWrite;
use meowland_protocol::{FrameBand, PaneAction, PaneToServer, SharedFrame, asynchronous::Outgoing};

use super::{kitty::Presenter, output};
use crate::{Error, Result};

/// How long to wait for the terminal to unlink a shared object before falling
/// back to inline transport.
const SHARED_TIMEOUT: Duration = Duration::from_millis(250);
/// First consumption check delay. The terminal usually unlinks within one
/// frame, so a short first delay beats a fixed 1 ms loop.
const INITIAL_POLL_DELAY: Duration = Duration::from_millis(1);
/// Upper bound on the delay between consumption checks, keeping the worst-case
/// detection latency well inside [`SHARED_TIMEOUT`].
const MAXIMUM_POLL_DELAY: Duration = Duration::from_millis(16);

/// A shared frame shown by reference that the terminal has not read yet.
struct Unread {
    frame: SharedFrame,
    deadline: Instant,
    poll_delay: Duration,
    next_poll: Instant,
}

/// Whether the terminal has read a shared frame.
enum Polled {
    /// Still unread; check again at [`Unread::next_poll`].
    Unread(Unread),
    /// The terminal unlinked the object.
    Consumed,
    /// The terminal did not read the object in time.
    Expired,
}

impl Unread {
    fn new(frame: SharedFrame, now: Instant) -> Self {
        Self {
            frame,
            deadline: now + SHARED_TIMEOUT,
            poll_delay: INITIAL_POLL_DELAY,
            next_poll: now,
        }
    }

    /// Check consumption, backing off so a slow terminal costs few syscalls.
    fn poll(mut self, now: Instant) -> Polled {
        match self.frame.exists() {
            Ok(false) => Polled::Consumed,
            Ok(true) if now < self.deadline => {
                self.next_poll = (now + self.poll_delay).min(self.deadline);
                self.poll_delay = (self.poll_delay * 2).min(MAXIMUM_POLL_DELAY);
                Polled::Unread(self)
            }
            Ok(true) => Polled::Expired,
            Err(error) => {
                tracing::warn!(%error, "checking upstream shared-frame consumption failed");
                Polled::Expired
            }
        }
    }
}

/// Own terminal-side presentation buffers and the frame awaiting its
/// acknowledgement.
pub(crate) struct PanePresentation {
    presenter: Presenter,
    pending: output::Output,
    /// The frame drawn into `pending` whose acknowledgement is not yet sent.
    frame: Option<PendingFrame>,
}

struct PendingFrame {
    /// The terminal holds no whole image to edit; request one with the
    /// acknowledgement.
    keyframe: bool,
    /// A shared frame the terminal has not read yet; the acknowledgement
    /// waits for it.
    unread: Option<Unread>,
}

impl PanePresentation {
    pub(crate) fn new() -> Self {
        Self {
            presenter: Presenter::default(),
            pending: output::Output::default(),
            frame: None,
        }
    }

    pub(crate) const fn pending(&self) -> bool {
        self.pending.pending()
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.pending.append(bytes);
    }

    /// Drop queued terminal bytes that a stalled terminal never accepted.
    pub(crate) fn discard(&mut self) {
        self.pending.discard();
    }

    /// Scratch space for terminal output that is not part of a frame.
    pub(crate) const fn buffer(&mut self) -> &mut Vec<u8> {
        self.pending.buffer()
    }

    pub(crate) fn output_progress(&self) -> std::rc::Rc<std::cell::Cell<usize>> {
        self.pending.progress()
    }

    pub(crate) fn output_end_position(&self) -> usize {
        self.pending.end_position()
    }

    /// Ask for a whole frame after the terminal's cell geometry changed, so
    /// the placement never keeps the old grid.
    pub(crate) fn cell_size_changed(&mut self, tx: &mut Outgoing) -> Result<()> {
        self.presenter.invalidate();
        tx.send(&PaneToServer::Action(PaneAction::Keyframe))?;
        Ok(())
    }

    fn unread(&self) -> Option<&Unread> {
        self.frame.as_ref().and_then(|frame| frame.unread.as_ref())
    }

    /// Next shared-frame consumption check.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.pending() {
            return None;
        }
        self.unread().map(|unread| unread.next_poll)
    }

    pub(crate) fn draw(&mut self, band: &FrameBand, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            return Err(Error::UnacknowledgedFrame);
        }
        let shown = self.presenter.present_into(band, self.pending.buffer());
        self.frame = Some(PendingFrame {
            keyframe: !shown,
            unread: None,
        });
        self.finish(tx)
    }

    pub(crate) fn draw_shared(&mut self, frame: SharedFrame, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            return Err(Error::UnacknowledgedFrame);
        }
        self.presenter.present_shared(&frame, self.pending.buffer());
        self.frame = Some(PendingFrame {
            keyframe: false,
            unread: Some(Unread::new(frame, Instant::now())),
        });
        self.finish(tx)
    }

    /// Check an unread shared frame once its deadline arrives.
    pub(crate) fn advance(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.unread().is_some() && !self.pending() {
            self.finish(tx)?;
        }
        Ok(())
    }

    #[expect(
        clippy::future_not_send,
        reason = "terminal output belongs to the calloop owner thread"
    )]
    pub(crate) async fn drain(&mut self, stdout: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        Ok(self.pending.drain(stdout).await?)
    }

    /// Acknowledge the frame once the terminal accepted all of its bytes and
    /// read any shared object it references.
    pub(crate) fn finish(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.pending() {
            return Ok(());
        }
        let Some(mut frame) = self.frame.take() else {
            return Ok(());
        };
        if let Some(unread) = frame.unread.take() {
            match unread.poll(Instant::now()) {
                Polled::Unread(unread) => {
                    frame.unread = Some(unread);
                    self.frame = Some(frame);
                    return Ok(());
                }
                Polled::Consumed => {}
                Polled::Expired => {
                    tracing::warn!("upstream shared frame remained unread; using inline transport");
                    self.presenter.invalidate();
                    frame.keyframe = true;
                    tx.send(&PaneToServer::Action(PaneAction::Inline))?;
                }
            }
        }
        if frame.keyframe {
            tx.send(&PaneToServer::Action(PaneAction::Keyframe))?;
        }
        tx.send(&PaneToServer::Action(PaneAction::Ack))?;
        Ok(())
    }

    /// Acknowledge an unfinished frame so the server is free to send the next
    /// one, then start over for the next presentation.
    pub(crate) fn abandon(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            tx.send(&PaneToServer::Action(PaneAction::Ack))?;
        }
        self.restart();
        Ok(())
    }

    /// Start over with a fresh placement, keeping queued terminal output and
    /// its stream position.
    pub(crate) fn restart(&mut self) {
        self.presenter = Presenter::default();
        self.frame = None;
    }
}

#[cfg(test)]
mod tests {
    use meowland_core::pixels::FrameSize;
    use meowland_protocol::SharedObject;

    use super::*;

    fn sent(tx: &mut Outgoing) -> Vec<PaneAction> {
        let mut bytes = futures::io::Cursor::new(Vec::new());
        {
            let mut flush = std::pin::pin!(tx.flush_to(&mut bytes));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            while flush.as_mut().poll(&mut context).is_pending() {}
        }
        let mut reader = std::io::Cursor::new(bytes.into_inner());
        let mut actions = Vec::new();
        while let Ok(message) = meowland_protocol::recv::<PaneToServer>(&mut reader) {
            if let PaneToServer::Action(action) = message {
                actions.push(action);
            }
        }
        actions
    }

    #[test]
    fn a_frame_before_the_previous_acknowledgement_is_refused() {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let mut presentation = PanePresentation::new();
        let mut tx = Outgoing::default();
        let band = FrameBand::new(size, 0, 0, 2, vec![3; 12]).expect("valid band");
        presentation.draw(&band, &mut tx).expect("first frame");
        assert!(presentation.pending(), "the first frame is still queued");
        assert!(matches!(
            presentation.draw(&band, &mut tx),
            Err(Error::UnacknowledgedFrame)
        ));
    }

    #[test]
    fn shared_frames_are_acknowledged_only_after_the_terminal_reads_them() {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let (object, _file) = SharedObject::create(size).expect("create shared frame");
        let mut presentation = PanePresentation::new();
        let mut tx = Outgoing::default();
        presentation
            .draw_shared(*object.frame(), &mut tx)
            .expect("draw shared frame");
        presentation.pending.discard();
        presentation.finish(&mut tx).expect("poll consumption");
        assert!(
            presentation.deadline().is_some(),
            "still waiting for the terminal"
        );

        object.frame().unlink().expect("consume shared frame");
        presentation.advance(&mut tx).expect("observe consumption");
        assert!(presentation.deadline().is_none());
        assert!(matches!(sent(&mut tx).as_slice(), [PaneAction::Ack]));
    }

    #[test]
    fn an_unread_shared_frame_falls_back_to_inline_keyframes() {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let (object, _file) = SharedObject::create(size).expect("create shared frame");
        let mut presentation = PanePresentation::new();
        let mut tx = Outgoing::default();
        presentation
            .draw_shared(*object.frame(), &mut tx)
            .expect("draw shared frame");
        presentation.pending.discard();
        if let Some(unread) = presentation
            .frame
            .as_mut()
            .and_then(|frame| frame.unread.as_mut())
        {
            unread.deadline = Instant::now();
        }
        presentation
            .advance(&mut tx)
            .expect("expire the shared frame");
        assert!(matches!(
            sent(&mut tx).as_slice(),
            [PaneAction::Inline, PaneAction::Keyframe, PaneAction::Ack]
        ));

        let band = FrameBand::new(size, 1, 0, 1, vec![2; 3]).expect("valid band");
        presentation.draw(&band, &mut tx).expect("draw band");
        assert!(
            matches!(
                sent(&mut tx).as_slice(),
                [PaneAction::Keyframe, PaneAction::Ack]
            ),
            "the unread image is no basis for edits"
        );
    }

    #[test]
    fn a_band_without_a_whole_image_requests_a_keyframe() {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let mut presentation = PanePresentation::new();
        let mut tx = Outgoing::default();
        let band = FrameBand::new(size, 1, 0, 1, vec![2; 3]).expect("valid band");
        presentation.draw(&band, &mut tx).expect("draw band");
        assert!(!presentation.pending(), "nothing to edit");
        assert!(matches!(
            sent(&mut tx).as_slice(),
            [PaneAction::Keyframe, PaneAction::Ack]
        ));
    }
}
