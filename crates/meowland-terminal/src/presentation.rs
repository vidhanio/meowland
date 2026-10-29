//! Pane frame presentation, acknowledgement, and throughput accounting.

use std::time::{Duration, Instant};

use futures::io::AsyncWrite;
use meowland_core::latency::Latency;
use meowland_kitty::{PresentOutcome, Presenter, present_shared};
use meowland_protocol::{
    FrameAck, FrameBand, FrameTransport, PaneAction, PaneToServer, SharedFrame,
    asynchronous::Outgoing,
};

use super::output;
use crate::Result;

mod shared;
use shared::{Consumption, SharedPresentation};

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
    fn frame(&mut self, frame: &PendingFrame, ack: FrameAck) {
        self.frames += 1;
        self.drawn += u32::from(frame.bytes > 0);
        self.resyncs += u32::from(ack == FrameAck::Resync);
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
    frame: Option<PendingFrame>,
    retry: Option<Instant>,
    shared_frame: Option<SharedPresentation>,
    transport: FrameTransport,
}

struct PendingFrame {
    started: Instant,
    encoded: Duration,
    written: Duration,
    write_started: Option<Instant>,
    bytes: usize,
    accepted: bool,
}

impl PanePresentation {
    pub(crate) fn new(cell: Option<(u16, u16)>, transport: FrameTransport) -> Self {
        Self {
            presenter: Presenter::new(cell),
            stats: PaneStats::new(),
            pending: output::Output::default(),
            frame: None,
            retry: None,
            shared_frame: None,
            transport,
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

    pub(crate) fn set_cell_size(&mut self, cell: Option<(u16, u16)>) {
        if self.presenter.set_cell_size(cell) && self.shared_frame.is_none() {
            self.retry = Some(Instant::now());
        }
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.retry.filter(|_| !self.pending())
    }

    pub(crate) fn draw(&mut self, band: FrameBand, tx: &mut Outgoing) -> Result<()> {
        debug_assert!(
            !self.pending() && self.frame.is_none(),
            "previous frame must finish before accepting another band"
        );
        let started = Instant::now();
        if band.y() == 0 && band.rgb().len() == band.size().rgb_len() {
            self.shared_frame = None;
        }
        if let Some(shared) = self.shared_frame.take() {
            match shared.into_band() {
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
        self.frame = Some(PendingFrame {
            encoded: started.elapsed(),
            written: Duration::ZERO,
            write_started: None,
            started,
            bytes: self.pending.len(),
            accepted: outcome != PresentOutcome::NeedsKeyframe,
        });
        self.retry = None;
        self.finish(tx)
    }

    pub(crate) fn draw_shared(&mut self, frame: SharedFrame, tx: &mut Outgoing) -> Result<()> {
        debug_assert!(
            !self.pending() && self.frame.is_none(),
            "previous frame must finish before accepting another frame"
        );
        if self.transport != FrameTransport::Shared {
            return self.reject_shared(tx);
        }
        let started = Instant::now();
        let shared = match SharedPresentation::new(frame) {
            Ok(shared) => shared,
            Err(error) => {
                tracing::warn!(%error, "upstream shared frame is unavailable; requesting RGB frames");
                return self.reject_shared(tx);
            }
        };
        self.presenter.invalidate();
        present_shared(&shared.frame, self.pending.buffer());
        self.shared_frame = Some(shared);
        self.frame = Some(PendingFrame {
            started,
            encoded: started.elapsed(),
            written: Duration::ZERO,
            write_started: None,
            bytes: self.pending.len(),
            accepted: true,
        });
        self.retry = Some(Instant::now());
        self.finish(tx)
    }

    fn disable_shared(&mut self, tx: &mut Outgoing) -> Result<()> {
        self.transport = FrameTransport::Inline;
        tx.send(&PaneToServer::Action(PaneAction::FrameTransport(
            FrameTransport::Inline,
        )))?;
        Ok(())
    }

    fn reject_shared(&mut self, tx: &mut Outgoing) -> Result<()> {
        self.presenter.invalidate();
        self.shared_frame = None;
        self.retry = None;
        self.disable_shared(tx)?;
        tx.send(&PaneToServer::Action(PaneAction::Ack(FrameAck::Resync)))?;
        Ok(())
    }

    pub(crate) fn advance(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self
            .shared_frame
            .as_ref()
            .is_some_and(|shared| !shared.consumed)
        {
            if !self.pending() {
                self.finish(tx)?;
            }
            return Ok(());
        }
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let started = Instant::now();
            self.presenter.repaint_into(self.pending.buffer());
            if let Some(frame) = &mut self.frame {
                frame.encoded += started.elapsed();
                frame.bytes += self.pending.len();
            }
            self.retry = None;
            self.finish(tx)?;
        }
        Ok(())
    }

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

    fn poll_shared(&mut self, tx: &mut Outgoing) -> Result<()> {
        let Some(shared) = &mut self.shared_frame else {
            return Ok(());
        };
        if shared.consumed || self.frame.is_none() {
            return Ok(());
        }
        match shared.poll() {
            Consumption::Waiting(deadline) => self.retry = Some(deadline),
            Consumption::Complete => self.retry = None,
            Consumption::Expired => {
                tracing::warn!("upstream shared frame remained unread; using inline transport");
                self.disable_shared(tx)?;
                let shared = self.shared_frame.take().expect("unread frame is retained");
                match shared.into_band() {
                    Ok(band) => {
                        let started = Instant::now();
                        let outcome = self.presenter.present_into(band, self.pending.buffer());
                        if outcome == PresentOutcome::NeedsKeyframe
                            && let Some(frame) = &mut self.frame
                        {
                            frame.accepted = false;
                        }
                        if let Some(frame) = &mut self.frame {
                            frame.bytes += self.pending.len();
                            frame.encoded += started.elapsed();
                        }
                        self.retry = None;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "unread shared frame could not be recovered");
                        self.retry = None;
                        if let Some(frame) = &mut self.frame {
                            frame.accepted = false;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.pending() {
            return Ok(());
        }
        self.poll_shared(tx)?;
        if self.pending() || self.retry.is_some() {
            return Ok(());
        }
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        let ack = if frame.accepted {
            FrameAck::Retained
        } else {
            FrameAck::Resync
        };
        self.stats.frame(&frame, ack);
        tx.send(&PaneToServer::Action(PaneAction::Ack(ack)))?;
        Ok(())
    }
}
