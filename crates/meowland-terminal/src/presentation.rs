//! Pane frame presentation, acknowledgement, and throughput accounting.
//!
//! A shared frame is acknowledged as soon as the terminal confirms consumption,
//! then copied into memory a bounded chunk per scheduling turn. The inline band
//! that follows usually finds those pixels ready, so it never pays for a full
//! read of the shared object on its own critical path.

use std::time::{Duration, Instant};

use futures::io::AsyncWrite;
use meowland_core::latency::Latency;
use meowland_kitty::{PresentOutcome, Presenter};
use meowland_protocol::{
    FrameAck, FrameBand, FrameTransport, PaneAction, PaneToServer, SharedFrame,
    asynchronous::Outgoing,
};

use super::output;
use crate::{Error, Result};

mod shared;
use shared::{Polled, Readback, Unread};

/// Log pane throughput to the user journal once per second.
struct PaneStats {
    window: Instant,
    frames: u32,
    drawn: u32,
    bytes: u64,
    encoded: Latency,
    written: Latency,
    retained: Latency,
    completion_wait: Latency,
    resyncs: u32,
}

impl PaneStats {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            frames: 0,
            drawn: 0,
            bytes: 0,
            encoded: Latency::default(),
            written: Latency::default(),
            retained: Latency::default(),
            completion_wait: Latency::default(),
            resyncs: 0,
        }
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "throughput is approximate telemetry; byte accounting itself remains integer-valued"
    )]
    fn frame(&mut self, frame: &PendingFrame) {
        self.frames += 1;
        self.drawn += u32::from(frame.bytes > 0);
        self.resyncs += u32::from(frame.ack == FrameAck::Resync);
        self.bytes += u64::try_from(frame.bytes).expect("encoded frame length fits u64");
        let retained = frame.started.elapsed();
        self.encoded.record(frame.encoded);
        self.written.record(frame.written);
        self.retained.record(retained);
        self.completion_wait.record(
            retained
                .saturating_sub(frame.encoded)
                .saturating_sub(frame.written),
        );
        let elapsed = self.window.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let elapsed = elapsed.as_secs_f64();
        let frames = std::mem::take(&mut self.frames);
        let drawn = std::mem::take(&mut self.drawn);
        let bytes = std::mem::take(&mut self.bytes);
        let resyncs = std::mem::take(&mut self.resyncs);
        self.window = Instant::now();
        tracing::info!(
            fps = f64::from(frames) / elapsed,
            drawn,
            resyncs,
            unchanged = frames.saturating_sub(drawn).saturating_sub(resyncs),
            mb_per_second = bytes as f64 / elapsed / 1e6,
            encode_us = ?self.encoded.summarize(),
            write_us = ?self.written.summarize(),
            retention_us = ?self.retained.summarize(),
            completion_wait_us = ?self.completion_wait.summarize(),
            "pane throughput and retention latency; excludes display presentation"
        );
    }
}

/// Own terminal-side presentation buffers together with their accounting.
pub(crate) struct PanePresentation {
    presenter: Presenter,
    stats: PaneStats,
    pending: output::Output,
    transport: FrameTransport,
    /// The frame drawn into `pending` whose acknowledgement is not yet sent.
    frame: Option<PendingFrame>,
    /// Pixels of the last consumed shared frame, copied into memory in the
    /// background until a band or repaint adopts them.
    retained: Option<Readback>,
    /// A grid change invalidated the placement; repaint it from retained
    /// pixels once they are in memory.
    repaint: bool,
}

struct PendingFrame {
    started: Instant,
    encoded: Duration,
    written: Duration,
    write_started: Option<Instant>,
    bytes: usize,
    ack: FrameAck,
    /// A shared frame the terminal has not read yet; the acknowledgement
    /// waits for it.
    unread: Option<Unread>,
}

impl PendingFrame {
    fn new(started: Instant, bytes: usize, ack: FrameAck, unread: Option<Unread>) -> Self {
        Self {
            started,
            encoded: started.elapsed(),
            written: Duration::ZERO,
            write_started: None,
            bytes,
            ack,
            unread,
        }
    }
}

const fn ack(outcome: PresentOutcome) -> FrameAck {
    match outcome {
        PresentOutcome::NeedsKeyframe => FrameAck::Resync,
        PresentOutcome::Encoded | PresentOutcome::Unchanged => FrameAck::Retained,
    }
}

