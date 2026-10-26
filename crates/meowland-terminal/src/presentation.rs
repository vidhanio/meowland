//! Pane frame presentation, acknowledgement, and throughput accounting.

use std::time::{Duration, Instant};

use meowland_core::Result;
use meowland_kitty::{PresentOutcome, Presenter, SharedMemory};
use meowland_protocol::asynchronous::Outgoing;
use meowland_protocol::{FrameAck, FrameBand, PaneAction, PaneToServer};
use tokio::io::AsyncWrite;

use super::output;

/// Log pane throughput to the user journal once per second.
struct PaneStats {
    window: Instant,
    frames: u32,
    drawn: u32,
    bytes: u64,
    encoded: Duration,
    written: Duration,
    dropped: u32,
}

impl PaneStats {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            frames: 0,
            drawn: 0,
            bytes: 0,
            encoded: Duration::ZERO,
            written: Duration::ZERO,
            dropped: 0,
        }
    }

    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "throughput is approximate telemetry; byte accounting itself remains integer-valued"
    )]
    fn frame(&mut self, bytes: usize, ack: FrameAck, encoded: Duration, written: Duration) {
        self.frames += 1;
        self.drawn += u32::from(bytes > 0);
        self.dropped += u32::from(ack == FrameAck::Resync);
        self.bytes += u64::try_from(bytes).expect("encoded frame length fits u64");
        self.encoded += encoded;
        self.written += written;
        let elapsed = self.window.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        let elapsed = elapsed.as_secs_f64();
        let frames = std::mem::take(&mut self.frames);
        let drawn = std::mem::take(&mut self.drawn);
        let bytes = std::mem::take(&mut self.bytes);
        let encoded = std::mem::take(&mut self.encoded);
        let written = std::mem::take(&mut self.written);
        let dropped = std::mem::take(&mut self.dropped);
        self.window = Instant::now();
        tracing::info!(
            fps = f64::from(frames) / elapsed,
            drawn,
            dropped,
            unchanged = frames.saturating_sub(drawn).saturating_sub(dropped),
            mb_per_second = bytes as f64 / elapsed / 1e6,
            encode_ms = encoded.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            write_ms = written.as_secs_f64() * 1000.0 / f64::from(frames.max(1)),
            "pane throughput"
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
}

struct PendingFrame {
    started: Instant,
    encoded: Duration,
    bytes: usize,
    accepted: bool,
}

impl PanePresentation {
    pub(crate) fn new(cell: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            presenter: Presenter::new(cell, shared),
            stats: PaneStats::new(),
            pending: output::Output::default(),
            frame: None,
            retry: None,
        }
    }

    pub(crate) const fn pending(&self) -> bool {
        self.pending.pending()
    }

    pub(crate) fn append(&mut self, bytes: &[u8]) {
        self.pending.append(bytes);
    }

    /// Scratch space for terminal output that is not part of a frame.
    pub(crate) const fn buffer(&mut self) -> &mut Vec<u8> {
        self.pending.buffer()
    }

    pub(crate) fn set_cell_size(&mut self, cell: Option<(u16, u16)>) {
        if self.presenter.set_cell_size(cell) {
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
        let outcome = self.presenter.present_into(band, self.pending.buffer());
        self.frame = Some(PendingFrame {
            encoded: started.elapsed(),
            started,
            bytes: self.pending.len(),
            accepted: outcome != PresentOutcome::NeedsKeyframe,
        });
        self.schedule(outcome);
        self.finish(tx)
    }

    pub(crate) fn advance(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self
            .deadline()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            let started = Instant::now();
            let outcome = self.presenter.repaint_into(self.pending.buffer());
            if let Some(frame) = &mut self.frame {
                frame.encoded += started.elapsed();
                frame.bytes += self.pending.len();
            }
            self.schedule(outcome);
            self.finish(tx)?;
        }
        Ok(())
    }

    const fn schedule(&mut self, outcome: PresentOutcome) {
        self.retry = match outcome {
            PresentOutcome::RetryAt(deadline) => Some(deadline),
            PresentOutcome::Unchanged | PresentOutcome::Encoded | PresentOutcome::NeedsKeyframe => {
                None
            }
        };
    }

    pub(crate) async fn drain(&mut self, stdout: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        self.pending.drain(stdout).await?;
        Ok(())
    }

    pub(crate) fn finish(&mut self, tx: &mut Outgoing) -> Result<()> {
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
        self.stats
            .frame(frame.bytes, ack, frame.encoded, frame.started.elapsed());
        tx.send(&PaneToServer::Action(PaneAction::Ack(ack)))?;
        Ok(())
    }
}
