//! Independent Wayland callback clock and single-in-flight pane frames.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use meowland_core::pixels::FrameSize;

use super::{Event, FRAME_INTERVAL, State, SurfaceAttributes, with_states};

/// Each pane owns its scheduling state. No separate dirty-pane index can drift
/// out of sync with attachment, resizing, or acknowledgements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presentation {
    Clean,
    Dirty,
    InFlight { dirty: bool },
}

pub struct PaneState {
    pub size: FrameSize,
    presentation: Presentation,
    shown: Vec<u8>,
    /// None means the retained pixels are not a valid basis for a row diff.
    shown_size: Option<FrameSize>,
}

impl PaneState {
    pub const fn new(size: FrameSize) -> Self {
        Self {
            size,
            presentation: Presentation::Clean,
            shown: Vec::new(),
            shown_size: None,
        }
    }

    pub const fn mark_dirty(&mut self) {
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
    fn frame(&mut self, pane: super::PaneId, pixels: &[u8]) -> Option<Event> {
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
    /// Wait for frame or activation deadlines; transfers wake through readiness
    /// sources.
    pub fn next_wakeup(&self, now: Instant) -> Option<Duration> {
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

    /// Pace visible clients at the output refresh rate and hidden clients at
    /// one Hz, independently of terminal acknowledgements.
    pub fn frame_callbacks(&mut self) {
        let now = Instant::now();
        let time = self.started.elapsed().as_millis() as u32;
        for (&id, window) in &mut self.windows {
            if !window.callback_pending || now < window.callback_due {
                continue;
            }
            let interval = if self.attachments.visible(id) {
                FRAME_INTERVAL
            } else {
                Duration::from_secs(1)
            };
            window.callback_due = now + interval;
            window.callback_pending = false;
            self.renderer
                .for_each_surface(window.surface.wl_surface(), |surface| {
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
                });
        }
    }

    pub fn resume_callbacks(&mut self, window: super::WindowId) {
        if self.attachments.visible(window)
            && let Some(window) = self.windows.get_mut(&window)
        {
            window.callback_due = window.callback_due.min(Instant::now() + FRAME_INTERVAL);
        }
    }

    pub fn pane_ack(&mut self, pane: super::PaneId, drawn: bool) {
        if let Some(entry) = self.attachments.panes.get_mut(&pane) {
            entry.presentation.ack(drawn);
        }
    }

    /// Borrow disjoint fields instead of allocating a list of due pane IDs.
    pub fn dispatch_frames(&mut self) {
        for (&pane, entry) in &mut self.attachments.panes {
            if !entry.presentation.due() {
                continue;
            }
            let root = entry
                .window()
                .and_then(|id| self.windows.get(&id))
                .map(|window| window.surface.wl_surface());
            let pixels = self
                .renderer
                .render(root, &self.snapshots, entry.presentation.size);
            if let Some(frame) = entry.presentation.frame(pane, pixels) {
                let _ = self.events.send(frame);
            }
        }
    }
}