impl PanePresentation {
    pub(crate) fn new(transport: FrameTransport) -> Self {
        Self {
            presenter: Presenter::default(),
            stats: PaneStats::new(),
            pending: output::Output::default(),
            transport,
            frame: None,
            retained: None,
            repaint: false,
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

    /// Replace the image placement after the terminal's cell geometry changed.
    ///
    /// A displayed shared frame is repainted as soon as its pixels are
    /// retained, so its placement never keeps the old grid either.
    pub(crate) const fn cell_size_changed(&mut self) {
        self.presenter.invalidate();
        self.repaint = true;
    }

    fn unread(&self) -> Option<&Unread> {
        self.frame.as_ref().and_then(|frame| frame.unread.as_ref())
    }

    /// Next deadline: shared-frame consumption, a pending repaint, or the next
    /// background readback turn.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        if self.pending() {
            return None;
        }
        if let Some(unread) = self.unread() {
            return Some(unread.next_poll());
        }
        let readback = self
            .retained
            .as_ref()
            .is_some_and(|retained| !retained.is_ready());
        (self.repaint_ready() || readback).then(Instant::now)
    }

    /// Whether a requested repaint has every retained pixel it needs.
    fn repaint_ready(&self) -> bool {
        self.repaint
            && self.unread().is_none()
            && self.retained.as_ref().is_none_or(Readback::is_ready)
    }

