//! Independent Wayland callback clock and single-in-flight pane frames.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;
use meowland_protocol::{FrameAck, FrameBand, FrameTransport, SharedObject};

mod shared;

use super::{Event, ServerToPane, State};

/// Each pane owns its scheduling state. No separate dirty-pane index can drift
/// out of sync with attachment, resizing, or acknowledgements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Clean,
    Dirty,
    InFlight { dirty: bool },
}

pub(crate) struct PaneState {
    pub size: FrameSize,
    presentation: Presentation,
    shown: Vec<u8>,
    /// None means the retained pixels are not a valid basis for a row diff.
    shown_size: Option<FrameSize>,
    pub(super) transport: FrameTransport,
    shared: Option<SharedObject>,
}

impl PaneState {
    pub(crate) const fn new(size: FrameSize) -> Self {
        Self {
            size,
            presentation: Presentation::Clean,
            shown: Vec::new(),
            shown_size: None,
            transport: FrameTransport::Inline,
            shared: None,
        }
    }

    pub(crate) const fn mark_dirty(&mut self) {
        self.presentation = match self.presentation {
            Presentation::InFlight { .. } => Presentation::InFlight { dirty: true },
            Presentation::Clean | Presentation::Dirty => Presentation::Dirty,
        };
    }

    fn ack(&mut self, ack: FrameAck) {
        let Presentation::InFlight { dirty } = self.presentation else {
            return;
        };
        self.shared = None;
        let resync = matches!(ack, FrameAck::Resync);
        if resync {
            self.shown_size = None;
        }
        self.presentation = if dirty || resync {
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
    fn frame(&mut self, pixels: &[u8]) -> Option<ServerToPane> {
        self.presentation = Presentation::Clean;
        let stride = self.size.rgb_stride();
        let rows = if self.shown_size == Some(self.size) {
            self.changed_rows(pixels, stride)?
        } else {
            0..pixels.len() / stride
        };
        let bytes = rows.start * stride..rows.end * stride;
        self.shown.resize(pixels.len(), 0);
        self.shown[bytes.clone()].copy_from_slice(&pixels[bytes.clone()]);
        self.shown_size = Some(self.size);
        self.presentation = Presentation::InFlight { dirty: false };
        Some(ServerToPane::Frame(
            FrameBand::new(
                self.size,
                u32::try_from(rows.start).expect("row offset fits validated frame height"),
                pixels[bytes].to_vec(),
            )
            .expect("composed row band fits its frame"),
        ))
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
    /// Wait for frame or activation deadlines; transfers wake through readiness
    /// sources.
    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if self
            .attachments
            .panes
            .values()
            .any(|pane| pane.presentation.due())
        {
            return Some(Duration::ZERO);
        }
        self.next_activation_deadline()
            .map(|deadline| deadline.saturating_duration_since(now))
            .into_iter()
            .chain(
                self.windows
                    .values()
                    .filter(|window| window.callback_pending)
                    .map(|window| window.callback_due.saturating_duration_since(now)),
            )
            .min()
    }

    /// Pace visible clients at the configured refresh rate and hidden clients
    /// at one Hz, independently of terminal acknowledgements.
    pub(crate) fn frame_callbacks(&mut self) {
        let now = Instant::now();
        let time = self.started.elapsed();
        for (&id, window) in &mut self.windows {
            if !window.callback_pending || now < window.callback_due {
                continue;
            }
            let interval = if self.attachments.visible(id) && self.catalog.drawable(id) {
                self.refresh_rate.interval()
            } else {
                Duration::from_secs(1)
            };
            let late = now
                .saturating_duration_since(window.callback_due)
                .as_nanos();
            let ticks = late / interval.as_nanos() + 1;
            window.callback_due += interval * u32::try_from(ticks).unwrap_or(u32::MAX);
            window.callback_pending = false;
            window.send_frame(&self.output, time);
        }
    }

    pub(crate) fn resume_callbacks(&mut self, window: super::WindowId) {
        if self.attachments.visible(window)
            && let Some(window) = self.windows.get_mut(&window)
        {
            window.callback_due = window
                .callback_due
                .min(Instant::now() + self.refresh_rate.interval());
        }
    }

    pub(crate) fn pane_ack(&mut self, pane: super::PaneId, ack: FrameAck) {
        if let Some(entry) = self.attachments.panes.get_mut(&pane) {
            entry.presentation.ack(ack);
        }
    }

    /// Borrow disjoint fields instead of allocating a list of due pane IDs.
    pub(crate) fn dispatch_frames(&mut self) {
        for (&pane, entry) in &mut self.attachments.panes {
            if !entry.presentation.due() {
                continue;
            }
            let root = entry
                .window()
                .filter(|&id| self.catalog.drawable(id))
                .and_then(|id| self.windows.get(&id))
                .map(|window| window.surface.wl_surface());
            let message = if entry.presentation.transport == FrameTransport::Shared {
                shared::render(
                    &mut entry.presentation,
                    &mut self.renderer,
                    root,
                    &self.surface_cache.snapshots,
                    self.scale.factor(),
                )
            } else {
                let pixels = self.renderer.render_scaled(
                    root,
                    &self.surface_cache.snapshots,
                    entry.presentation.size,
                    self.scale.factor(),
                );
                entry.presentation.frame(pixels)
            };
            if let Some(message) = message {
                self.events.push(Event { pane, message });
            }
        }
    }
}
