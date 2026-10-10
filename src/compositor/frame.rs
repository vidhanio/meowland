//! Independent Wayland callback clock and single-in-flight pane frames.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use super::{Event, FRAME_INTERVAL, State, SurfaceAttributes, TRANSFER_POLL_INTERVAL, with_states};
use crate::pixels::FrameSize;

/// Each pane owns its scheduling state. No separate dirty-pane index can drift
/// out of sync with attachment, resizing, or acknowledgements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Clean,
    Dirty,
    InFlight { dirty: bool },
}

pub(super) struct PaneState {
    pub(super) window: u64,
    pub(super) size: FrameSize,
    presentation: Presentation,
    shown: Vec<u8>,
    /// None means the retained pixels are not a valid basis for a row diff.
    shown_size: Option<FrameSize>,
}

impl PaneState {
    pub(super) fn new(window: u64, size: FrameSize) -> Self {
        Self {
            window,
            size,
            presentation: Presentation::Clean,
            shown: Vec::new(),
            shown_size: None,
        }
    }

    pub(super) const fn mark_dirty(&mut self) {
        self.presentation = match self.presentation {
            Presentation::InFlight { .. } => Presentation::InFlight { dirty: true },
            Presentation::Clean | Presentation::Dirty => Presentation::Dirty,
        };
    }

    const fn ack(&mut self, drawn: bool) {
        let Presentation::InFlight { dirty } = self.presentation else {
            return;
        };
        if !drawn {
            self.shown_size = None;
        }
        self.presentation = if dirty || !drawn {
            Presentation::Dirty
        } else {
            Presentation::Clean
        };
    }

    fn due(&self) -> bool {
        self.presentation == Presentation::Dirty
    }

    /// Record exactly the pixels sent, not a newer commit or an unacknowledged
    /// assumption. The allocation crossing threads is only the changed band.
    fn frame(&mut self, pane: u64, pixels: &[u8]) -> Option<Event> {
        self.presentation = Presentation::Clean;
        let stride = self.size.rgb_stride();
        let rows = if self.shown_size == Some(self.size) {
            self.changed_rows(pixels, stride)?
        } else {
            0..self.size.height() as usize
        };
        let bytes = rows.start * stride..rows.end * stride;
        self.shown.resize(pixels.len(), 0);
        self.shown[bytes.clone()].copy_from_slice(&pixels[bytes.clone()]);
        self.shown_size = Some(self.size);
        self.presentation = Presentation::InFlight { dirty: false };
        Some(Event::Frame {
            pane,
            width: self.size.width(),
            height: self.size.height(),
            y: rows.start as u32,
            rgb: pixels[bytes].to_vec(),
        })
    }

    fn changed_rows(&self, pixels: &[u8], stride: usize) -> Option<Range<usize>> {
        if self.shown.len() != pixels.len() {
            return Some(0..pixels.len() / stride);
        }
        let differs = |(old, new): (&[u8], &[u8])| old != new;
        let mut rows = self
            .shown
            .chunks_exact(stride)
            .zip(pixels.chunks_exact(stride));
        let first = rows.position(differs)?;
        let end = rows
            .rposition(differs)
            .map_or(first + 1, |last| first + last + 2);
        Some(first..end)
    }
}

impl State {
    /// Sleep until frame work is due; only unfinished transfers require
    /// polling.
    pub(super) fn next_wakeup(&self, now: Instant) -> Duration {
        let mut wait = if self.pending_imports.is_empty() && !self.clipboard.pending() {
            Duration::from_secs(60)
        } else {
            TRANSFER_POLL_INTERVAL
        };
        if let Some(deadline) = self.next_activation_deadline() {
            wait = wait.min(deadline.saturating_duration_since(now));
        }
        for window in self.windows.values() {
            wait = wait.min(window.callback_due.saturating_duration_since(now));
        }
        if self.panes.values().any(PaneState::due) {
            return Duration::ZERO;
        }
        wait
    }

    /// Run Wayland callbacks at ~120Hz, independently of terminal acks.
    pub(super) fn frame_callbacks(&mut self) {
        let now = Instant::now();
        let time = self.started.elapsed().as_millis() as u32;
        for window in self.windows.values_mut() {
            if now < window.callback_due {
                continue;
            }
            let late = now.duration_since(window.callback_due).as_nanos();
            let ticks = late / FRAME_INTERVAL.as_nanos() + 1;
            window.callback_due += FRAME_INTERVAL * u32::try_from(ticks).unwrap_or(u32::MAX);
            self.renderer.stack.rebuild(window.surface.wl_surface());
            for (surface, _) in &self.renderer.stack.surfaces {
                with_states(surface, |states| {
                    for callback in states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .frame_callbacks
                        .drain(..)
                    {
                        callback.done(time);
                    }
                });
            }
            self.renderer.stack.clear();
        }
    }

    pub(super) fn pane_ack(&mut self, pane: u64, drawn: bool) {
        if let Some(entry) = self.panes.get_mut(&pane) {
            entry.ack(drawn);
        }
    }

    /// Borrow disjoint fields instead of allocating a list of due pane IDs.
    pub(super) fn dispatch_frames(&mut self) {
        for (&pane, entry) in &mut self.panes {
            if !entry.due() {
                continue;
            }
            let pixels =
                self.renderer
                    .render(&self.windows, &self.snapshots, entry.window, entry.size);
            if let Some(frame) = entry.frame(pane, pixels) {
                let _ = self.events.send(frame);
            }
        }
    }
}