    pub(crate) fn draw(&mut self, band: FrameBand, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            return Err(Error::UnacknowledgedFrame);
        }
        let started = Instant::now();
        let retained = self.retained.take();
        if !band.is_whole()
            && let Some(retained) = retained
        {
            match retained.into_band() {
                Ok(previous) => {
                    self.presenter.retain_frame(previous);
                }
                Err(error) => {
                    tracing::warn!(%error, "retained upstream frame could not be read");
                    return self.reject_shared(tx);
                }
            }
        }
        let outcome = self.presenter.present_into(band, self.pending.buffer());
        self.repaint = false;
        self.frame = Some(PendingFrame::new(
            started,
            self.pending.len(),
            ack(outcome),
            None,
        ));
        self.finish(tx)
    }

    pub(crate) fn draw_shared(&mut self, frame: SharedFrame, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            return Err(Error::UnacknowledgedFrame);
        }
        if self.transport != FrameTransport::Shared {
            return self.reject_shared(tx);
        }
        let started = Instant::now();
        let unread = match Unread::new(frame, started) {
            Ok(unread) => unread,
            Err(error) => {
                tracing::warn!(%error, "upstream shared frame is unavailable; requesting RGB frames");
                return self.reject_shared(tx);
            }
        };
        self.presenter
            .present_shared(unread.frame(), self.pending.buffer());
        self.repaint = false;
        self.retained = None;
        self.frame = Some(PendingFrame::new(
            started,
            self.pending.len(),
            FrameAck::Retained,
            Some(unread),
        ));
        self.finish(tx)
    }

    fn disable_shared(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.transport == FrameTransport::Inline {
            return Ok(());
        }
        self.transport = FrameTransport::Inline;
        tx.send(&PaneToServer::Action(PaneAction::FrameTransport(
            FrameTransport::Inline,
        )))?;
        Ok(())
    }

    /// Refuse the frame being drawn and ask for complete RGB frames instead.
    fn reject_shared(&mut self, tx: &mut Outgoing) -> Result<()> {
        self.presenter.invalidate();
        self.retained = None;
        self.repaint = false;
        self.disable_shared(tx)?;
        tx.send(&PaneToServer::Action(PaneAction::Ack(FrameAck::Resync)))?;
        Ok(())
    }

    pub(crate) fn advance(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.unread().is_some() {
            if !self.pending() {
                self.finish(tx)?;
            }
            return Ok(());
        }
        if let Some(retained) = &mut self.retained
            && let Err(error) = retained.step()
        {
            tracing::warn!(%error, "retained upstream frame could not be read");
            self.retained = None;
            self.presenter.invalidate();
            self.repaint = false;
        }
        if self.repaint_ready() && !self.pending() {
            self.repaint(tx)?;
        }
        Ok(())
    }

    /// Replace the placement from retained pixels, first adopting a retained
    /// shared frame.
    fn repaint(&mut self, tx: &mut Outgoing) -> Result<()> {
        let started = Instant::now();
        match self.retained.take() {
            Some(Readback::Ready(band)) => {
                self.presenter.retain_frame(band);
            }
            reading => self.retained = reading,
        }
        let outcome = self.presenter.repaint_into(self.pending.buffer());
        if let Some(frame) = &mut self.frame {
            frame.encoded += started.elapsed();
            frame.bytes += self.pending.len();
            if ack(outcome) == FrameAck::Resync {
                frame.ack = FrameAck::Resync;
            }
        }
        self.repaint = false;
        self.finish(tx)
    }

    #[expect(
        clippy::future_not_send,
        reason = "terminal output belongs to the calloop owner thread"
    )]
    pub(crate) async fn drain(&mut self, stdout: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        if let Some(frame) = &mut self.frame {
            frame.write_started.get_or_insert_with(Instant::now);
        }
        self.pending.drain(stdout).await?;
        if !self.pending()
            && let Some(frame) = &mut self.frame
            && let Some(started) = frame.write_started.take()
        {
            frame.written += started.elapsed();
        }
        Ok(())
    }

    fn poll_unread(&mut self, tx: &mut Outgoing) -> Result<()> {
        let Some(mut frame) = self.frame.take() else {
            return Ok(());
        };
        if let Some(unread) = frame.unread.take() {
            match unread.poll(Instant::now()) {
                Polled::Unread(unread) => frame.unread = Some(unread),
                Polled::Consumed(readback) => self.retained = Some(readback),
                Polled::Expired(unread) => {
                    tracing::warn!("upstream shared frame remained unread; using inline transport");
                    self.disable_shared(tx)?;
                    self.present_unread(&mut frame, unread);
                }
            }
        }
        self.frame = Some(frame);
        Ok(())
    }

    /// Show an unread shared frame inline instead.
    fn present_unread(&mut self, frame: &mut PendingFrame, unread: Unread) {
        let started = Instant::now();
        self.presenter.invalidate();
        self.repaint = false;
        match unread.into_band() {
            Ok(band) => {
                let outcome = self.presenter.present_into(band, self.pending.buffer());
                if ack(outcome) == FrameAck::Resync {
                    frame.ack = FrameAck::Resync;
                }
                frame.bytes += self.pending.len();
                frame.encoded += started.elapsed();
            }
            Err(error) => {
                tracing::warn!(%error, "unread shared frame could not be recovered");
                frame.ack = FrameAck::Resync;
            }
        }
    }

    pub(crate) fn finish(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.pending() {
            return Ok(());
        }
        self.poll_unread(tx)?;
        if self.pending() || self.unread().is_some() || self.repaint_ready() {
            return Ok(());
        }
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        self.stats.frame(&frame);
        tx.send(&PaneToServer::Action(PaneAction::Ack(frame.ack)))?;
        Ok(())
    }

    /// Acknowledge an unfinished frame so the server is free to send the next
    /// one, then start over inline for the next presentation.
    pub(crate) fn abandon(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.frame.is_some() {
            tx.send(&PaneToServer::Action(PaneAction::Ack(FrameAck::Resync)))?;
        }
        self.restart(FrameTransport::Inline);
        Ok(())
    }

    /// Start over with a fresh placement and transport, keeping queued
    /// terminal output and its stream position.
    pub(crate) fn restart(&mut self, transport: FrameTransport) {
        self.presenter = Presenter::default();
        self.stats = PaneStats::new();
        self.transport = transport;
        self.frame = None;
        self.retained = None;
        self.repaint = false;
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use meowland_core::pixels::FrameSize;
    use meowland_protocol::SharedObject;

    use super::*;

    fn consumed_shared() -> (PanePresentation, Outgoing) {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let (object, mut file) = SharedObject::create(size).expect("create shared frame");
        file.write_all(&[1; 12]).expect("write shared pixels");
        let mut presentation = PanePresentation::new(FrameTransport::Shared);
        let mut tx = Outgoing::default();
        presentation
            .draw_shared(*object.frame(), &mut tx)
            .expect("draw shared frame");
        presentation.pending.discard();
        object.frame().unlink().expect("consume shared frame");
        presentation
            .finish(&mut tx)
            .expect("acknowledge consumption");
        assert!(presentation.frame.is_none(), "consumption acknowledges");
        while presentation
            .retained
            .as_ref()
            .is_some_and(|retained| !retained.is_ready())
        {
            presentation.advance(&mut tx).expect("retain shared pixels");
        }
        (presentation, tx)
    }

    #[test]
    fn cell_change_repaints_consumed_shared_frame_without_new_band() {
        let (mut presentation, mut tx) = consumed_shared();
        presentation.cell_size_changed();
        assert!(presentation.deadline().is_some());
        presentation.advance(&mut tx).expect("repaint");
        assert!(
            presentation
                .pending
                .buffer()
                .windows(3)
                .any(|part| part == b"a=T")
        );
    }

    #[test]
    fn cell_change_replaces_shared_placement_when_partial_band_arrives() {
        let (mut presentation, mut tx) = consumed_shared();
        presentation.cell_size_changed();
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let band = FrameBand::new(size, 1, 0, 1, vec![2; 3]).expect("valid band");
        presentation.draw(band, &mut tx).expect("draw band");
        assert!(
            presentation
                .pending
                .buffer()
                .windows(3)
                .any(|part| part == b"a=T")
        );
    }

    #[test]
    fn a_frame_before_the_previous_acknowledgement_is_refused() {
        let size = FrameSize::new(2, 2).expect("valid dimensions");
        let mut presentation = PanePresentation::new(FrameTransport::Inline);
        let mut tx = Outgoing::default();
        let band = || FrameBand::new(size, 0, 0, 2, vec![3; 12]).expect("valid band");
        presentation.draw(band(), &mut tx).expect("first frame");
        assert!(presentation.pending(), "the first frame is still queued");
        assert!(matches!(
            presentation.draw(band(), &mut tx),
            Err(Error::UnacknowledgedFrame)
        ));
    }
}
