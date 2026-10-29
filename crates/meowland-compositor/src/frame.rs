//! Independent Wayland callback clocks and damage-aware retained pane frames.

use std::time::{Duration, Instant};

use meowland_core::{latency::Latency, pixels::FrameSize};
use meowland_protocol::{FrameAck, FrameBand, FrameTransport, SharedObject};
use meowland_render::{Renderer, Scene, Snapshot, SurfaceDamage};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

use super::{Event, PaneId, ServerToPane, State};

mod publication;

use publication::Publication;

#[derive(Clone, Copy, Debug)]
enum Presentation {
    Clean,
    Dirty,
    InFlight { changed: Instant, sent: Instant },
}

struct FrameStats {
    window: Instant,
    frames: u32,
    render: Latency,
    publish: Latency,
    change_to_render: Latency,
    send_to_ack: Latency,
    change_to_ack: Latency,
}

impl FrameStats {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            frames: 0,
            render: Latency::default(),
            publish: Latency::default(),
            change_to_render: Latency::default(),
            send_to_ack: Latency::default(),
            change_to_ack: Latency::default(),
        }
    }

    fn ack(&mut self, pane: PaneId, changed: Instant, sent: Instant) {
        let now = Instant::now();
        self.send_to_ack.record(now.saturating_duration_since(sent));
        self.change_to_ack
            .record(now.saturating_duration_since(changed));
        self.frames += 1;
        if now.duration_since(self.window) >= Duration::from_secs(1) {
            tracing::debug!(
                ?pane,
                frames = self.frames,
                render_us = ?self.render.summarize(),
                publish_us = ?self.publish.summarize(),
                change_to_render_us = ?self.change_to_render.summarize(),
                send_to_ack_us = ?self.send_to_ack.summarize(),
                change_to_ack_us = ?self.change_to_ack.summarize(),
                "pane latency; acknowledgement measures retention, not display presentation"
            );
            self.frames = 0;
            self.window = now;
        }
    }
}

pub(crate) struct PaneState {
    pub size: FrameSize,
    presentation: Presentation,
    changed: Option<Instant>,
    scene: Scene,
    sent_size: Option<FrameSize>,
    pub(super) transport: FrameTransport,
    shared: Option<SharedObject>,
    stats: FrameStats,
}

impl PaneState {
    pub(crate) fn new(size: FrameSize) -> Self {
        Self {
            size,
            presentation: Presentation::Clean,
            changed: None,
            scene: Scene::default(),
            sent_size: None,
            transport: FrameTransport::Inline,
            shared: None,
            stats: FrameStats::new(),
        }
    }

    fn schedule(&mut self, changed: Instant) {
        self.changed = Some(
            self.changed
                .map_or(changed, |previous| previous.min(changed)),
        );
        if !matches!(self.presentation, Presentation::InFlight { .. }) {
            self.presentation = Presentation::Dirty;
        }
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.scene.invalidate();
        self.schedule(Instant::now());
    }

    pub(crate) fn damage(&mut self, surface: &WlSurface, damage: SurfaceDamage, changed: Instant) {
        self.scene.damage(surface, damage);
        self.schedule(changed);
    }

    pub(crate) fn ack(&mut self, pane: PaneId, ack: FrameAck) {
        let Presentation::InFlight { changed, sent } = self.presentation else {
            return;
        };
        self.stats.ack(pane, changed, sent);
        self.shared = None;
        self.presentation = Presentation::Clean;
        if ack == FrameAck::Resync {
            self.sent_size = None;
            self.mark_dirty();
        } else if self.changed.is_some() {
            self.presentation = Presentation::Dirty;
        }
    }

    pub(crate) const fn due(&self) -> bool {
        matches!(self.presentation, Presentation::Dirty)
    }

