//! Pane frame presentation, acknowledgement, and throughput accounting.

use std::time::{Duration, Instant};

use meowland_core::Result;
use meowland_kitty::{Presenter, SharedMemory};
use meowland_protocol::{PaneToServer, asynchronous::Outgoing};
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

    fn frame(&mut self, bytes: usize, dropped: bool, encoded: Duration, written: Duration) {
        self.frames += 1;
        self.drawn += u32::from(bytes > 0);
        self.dropped += u32::from(dropped);
        self.bytes += bytes as u64;
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
pub struct PanePresentation {
    presenter: Presenter,
    stats: PaneStats,
    pending: output::Output,
    frame: Option<PendingFrame>,
}

struct PendingFrame {
    started: Instant,
    encoded: Duration,
    bytes: usize,
    dropped: bool,
}

impl PanePresentation {
    pub fn new(cell: Option<(u16, u16)>, shared: Option<SharedMemory>) -> Self {
        Self {
            presenter: Presenter::new(cell, shared),
            stats: PaneStats::new(),
            pending: output::Output::default(),
            frame: None,
        }
    }

    pub const fn pending(&self) -> bool {
        self.pending.pending()
    }

    pub fn append(&mut self, bytes: &[u8]) {
        self.pending.append(bytes);
    }

    /// Scratch space for terminal output that is not part of a frame.
    pub const fn buffer(&mut self) -> &mut Vec<u8> {
        self.pending.buffer()
    }

    pub fn set_cell_size(&mut self, cell: Option<(u16, u16)>) {
        self.presenter.set_cell_size(cell);
    }

    pub fn draw(
        &mut self,
        width: u32,
        height: u32,
        y: u32,
        rgb: Vec<u8>,
        tx: &mut Outgoing,
    ) -> Result<()> {
        debug_assert!(!self.pending());
        let encode_started = Instant::now();
        self.presenter
            .present_into(width, height, y, rgb, self.pending.buffer());
        self.frame = Some(PendingFrame {
            encoded: encode_started.elapsed(),
            started: Instant::now(),
            bytes: self.pending.len(),
            dropped: self.presenter.dropped(),
        });
        self.finish(tx)
    }

    pub async fn drain(&mut self, stdout: &mut (impl AsyncWrite + Unpin)) -> Result<()> {
        self.pending.drain(stdout).await?;
        Ok(())
    }

    pub fn finish(&mut self, tx: &mut Outgoing) -> Result<()> {
        if self.pending() {
            return Ok(());
        }
        let Some(frame) = self.frame.take() else {
            return Ok(());
        };
        self.stats.frame(
            frame.bytes,
            frame.dropped,
            frame.encoded,
            frame.started.elapsed(),
        );
        tx.send(&PaneToServer::Ack {
            drawn: !frame.dropped,
        })?;
        Ok(())
    }
}