    fn frame(
        &mut self,
        renderer: &mut Renderer,
        root: Option<&WlSurface>,
        snapshots: &std::collections::HashMap<WlSurface, Snapshot>,
        scale: f64,
    ) -> Option<ServerToPane> {
        let started = Instant::now();
        let changed = self.changed.take().unwrap_or(started);
        self.presentation = Presentation::Clean;
        let mut publication = Publication::new(self.size, self.transport, self.sent_size);
        let rows = renderer.update(
            &mut self.scene,
            root,
            snapshots,
            self.size,
            scale,
            |changed, ready| publication.rows(changed, ready),
        );
        let rows = if self.sent_size == Some(self.size) {
            rows?
        } else {
            0..usize::try_from(self.size.height()).expect("validated height fits usize")
        };
        self.stats
            .render
            .record(started.elapsed().saturating_sub(publication.elapsed));
        self.stats
            .change_to_render
            .record(started.saturating_duration_since(changed));
        self.sent_size = Some(self.size);
        let publishing = Instant::now();
        let streamed = publication.elapsed;
        let object = publication.finish(self.scene.pixels());
        self.transport = publication.transport;
        let message = if let Some(object) = object {
            let frame = *object.frame();
            self.shared = Some(object);
            ServerToPane::SharedFrame(frame)
        } else {
            self.band(rows)
        };
        self.stats.publish.record(streamed + publishing.elapsed());
        self.presentation = Presentation::InFlight {
            changed,
            sent: Instant::now(),
        };
        Some(message)
    }

    fn band(&self, rows: std::ops::Range<usize>) -> ServerToPane {
        let bytes = rows.start * self.size.rgb_stride()..rows.end * self.size.rgb_stride();
        ServerToPane::Frame(
            FrameBand::new(
                self.size,
                u32::try_from(rows.start).expect("row offset fits validated height"),
                self.scene.pixels()[bytes].to_vec(),
            )
            .expect("composed row band fits its frame"),
        )
    }
}

impl State {
    /// Wait for frame or activation deadlines; transfers wake through readiness
    /// sources.
    pub(crate) fn next_wakeup(&self, now: Instant) -> Option<Duration> {
        if self.attachments.due_frames() > 0 {
            return Some(Duration::ZERO);
        }
        self.activation_deadline
            .map(|deadline| deadline.saturating_duration_since(now))
            .into_iter()
            .chain(
                self.callback_deadline
                    .map(|deadline| deadline.saturating_duration_since(now)),
            )
            .min()
    }

    /// Pace visible clients at the configured refresh rate and hidden clients
    /// at one Hz.
    pub(crate) fn frame_callbacks(&mut self) {
        let now = Instant::now();
        if self.callback_deadline.is_none_or(|deadline| now < deadline) {
            return;
        }
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
        self.refresh_callback_deadline();
    }

    /// Recompute the earliest pending callback after a window is added,
    /// removed, or acknowledges a frame.
    pub(crate) fn refresh_callback_deadline(&mut self) {
        self.callback_deadline = self
            .windows
            .values()
            .filter(|window| window.callback_pending)
            .map(|window| window.callback_due)
            .min();
    }

    /// Queue a frame callback at the window's paced deadline.
    pub(crate) fn request_callback(&mut self, window: super::WindowId) {
        let Some(entry) = self.windows.get_mut(&window) else {
            return;
        };
        entry.callback_pending = true;
        let due = entry.callback_due;
        self.callback_deadline = Some(
            self.callback_deadline
                .map_or(due, |deadline| deadline.min(due)),
        );
    }

    pub(crate) fn resume_callbacks(&mut self, window: super::WindowId) {
        if self.attachments.visible(window)
            && let Some(window) = self.windows.get_mut(&window)
        {
            window.callback_due = window
                .callback_due
                .min(Instant::now() + self.refresh_rate.interval());
            if window.callback_pending {
                let due = window.callback_due;
                self.callback_deadline = Some(
                    self.callback_deadline
                        .map_or(due, |deadline| deadline.min(due)),
                );
            }
        }
    }

    pub(crate) fn pane_ack(&mut self, pane: super::PaneId, ack: FrameAck) {
        self.attachments.ack(pane, ack);
    }

    pub(crate) fn dispatch_frames(&mut self) {
        if self.attachments.due_frames() == 0 {
            return;
        }
        for (&pane, entry) in self.attachments.iter_mut() {
            if !entry.presentation.due() {
                continue;
            }
            let root = entry
                .window()
                .filter(|&id| self.catalog.drawable(id))
                .and_then(|id| self.windows.get(&id))
                .map(|window| window.surface.wl_surface());
            if let Some(message) = entry.presentation.frame(
                &mut self.renderer,
                root,
                &self.surface_cache.snapshots,
                self.scale.factor(),
            ) {
                self.events.push(Event { pane, message });
            }
        }
        self.attachments.clear_due();
    }
}
